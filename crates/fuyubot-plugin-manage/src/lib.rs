//! 内置插件：`/plugins` 列出插件目录里装了哪些插件。
//!
//! 插件自己算不出「插件目录」在哪——它跑在**宿主进程**里，`current_exe()` 拿到的是
//! `fuyubot.exe`。所以目录由宿主在 `plugin_start` 的配置里给：
//! `{"plugins_dir": "<绝对路径>"}`；没给就退回 `plugins/`。
//!
//! 它列的是**插件目录里的文件**（装了哪些），不是「宿主进程里加载了哪些」——
//! 后者 ABI 还问不到（`fuyubot-abi` 没有 `list_plugins` 桥，见 docs/gaps.md）。

use std::path::{Path, PathBuf};

use fuyubot_abi::{HostApi, LogLevel, Plugin, define_plugin};
use fuyubot_simplify::{
    Command, CommandSubscription, SendMessageParams, Simplify, Text, on_command,
};

/// 插件动态库的扩展名（三种平台；不依赖 loader，插件不该拖宿主的依赖图）。
const LIBRARY_EXTENSIONS: [&str; 3] = ["dll", "so", "dylib"];

struct ManagePlugin {
    host: HostApi,
    subscription: Option<CommandSubscription>,
}

impl Plugin for ManagePlugin {
    fn init(host: HostApi) -> Result<Self, String> {
        Ok(Self {
            host,
            subscription: None,
        })
    }

    fn start(&mut self, config_json: &str) -> Result<(), String> {
        let host = self.host;
        let plugins_dir = plugins_dir_from(config_json);
        let subscription = on_command(host, move |command: &Command| {
            if !command.is("plugins") {
                return;
            }
            let params = SendMessageParams {
                text: Some(Text::plain(list_plugins(&plugins_dir))),
                reply_to_message_id: Some(command.message_id),
                ..SendMessageParams::default()
            };
            if let Err(error) = Simplify::new(host).send_message(command.chat_id, &params) {
                host.log(LogLevel::Error, &format!("manage 插件回复失败: {error}"));
            }
        });
        let subscription = subscription.ok_or_else(|| "订阅 updateNewMessage 失败".to_owned())?;
        self.subscription = Some(subscription);
        Ok(())
    }

    fn stop(&mut self) {
        self.subscription = None;
    }
}

/// 宿主给的插件目录；没给（或给了空串）就退回 `plugins/`。
fn plugins_dir_from(config_json: &str) -> PathBuf {
    serde_json::from_str::<serde_json::Value>(config_json)
        .ok()
        .and_then(|config| config.get("plugins_dir")?.as_str().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("plugins"))
}

/// 插件目录里的动态库（按文件名排序）。
fn list_plugins(dir: &Path) -> String {
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
    let mut text = format!("已安装 {} 个插件：\n", names.len());
    for name in names {
        text.push_str(&format!("• {name}\n"));
    }
    text.push_str("\n（插件目录里的文件；装卸用 `fuyubot plugin install`。）");
    text
}

define_plugin!(ManagePlugin, "fuyubot.manage");
