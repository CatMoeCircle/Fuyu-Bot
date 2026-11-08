//! `start(options_json)` 的解析与校验。
//!
//! 为什么是 JSON 而不是结构体：跨 ABI 传结构体要绑死字段顺序 / 对齐 / 指针；
//! JSON 加字段不破 ABI，也和本仓既有的「op JSON / 更新 JSON」习惯一致。
//!
//! 为什么**启动时不读 `fuyubot.toml`**：Android 上没有给人编辑配置文件的场景。
//! `telegram` / `plugins` 的唯一来源就是这次调用传进来的 JSON，多一份 schema 就多一处漂移。
//!
//! 但 `<root>/fuyubot.toml` 仍然会被读：它只承担「**可选的**用户配置层」——存在时只读回
//! `[plugin.*]` 一段，其它段不参与解析（`telegram` / `plugins` 以这份 options JSON 为准）。
//! 插件 `set_config` 的落盘在 `<root>/plugin-config/<插件名>.toml`（由 `fuyubot-loader` 管），
//! 所以这个文件**运行时不会被写**。
//! 优先级：options 的 `plugin_config` > `fuyubot.toml` 的插件段（显式启动参数压过文件）。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use fuyubot_app::{AppHost, AppOptions, LogLevel, TdlibSettings};
use fuyubot_tdlib::{AccountType, LOGIN_BUDGET, TdlibParameters};
use serde::Deserialize;
use serde_json::{Map, Value, json};

/// 可选的**用户配置层**文件名（与 CLI 的项目配置同名；运行时只读它、不写它）。
pub const CONFIG_FILE: &str = "fuyubot.toml";

/// `start` 的入参。字段语义见 `include/fuyubot_jni.h` 与设计文档第 9.3 节。
#[derive(Debug, Clone, Deserialize)]
pub struct Options {
    /// 项目根。**必填、绝对路径、必须是 app 私有目录**（`/sdcard` 是 noexec）。
    pub root: String,
    /// 插件目录，相对 `root`；缺省 `plugins`。
    #[serde(default)]
    pub plugin_dir: Option<String>,
    /// TDLib 数据目录，相对 `root`；缺省 `data`。
    #[serde(default)]
    pub data_dir: Option<String>,
    /// 插件日志门槛；缺省 `info`。
    #[serde(default)]
    pub log_level: Option<String>,
    /// Telegram 的 `api_id`（int32，与 `setTdlibParameters` 一致）。
    pub api_id: i32,
    /// `api_hash`。**不进配置树**（约束 41 的精神：凭据不落盘、不给插件看）。
    pub api_hash: String,
    /// Bot Token。有值就走 `AccountType::Bot`，全程不打扰 UI。
    #[serde(default)]
    pub bot_token: Option<String>,
    /// tdjson 所在目录（通常是 `applicationInfo.nativeLibraryDir`）。
    ///
    /// 缺省时不设环境变量：要求宿主先 `System.loadLibrary("tdjson")`，
    /// loader 的优先级 4（裸库名）会命中已加载的那个。
    #[serde(default)]
    pub lib_dir: Option<String>,
    /// 插件自己的配置段：插件名 → 该段的 JSON 对象。
    ///
    /// 点分的插件名直接写 `"fuyubot.manage"` 会自动展开成嵌套表（等价于 TOML 的
    /// `[plugin.fuyubot.manage]`）；写成嵌套对象也行，两者结果相同。
    ///
    /// 这是**本次启动的初始值**：`<root>/fuyubot.toml` 里已有的插件段先垫底，
    /// 这里给的键覆盖它。缺省 = 不额外给（只看文件里上次存的）。
    #[serde(default)]
    pub plugin_config: Option<Map<String, Value>>,
    /// TDLib 自己的日志详细度（0 起，越大越啰嗦）；缺省用 TDLib 默认（1）。
    #[serde(default)]
    pub tdlib_log_verbosity: Option<i32>,
}

