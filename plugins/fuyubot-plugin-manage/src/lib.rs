//! 内置插件：`/plugins` 列出**宿主进程里真在跑**的插件，外加已禁用的。
//!
//! 插件自己算不出「插件目录」在哪——它跑在**宿主进程**里，`current_exe()` 拿到的是
//! `fuyubot.exe`。所以目录由宿主在 `plugin_start` 的配置里给：
//! `{"plugins_dir": "<绝对路径>"}`；没给就退回 `plugins/`。
//!
//! 数据有两条：运行状态问宿主的 `fuyubot.host.plugins` service（真相在进程里）；
//! 展示名 / 介绍优先用宿主报告里并进来的 `display_name` / `description`
//! （来自插件自己的 `plugin_manifest` 或目录包的 `plugin.json`），报告里没有的
//! （比如脚本插件、老插件）再退回读 `plugins_dir` 下的 `plugin.json`。
//!
//! 每行格式：`名称（id）— 介绍`。老插件没有展示名就只写 id，没有介绍就省掉后半段。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use fuyubot_abi::{HostApi, LogLevel, Plugin, define_plugin};
use fuyubot_simplify::{Command, CommandSubscription, SendMessageParams, Text, on_command};

/// 插件动态库的扩展名（三种平台；不依赖 loader，插件不该拖宿主的依赖图）。
const LIBRARY_EXTENSIONS: [&str; 3] = ["dll", "so", "dylib"];

/// 禁用目录（与加载器契约一致：`plugins/.disabled/<id>/`）。
const DISABLED_DIR: &str = ".disabled";
/// 旧面板的禁用后缀（与加载器 `DISABLED_SUFFIX` 同一个值）。
const DISABLED_SUFFIX: &str = ".disable";
/// 脚本插件目录（`plugins/scripts/<id>/`）。
const SCRIPTS_DIR: &str = "scripts";
/// 分发格式 v1 的清单文件名。
const MANIFEST_FILE: &str = "plugin.json";
/// 一条介绍最多显示多少个字符（Telegram 单条上限 4096，长介绍会挤掉别的行）。
const DESCRIPTION_LIMIT: usize = 80;

struct ManagePlugin {
    host: HostApi,
    subscription: Option<CommandSubscription>,
    /// 还在跑的异步回复（`stop()` 里 join 掉：卸载之后不能再跑库里的代码）。
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
            let text = match running_plugins(host, &plugins_dir) {
                Some(report) => report,
                None => installed_plugins(&plugins_dir, &prefix),
            };
            let params = SendMessageParams {
                text: Some(Text::plain(text)),
                ..SendMessageParams::default()
            };
            // 命令回调跑在接收循环线程上：用 then（reply 会阻塞等 TDLib）。
            let handle = command.reply_then(host, &params, move |result| {
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

        // 经宿主的命令能力表登记（`fuyubot.host.commands`）：服务由宿主预置，
        // 不再依赖 help 插件先启动。
        match host.call_service(
            "fuyubot.host.commands",
            "register",
            r#"{"name":"plugins","description":"列出已安装和已禁用的插件"}"#,
        ) {
            Ok(reply) => host.log(
                LogLevel::Info,
                &format!("manage 插件已向宿主登记命令：{reply}"),
            ),
            Err(error) => host.log(
                LogLevel::Warn,
                &format!("manage 插件向宿主登记命令失败：{error}"),
            ),
        }

        Ok(())
    }

    /// `/plugins` 列表里显示的名字。
    fn display_name() -> &'static str {
        "插件管理"
    }

