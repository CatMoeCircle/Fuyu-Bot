//! `start(options_json)` 的解析与校验。
//!
//! 为什么是 JSON 而不是结构体：跨 ABI 传结构体要绑死字段顺序 / 对齐 / 指针；
//! JSON 加字段不破 ABI，也和本仓既有的「op JSON / 更新 JSON」习惯一致。
//!
//! 为什么**不读 `fuyubot.toml`**：Android 上没有给人编辑配置文件的场景。
//! 多一份 schema 就多一处漂移——配置的唯一来源就是这次调用传进来的 JSON。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use fuyubot_app::{AppHost, AppOptions, LogLevel, TdlibSettings};
use fuyubot_tdlib::{AccountType, LOGIN_BUDGET, TdlibParameters};
use serde::Deserialize;

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

    /// 装进宿主运行时的配置树（插件经 ABI 的 `get_config` 读它）。
    ///
    /// 只放**非凭据**：`api_hash` / `bot_token` 一律不进去（插件不该拿到它们）。
    /// 形状照 `fuyubot.toml` 的 `[telegram]` / `[plugins]`，插件的既有写法不用改。
    pub fn config_json(&self) -> serde_json::Value {
        serde_json::json!({
            "telegram": {
                "api_id": self.api_id,
                "data_dir": self.data_dir.clone().unwrap_or_else(|| "data".to_owned()),
                "account": if self.bot_token.is_some() { "bot" } else { "user" },
            },
            "plugins": {
                "dir": self.plugin_dir.clone().unwrap_or_else(|| "plugins".to_owned()),
                "log_level": log_level_name(self.log_level()),
            },
            "plugin": {},
        })
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
            // Android 上没有配置文件可回写：`set_config` 只改内存。
            config_path: None,
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

/// 与 CLI 的 `scaffold::parse_log_level` 认同一套写法（那边是权威，改那里要同步这里）。
fn parse_log_level(raw: &str) -> Result<LogLevel, String> {
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
}