impl Options {
    /// 解析 + 校验。任何一条不满足都在这里说清楚，别等到建完客户端才报。
    pub fn parse(json: &str) -> Result<Self, String> {
        let options: Self = serde_json::from_str(json)
            .map_err(|error| format!("options JSON 解析失败：{error}"))?;

        if !Path::new(&options.root).is_absolute() {
            return Err(format!(
                "root 必须是绝对路径（Android 上 cwd 是 `/`），收到 {:?}",
                options.root
            ));
        }
        if options.api_id <= 0 {
            return Err(format!("api_id 必须是正整数，收到 {}", options.api_id));
        }
        if options.api_hash.trim().is_empty() {
            return Err("api_hash 不能为空".to_owned());
        }
        if let Some(raw) = &options.log_level {
            parse_log_level(raw)?;
        }
        if let Some(sections) = &options.plugin_config {
            for (name, section) in sections {
                // 插件名是点路径（宿主按 `.` 逐段下钻），空段会让查找永远落空。
                if name.is_empty() || name.split('.').any(str::is_empty) {
                    return Err(format!(
                        "plugin_config 的插件名 {name:?} 不合法：不能为空，也不能以点开头/结尾或出现连续点"
                    ));
                }
                if !section.is_object() {
                    return Err(format!(
                        "plugin_config.{name} 必须是一个 JSON 对象（一段配置），收到 {section}"
                    ));
                }
            }
        }
        Ok(options)
    }

    pub fn root_path(&self) -> PathBuf {
        PathBuf::from(&self.root)
    }

    pub fn data_path(&self) -> PathBuf {
        self.root_path()
            .join(self.data_dir.as_deref().unwrap_or("data"))
    }

    pub fn plugin_path(&self) -> PathBuf {
        self.root_path()
            .join(self.plugin_dir.as_deref().unwrap_or("plugins"))
    }

    /// 显式给了 `lib_dir` 且它真是目录才算数（APK 内路径 `…/base.apk!/lib/…` 不是目录）。
    pub fn tdjson_dir(&self) -> Option<&str> {
        self.lib_dir
            .as_deref()
            .filter(|dir| !dir.trim().is_empty())
            .filter(|dir| Path::new(dir).is_dir())
    }

    pub fn log_level(&self) -> LogLevel {
        // `parse` 已经验过；这里再解析一次不会失败，失败也只能退回 Info。
        self.log_level
            .as_deref()
            .and_then(|raw| parse_log_level(raw).ok())
            .unwrap_or(LogLevel::Info)
    }

    /// `setTdlibParameters` 的参数。设备名带上 `android`，在 Telegram 的「设备」列表里一眼能认。
    pub fn parameters(&self, data_dir: &Path) -> TdlibParameters {
        let mut parameters =
            TdlibParameters::new(data_dir.to_path_buf(), self.api_id, self.api_hash.clone());
        parameters.device_model = "fuyubot-android".to_owned();
        parameters
    }

    /// **用户配置层**的路径（可选：存在时只读回 `[plugin.*]`）。
    ///
    /// 宿主运行时也拿它的父目录当插件运行时落盘的根（`<root>/plugin-config/`），
    /// 所以这个路径必须是绝对路径、且目录可写。
    pub fn config_path(&self) -> PathBuf {
        self.root_path().join(CONFIG_FILE)
    }

    /// 装进宿主运行时的配置树（插件经 ABI 的 `get_config` 读它）。
    ///
    /// 只放**非凭据**：`api_hash` / `bot_token` 一律不进去（插件不该拿到它们）。
    /// 形状照 `fuyubot.toml` 的 `[telegram]` / `[plugins]`，插件的既有写法不用改。
    pub fn config_json(&self) -> serde_json::Value {
        json!({
            "telegram": {
                "api_id": self.api_id,
                "data_dir": self.data_dir.clone().unwrap_or_else(|| "data".to_owned()),
                "account": if self.bot_token.is_some() { "bot" } else { "user" },
            },
            "plugins": {
                "dir": self.plugin_dir.clone().unwrap_or_else(|| "plugins".to_owned()),
                "log_level": log_level_name(self.log_level()),
            },
            "plugin": self.plugin_tree(),
        })
    }