    /// `/plugins` 列表里那一行介绍。
    fn description() -> &'static str {
        "列出已安装和已禁用的插件"
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
///
/// 每行是 `名称（id）— 介绍`：展示名 / 介绍优先用报告里宿主并进来的
/// `display_name` / `description`（`fuyubot-app` 在装插件时写入），缺了再从
/// `plugins_dir` 下的 `plugin.json` 补（脚本插件走这条）。
fn running_plugins(host: HostApi, plugins_dir: &Path) -> Option<String> {
    let json = host.call_service(REPORT_SERVICE, "report", "{}").ok()?;
    let value: serde_json::Value = serde_json::from_str(&json).ok()?;
    let plugins = value.get("plugins")?.as_array()?;
    Some(render_report(plugins, plugins_dir))
}

/// 把宿主报告渲染成给人看的文本（纯函数，便于单测）。
fn render_report(plugins: &[serde_json::Value], plugins_dir: &Path) -> String {
    let disk = disk_metadata(plugins_dir);

    let mut active = Vec::new();
    let mut waiting = Vec::new();
    for entry in plugins {
        let id = entry.get("name").and_then(|v| v.as_str()).unwrap_or("?");
        let (display_name, description) = entry_metadata(entry, id, &disk);
        let line = label(id, &display_name, &description);
        match entry.get("state").and_then(|v| v.as_str()) {
            Some("active") => active.push(line),
            Some("pending") => {
                let missing: Vec<&str> = entry
                    .get("missing")
                    .and_then(|v| v.as_array())
                    .map(|items| items.iter().filter_map(|v| v.as_str()).collect())
                    .unwrap_or_default();
                waiting.push(format!("{line}（等 {}）", missing.join(", ")));
            }
            Some("failed") => {
                let reason = entry.get("reason").and_then(|v| v.as_str()).unwrap_or("?");
                waiting.push(format!("{line}（起不来：{reason}）"));
            }
            // `activating` 是瞬时态，不报——报了会让人以为它卡住了。
            _ => {}
        }
    }

    let mut text = if active.is_empty() {
        "宿主进程里还没有插件在跑。\n".to_owned()
    } else {
        format!("宿主里 {} 个插件在跑：\n", active.len())
    };
    for line in &active {
        text.push_str(&format!("• {line}\n"));
    }
    if !waiting.is_empty() {
        text.push_str(&format!("\n还没起来 {} 个：\n", waiting.len()));
        for line in &waiting {
            text.push_str(&format!("• {line}\n"));
        }
    }

    let disabled = disabled_plugins(plugins_dir);
    if !disabled.is_empty() {
        text.push_str(&format!("\n已禁用 {} 个（重启后生效）：\n", disabled.len()));
        for line in &disabled {
            text.push_str(&format!("• {line}\n"));
        }
    }
    text.push_str("\n（这是宿主进程里的真实状态；介绍来自插件自报的 manifest 或 plugin.json。）");
    text
}

/// 一个插件清单里跟展示有关的两个字段。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Meta {
    display_name: String,
    description: String,
}

impl Meta {
    /// 读 `<dir>/plugin.json`；不是插件包（没有 `id`）就 `None`。
    ///
    /// 只读文件，**不 dlopen**——禁用的插件也走这条路，不能因为「想显示名字」把它加载回来。
    fn read(dir: &Path) -> Option<(String, Meta)> {
        let text = std::fs::read_to_string(dir.join(MANIFEST_FILE)).ok()?;
        let value: serde_json::Value = serde_json::from_str(&text).ok()?;
        let id = value.get("id")?.as_str()?.trim();
        if id.is_empty() {
            return None;
        }
        let text_of = |key: &str| {
            value
                .get(key)
                .and_then(|value| value.as_str())
                .unwrap_or("")
                .trim()
                .to_owned()
        };
        Some((
            id.to_owned(),
            Meta {
                display_name: text_of("name"),
                description: text_of("description"),
            },
        ))
    }
}

/// 插件目录里所有包的 `id → 展示元信息`（包在 `plugins/<id>/`，脚本在
/// `plugins/scripts/<id>/`）。只读清单，不 dlopen。
fn disk_metadata(plugins_dir: &Path) -> BTreeMap<String, Meta> {
    let mut out = BTreeMap::new();
    for root in [plugins_dir.to_path_buf(), plugins_dir.join(SCRIPTS_DIR)] {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if !path.is_dir() || name.starts_with('.') || name.ends_with(DISABLED_SUFFIX) {
                continue;
            }
            if let Some((id, meta)) = Meta::read(&path) {
                out.entry(id).or_insert(meta);
            }
        }
    }
    out
}

/// 一条报告项最终用的展示名 / 介绍：报告里宿主并进来的优先，缺了再从磁盘清单补。
fn entry_metadata(
    entry: &serde_json::Value,
    id: &str,
    disk: &BTreeMap<String, Meta>,
) -> (String, String) {
    let from_report = |key: &str| {
        entry
            .get(key)
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .trim()
            .to_owned()
    };
    let mut display_name = from_report("display_name");
    let mut description = from_report("description");
    if let Some(meta) = disk.get(id) {
        if display_name.is_empty() {
            display_name = meta.display_name.clone();
        }
        if description.is_empty() {
            description = meta.description.clone();
        }
    }
    (display_name, description)
}

