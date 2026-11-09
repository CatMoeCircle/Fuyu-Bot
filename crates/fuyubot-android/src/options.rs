//! `start(options_json)` 的解析与校验。
//!
//! **配置在 `<root>/fuyubot.toml` 里**（与 CLI 同一份 schema，实现在 `fuyubot_app::config`）。
//! 这样 Android 上改配置不需要重新编 APK，也跟 CLI 的文档/工具对得上。
//!
//! options 只留「运行时才有 / 不能落盘」的几项：
//! - `root`（必填、绝对路径、app 私有目录）——决定 `fuyubot.toml` 在哪；
//! - `bot_token`：凭据，**不落盘**（约束 41），只在本次启动用；
//! - `lib_dir`：tdjson 所在目录 = 设备上的安装路径，落盘会被 APK 更新换成死路径；
//! - `data_dir` / `plugin_dir` / `log_level`：**显式覆盖** `fuyubot.toml` 里的同名项
//!   （壳在设置里改了、又不想写文件时用；写了文件的话这两处必须一致）。
//!
//! `api_id` / `api_hash` **只从文件读**（`[telegram]` 段，与 CLI 完全一致）。没有文件 /
//! 没有凭据就报错，并把用户指向 [`fuyubot_app::init_config`]（JNI 的 `config_init`）。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use fuyubot_app::config::{CONFIG_FILE, Config, load_api_hash, load_config, parse_log_level};
use fuyubot_app::{AppHost, AppOptions, TdlibSettings};
use fuyubot_tdlib::{AccountType, LOGIN_BUDGET, TdlibParameters};
use serde::Deserialize;

/// `start` 的入参。字段语义见 `include/fuyubot_jni.h` 与设计文档第 9.3 节。
#[derive(Debug, Clone, Deserialize)]
pub struct Options {
    /// 项目根。**必填、绝对路径、必须是 app 私有目录**（`/sdcard` 是 noexec）。
    /// `fuyubot.toml`、`data/`、`plugins/`、`logs/` 都在它下面。
    pub root: String,
    /// 覆盖 `[telegram].data_dir`（相对 root）；缺省 = 文件里的值，再缺省 `data`。
    #[serde(default)]
    pub data_dir: Option<String>,
    /// 覆盖 `[plugins].dir`（相对 root）；缺省 = 文件里的值，再缺省 `plugins`。
    #[serde(default)]
    pub plugin_dir: Option<String>,
    /// 覆盖 `[plugins].log_level`；缺省 = 文件里的值，再缺省 `info`。
    #[serde(default)]
    pub log_level: Option<String>,
    /// Bot Token。有值就走 bot 登录，全程不打扰 UI；**不落盘**（约束 41）。
    /// 账户类型仍以 `[telegram].account_type` 为准：`user` 账户下给了 token 会被忽略并告警。
    #[serde(default)]
    pub bot_token: Option<String>,
    /// tdjson 所在目录（通常是 `applicationInfo.nativeLibraryDir`）。
    ///
    /// 缺省时先看 `[tdlib].path`；两者都没有就不设环境变量，要求宿主先
    /// `System.loadLibrary("tdjson")`，loader 的优先级 4（裸库名）会命中已加载的那个。
    #[serde(default)]
    pub lib_dir: Option<String>,
}

impl Options {
    /// 解析 + 校验。任何一条不满足都在这里说清楚，别等到建完客户端才报。
    ///
    /// 这里**只验 options 自己**；`fuyubot.toml` 存不存在、凭据填没填，由 [`Self::app_options`]
    /// 在真正启动前一次性报出来（错误串同样会进 `last_error`）。
    pub fn parse(json: &str) -> Result<Self, String> {
        let options: Self = serde_json::from_str(json)
            .map_err(|error| format!("options JSON 解析失败：{error}"))?;

        if !Path::new(&options.root).is_absolute() {
            return Err(format!(
                "root 必须是绝对路径（Android 上 cwd 是 `/`），收到 {:?}",
                options.root
            ));
        }
        if let Some(raw) = &options.log_level {
            parse_log_level(raw)?;
        }
        for (name, value) in [
            ("data_dir", &options.data_dir),
            ("plugin_dir", &options.plugin_dir),
        ] {
            if let Some(raw) = value {
                if raw.trim().is_empty() {
                    return Err(format!("{name} 不能是空串"));
                }
            }
        }
        Ok(options)
    }

    pub fn root_path(&self) -> PathBuf {
        PathBuf::from(&self.root)
    }

    /// 配置文件：`<root>/fuyubot.toml`（与 CLI 同名同位置）。
    pub fn config_path(&self) -> PathBuf {
        self.root_path().join(CONFIG_FILE)
    }

