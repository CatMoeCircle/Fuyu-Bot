//! 工作线程里的运行时入口：建目录 + 接日志 → `fuyubot-app` 起完 → 等停止位 → 收摊。
//!
//! 顺序、登录、插件加载全在 [`fuyubot_app::App`] 里（与 CLI 共用同一份）；
//! 这里只剩 Android 特有的四件事：目录、日志重定向、停止位、JNI 宿主。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use fuyubot_app::{App, AppOptions};

use crate::options::Options;
use crate::state;

/// 等 TDLib 走完 `close` → `authorizationStateClosed` 的预算（与 CLI 一致）。
const TD_CLOSE_TIMEOUT: Duration = Duration::from_secs(3);
/// 等停止位的轮询间隔（与 CLI 的运行循环一致）。
const STOP_POLL: Duration = Duration::from_millis(500);
/// 运行期日志开关所在的目录（与 CLI 的实例状态目录同名）。
const LOG_LEVEL_DIR: &str = ".fuyubot";
/// 运行期日志开关的文件名（与 CLI 的控制文件协议一致）：内容是级别名。
const LOG_LEVEL_FILE: &str = "log-level";
/// 日志开关的轮询间隔：切级别最迟半秒生效。
const LOG_LEVEL_POLL: Duration = Duration::from_millis(500);

/// 控制文件路径：`<root>/.fuyubot/log-level`。
fn log_level_path(root: &Path) -> PathBuf {
    root.join(LOG_LEVEL_DIR).join(LOG_LEVEL_FILE)
}

/// 读运行期日志请求。`None` = 没有请求（文件不存在 / 读不了 / 内容为空）。
///
/// 内容按 CLI 那套写法认（`error` / `warn` / `info` / `debug` / `trace`，大小写不敏感）；
/// 写错了不算请求——调用方会告警一次并保持当前级别，而不是让日志突然停掉。
fn requested_log_level(root: &Path) -> Option<String> {
    let text = std::fs::read_to_string(log_level_path(root)).ok()?;
    let raw = text.trim();
    (!raw.is_empty()).then(|| raw.to_owned())
}

/// 在 worker 线程里跑完整条生命周期。返回 `Ok` = 正常收摊。
///
/// `app_options` 由 `lib.rs` 在**起线程之前**解析好（缺 `fuyubot.toml` / 缺凭据这类错
/// 要能作为 `fuyubot_start` 的参数错误直接返回，而不是起完线程再在 `onState` 里报）。
pub fn run(options: &Options, app_options: AppOptions) -> Result<(), String> {
    let root = options.root_path();
    // 目录来自**生效的**配置（options 的覆盖已经合进去了），不是 options 原样。
    let data_dir = app_options.data_dir.clone();
    let plugin_dir = app_options.plugin_dir.clone();
    let logs = root.join("logs");

    for dir in [&root, &data_dir, &plugin_dir, &logs] {
        std::fs::create_dir_all(dir)
            .map_err(|error| format!("建不了 {}：{error}", dir.display()))?;
    }
    // 必须在 init_tracing 之前：订阅者一装好就会往 stderr 写。
    crate::log::redirect_stderr(&logs.join("fuyubot.log"));
    crate::log::init_tracing();
    tracing::info!(
        target: "fuyubot::jni",
        "fuyubotjni {} 启动：root={} data={} plugins={}",
        env!("CARGO_PKG_VERSION"),
        root.display(),
        data_dir.display(),
        plugin_dir.display(),
    );

    let runtime =
        tokio::runtime::Runtime::new().map_err(|error| format!("建不了 tokio 运行时：{error}"))?;
    runtime.block_on(run_async(app_options, root))
}

