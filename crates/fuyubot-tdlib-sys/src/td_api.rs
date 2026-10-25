//! 编译期从 `td_api.tl` 生成的 TDLib 类型。
//!
//! 这个模块的内容**全部**来自 `build.rs`：它把 `td_api.tl` 转成 Rust 源码写进
//! `$OUT_DIR/td_api_generated.rs`，这里用 `include!` 展成模块。运行时不做任何生成、
//! 也不读任何文件。
//!
//! - 每个**类**（`Message`、`User`…）是一个 `enum`，构造器是它的内联结构体变体，
//!   带 `#[serde(tag = "@type")]`，所以能直接反序列化 TDLib 的 JSON。
//! - 每个**函数**（`getMe`、`sendMessage`…）是一个 `struct`，带 `METHOD` 常量，
//!   直接喂给 `td_invoke_simplified`。
//! - 生成的规则与取舍写在 [`crate::tl_codegen`] 里。
//!
//! 构建时拿不到 `td_api.tl` 时**构建会失败**（见约束 25/27），所以这个模块一定是有内容的：
//! 要么来自 Releases 下载的真文件，要么来自 `TDLIB_TD_API_TL` 指的那份。
//!
//! ```ignore
//! let request = td_api::SendMessage {
//!     chat_id: 42,
//!     input_message_content: Some(Box::new(td_api::InputMessageContent::InputMessageText { .. })),
//!     ..Default::default()
//! };
//! host.td_invoke_simplified(td_api::SendMessage::METHOD, &serde_json::to_string(&request)?)?;
//! ```
//!
//! 上面那段不参与 doctest：具体类型取决于构建时用的那份 `td_api.tl`。

include!(concat!(env!("OUT_DIR"), "/td_api_generated.rs"));