    /// `plugin` 子树 = 文件里的用户配置 ⊕ 这次 JSON 给的（后者覆盖前者，逐键深合并）。
    ///
    /// 为什么文件垫底、JSON 压顶：`plugin_config` 是**显式启动参数**（壳从用户设置里读的），
    /// 用户刚改的值必须生效；`<root>/fuyubot.toml` 只在这次没给同名键时兜底。
    /// 插件自己的运行时落盘（`plugin-config/`）再低一层，由宿主运行时在装配置时合并。
    fn plugin_tree(&self) -> Value {
        let mut tree = persisted_plugin_sections(&self.config_path());
        if let Some(sections) = &self.plugin_config {
            for (name, section) in sections {
                insert_dotted(&mut tree, name, section.clone());
            }
        }
        tree
    }

    /// 翻译成编排层的输入：目录、配置树、TDLib 设置、交互出口。
    ///
    /// 这就是「Android 与 CLI 的差别只剩这一段」的地方——剩下的顺序全在 `fuyubot-app`。
    pub fn app_options(&self, host: Arc<dyn AppHost>) -> AppOptions {
        let data_dir = self.data_path();
        AppOptions {
            data_dir: data_dir.clone(),
            plugin_dir: self.plugin_path(),
            log_level: self.log_level(),
            log_plugins: Vec::new(),
            config: self.config_json(),
            // 给宿主一个配置根：插件 `set_config` 落盘到它旁边的 `plugin-config/`。
            config_path: Some(self.config_path()),
            tdjson_path: self.tdjson_dir().map(PathBuf::from),
            tdlib: Some(TdlibSettings {
                parameters: self.parameters(&data_dir),
                account: if self.bot_token.is_some() {
                    AccountType::Bot
                } else {
                    AccountType::User
                },
                bot_token: self.bot_token.clone(),
                log_verbosity: self.tdlib_log_verbosity,
                login_budget: LOGIN_BUDGET,
                // 总要走一次登录：有 session 就是「直接 Ready」，没有就走交互。
                login: true,
            }),
            host,
        }
    }
}

/// 读 `<root>/fuyubot.toml` 里的 `[plugin.*]` 段并转成 JSON。
///
/// 文件不存在是**正常**的（还没人写过配置）；存在但读不了 / 不是合法 TOML 要留一条警告——
/// 静默当空表会让用户以为「插件把自己刚写的设置吃了」。
fn persisted_plugin_sections(path: &Path) -> Value {
    if !path.exists() {
        return json!({});
    }
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) => {
            tracing::warn!(
                target: "fuyubot::jni",
                "读不了插件配置 {}：{error}（本次按空配置启动）",
                path.display()
            );
            return json!({});
        }
    };
    let value = match toml::from_str::<toml::Value>(&text) {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(
                target: "fuyubot::jni",
                "{} 不是合法的 TOML：{error}（本次按空配置启动）",
                path.display()
            );
            return json!({});
        }
    };
    let sections = value.get("plugin").and_then(toml::Value::as_table);
    match sections {
        Some(table) => serde_json::to_value(table).unwrap_or_else(|error| {
            tracing::warn!(
                target: "fuyubot::jni",
                "{} 的 [plugin.*] 段转不成 JSON：{error}（本次按空配置启动）",
                path.display()
            );
            json!({})
        }),
        None => json!({}),
    }
}

/// 把点分插件名插进 `plugin` 子树：`fuyubot.manage` → `{"fuyubot": {"manage": …}}`。
///
/// 中间层已存在就往下走（允许 `{"a": {…}}` 与 `"a.b"` 同时出现）；中间层不是对象时换掉它。
/// 叶子已存在且两边都是对象时**逐键深合并**——JSON 里没提的键要留给文件里的值。
fn insert_dotted(tree: &mut Value, name: &str, section: Value) {
    match name.split_once('.') {
        None => {
            if let Some(object) = tree.as_object_mut() {
                match object.get_mut(name) {
                    Some(existing) if existing.is_object() && section.is_object() => {
                        merge_json(existing, &section);
                    }
                    _ => {
                        object.insert(name.to_owned(), section);
                    }
                }
            }
        }
        Some((head, rest)) => {
            if let Some(object) = tree.as_object_mut() {
                let child = object.entry(head.to_owned()).or_insert_with(|| json!({}));
                if !child.is_object() {
                    *child = json!({});
                }
                insert_dotted(child, rest, section);
            }
        }
    }
}