/// 已禁用的插件：`plugins/.disabled/<id>/`、`plugins/<id>.disable/` 与
/// `plugins/<stem>.dll.disable`。
///
/// 名字能读清单就读，读不到就用目录 / 文件名——**不 dlopen**（禁用就是不想让它跑）。
fn disabled_plugins(dir: &Path) -> Vec<String> {
    let mut rows: Vec<String> = Vec::new();

    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir.join(DISABLED_DIR)) {
        dirs.extend(
            entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.is_dir()),
        );
    }
    if let Ok(entries) = std::fs::read_dir(dir) {
        dirs.extend(entries.flatten().map(|entry| entry.path()).filter(|path| {
            path.is_dir()
                && path
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().ends_with(DISABLED_SUFFIX))
        }));
    }
    dirs.sort();
    dirs.dedup();

    for path in dirs {
        let disk_name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let fallback = disk_name
            .strip_suffix(DISABLED_SUFFIX)
            .unwrap_or(&disk_name)
            .to_owned();
        let line = match Meta::read(&path) {
            Some((id, meta)) => label(&id, &meta.display_name, &meta.description),
            None => label(&fallback, "", ""),
        };
        rows.push(format!("{line}（已禁用）"));
    }

    // 旧形态：`foo.dll.disable`（文件名里没有 id，只能显示文件主干）。
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Some(name) = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
            else {
                continue;
            };
            let Some(enabled) = name.strip_suffix(DISABLED_SUFFIX) else {
                continue;
            };
            let stem = Path::new(enabled)
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_else(|| enabled.to_owned());
            rows.push(format!("{}（已禁用）", label(&stem, "", "")));
        }
    }

    rows.sort();
    rows.dedup();
    rows
}

/// 一行的主体：`名称（id）— 介绍`。
///
/// 没有展示名（或展示名就等于 id）就只写 id；没有介绍就省掉破折号。
fn label(id: &str, display_name: &str, description: &str) -> String {
    let display_name = display_name.trim();
    let head = if display_name.is_empty() || display_name == id {
        id.to_owned()
    } else {
        format!("{display_name}（{id}）")
    };
    let description = one_line(description);
    if description.is_empty() {
        head
    } else {
        format!("{head}— {description}")
    }
}

/// 介绍压成一行并截断：Telegram 一条消息装不下长文，列表也不是详情页。
fn one_line(text: &str) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = text.chars();
    let head: String = chars.by_ref().take(DESCRIPTION_LIMIT).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

/// 读不到宿主报告时的兜底：只列**目录里**的插件（名称 / id / 介绍来自清单）。
fn installed_plugins(dir: &Path, prefix: &str) -> String {
    let mut rows = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return format!("{} 里还没有插件。", dir.display());
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        if path.is_file() {
            let extension = path.extension().and_then(|value| value.to_str());
            if extension.is_some_and(|extension| LIBRARY_EXTENSIONS.contains(&extension)) {
                rows.push(
                    path.file_stem()
                        .map(|stem| stem.to_string_lossy().into_owned())
                        .unwrap_or(name),
                );
            }
            continue;
        }
        if !path.is_dir() || name == SCRIPTS_DIR || name.ends_with(DISABLED_SUFFIX) {
            continue;
        }
        match Meta::read(&path) {
            Some((id, meta)) => rows.push(label(&id, &meta.display_name, &meta.description)),
            None => rows.push(name),
        }
    }

    if let Ok(entries) = std::fs::read_dir(dir.join(SCRIPTS_DIR)) {
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if !path.is_dir() || name.starts_with('.') || name.ends_with(DISABLED_SUFFIX) {
                continue;
            }
            match Meta::read(&path) {
                Some((id, meta)) => rows.push(label(&id, &meta.display_name, &meta.description)),
                None => rows.push(name),
            }
        }
    }

    rows.sort();
    if rows.is_empty() {
        return format!("{} 里还没有插件。", dir.display());
    }
    let mut text = format!("{prefix}{} 个插件：\n", rows.len());
    for row in rows {
        text.push_str(&format!("• {row}\n"));
    }
    text.push_str("\n（插件目录里的文件；装卸用 `fuyubot plugin install`。）");
    text
}

