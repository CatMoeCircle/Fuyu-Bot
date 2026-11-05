//! 工作线程里的运行时入口：建目录 + 接日志 → `fuyubot-app` 起完 → 等停止位 → 收摊。
//!
//! 顺序、登录、插件加载全在 [`fuyubot_app::App`] 里（与 CLI 共用同一份）；
//! 这里只剩 Android 特有的四件事：目录、日志重定向、停止位、JNI 宿主。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use fuyubot_app::{App, AppHost};

use crate::host::JniHost;
use crate::options::Options;
use crate::state;

/// 等 TDLib 走完 `close` → `authorizationStateClosed` 的预算（与 CLI 一致）。
const TD_CLOSE_TIMEOUT: Duration = Duration::from_secs(3);
/// 等停止位的轮询间隔（与 CLI 的运行循环一致）。
const STOP_POLL: Duration = Duration::from_millis(500);

/// 在 worker 线程里跑完整条生命周期。返回 `Ok` = 正常收摊。
pub fn run(options: &Options) -> Result<(), String> {
    let root = options.root_path();
    let data_dir = options.data_path();
    let plugin_dir = options.plugin_path();
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
    runtime.block_on(run_async(options, root))
}

async fn run_async(options: &Options, root: PathBuf) -> Result<(), String> {
    let host: Arc<dyn AppHost> = Arc::new(JniHost);
    let app_options = options.app_options(host);

    // 与 CLI 同一条顺序：boot（内核 + 宿主 + 接 TDLib）→ 装插件 → 登录。
    // 拆开三段的唯一原因在中间：视图要**赶在登录之前**装上——登录要等人回答，
    // 甚至可能失败，那期间 Java 也得能查插件列表 / 数量 / 跳过原因。
    let mut app = App::boot(app_options)
        .await
        .map_err(|error| format!("启动失败：{error}"))?;

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
    tracing::info!(target: "fuyubot::jni", "收摊完成（root={}）", root.display());
    Ok(())
}
