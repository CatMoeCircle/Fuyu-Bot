//! TDLib 接入层：原生库加载与 [`TdClient`]。
//!
//! 类型与 FFI 绑定（`td_api` / `schema` / `ffi`）住在
//! [`fuyubot-tdlib-sys`](fuyubot_tdlib_sys) 里，本 crate 全部重导出——
//! 已有调用方的路径（`fuyubot_tdlib::TdApiSchema` 等）不变；
//! 只需要类型/元数据的下游直接依赖 sys，不必拖进加载与网络依赖。
//!
//! 本 crate 只做传输层和加载层，不含任何 TDLib 业务方法。
//! 库路径在运行时按优先级解析（见 [`loader::resolve`]），编译期不做静态链接。

pub mod authorize;
pub mod client;
pub mod loader;

// 绑定层（类型、元数据、FFI 声明）整体来自 sys；模块名也一并挂到本 crate 根上，
// 这样 `crate::ffi` / `crate::schema` 这些内部路径与 `fuyubot_tdlib::td_api`
// 这些外部路径都继续成立。
pub use fuyubot_tdlib_sys::{
    DEFAULT_LOG_VERBOSITY, EntryKind, SchemaError, TdApi, TdApiSchema, TlEntry, TlParam,
};
pub use fuyubot_tdlib_sys::{ffi, schema, td_api, tl_schema};

pub use authorize::{
    AccountType, AuthState, LoginError, LoginMethod, LoginOutcome, Prompter, Question,
    TdlibParameters, log_in, log_in_with, question_for, request_for,
};
pub use client::{
    EXTRA_FIELD, PendingRequest, ReceiveLoop, RequestId, Routed, TdClient, TdResponse, TdUpdate,
    TdlibError, inject_extra, route,
};
pub use loader::{
    LibrarySource, LoadEnv, ResolvedLibrary, TDLIB_VERSION, TdlibLoadError, resolve, resolve_with,
};
