//! 编译期烘焙进来的 TDLib API 元数据——`td_api.tl` 的转换产物。
//!
//! 转换**只在编译期发生**：`build.rs` 解析 `td_api.tl`（来源是 `TDLIB_TD_API_TL` 或 vendored
//! 目录）并把它转成 JSON 写进 `$OUT_DIR/td_api.json`，本模块用 `include_str!` 把那份 JSON
//! 烤进二进制。于是：
//!
//! - **运行时不碰文件系统**，也不需要 `td_api.tl` 还在磁盘上；
//! - vendored 出来的可执行文件自带 API 定义（部署时少一个外部依赖）；
//! - 转换实现只有 [`crate::tl_schema`] 一份，编译期与测试走的是同一条路。
//!
//! 为什么转成元数据而不是生成几百个 Rust 结构体：本项目的主旨是「不封装 TDLib 方法，
//! 插件直接 `td_invoke(method_json)`」，生成全套类型恰好与它相悖。元数据能提供的是：
//!
//! 1. **发请求前就能查错**：[`TdApiSchema::check_request`] 挡掉拼错的方法名、拼错的参数名，
//!    以及「把构造器当方法调」这种典型错误（TDLib 只会回一条 error 更新）。
//! 2. **跨语言可用的清单**：[`TdApiSchema::to_json`] 是纯 JSON，工具注册表、插件、外部脚本
//!    都能直接吃（例如把每个函数登记成一个工具）。
//!
//! 函数与构造器的区分靠文件里的分区标记，细节见 [`crate::tl_schema`]。

use std::path::Path;
use std::sync::OnceLock;

use serde_json::Value;

use crate::tl_schema;

pub use crate::tl_schema::{EntryKind, TlEntry, TlParam};

/// `build.rs` 在编译期写下的转换产物；**构建时没拿到 `td_api.tl` 时它的内容是 `{}`**。
///
/// 它必须永远存在：`build.rs` 在任何失败路径上都会写出一个文件，所以「没拿到 td_api.tl」
/// 不会变成编译错误。（见 `AGENTS.md` 约束 27。）
const EMBEDDED_SCHEMA: &str = include_str!(concat!(env!("OUT_DIR"), "/td_api.json"));

/// 元数据坏掉的原因。**没有 IO 类错误**：运行时不读任何文件。
#[derive(Debug, thiserror::Error)]
pub enum SchemaError {
    #[error("烘焙进来的 API 元数据不合法: {0}")]
    Malformed(String),
    #[error("td_api.tl 里没有 {0}")]
    UnknownMethod(String),
    #[error("{0} 是构造器（---types--- 段里的），不能当方法调")]
    NotAFunction(String),
    #[error("方法 {method} 没有名为 {param} 的参数")]
    UnknownParam { method: String, param: String },
    #[error("参数必须是 JSON 对象")]
    BadParams,
}

/// TDLib 的 API 元数据。
#[derive(Debug, Clone, Default)]
pub struct TdApiSchema {
    entries: Vec<TlEntry>,
    /// 这份元数据是从哪个 `td_api.tl` 转来的（编译期记进 JSON 的，仅用于诊断）。
    source: Option<std::path::PathBuf>,
}

impl TdApiSchema {
    // ── 构造 ──────────────────────────────────────────────────────