define_plugin!(ManagePlugin, "fuyubot.manage");

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试用的临时目录：放**工作区的 `target/` 里**，不用系统临时目录——
    /// 沙箱化跑测试时 `%TEMP%` 可能不可写（2026 实测：建目录直接 access denied）。
    fn temp(name: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/manage-tests")
            .join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        dir
    }

    fn write_manifest(bundle: &Path, id: &str, name: &str, description: &str) {
        std::fs::create_dir_all(bundle).expect("建包目录");
        std::fs::write(
            bundle.join(MANIFEST_FILE),
            format!(
                r#"{{"schema":1,"id":"{id}","name":"{name}","version":"1.0.0","kind":"native","entry":"x.dll","description":"{description}"}}"#
            ),
        )
        .expect("写清单");
    }

    fn write_bundle(dir: &Path, id: &str, name: &str, description: &str) {
        write_manifest(&dir.join(id), id, name, description);
    }

    /// 每行的主体是 `名称（id）— 介绍`；缺什么就省什么，但不能出现 `（id）` 重复。
    #[test]
    fn label_uses_display_name_id_and_description() {
        assert_eq!(
            label("fuyubot.manage", "插件管理", "列出插件"),
            "插件管理（fuyubot.manage）— 列出插件"
        );
        assert_eq!(label("old.plugin", "", ""), "old.plugin");
        // 展示名与 id 相同时不重复写两遍。
        assert_eq!(label("same", "same", ""), "same");
        assert_eq!(label("x.id", "名字", "  "), "名字（x.id）");
    }

    /// 介绍里的换行 / 多空格要压成一行；超长截断，不能把整条回复撑爆。
    #[test]
    fn one_line_collapses_whitespace_and_truncates() {
        assert_eq!(one_line("第一行\n第二行\t第三行"), "第一行 第二行 第三行");
        let long = "很".repeat(DESCRIPTION_LIMIT + 5);
        let line = one_line(&long);
        assert!(line.ends_with('…'), "{line}");
        assert_eq!(line.chars().count(), DESCRIPTION_LIMIT + 1);
    }

    /// 禁用的包：名字 / 介绍从 `.disabled/<id>/plugin.json` 读，不 dlopen。
    #[test]
    fn disabled_plugins_reads_bundle_manifests() {
        let dir = temp("disabled-bundles");
        write_bundle(
            &dir.join(DISABLED_DIR),
            "com.example.weather",
            "天气",
            "查天气",
        );
        // 旧面板的目录形态：`<id>.disable/`（清单就在这个目录根上）。
        write_manifest(
            &dir.join(format!("old.style{DISABLED_SUFFIX}")),
            "old.style",
            "旧样式",
            "",
        );

        let rows = disabled_plugins(&dir);
        assert!(
            rows.contains(&"天气（com.example.weather）— 查天气（已禁用）".to_owned()),
            "{rows:?}"
        );
        assert!(
            rows.contains(&"旧样式（old.style）（已禁用）".to_owned()),
            "{rows:?}"
        );
    }

    /// 旧形态的 `foo.dll.disable` 没有清单，只能显示文件主干（至少认得出是哪个）。
    #[test]
    fn disabled_plugins_lists_renamed_libraries() {
        let dir = temp("disabled-files");
        std::fs::write(dir.join("weather.dll.disable"), b"x").expect("写库文件");

        let rows = disabled_plugins(&dir);
        assert_eq!(rows, vec!["weather（已禁用）".to_owned()]);
    }

    /// 目录里的包（以及脚本包）能被索引成 `id → 展示元信息`。
    #[test]
    fn disk_metadata_reads_bundles_and_scripts() {
        let dir = temp("disk-meta");
        write_bundle(&dir, "com.example.weather", "天气", "查天气");
        write_bundle(&dir.join(SCRIPTS_DIR), "com.example.music", "音乐", "点歌");

        let meta = disk_metadata(&dir);
        assert_eq!(meta["com.example.weather"].display_name, "天气");
        assert_eq!(meta["com.example.weather"].description, "查天气");
        assert_eq!(meta["com.example.music"].display_name, "音乐");
    }

    /// 整段输出：运行的用 `名称（id）— 介绍`，等依赖 / 失败带原因，已禁用单列一段。
    #[test]
    fn render_report_formats_names_ids_and_descriptions() {
        let dir = temp("render");
        write_bundle(
            &dir.join(DISABLED_DIR),
            "com.example.weather",
            "天气",
            "查天气",
        );

        let plugins = vec![
            serde_json::json!({
                "name": "fuyubot.start",
                "state": "active",
                "display_name": "启动问候",
                "description": "回复 /start 的欢迎语",
            }),
            serde_json::json!({
                "name": "fuyubot.music",
                "state": "pending",
                "missing": ["fuyubot.host.td"],
                "display_name": "音乐",
                "description": "点歌",
            }),
            serde_json::json!({
                "name": "fuyubot.broken",
                "state": "failed",
                "reason": "plugin_start 返回 3",
            }),
        ];

        let text = render_report(&plugins, &dir);
        assert!(text.starts_with("宿主里 1 个插件在跑：\n"), "{text}");
        assert!(
            text.contains("• 启动问候（fuyubot.start）— 回复 /start 的欢迎语"),
            "{text}"
        );
        assert!(
            text.contains("• 音乐（fuyubot.music）— 点歌（等 fuyubot.host.td）"),
            "{text}"
        );
        assert!(
            text.contains("• fuyubot.broken（起不来：plugin_start 返回 3）"),
            "{text}"
        );
        assert!(
            text.contains("• 天气（com.example.weather）— 查天气（已禁用）"),
            "{text}"
        );
    }
}
