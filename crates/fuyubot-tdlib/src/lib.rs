//! TDLib 接入层：FFI 类型、原生库加载与 [`TdClient`]。
//!
//! 只做传输层和加载层，不含任何 TDLib 业务方法。
//! 库路径在运行时按优先级解析（见 [`loader::resolve`]），编译期不做静态链接。

pub mod authorize;
pub mod client;
pub mod ffi;
pub mod loader;
pub mod schema;
pub mod td_api;
pub mod tl_codegen;
pub mod tl_schema;

pub use authorize::{
    AccountType, AuthState, LoginError, LoginMethod, LoginOutcome, Prompter, Question,
    TdlibParameters, log_in, question_for, request_for,
};
pub use client::{
    EXTRA_FIELD, PendingRequest, RequestId, Routed, TdClient, TdResponse, TdUpdate, TdlibError,
    inject_extra, route,
};
pub use ffi::TdApi;
pub use loader::{
    LibrarySource, LoadEnv, ResolvedLibrary, TDLIB_VERSION, TdlibLoadError, resolve, resolve_with,
};
pub use schema::{EntryKind, SchemaError, TdApiSchema, TlEntry, TlParam};