    /// **编译期烘焙进来的那份**，唯一的生产入口。
    ///
    /// 它**一定存在**：`td_api.tl` 是构建的必需输入，拿不到或解析不了时构建就已经失败了
    /// （见 `build.rs` 的 `acquire_schema` / `bake_schema`）。所以这里返回的是引用而不是 `Option`。
    /// 万一烘焙进来的 JSON 坏了，那是构建脚本的 bug，直接 panic 而不是让调用方去猜。
    /// 只在首次调用时解析一次。
    pub fn compiled() -> &'static Self {
        static COMPILED: OnceLock<TdApiSchema> = OnceLock::new();
        COMPILED.get_or_init(|| {
            let schema = Self::from_json_str(EMBEDDED_SCHEMA)
                .expect("烘焙进来的 API 元数据坏了——这是 build.rs 的 bug");
            assert!(
                !schema.is_empty(),
                "烘焙进来的 API 元数据是空的——这是 build.rs 的 bug"
            );
            schema
        })
    }

    /// 从 [`to_json`](Self::to_json) 的产物还原（[`compiled`](Self::compiled) 走的就是这条路）。
    pub fn from_json(value: &Value) -> Result<Self, SchemaError> {
        let (entries, source) = tl_schema::from_json(value).map_err(SchemaError::Malformed)?;
        Ok(Self {
            entries,
            source: source.map(std::path::PathBuf::from),
        })
    }

    /// 从 JSON 文本还原。
    pub fn from_json_str(text: &str) -> Result<Self, SchemaError> {
        let value: Value = serde_json::from_str(text)
            .map_err(|e| SchemaError::Malformed(format!("不是合法 JSON: {e}")))?;
        Self::from_json(&value)
    }

    // ── 查询 ──────────────────────────────────────────────────────

    /// 这份元数据是从哪个文件转来的（编译期记下的；手工拼出来的 schema 没有来源）。
    pub fn source(&self) -> Option<&Path> {
        self.source.as_deref()
    }

    pub fn entries(&self) -> &[TlEntry] {
        &self.entries
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn get(&self, name: &str) -> Option<&TlEntry> {
        self.entries.iter().find(|entry| entry.name == name)
    }

    /// 可调用的方法（`---functions---` 段）。
    pub fn functions(&self) -> impl Iterator<Item = &TlEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.kind == EntryKind::Function)
    }

    /// 构造器（`---types---` 段）。
    pub fn constructors(&self) -> impl Iterator<Item = &TlEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.kind == EntryKind::Type)
    }

    /// 分区标记缺失、判定不了归属的条目。
    pub fn unresolved(&self) -> impl Iterator<Item = &TlEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.kind == EntryKind::Unknown)
    }

    pub fn function_names(&self) -> Vec<&str> {
        self.functions().map(|entry| entry.name.as_str()).collect()
    }

    /// 转成跨语言能直接吃的 JSON（`build.rs` 写盘用的是同一个函数，形状因此不可能不一致）。
    pub fn to_json(&self) -> Value {
        let source = self.source.as_ref().map(|path| path.display().to_string());
        tl_schema::to_json(&self.entries, source.as_deref())
    }

    /// 仅函数表（`to_json()["functions"]` 那一段）。
    ///
    /// 宿主的内置工具 `fuyubot.td.functions` 就返回它（缺口 13③）：运行时因此能问
    /// 「这版 TDLib 有哪些方法」，而不必把三千多条再拷一份。
    pub fn functions_json(&self) -> Value {
        self.to_json()
            .get("functions")
            .cloned()
            .unwrap_or_else(|| Value::Array(Vec::new()))
    }

    // ── 校验 ──────────────────────────────────────────────────────

    /// 发请求前的本地校验：方法名存在、不是构造器、参数名都认得。
    ///
    /// **不检查必填**：`td_api.tl` 语法里没有「可选」标记（可选性只写在注释的
    /// `Optional.` 里），而 TDLib 自己会报缺参数。这里只挡拼写类错误。
    pub fn check_request(&self, name: &str, params: &Value) -> Result<(), SchemaError> {
        let entry = self
            .get(name)
            .ok_or_else(|| SchemaError::UnknownMethod(name.to_owned()))?;
        if entry.kind == EntryKind::Type {
            return Err(SchemaError::NotAFunction(name.to_owned()));
        }
        let object = params.as_object().ok_or(SchemaError::BadParams)?;
        for key in object.keys() {
            if key == "@type" {
                continue;
            }
            if !entry.has_param(key) {
                return Err(SchemaError::UnknownParam {
                    method: name.to_owned(),
                    param: key.clone(),
                });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "---types---\n\
         updateNewMessage message:Message = Update;\n\
         ---functions---\n\
         getMe = User;\n\
         sendMessage chat_id:int53 text:string = Message;\n";

    /// 走一遍 `build.rs` 的路：TL 文本 → 条目 → JSON → `TdApiSchema`。
    ///
    /// 生产路径里这一步发生在编译期，测试只能自己把两段拼起来——
    /// 用的仍是同一个 [`crate::tl_schema`]，所以拼出来的形状和烤进去的一样。
    fn sample() -> TdApiSchema {
        let entries = tl_schema::parse(SAMPLE).unwrap();
        TdApiSchema::from_json(&tl_schema::to_json(&entries, Some("sample.tl"))).unwrap()
    }

    #[test]
    fn json_round_trip_preserves_everything() {
        let schema = sample();
        let back = TdApiSchema::from_json(&schema.to_json()).unwrap();
        assert_eq!(back.len(), schema.len());
        assert_eq!(back.get("sendMessage").unwrap().kind, EntryKind::Function);
        assert_eq!(back.get("updateNewMessage").unwrap().kind, EntryKind::Type);
        assert_eq!(back.source(), Some(Path::new("sample.tl")));
        // 再转一次应当逐字节一致
        assert_eq!(back.to_json(), schema.to_json());
    }

    /// `functions_json` 必须与 `to_json()["functions"]` 逐字节一致（同一份实现）。
    #[test]
    fn functions_json_is_the_functions_bucket() {
        let schema = sample();
        let functions = schema.functions_json();
        assert_eq!(functions, schema.to_json()["functions"]);
        assert_eq!(functions.as_array().unwrap().len(), 2);
        assert!(
            functions
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["name"] == "sendMessage")
        );
    }

    #[test]
    fn json_string_round_trip_is_what_compile_time_uses() {
        // build.rs 写盘、compiled() 读回来的就是这条路径
        let text = serde_json::to_string(&sample().to_json()).unwrap();
        let back = TdApiSchema::from_json_str(&text).unwrap();
        assert_eq!(back.function_names(), ["getMe", "sendMessage"]);
        assert_eq!(back.get("sendMessage").unwrap().params.len(), 2);
    }

    #[test]
    fn a_broken_payload_is_an_error_not_a_panic() {
        assert!(TdApiSchema::from_json_str("{").is_err());
        assert!(TdApiSchema::from_json(&serde_json::json!([1])).is_err());
    }

    #[test]
    fn the_build_always_bakes_a_usable_schema() {
        // 构建成功就说明 `td_api.tl` 已经拿到并转换过：拿不到的话 build.rs 会让构建失败。
        // 所以 `compiled()` 没有 Option，这里也可以**无条件**断言内容。
        let schema = TdApiSchema::compiled();
        assert!(!schema.is_empty(), "烤进来的 schema 不该是空的");
        assert!(
            schema.functions().next().is_some(),
            "至少要认出函数，否则 build.rs 会拒绝构建"
        );
        assert!(
            schema.constructors().next().is_some(),
            "至少要认出构造器，否则 build.rs 会拒绝构建"
        );
        // 同一个静态实例，两次调用拿到的是同一份
        assert!(std::ptr::eq(schema, TdApiSchema::compiled()));
    }

    #[test]
    fn check_request_catches_the_usual_typos() {
        let schema = sample();

        assert!(
            schema
                .check_request(
                    "sendMessage",
                    &serde_json::json!({"chat_id": 1, "text": "hi"})
                )
                .is_ok()
        );
        // @type 由宿主补，要放行
        assert!(
            schema
                .check_request("sendMessage", &serde_json::json!({"@type": "sendMessage"}))
                .is_ok()
        );
        assert!(matches!(
            schema.check_request("sendMesage", &serde_json::json!({})),
            Err(SchemaError::UnknownMethod(_))
        ));
        assert!(matches!(
            schema.check_request("sendMessage", &serde_json::json!({"txt": "x"})),
            Err(SchemaError::UnknownParam { .. })
        ));
        assert!(matches!(
            schema.check_request("updateNewMessage", &serde_json::json!({})),
            Err(SchemaError::NotAFunction(_))
        ));
        assert!(matches!(
            schema.check_request("sendMessage", &serde_json::json!([1, 2])),
            Err(SchemaError::BadParams)
        ));
    }

    #[test]
    fn unresolved_entries_are_callable_but_not_constructors() {
        // 没有分区标记时判不了归属：既不当构造器拦掉，也不假装知道它是函数。
        let entries = tl_schema::parse("getMe = User;\n").unwrap();
        let schema = TdApiSchema::from_json(&tl_schema::to_json(&entries, None)).unwrap();
        assert_eq!(schema.unresolved().count(), 1);
        assert!(schema.functions().next().is_none());
        assert!(
            schema
                .check_request("getMe", &serde_json::json!({}))
                .is_ok()
        );
        assert!(
            schema
                .check_request("nope", &serde_json::json!({}))
                .is_err()
        );
    }
}
