//! TDLib 的类型与 FFI 绑定（`-sys` 层）：**只提供绑定，不做加载、不做业务**。
//!
//! 与 [`fuyubot-tdlib`](https://docs.rs/fuyubot-tdlib) 的分工：
//!
//! - **本 crate（sys）**：`td_api.tl` 的编译期转换（[`td_api`] 类型 + [`schema`] 元数据）、
//!   C ABI 函数指针声明（[`ffi`]）。依赖只有 serde 家族，**没有** tokio / libloading /
//!   网络——下游只想拿类型做编辑器提示、发请求前校验时，依赖本 crate 就够。
//! - **fuyubot-tdlib**：原生库加载（`loader`）、接收循环（`client`）、登录状态机
//!   （`authorize`），并重导出本 crate 的全部公开 API。
//!
//! 类型**全部**在编译期生成（[`td_api`] 用 `include!` 展开 `$OUT_DIR` 的产物），
//! 运行时不读文件、不解析、不生成。生成规则见 [`tl_codegen`]，解析见 [`tl_schema`]。

pub mod ffi;
pub mod schema;
pub mod td_api;
pub mod tl_codegen;
pub mod tl_schema;

// TL 文档标记 → rustdoc 的转换（只给 tl_codegen 生成期用，不进公开 API）。
mod docs;

// 构建脚本共享的缓存/下载设施：正常编译不带它（ureq 不进运行时依赖图），
// 只在测试下编译进来跑它自带的测试。
#[cfg(test)]
#[allow(dead_code)]
mod build_support;

pub use ffi::{DEFAULT_LOG_VERBOSITY, TdApi};
pub use schema::{EntryKind, SchemaError, TdApiSchema, TlEntry, TlParam};