/// 深合并：`src` 里出现的键覆盖 `dst`，对象逐层递归，没提到的键原样留着。
fn merge_json(dst: &mut Value, src: &Value) {
    match (dst, src) {
        (Value::Object(dst), Value::Object(src)) => {
            for (key, value) in src {
                match dst.get_mut(key) {
                    Some(slot) => merge_json(slot, value),
                    None => {
                        dst.insert(key.clone(), value.clone());
                    }
                }
            }
        }
        (dst, src) => *dst = src.clone(),
    }
}

/// 与 CLI 的 `scaffold::parse_log_level` 认同一套写法（那边是权威，改那里要同步这里）。
///
/// `pub(crate)`：`run` 的运行期日志控制文件也用它解析（同一个写法）。
pub(crate) fn parse_log_level(raw: &str) -> Result<LogLevel, String> {
    match raw.to_ascii_lowercase().as_str() {
        "error" => Ok(LogLevel::Error),
        "warn" | "warning" => Ok(LogLevel::Warn),
        "info" => Ok(LogLevel::Info),
        "debug" => Ok(LogLevel::Debug),
        "trace" => Ok(LogLevel::Trace),
        _ => Err(format!(
            "log_level = {raw:?} 认不出来，只能写 error / warn / info / debug / trace"
        )),
    }
}

