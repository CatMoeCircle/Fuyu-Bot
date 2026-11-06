//! 宿主应用的编排层：CLI 与 Android JNI 共用**一条**启动顺序。
//!
//! ```text
//! fuyubot-core    内核（Context / Effect / PluginManager）
//! fuyubot-loader  宿主运行时、VTable、插件加载、更新管线
//! fuyubot-tdlib   TDLib 加载、接收循环、授权状态机、Session
//!      └──────── fuyubot-app（本 crate）：按顺序接起来 + 一个 AppHost 抽象
//!                     ├── fuyubot-cli：控制台 / TUI（先敲命令，再 `--tui`）
//!                     └── fuyubot-android：JNI 导出（`libfuyubotjni.so`）
//! ```
//!
//! 用法（两个宿主都一样）：
//!
//! ```ignore
//! let options = AppOptions { /* 目录、配置、TDLib 设置、host */ };
//! let mut app = App::start(options).await?;   // boot + 装插件 + 登录
//! let receive = app.dispatch();               // 开始分发
//! // … 跑自己的循环（状态栏、停止位 …）
//! app.shutdown(receive, Duration::from_secs(3)).await?;
//! ```
//!
//! 顺序为什么不能换、哪一步最容易被误会，写在 [`app`] 的模块文档里。

mod app;
mod host;
mod live;
mod prompt;
mod tdstate;

pub use app::{App, AppError, AppOptions, TdlibSettings};
pub use host::{AppHost, AppState, AskKind, SilentHost};
pub use live::{LiveView, PluginInfo, PluginLoadReport, PluginSkip, empty_plugins_json};
pub use prompt::{HostPrompter, kind_of};
pub use tdstate::{
    AUTH_UPDATE, TdState, TdStateWatch, empty_auth_state_json, empty_connection_state_json,
};

// 宿主要用的那几个类型，从这里拿就够——不必为了 `LogLevel` 再直接依赖 loader。
pub use fuyubot_loader::LogLevel;