async fn run_async(app_options: AppOptions, root: PathBuf) -> Result<(), String> {
    // 与 CLI 同一条顺序：boot（内核 + 宿主 + 接 TDLib）→ 装插件 → 登录。
    // 拆开三段的唯一原因在中间：视图要**赶在登录之前**装上——登录要等人回答，
    // 甚至可能失败，那期间 Java 也得能查插件列表 / 数量 / 跳过原因。
    let mut app = App::boot(app_options)
        .await
        .map_err(|error| format!("启动失败：{error}"))?;

    // 运行期调日志：壳写 `<root>/.fuyubot/log-level`，这里每 500ms 看一次（与 CLI 同一个
    // 文件协议，**不需要新增 JNI 方法**）。**赶在装插件 / 登录之前起**——登录窗口可能等很久，
    // 那正是最需要把级别调到 debug / trace 的时候。
    let log_runtime = Arc::clone(app.runtime());
    let log_root = root.clone();
    let log_level_watch = tokio::spawn(async move {
        let mut applied: Option<String> = None;
        loop {
            if state::stop_requested() {
                break;
            }
            if let Some(raw) = requested_log_level(&log_root) {
                // 只在内容变了时动作：同一个值每 500ms 重复解析 + 重复打日志没意义。
                if applied.as_deref() != Some(raw.as_str()) {
                    match fuyubot_app::config::parse_log_level(&raw) {
                        Ok(level) => {
                            log_runtime.set_log_verbosity(level);
                            tracing::info!(
                                target: "fuyubot::jni",
                                "运行期日志级别切到 {raw}（{}）",
                                log_level_path(&log_root).display()
                            );
                        }
                        Err(error) => {
                            tracing::warn!(
                                target: "fuyubot::jni",
                                "忽略日志级别请求：{error}（保持当前级别）"
                            );
                        }
                    }
                    applied = Some(raw);
                }
            }
            tokio::time::sleep(LOG_LEVEL_POLL).await;
        }
    });

    // 装插件：`AppHost::plugin_report` 在这里把清单 + 计数 + 跳过原因投给
    // `Callback.onPlugins`，登录窗口弹出之前 APK 就拿到了（日志里另有一条汇总）。
    app.load_plugins()
        .map_err(|error| format!("启动失败：{error}"))?;

    // 三个查询接口（插件列表 / 详情 / TDLib JSON）从 Java 调用线程访问这份视图；
    // 守卫保证收摊、报错、panic 都会摘掉它（视图不能比运行时活得久）。
    let _live = crate::live::LiveGuard::install(app.live_view());

    // 登录（阻塞；期间没有接收循环）。还没登录时 `nativePlugins()` 已经有内容了。
    if app.should_login() {
        app.login().map_err(|error| format!("启动失败：{error}"))?;
    }

    let receive = app.dispatch();
    while !state::stop_requested() {
        tokio::time::sleep(STOP_POLL).await;
    }

    let closed = app
        .shutdown(receive, TD_CLOSE_TIMEOUT)
        .await
        .map_err(|error| format!("收摊失败：{error}"))?;
    if closed {
        tracing::info!(target: "fuyubot::jni", "TDLib 已关闭（数据库已刷盘）");
    } else {
        tracing::warn!(
            target: "fuyubot::jni",
            "TDLib 没在 {TD_CLOSE_TIMEOUT:?} 内确认关闭，直接销毁客户端"
        );
    }
    // 停止位已经置上，它下一圈就会自己退出；显式 abort 只是别让它多等半秒。
    log_level_watch.abort();
    tracing::info!(target: "fuyubot::jni", "收摊完成（root={}）", root.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuyubot_app::LogLevel;

    fn scratch(name: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-tmp/fuyubot-android")
            .join(name)
            .join(std::process::id().to_string());
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 没有文件 / 空文件 = 没有请求；有内容就去掉首尾空白原样给出（解析交给 options）。
    #[test]
    fn log_level_requests_are_read_from_the_control_file() {
        let dir = scratch("log-level");
        assert!(requested_log_level(&dir).is_none(), "没有文件就没有请求");

        std::fs::create_dir_all(dir.join(LOG_LEVEL_DIR)).unwrap();
        std::fs::write(log_level_path(&dir), "   ").unwrap();
        assert!(requested_log_level(&dir).is_none(), "空内容不算请求");

        std::fs::write(log_level_path(&dir), "trace\n").unwrap();
        assert_eq!(requested_log_level(&dir).as_deref(), Some("trace"));

        std::fs::write(log_level_path(&dir), "\tDebug \r\n").unwrap();
        assert_eq!(requested_log_level(&dir).as_deref(), Some("Debug"));

        // 内容走 CLI 同一套解析：合法别名认，乱写要能报错（调用方会告警并保持当前级别）。
        assert_eq!(
            fuyubot_app::config::parse_log_level("Warning").unwrap(),
            LogLevel::Warn
        );
        std::fs::write(log_level_path(&dir), "verbose").unwrap();
        assert!(
            fuyubot_app::config::parse_log_level(&requested_log_level(&dir).unwrap()).is_err(),
            "写错的级别要能被识别出来"
        );
    }
}
