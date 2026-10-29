//! 内置插件：`/plugins` 列出插件目录里装了哪些插件。
//!
//! 插件自己算不出「插件目录」在哪——它跑在**宿主进程**里，`current_exe()` 拿到的是
//! `fuyubot.exe`。所以目录由宿主在 `plugin_start` 的配置里给：
//! `{"plugins_dir": "<绝对路径>"}`；没给就退回 `plugins/`。
//!
//! 它列的是**插件目录里的文件**（装了哪些），不是「宿主进程里加载了哪些」——
//! 后者 ABI 还问不到（`fuyubot-abi` 没有 `list_plugins` 桥，见 docs/gaps.md）。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use fuyubot_abi::{HostApi, LogLevel, Plugin, define_plugin};
use fuyubot_simplify::{
    Command, CommandSubscription, SendMessageParams, Simplify, Text, on_command,
};

/// 插件动态库的扩展名（三种平台；不依赖 loader，插件不该拖宿主的依赖图）。
const LIBRARY_EXTENSIONS: [&str; 3] = ["dll", "so", "dylib"];

struct ManagePlugin {
    host: HostApi,
    subscription: Option<CommandSubscription>,
    /// 还在跑的异步发送（`stop()` 里 join 掉：卸载之后不能再跑库里的代码）。
    sending: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl Plugin for ManagePlugin {
    fn init(host: HostApi) -> Result<Self, String> {
        Ok(Self {
            host,
            subscription: None,
            sending: Arc::new(Mutex::new(Vec::new())),
        })
    }

    fn start(&mut self, config_json: &str) -> Result<(), String> {
        let host = self.host;
        let plugins_dir = plugins_dir_from(config_json);
        // 读自己的配置段 `[plugin.fuyubot.manage]`（经 ABI 的 get_config），没配就用默认值。
        // 这演示了「插件读全局配置」：只能读，写别的段会被宿主拒绝。
        let prefix = host
            .get_config("fuyubot.manage", "reply_prefix")
            .unwrap_or_else(|| "已安装 ".to_owned());
        let sending = Arc::clone(&self.sending);
        let subscription = on_command(host, move |command: &Command| {
            if !command.is("plugins") {
                return;
            }
            // 优先报**进程里真在跑的**（宿主预置的 `fuyubot.host.plugins` service），
            // 读不到才回退到目录清单——那条路只能看到「装了哪些文件」。
            let text = match running_plugins(host) {
                Some(report) => report,
                None => list_plugins(&plugins_dir, &prefix),
            };
            let params = SendMessageParams {
                text: Some(Text::plain(text)),
                reply_to_message_id: Some(command.message_id),
                ..SendMessageParams::default()
            };
            // 命令回调跑在接收循环线程上：用 then，别在回调里等结果。
            let handle =
                Simplify::new(host).send_message_then(command.chat_id, &params, move |result| {
                    if let Err(error) = result {
                        host.log(LogLevel::Error, &format!("manage 插件回复失败: {error}"));
                    }
                });
            if let Ok(mut sending) = sending.lock() {
                sending.push(handle);
            }
        });
        let subscription = subscription.ok_or_else(|| "订阅 updateNewMessage 失败".to_owned())?;
        self.subscription = Some(subscription);
        Ok(())
    }

    fn stop(&mut self) {
        self.subscription = None;
        let mut sending = self
            .sending
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        for handle in sending.drain(..) {
            let _ = handle.join();
        }
    }
}

/// 宿主给的插件目录；没给（或给了空串）就退回 `plugins/`。
fn plugins_dir_from(config_json: &str) -> PathBuf {
    serde_json::from_str::<serde_json::Value>(config_json)
        .ok()
        .and_then(|config| config.get("plugins_dir")?.as_str().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("plugins"))
}

/// 宿主预置的「插件清单」service 的名字（`fuyubot-loader` 的
/// `PLUGIN_REPORT_SERVICE`；这里写死一份是因为插件不该依赖 loader——那会把
/// 宿主的依赖图拖进插件）。
const REPORT_SERVICE: &str = "fuyubot.host.plugins";

/// 问宿主「进程里到底加载了哪些插件」。读不到就 `None`（回退到目录清单）。
///
/// 走的是 ABI 现成的 `get_service` + 方法调用，**没有新桥**：服务表按名字寻址，
/// 宿主把自己的清单也注册成一个服务，插件就当它是别人提供的服务来用。
fn running_plugins(host: HostApi) -> Option<String> {
    let json = host.call_service(REPORT_SERVICE, "report", "{}").ok()?;
    let value: serde_json::Value = serde_json::from_str(&json).ok()?;
    let plugins = value.get("plugins")?.as_array()?;
    if plugins.is_empty() {
        return Some("宿主进程里还没有插件在跑。".to_owned());
    }

    let mut active = Vec::new();
    let mut waiting = Vec::new();
    for entry in plugins {
        let name = entry.get("name").and_then(|v| v.as_str()).unwrap_or("?");
        match entry.get("state").and_then(|v| v.as_str()) {
            Some("active") => active.push(name.to_owned()),
            Some("pending") => {
                let missing: Vec<&str> = entry
                    .get("missing")
                    .and_then(|v| v.as_array())
                    .map(|items| items.iter().filter_map(|v| v.as_str()).collect())
                    .unwrap_or_default();
                waiting.push(format!("{name}（等 {}）", missing.join(", ")));
            }
            Some("failed") => {
                let reason = entry.get("reason").and_then(|v| v.as_str()).unwrap_or("?");
                waiting.push(format!("{name}（起不来：{reason}）"));
            }
            // `activating` 是瞬时态，不报——报了会让人以为它卡住了。
            _ => {}
        }
    }

    let mut text = format!("宿主里 {} 个插件在跑：\n", active.len());
    for name in &active {
        text.push_str(&format!("• {name}\n"));
    }
    if !waiting.is_empty() {
        text.push_str(&format!("\n还没起来 {} 个：\n", waiting.len()));
        for item in &waiting {
            text.push_str(&format!("• {item}\n"));
        }
    }
    text.push_str("\n（这是宿主进程里的真实状态，不是目录里的文件。）");
    Some(text)
}

/// 插件目录里的动态库（按文件名排序）。
fn list_plugins(dir: &Path, prefix: &str) -> String {
    let mut names: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let extension = path.extension().and_then(|extension| extension.to_str());
        if !extension.is_some_and(|extension| LIBRARY_EXTENSIONS.contains(&extension)) {
            continue;
        }
        if let Some(stem) = path.file_stem() {
            names.push(stem.to_string_lossy().into_owned());
        }
    }
    names.sort();

    if names.is_empty() {
        return format!("{} 里还没有插件。", dir.display());
    }
    let mut text = format!("{prefix}{} 个插件：\n", names.len());
    for name in names {
        text.push_str(&format!("• {name}\n"));
    }
    text.push_str("\n（插件目录里的文件；装卸用 `fuyubot plugin install`。）");
    text
}

define_plugin!(ManagePlugin, "fuyubot.manage");