    /// 显式给了 `lib_dir` 且它真是目录才算数（APK 内路径 `…/base.apk!/lib/…` 不是目录）。
    pub fn tdjson_dir(&self) -> Option<&str> {
        self.lib_dir
            .as_deref()
            .filter(|dir| !dir.trim().is_empty())
            .filter(|dir| Path::new(dir).is_dir())
    }

    /// 读 `fuyubot.toml`、套上这次的覆盖项、把凭据取出来。
    ///
    /// 失败信息都要能直接给到用户（进 `last_error` / `onState(FAILED)`）：缺文件、缺 api_id、
    /// 缺 api_hash 是三种最常见的首启错误，各说各的，别都报「启动失败」。
    fn resolve(&self) -> Result<Resolved, String> {
        let path = self.config_path();
        if !path.is_file() {
            return Err(format!(
                "缺配置文件 {}：先调 fuyubot_config_init 写一份（或手工放一份 fuyubot.toml，\
                 里面要有 [telegram].api_id / api_hash）",
                path.display()
            ));
        }
        let mut config = load_config(&path)?;
        // 覆盖只改这一份内存里的树：写回文件是插件 `set_config` 的事，壳的覆盖不落盘。
        if let Some(dir) = &self.data_dir {
            config.telegram.data_dir = Some(dir.trim().to_owned());
        }
        if let Some(dir) = &self.plugin_dir {
            config.plugins.dir = Some(dir.trim().to_owned());
        }
        if let Some(raw) = &self.log_level {
            config.plugins.log_level = Some(raw.trim().to_owned());
        }

        let api_id = config.telegram.api_id(&config.bot);
        if api_id <= 0 {
            return Err(format!(
                "{} 的 [telegram].api_id 没填（或不是正数）：用 fuyubot_config_init 写进去",
                path.display()
            ));
        }
        let api_hash = load_api_hash(&path).ok_or_else(|| {
            format!(
                "{} 的 [telegram].api_hash 是空的：用 fuyubot_config_init 写进去",
                path.display()
            )
        })?;
        let account = config.telegram.account_type()?;
        Ok(Resolved {
            path,
            config,
            api_id,
            api_hash,
            account,
        })
    }

    /// 翻译成编排层的输入：目录、配置树、TDLib 设置、交互出口。
    ///
    /// 这就是「Android 与 CLI 的差别只剩这一段」的地方——剩下的顺序全在 `fuyubot-app`。
    pub fn app_options(&self, host: Arc<dyn AppHost>) -> Result<AppOptions, String> {
        let resolved = self.resolve()?;
        let root = self.root_path();
        let data_dir = resolved.config.telegram.data_dir(&root);
        let plugin_dir = resolved.config.plugin_dir(&root);
        let log_level = resolved.config.plugins.log_level()?;
        // 配置树要反映**生效值**：插件 `get_config("plugins","dir")` 不该看到被覆盖掉的旧值。
        let config = resolved.config.to_json()?;

        // `[tdlib].path` 是给桌面用的；Android 上设备路径只能由壳在 `lib_dir` 里给。
        let tdjson_path = self.tdjson_dir().map(PathBuf::from).or_else(|| {
            resolved
                .config
                .tdlib
                .path
                .as_deref()
                .map(str::trim)
                .filter(|path| !path.is_empty())
                .map(PathBuf::from)
        });

        // user 账户拿着 token 没有意义：忽略它，但要说一声——壳那边多半是把账号类型选错了。
        let bot_token = match resolved.account {
            AccountType::Bot => self.bot_token.clone(),
            AccountType::User => {
                if self.bot_token.is_some() {
                    tracing::warn!(
                        target: "fuyubot::jni",
                        "[telegram].account_type = \"user\"：忽略 options 里的 bot_token"
                    );
                }
                None
            }
        };

        Ok(AppOptions {
            data_dir: data_dir.clone(),
            plugin_dir,
            log_level,
            log_plugins: Vec::new(),
            config,
            // 给宿主一个配置根：插件 `set_config` 落盘到它旁边的 `plugin-config/`，
            // 同时让用户配置（本文件）里的键保持「钉住」语义。
            config_path: Some(resolved.path),
            tdjson_path,
            tdlib: Some(TdlibSettings {
                parameters: parameters(&data_dir, resolved.api_id, resolved.api_hash)?,
                account: resolved.account,
                bot_token,
                log_verbosity: Some(resolved.config.tdlib.log_verbosity()),
                login_budget: LOGIN_BUDGET,
                // 总要走一次登录：有 session 就是「直接 Ready」，没有就走交互。
                login: true,
            }),
            host,
        })
    }
}

/// 一次启动解析出来的东西（文件里的值 + 凭据）。
struct Resolved {
    path: PathBuf,
    config: Config,
    api_id: i64,
    api_hash: String,
    account: AccountType,
}