fn log_level_name(level: LogLevel) -> &'static str {
    match level {
        LogLevel::Error => "error",
        LogLevel::Warn => "warn",
        LogLevel::Info => "info",
        LogLevel::Debug => "debug",
        LogLevel::Trace => "trace",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试里的 root：**必须按平台给绝对路径**——Windows 上 `/data/...` 不是绝对路径
    /// （没有盘符），而校验本身是对的（Android 上它确实是绝对的）。
    fn root() -> &'static str {
        if cfg!(windows) {
            "C:/data/user/0/com.example.bot/files"
        } else {
            "/data/user/0/com.example.bot/files"
        }
    }

    fn full() -> String {
        format!(
            r#"{{
                "root": "{}",
                "api_id": 12345,
                "api_hash": "0123456789abcdef",
                "bot_token": "1:AAA"
            }}"#,
            root()
        )
    }

    #[test]
    fn parses_the_minimum_and_fills_defaults() {
        let options = Options::parse(&full()).unwrap();
        // 用 `Path::join` 拼期望值：Windows 上分隔符是 `\`，别拿字符串硬比。
        assert_eq!(options.data_path(), Path::new(root()).join("data"));
        assert_eq!(options.plugin_path(), Path::new(root()).join("plugins"));
        assert_eq!(options.log_level(), LogLevel::Info);
        assert!(options.tdjson_dir().is_none());
        assert_eq!(options.config_json()["telegram"]["account"], "bot");
    }

    #[test]
    fn relative_root_is_rejected() {
        let json = full().replace(root(), "files");
        let error = Options::parse(&json).unwrap_err();
        assert!(error.contains("绝对路径"), "{error}");
    }

    #[test]
    fn credentials_never_enter_the_config_tree() {
        let options = Options::parse(&full()).unwrap();
        let tree = options.config_json().to_string();
        assert!(!tree.contains("0123456789abcdef"), "{tree}");
        assert!(!tree.contains("1:AAA"), "{tree}");
    }

    #[test]
    fn log_level_accepts_the_documented_aliases() {
        assert_eq!(parse_log_level("WARNING").unwrap(), LogLevel::Warn);
        assert_eq!(parse_log_level("trace").unwrap(), LogLevel::Trace);
        assert!(parse_log_level("chatty").is_err());
    }

    #[test]
    fn lib_dir_must_be_a_real_directory() {
        let mut options = Options::parse(&full()).unwrap();
        options.lib_dir = Some("/data/app/base.apk!/lib/arm64-v8a".to_owned());
        assert!(options.tdjson_dir().is_none());
        // crate 目录一定是真目录（别用 `temp_dir()`：MSYS 会把 TEMP 改成 /tmp）。
        options.lib_dir = Some(env!("CARGO_MANIFEST_DIR").to_owned());
        assert!(options.tdjson_dir().is_some());
    }

    /// 每个测试一个干净的目录（`target/test-tmp/`，别用 `temp_dir()`，理由同上）。
    fn scratch(name: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-tmp/fuyubot-android")
            .join(name)
            .join(std::process::id().to_string());
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 用真实目录当 root 拼 options（点分插件名等场景都走这里）。
    fn options_in(dir: &Path, plugin_config: serde_json::Value) -> Options {
        let mut value = json!({
            "root": dir.to_string_lossy(),
            "api_id": 1,
            "api_hash": "hash",
        });
        if !plugin_config.is_null() {
            value["plugin_config"] = plugin_config;
        }
        Options::parse(&value.to_string()).unwrap()
    }

    #[test]
    fn plugin_config_expands_dotted_names_into_nested_tables() {
        let dir = scratch("plugin-config-dotted");
        let options = options_in(
            &dir,
            json!({ "fuyubot.manage": { "reply_prefix": "已安装：" } }),
        );
        let tree = options.config_json();
        // `[plugin.fuyubot.manage]` 在树里必须是嵌套表——宿主按点路径下钻。
        assert_eq!(
            tree["plugin"]["fuyubot"]["manage"]["reply_prefix"],
            "已安装："
        );
        // 嵌套写法等价。
        let nested = options_in(&dir, json!({ "my": { "weather": { "city": "上海" } } }));
        assert_eq!(
            nested.config_json()["plugin"]["my"]["weather"]["city"],
            "上海"
        );
    }

    #[test]
    fn plugin_config_rejects_bad_shapes() {
        let dir = scratch("plugin-config-bad");
        let bad_name = Options::parse(
            &json!({ "root": dir, "api_id": 1, "api_hash": "h",
                     "plugin_config": { "a..b": {} } })
            .to_string(),
        )
        .unwrap_err();
        assert!(bad_name.contains("插件名"), "{bad_name}");

        let bad_value = Options::parse(
            &json!({ "root": dir, "api_id": 1, "api_hash": "h",
                     "plugin_config": { "a": 1 } })
            .to_string(),
        )
        .unwrap_err();
        assert!(bad_value.contains("JSON 对象"), "{bad_value}");
    }

    /// `<root>/fuyubot.toml` 是**可选用户配置层**：这次 JSON 给的同名键优先，其余由文件兜底。
    #[test]
    fn user_config_file_is_the_base_and_explicit_json_wins() {
        let dir = scratch("plugin-config-persisted");
        std::fs::write(
            dir.join(CONFIG_FILE),
            "[plugin.fuyubot.manage]\nreply_prefix = \"from-file\"\nkept = 1\n",
        )
        .unwrap();

        let options = options_in(
            &dir,
            json!({ "fuyubot.manage": { "reply_prefix": "from-json" } }),
        );
        let tree = options.config_json();
        assert_eq!(
            tree["plugin"]["fuyubot"]["manage"]["reply_prefix"], "from-json",
            "显式启动参数要压过用户文件"
        );
        assert_eq!(
            tree["plugin"]["fuyubot"]["manage"]["kept"], 1,
            "这次没给的键由用户文件兜底"
        );
        // 配置文件就在 root 下，且与 CLI 同名。
        assert_eq!(options.config_path(), dir.join(CONFIG_FILE));
    }

    #[test]
    fn a_broken_config_file_falls_back_to_empty_instead_of_failing_startup() {
        let dir = scratch("plugin-config-broken");
        std::fs::write(dir.join(CONFIG_FILE), "this is not = = toml").unwrap();
        let options = options_in(&dir, serde_json::Value::Null);
        assert_eq!(options.config_json()["plugin"], json!({}));
    }
}