/// `setTdlibParameters` 的参数。设备名带上 `android`，在 Telegram 的「设备」列表里一眼能认。
fn parameters(data_dir: &Path, api_id: i64, api_hash: String) -> Result<TdlibParameters, String> {
    let api_id = i32::try_from(api_id)
        .map_err(|_| format!("[telegram].api_id = {api_id} 超出 int32 范围"))?;
    let mut parameters = TdlibParameters::new(data_dir.to_path_buf(), api_id, api_hash);
    parameters.device_model = "fuyubot-android".to_owned();
    Ok(parameters)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuyubot_app::LogLevel;
    use fuyubot_app::config::InitRequest;
    use serde_json::json;

    /// 测试里的 root：**必须按平台给绝对路径**——Windows 上 `/data/...` 不是绝对路径
    /// （没有盘符），而校验本身是对的（Android 上它确实是绝对的）。
    fn root() -> &'static str {
        if cfg!(windows) {
            "C:/data/user/0/com.example.bot/files"
        } else {
            "/data/user/0/com.example.bot/files"
        }
    }

    /// 每个测试一个干净的目录（`target/test-tmp/`，别用 `temp_dir()`：MSYS 会把 TEMP 改成 /tmp）。
    fn scratch(name: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-tmp/fuyubot-android")
            .join(name)
            .join(std::process::id().to_string());
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 在 `dir` 下生成一份真配置（api_id / api_hash 都填好），返回 `start` 用的 options JSON。
    fn prepared(dir: &Path) -> String {
        fuyubot_app::config::init_config(
            dir,
            &InitRequest {
                name: Some("demo".to_owned()),
                api_id: Some(12345),
                api_hash: Some("0123456789abcdef".to_owned()),
                ..InitRequest::default()
            },
        )
        .unwrap();
        json!({ "root": dir.to_string_lossy() }).to_string()
    }

    #[test]
    fn parses_the_minimum() {
        let dir = scratch("minimum");
        let options = Options::parse(&prepared(&dir)).unwrap();
        assert_eq!(options.root_path(), dir);
        assert_eq!(options.config_path(), dir.join(CONFIG_FILE));
        assert!(options.data_dir.is_none());
        assert!(options.tdjson_dir().is_none());

        let app = options
            .app_options(Arc::new(fuyubot_app::SilentHost))
            .unwrap();
        // 模板默认：`data/`、`plugins/`、info、bot。
        assert_eq!(app.data_dir, dir.join("data"));
        assert_eq!(app.plugin_dir, dir.join("plugins"));
        assert_eq!(app.log_level, LogLevel::Info);
        assert_eq!(app.tdlib.as_ref().unwrap().account, AccountType::Bot);
    }

    #[test]
    fn relative_root_is_rejected() {
        let json = json!({ "root": "files" }).to_string();
        let error = Options::parse(&json).unwrap_err();
        assert!(error.contains("绝对路径"), "{error}");
    }

    /// 旧 APK 还会传 api_id / api_hash / plugin_config：这些字段已经不认识，**直接忽略**。
    /// 真正起不来是因为缺 `fuyubot.toml`，错误串必须指向 `fuyubot_config_init`。
    #[test]
    fn legacy_options_are_ignored_and_missing_config_says_how_to_fix() {
        let dir = scratch("legacy");
        let options = Options::parse(
            &json!({
                "root": dir.to_string_lossy(),
                "api_id": 1,
                "api_hash": "h",
                "plugin_config": { "fuyubot.manage": {} },
            })
            .to_string(),
        )
        .unwrap();
        let error = options
            .app_options(Arc::new(fuyubot_app::SilentHost))
            .err()
            .unwrap();
        assert!(error.contains("fuyubot_config_init"), "{error}");
        assert!(error.contains("fuyubot.toml"), "{error}");
    }

    /// 缺 api_id / 缺 api_hash 分别要报各自那一句（首启最常见，别混成「启动失败」）。
    #[test]
    fn missing_credentials_each_say_what_is_missing() {
        let dir = scratch("no-api-id");
        std::fs::write(dir.join(CONFIG_FILE), "[telegram]\napi_hash = \"h\"\n").unwrap();
        let options =
            Options::parse(&json!({ "root": dir.to_string_lossy() }).to_string()).unwrap();
        let error = options
            .app_options(Arc::new(fuyubot_app::SilentHost))
            .err()
            .unwrap();
        assert!(error.contains("api_id"), "{error}");

        let dir = scratch("no-api-hash");
        std::fs::write(dir.join(CONFIG_FILE), "[telegram]\napi_id = 7\n").unwrap();
        let options =
            Options::parse(&json!({ "root": dir.to_string_lossy() }).to_string()).unwrap();
        let error = options
            .app_options(Arc::new(fuyubot_app::SilentHost))
            .err()
            .unwrap();
        assert!(error.contains("api_hash"), "{error}");
    }

    /// 显式覆盖压过文件，并且**配置树里也是生效值**（插件读到的不能是旧值）。
    #[test]
    fn explicit_overrides_win_and_show_up_in_the_config_tree() {
        let dir = scratch("overrides");
        prepared(&dir);
        let options = Options::parse(
            &json!({
                "root": dir.to_string_lossy(),
                "data_dir": "db",
                "plugin_dir": "ext",
                "log_level": "Debug",
            })
            .to_string(),
        )
        .unwrap();
        let app = options
            .app_options(Arc::new(fuyubot_app::SilentHost))
            .unwrap();
        assert_eq!(app.data_dir, dir.join("db"));
        assert_eq!(app.plugin_dir, dir.join("ext"));
        assert_eq!(app.log_level, LogLevel::Debug);
        assert_eq!(app.config["telegram"]["data_dir"], "db");
        assert_eq!(app.config["plugins"]["dir"], "ext");
        assert_eq!(app.config["plugins"]["log_level"], "Debug");
    }

    /// 凭据**不进配置树**：那棵树会整棵交给宿主运行时，插件 `get_config` 能读任意段。
    #[test]
    fn credentials_never_enter_the_config_tree() {
        let dir = scratch("secrets");
        let options = Options::parse(&prepared(&dir)).unwrap();
        let app = options
            .app_options(Arc::new(fuyubot_app::SilentHost))
            .unwrap();
        let tree = app.config.to_string();
        assert!(!tree.contains("0123456789abcdef"), "{tree}");
    }

    /// 文件里的插件段原样进配置树（与 CLI 同一个 schema）。
    #[test]
    fn plugin_sections_come_from_the_file() {
        let dir = scratch("plugin-sections");
        prepared(&dir);
        let mut text = std::fs::read_to_string(dir.join(CONFIG_FILE)).unwrap();
        text.push_str("\n[plugin.fuyubot.manage]\nreply_prefix = \"已安装：\"\n");
        std::fs::write(dir.join(CONFIG_FILE), text).unwrap();

        let options =
            Options::parse(&json!({ "root": dir.to_string_lossy() }).to_string()).unwrap();
        let app = options
            .app_options(Arc::new(fuyubot_app::SilentHost))
            .unwrap();
        assert_eq!(
            app.config["plugin"]["fuyubot"]["manage"]["reply_prefix"],
            "已安装："
        );
    }

    #[test]
    fn log_level_accepts_the_documented_aliases() {
        assert_eq!(parse_log_level("WARNING").unwrap(), LogLevel::Warn);
        assert_eq!(parse_log_level("trace").unwrap(), LogLevel::Trace);
        assert!(parse_log_level("chatty").is_err());
    }

    /// `user` 账户下给了 token：忽略它（否则壳选错账号类型会一直等不到登录）。
    #[test]
    fn a_bot_token_is_ignored_for_user_accounts() {
        let dir = scratch("user-token");
        fuyubot_app::config::init_config(
            &dir,
            &InitRequest {
                api_id: Some(1),
                api_hash: Some("h".to_owned()),
                account_type: Some("user".to_owned()),
                ..InitRequest::default()
            },
        )
        .unwrap();
        let options = Options::parse(
            &json!({ "root": dir.to_string_lossy(), "bot_token": "1:AAA" }).to_string(),
        )
        .unwrap();
        let app = options
            .app_options(Arc::new(fuyubot_app::SilentHost))
            .unwrap();
        assert_eq!(app.tdlib.as_ref().unwrap().account, AccountType::User);
        assert!(app.tdlib.as_ref().unwrap().bot_token.is_none());
    }

    #[test]
    fn lib_dir_must_be_a_real_directory() {
        let dir = scratch("lib-dir");
        let mut options = Options::parse(&prepared(&dir)).unwrap();
        options.lib_dir = Some("/data/app/base.apk!/lib/arm64-v8a".to_owned());
        assert!(options.tdjson_dir().is_none());
        // crate 目录一定是真目录（别用 `temp_dir()`：MSYS 会把 TEMP 改成 /tmp）。
        options.lib_dir = Some(env!("CARGO_MANIFEST_DIR").to_owned());
        assert!(options.tdjson_dir().is_some());
    }

    #[test]
    fn empty_overrides_are_rejected() {
        let dir = scratch("empty-override");
        let error =
            Options::parse(&json!({ "root": dir.to_string_lossy(), "data_dir": "  " }).to_string())
                .unwrap_err();
        assert!(error.contains("data_dir"), "{error}");
    }
}
