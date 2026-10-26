//! 工具注册表：JSON Schema 描述 + 跨语言调用。
//!
//! 只依赖 `fuyubot-core`。注册/注销通过 [`ToolRegistry::on_change`] 的监听器广播，
//! 由调用方（宿主）决定要不要转成 `fuyubot-event` 的事件——本 crate 不认识事件系统。

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{CStr, CString, c_char};
use std::sync::{Arc, Mutex, MutexGuard};

use fuyubot_core::{Context, Effect, KernelError};
use serde_json::{Value, json};

/// 工具处理器：C ABI，收 JSON 字符串、还 JSON 字符串。
///
/// 返回的指针只需活到**下一次调用本函数之前**：registry 拿到后立刻拷贝。
/// Rust 侧实现用 [`reply`] 生成返回值。
pub type ToolHandlerFn = extern "C" fn(input_json: *const c_char) -> *const c_char;

thread_local! {
    static REPLY: std::cell::RefCell<CString> = std::cell::RefCell::new(CString::default());
}

/// 把一段 JSON 放进线程局部缓冲并返回指针，供 [`ToolHandlerFn`] 返回。
///
/// 线程局部正好满足「活到下一位调用者读完」这个契约。内含 NUL 时退化成空串。
pub fn reply(json: &str) -> *const c_char {
    REPLY.with(|slot| {
        *slot.borrow_mut() = CString::new(json).unwrap_or_default();
        slot.borrow().as_ptr()
    })
}

/// 工具请求标识：给异步/跨语言调用做关联用。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ToolRequestId(u64);

impl std::fmt::Display for ToolRequestId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "#{}", self.0)
    }
}

impl ToolRequestId {
    pub fn as_u64(self) -> u64 {
        self.0
    }
}

/// 工具元数据 + 处理器。
#[derive(Debug, Clone)]
pub struct ToolDescriptor {
    /// 全局唯一，建议 `plugin_name.tool_name`。
    pub name: String,
    pub version: semver::Version,
    pub description: String,
    /// JSON Schema，描述入参。
    pub input_schema: Value,
    /// JSON Schema，描述返回。留空（`null` 或 `{}`）表示不校验。
    pub output_schema: Value,
    pub required_permissions: Vec<String>,
    pub idempotent: bool,
    pub timeout_ms: u64,
    pub is_async: bool,
    /// `None` = 只有元数据的声明型工具（跨语言注册时常见，本进程没有可调代码）。
    pub handler: Option<ToolHandlerFn>,
}

impl ToolDescriptor {
    /// 建一个描述：版本默认 `0.1.0`、权限为空、`timeout_ms` 为 30000。
    /// 其余字段是 `pub` 的，直接改。
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
        handler: Option<ToolHandlerFn>,
    ) -> Self {
        Self {
            name: name.into(),
            version: semver::Version::new(0, 1, 0),
            description: description.into(),
            input_schema,
            output_schema: Value::Null,
            required_permissions: Vec::new(),
            idempotent: false,
            timeout_ms: 30_000,
            is_async: false,
            handler,
        }
    }

    /// 跨语言传的元数据：**不含处理器指针**（指针没法序列化）。
    pub fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "version": self.version.to_string(),
            "description": self.description,
            "input_schema": self.input_schema,
            "output_schema": self.output_schema,
            "required_permissions": self.required_permissions,
            "idempotent": self.idempotent,
            "timeout_ms": self.timeout_ms,
            "is_async": self.is_async,
            "callable": self.handler.is_some(),
        })
    }

    /// [`to_json`](Self::to_json) 的逆操作。
    ///
    /// 处理器一律是 `None`：JSON 里没有函数指针的位置（C ABI 那边也一样）。
    pub fn from_json(value: &Value) -> Result<Self, ToolError> {
        let object = value
            .as_object()
            .ok_or_else(|| ToolError::BadDescriptor("必须是 JSON 对象".to_owned()))?;

        let name = object
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::BadDescriptor("缺少 name".to_owned()))?;

        let mut descriptor = Self::new(
            name,
            object
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default(),
            object.get("input_schema").cloned().unwrap_or(Value::Null),
            None,
        );

        if let Some(version) = object.get("version").and_then(Value::as_str) {
            descriptor.version = semver::Version::parse(version)
                .map_err(|e| ToolError::BadDescriptor(format!("version 不合法: {e}")))?;
        }
        if let Some(schema) = object.get("output_schema") {
            descriptor.output_schema = schema.clone();
        }
        if let Some(permissions) = object.get("required_permissions") {
            let permissions = permissions.as_array().ok_or_else(|| {
                ToolError::BadDescriptor("required_permissions 必须是数组".to_owned())
            })?;
            descriptor.required_permissions = permissions
                .iter()
                .map(|item| {
                    item.as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| ToolError::BadDescriptor("权限项必须是字符串".to_owned()))
                })
                .collect::<Result<_, _>>()?;
        }
        if let Some(idempotent) = object.get("idempotent").and_then(Value::as_bool) {
            descriptor.idempotent = idempotent;
        }
        if let Some(is_async) = object.get("is_async").and_then(Value::as_bool) {
            descriptor.is_async = is_async;
        }
        if let Some(timeout_ms) = object.get("timeout_ms").and_then(Value::as_u64) {
            descriptor.timeout_ms = timeout_ms;
        }
        Ok(descriptor)
    }
}

/// 一次工具调用的结果。
#[derive(Debug, Clone)]
pub struct ToolCall {
    pub request_id: ToolRequestId,
    pub name: String,
    pub output: Value,
}

/// 注册/注销广播。`kind()` 就是跨语言总线上要发的 event_type。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolEvent {
    Registered(String),
    Unregistered(String),
}

impl ToolEvent {
    pub fn kind(&self) -> &'static str {
        match self {
            ToolEvent::Registered(_) => "tool_registered",
            ToolEvent::Unregistered(_) => "tool_unregistered",
        }
    }

    pub fn tool_name(&self) -> &str {
        match self {
            ToolEvent::Registered(name) | ToolEvent::Unregistered(name) => name,
        }
    }

    /// 交给外部事件总线时用的负载。
    pub fn to_payload(&self) -> Value {
        json!({ "event": self.kind(), "name": self.tool_name() })
    }
}

/// 工具相关错误。
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("工具 {0} 没有注册")]
    NotFound(String),
    #[error("工具 {0} 已经注册过了")]
    AlreadyRegistered(String),
    #[error("工具名不能为空")]
    EmptyName,
    #[error("工具描述不合法: {0}")]
    BadDescriptor(String),
    #[error("工具 {name} 的 schema 非法: {detail}")]
    BadSchema { name: String, detail: String },
    #[error("入参不合法: {0}")]
    InvalidInput(String),
    #[error("返回值不合法: {0}")]
    InvalidOutput(String),
    #[error("调用工具 {name} 缺少权限: {missing}")]
    PermissionDenied { name: String, missing: String },
    #[error("工具 {0} 只有元数据，没有可调用的处理器")]
    NoHandler(String),
    #[error("查询条件不合法: {0}")]
    BadFilter(String),
    #[error("入参里含 NUL 字节")]
    InteriorNul,
    #[error("处理器没有返回合法 JSON: {0}")]
    BadHandlerOutput(String),
    #[error(
        "Context 里还没有 ToolRegistry：先 `let _guard = fuyubot_tool::install(&ctx)?;` 并持有它"
    )]
    NotInstalled,
    #[error(transparent)]
    Kernel(#[from] KernelError),
}

/// 校验阶段，决定错误落到 `InvalidInput` 还是 `InvalidOutput`。
#[derive(Clone, Copy)]
enum Direction {
    Input,
    Output,
}

/// 变更监听器。
type Listener = Arc<dyn Fn(&ToolEvent) -> fuyubot_core::Result<()> + Send + Sync>;

#[derive(Default)]
struct Inner {
    next_id: u64,
    /// `BTreeMap`：`list_tools` 的输出稳定，跨语言比对不会因顺序抖动。
    tools: BTreeMap<String, ToolDescriptor>,
    permissions: BTreeSet<String>,
    listeners: Vec<Listener>,
    /// 工具表的版本号：注册 / 注销都 +1。
    ///
    /// 给缓存了 [`ToolRegistry::list_tools_json`] 结果的调用方做失效判断——
    /// 工具表变更不频繁，而列工具是热路径（插件每次都要问一遍）。
    revision: u64,
}

/// 工具注册表。`Clone` 共享同一张表。
#[derive(Clone, Default)]
pub struct ToolRegistry {
    inner: Arc<Mutex<Inner>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册工具，返回的句柄 drop 即注销。
    ///
    /// schema 在这里就先编译一遍：写错了要当场报，别拖到调用时才炸。
    pub fn register_tool(&self, descriptor: ToolDescriptor) -> Result<ToolHandle, ToolError> {
        if descriptor.name.is_empty() {
            return Err(ToolError::EmptyName);
        }
        check_schema(&descriptor.name, &descriptor.input_schema)?;
        check_schema(&descriptor.name, &descriptor.output_schema)?;

        {
            let mut inner = self.lock();
            if inner.tools.contains_key(&descriptor.name) {
                return Err(ToolError::AlreadyRegistered(descriptor.name));
            }
            inner
                .tools
                .insert(descriptor.name.clone(), descriptor.clone());
            inner.revision += 1;
        }
        self.broadcast(ToolEvent::Registered(descriptor.name.clone()));

        let table = Arc::clone(&self.inner);
        let key = descriptor.name.clone();
        let effect = Effect::new(move || {
            {
                let mut inner = lock(&table);
                inner.tools.remove(&key);
                inner.revision += 1;
            }
            broadcast_to(&table, ToolEvent::Unregistered(key));
        });

        Ok(ToolHandle {
            name: descriptor.name,
            effect,
        })
    }

    /// 调用工具：查表 → 校验权限 → 校验 input_schema → 调处理器 → 校验 output_schema。
    pub fn call_tool(&self, name: &str, input: &Value) -> Result<ToolCall, ToolError> {
        let (descriptor, request_id, missing) = {
            let mut inner = self.lock();
            let descriptor = inner
                .tools
                .get(name)
                .cloned()
                .ok_or_else(|| ToolError::NotFound(name.to_owned()))?;
            let missing: Vec<String> = descriptor
                .required_permissions
                .iter()
                .filter(|permission| !inner.permissions.contains(permission.as_str()))
                .cloned()
                .collect();
            let request_id = ToolRequestId(inner.next_id);
            inner.next_id += 1;
            (descriptor, request_id, missing)
        };

        if !missing.is_empty() {
            return Err(ToolError::PermissionDenied {
                name: name.to_owned(),
                missing: missing.join(", "),
            });
        }

        validate(name, &descriptor.input_schema, input, Direction::Input)?;
        let handler = descriptor
            .handler
            .ok_or_else(|| ToolError::NoHandler(name.to_owned()))?;

        let request = CString::new(input.to_string()).map_err(|_| ToolError::InteriorNul)?;
        let raw = handler(request.as_ptr());
        let output = if raw.is_null() {
            Value::Null
        } else {
            // SAFETY: 契约规定这个指针在下一次调用处理器之前有效，我们立刻读完。
            let text = unsafe { CStr::from_ptr(raw) }.to_string_lossy();
            serde_json::from_str(&text)
                .map_err(|_| ToolError::BadHandlerOutput(text.into_owned()))?
        };
        validate(name, &descriptor.output_schema, &output, Direction::Output)?;

        Ok(ToolCall {
            request_id,
            name: name.to_owned(),
            output,
        })
    }

    /// 工具表的版本号：注册 / 注销都让它 +1。
    ///
    /// 缓存 [`list_tools_json`](Self::list_tools_json) 的一方拿它做失效判断：
    /// 版本号没变，序列化的结果就不会变。
    pub fn revision(&self) -> u64 {
        self.lock().revision
    }

    /// 全部工具，按名字排序。
    pub fn list_tools(&self) -> Vec<ToolDescriptor> {
        self.lock().tools.values().cloned().collect()
    }

    pub fn list_tools_json(&self) -> Value {
        Value::Array(
            self.list_tools()
                .iter()
                .map(ToolDescriptor::to_json)
                .collect(),
        )
    }

    /// 按条件过滤。支持的键：`name`（子串，忽略大小写）、`permission`、`idempotent`、`is_async`。
    ///
    /// 不认识的键直接报错——静默忽略只会让写错条件的人拿到「全部工具」还以为过滤生效了。
    pub fn query_tools(&self, filter: &Value) -> Result<Vec<ToolDescriptor>, ToolError> {
        let Some(object) = filter.as_object() else {
            return Err(ToolError::BadFilter("过滤条件必须是 JSON 对象".to_owned()));
        };
        for key in object.keys() {
            if !matches!(
                key.as_str(),
                "name" | "permission" | "idempotent" | "is_async"
            ) {
                return Err(ToolError::BadFilter(key.clone()));
            }
        }

        let needle = object
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_ascii_lowercase);
        let permission = object.get("permission").and_then(Value::as_str);
        let idempotent = object.get("idempotent").and_then(Value::as_bool);
        let is_async = object.get("is_async").and_then(Value::as_bool);

        Ok(self
            .list_tools()
            .into_iter()
            .filter(|tool| {
                needle
                    .as_ref()
                    .is_none_or(|needle| tool.name.to_ascii_lowercase().contains(needle))
                    && permission.is_none_or(|permission| {
                        tool.required_permissions.iter().any(|p| p == permission)
                    })
                    && idempotent.is_none_or(|value| tool.idempotent == value)
                    && is_async.is_none_or(|value| tool.is_async == value)
            })
            .collect())
    }

    pub fn query_tools_json(&self, filter: &Value) -> Result<Value, ToolError> {
        Ok(Value::Array(
            self.query_tools(filter)?
                .iter()
                .map(ToolDescriptor::to_json)
                .collect(),
        ))
    }

    /// 给调用方授权。宿主用它决定插件能碰哪些工具。
    pub fn grant(&self, permission: impl Into<String>) {
        self.lock().permissions.insert(permission.into());
    }

    pub fn revoke(&self, permission: &str) {
        self.lock().permissions.remove(permission);
    }

    pub fn granted(&self) -> Vec<String> {
        self.lock().permissions.iter().cloned().collect()
    }

    /// 监听注册/注销，返回的句柄 drop 即摘掉监听。
    pub fn on_change(
        &self,
        listener: impl Fn(&ToolEvent) -> fuyubot_core::Result<()> + Send + Sync + 'static,
    ) -> Effect {
        let id = {
            let mut inner = self.lock();
            let id = inner.listeners.len();
            inner.listeners.push(Arc::new(listener));
            id
        };
        let table = Arc::clone(&self.inner);
        Effect::new(move || {
            if let Some(slot) = lock(&table).listeners.get_mut(id) {
                // 用「换成空壳」代替删除：下标不会漂移，也不会动到别人的监听。
                *slot = Arc::new(|_| Ok(()));
            }
        })
    }

    pub fn contains(&self, name: &str) -> bool {
        self.lock().tools.contains_key(name)
    }

    pub fn len(&self) -> usize {
        self.lock().tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lock().tools.is_empty()
    }

    /// ponytail: 监听器报错只忽略（注册路径没地方报）；要观测就让监听器自己记日志
    fn broadcast(&self, event: ToolEvent) {
        broadcast_to(&self.inner, event);
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        lock(&self.inner)
    }
}

impl std::fmt::Debug for ToolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolRegistry")
            .field("tools", &self.lock().tools.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// 中毒的锁也照用：一个工具的 panic 不该让整张表卡死。
fn lock(table: &Arc<Mutex<Inner>>) -> MutexGuard<'_, Inner> {
    table.lock().unwrap_or_else(|e| e.into_inner())
}

/// 监听器在锁**外面**调用：它可能反过来查注册表。
fn broadcast_to(table: &Arc<Mutex<Inner>>, event: ToolEvent) {
    let listeners = lock(table).listeners.clone();
    for listener in listeners {
        let _ = listener(&event);
    }
}

/// schema 为空（`null` 或 `{}`）表示不校验。
fn is_empty_schema(schema: &Value) -> bool {
    schema.is_null() || schema.as_object().is_some_and(|object| object.is_empty())
}

fn check_schema(name: &str, schema: &Value) -> Result<(), ToolError> {
    if is_empty_schema(schema) {
        return Ok(());
    }
    jsonschema::validator_for(schema)
        .map(|_| ())
        .map_err(|e| ToolError::BadSchema {
            name: name.to_owned(),
            detail: e.to_string(),
        })
}

fn validate(
    name: &str,
    schema: &Value,
    value: &Value,
    direction: Direction,
) -> Result<(), ToolError> {
    if is_empty_schema(schema) {
        return Ok(());
    }
    // ponytail: 每次调用都重新编译 schema；调用量大时把 Validator 缓存进 ToolDescriptor
    let validator = jsonschema::validator_for(schema).map_err(|e| ToolError::BadSchema {
        name: name.to_owned(),
        detail: e.to_string(),
    })?;
    validator.validate(value).map_err(|e| match direction {
        Direction::Input => ToolError::InvalidInput(e.to_string()),
        Direction::Output => ToolError::InvalidOutput(e.to_string()),
    })
}

/// 工具句柄。drop 即注销。
#[must_use = "工具句柄一被丢弃就立刻注销刚注册的工具；要保留就把它存起来"]
pub struct ToolHandle {
    name: String,
    effect: Effect,
}

impl ToolHandle {
    pub fn name(&self) -> &str {
        &self.name
    }

    /// 显式注销。
    pub fn remove(self) {
        drop(self.effect);
    }
}

impl std::fmt::Debug for ToolHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolHandle")
            .field("name", &self.name)
            .finish()
    }
}

/// 把注册表装进内核 [`Context`]。
///
/// 返回的 `Effect` **必须一直持有**：它一 drop，注册表就从 Context 里消失了。
pub fn install(ctx: &Context) -> Result<Effect, ToolError> {
    Ok(ctx.provide(ToolRegistry::new())?)
}

/// `Context` 上的工具入口。
pub trait ToolContextExt {
    fn tools(&self) -> Result<Arc<ToolRegistry>, ToolError>;
    fn register_tool(&self, descriptor: ToolDescriptor) -> Result<ToolHandle, ToolError>;
    fn call_tool(&self, name: &str, input: &Value) -> Result<ToolCall, ToolError>;
}

impl ToolContextExt for Context {
    fn tools(&self) -> Result<Arc<ToolRegistry>, ToolError> {
        self.get::<ToolRegistry>()
            .map_err(|_| ToolError::NotInstalled)
    }

    fn register_tool(&self, descriptor: ToolDescriptor) -> Result<ToolHandle, ToolError> {
        self.tools()?.register_tool(descriptor)
    }

    fn call_tool(&self, name: &str, input: &Value) -> Result<ToolCall, ToolError> {
        self.tools()?.call_tool(name, input)
    }
}

/// 给 C/C++/Zig 用的 JSON 入口。
pub mod ffi {
    //! 进程级默认注册表 + 几个 C ABI 函数。
    //!
    //! 返回值一律是 JSON；出错时返回 `{"error": "..."}` 而不是空指针，
    //! 调用方读完必须交给 [`fuyubot_tool_string_free`] 释放。

    use std::ffi::{CStr, CString, c_char};
    use std::sync::OnceLock;

    use serde_json::Value;

    use super::ToolRegistry;

    /// 进程级默认注册表。
    pub fn registry() -> &'static ToolRegistry {
        static REGISTRY: OnceLock<ToolRegistry> = OnceLock::new();
        REGISTRY.get_or_init(ToolRegistry::new)
    }

    fn to_c_string(json: &Value) -> *mut c_char {
        match CString::new(json.to_string()) {
            Ok(text) => text.into_raw(),
            Err(_) => std::ptr::null_mut(),
        }
    }

    fn error_json(message: impl std::fmt::Display) -> *mut c_char {
        to_c_string(&serde_json::json!({ "error": message.to_string() }))
    }

    /// 全部工具的元数据 JSON 数组。
    #[unsafe(no_mangle)]
    pub extern "C" fn fuyubot_tool_list_tools() -> *mut c_char {
        to_c_string(&registry().list_tools_json())
    }

    /// 按条件过滤；`filter_json` 为空指针时等同于列出全部。
    ///
    /// # Safety
    /// `filter_json` 必须是 NUL 结尾的合法 UTF-8，或者空指针。
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn fuyubot_tool_query_tools(filter_json: *const c_char) -> *mut c_char {
        let registry = registry();
        if filter_json.is_null() {
            return to_c_string(&registry.list_tools_json());
        }
        // SAFETY: 调用方按上面的契约保证指针合法。
        let text = unsafe { CStr::from_ptr(filter_json) }.to_string_lossy();
        let Ok(filter) = serde_json::from_str::<Value>(&text) else {
            return error_json("过滤条件不是合法 JSON");
        };
        match registry.query_tools_json(&filter) {
            Ok(json) => to_c_string(&json),
            Err(e) => error_json(e),
        }
    }

    /// 释放本模块返回的字符串。
    ///
    /// # Safety
    /// `ptr` 必须来自本模块的返回值，且只能释放一次。
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn fuyubot_tool_string_free(ptr: *mut c_char) {
        if !ptr.is_null() {
            // SAFETY: 由 `CString::into_raw` 产生，交还给 `CString` 即释放。
            drop(unsafe { CString::from_raw(ptr) });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    extern "C" fn echo_handler(input: *const c_char) -> *const c_char {
        // SAFETY: registry 保证传入的是 NUL 结尾的合法字符串。
        let text = unsafe { CStr::from_ptr(input) }.to_string_lossy();
        let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        reply(&json!({ "echo": value }).to_string())
    }

    extern "C" fn boom_handler(_: *const c_char) -> *const c_char {
        reply("这不是 JSON")
    }

    fn echo_tool(name: &str) -> ToolDescriptor {
        ToolDescriptor::new(
            name,
            "回显入参",
            json!({
                "type": "object",
                "properties": { "text": { "type": "string" } },
                "required": ["text"],
                "additionalProperties": false
            }),
            Some(echo_handler),
        )
    }

    /// 验收 3：注册 foo → list_tools 含 foo → call_tool 成功 → drop 后 list 不再含 foo。
    #[test]
    fn register_call_list_then_drop() {
        let registry = ToolRegistry::new();
        let handle = registry.register_tool(echo_tool("foo")).unwrap();

        assert!(registry.contains("foo"));
        let listed = registry.list_tools_json();
        assert!(
            listed
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| tool["name"] == "foo")
        );
        assert_eq!(listed[0]["callable"], json!(true));

        let call = registry.call_tool("foo", &json!({ "text": "hi" })).unwrap();
        assert_eq!(call.name, "foo");
        assert_eq!(call.output["echo"]["text"], json!("hi"));
        assert_eq!(call.request_id.to_string(), "#0");

        drop(handle);
        assert!(!registry.contains("foo"));
        assert!(registry.list_tools_json().as_array().unwrap().is_empty());
        assert!(matches!(
            registry.call_tool("foo", &json!({ "text": "hi" })),
            Err(ToolError::NotFound(_))
        ));
    }

    #[test]
    fn input_schema_is_enforced() {
        let registry = ToolRegistry::new();
        let _handle = registry.register_tool(echo_tool("foo")).unwrap();

        assert!(matches!(
            registry.call_tool("foo", &json!({ "text": 1 })),
            Err(ToolError::InvalidInput(_))
        ));
        assert!(matches!(
            registry.call_tool("foo", &json!({ "text": "hi", "extra": 1 })),
            Err(ToolError::InvalidInput(_))
        ));
    }

    #[test]
    fn output_schema_is_enforced() {
        let registry = ToolRegistry::new();
        let mut descriptor = echo_tool("foo");
        descriptor.output_schema = json!({ "type": "string" });
        let _handle = registry.register_tool(descriptor).unwrap();

        assert!(matches!(
            registry.call_tool("foo", &json!({ "text": "hi" })),
            Err(ToolError::InvalidOutput(_))
        ));
    }

    #[test]
    fn broken_handler_output_and_bad_schema_are_errors() {
        let registry = ToolRegistry::new();
        let _boom = registry
            .register_tool(ToolDescriptor::new(
                "boom",
                "返回坏 JSON",
                json!({}),
                Some(boom_handler),
            ))
            .unwrap();
        assert!(matches!(
            registry.call_tool("boom", &json!({})),
            Err(ToolError::BadHandlerOutput(_))
        ));

        let mut broken = echo_tool("broken");
        broken.input_schema = json!({ "type": 42 });
        assert!(matches!(
            registry.register_tool(broken),
            Err(ToolError::BadSchema { .. })
        ));
    }

    #[test]
    fn permissions_gate_the_call() {
        let registry = ToolRegistry::new();
        let mut guarded = echo_tool("write");
        guarded.required_permissions = vec!["db.write".into()];
        let mut metadata_only = echo_tool("declared");
        metadata_only.handler = None;

        let _write = registry.register_tool(guarded).unwrap();
        let _declared = registry.register_tool(metadata_only).unwrap();

        let error = registry
            .call_tool("write", &json!({ "text": "hi" }))
            .unwrap_err();
        assert!(
            matches!(error, ToolError::PermissionDenied { .. }),
            "{error}"
        );

        registry.grant("db.write");
        assert!(
            registry
                .call_tool("write", &json!({ "text": "hi" }))
                .is_ok()
        );

        assert!(matches!(
            registry.call_tool("declared", &json!({ "text": "hi" })),
            Err(ToolError::NoHandler(_))
        ));

        registry.revoke("db.write");
        assert_eq!(registry.granted(), Vec::<String>::new());
    }

    #[test]
    fn query_filters_and_rejects_unknown_keys() {
        let registry = ToolRegistry::new();
        let mut write = echo_tool("db.write");
        write.required_permissions = vec!["db.write".into()];
        write.idempotent = true;
        let _a = registry.register_tool(echo_tool("chat.echo")).unwrap();
        let _b = registry.register_tool(write).unwrap();

        let names = |value: Value| -> Vec<String> {
            value
                .as_array()
                .unwrap()
                .iter()
                .map(|tool| tool["name"].as_str().unwrap().to_owned())
                .collect()
        };

        assert_eq!(
            names(registry.query_tools_json(&json!({})).unwrap()),
            ["chat.echo", "db.write"]
        );
        assert_eq!(
            names(
                registry
                    .query_tools_json(&json!({ "name": "ECHO" }))
                    .unwrap()
            ),
            ["chat.echo"]
        );
        assert_eq!(
            names(
                registry
                    .query_tools_json(&json!({ "permission": "db.write" }))
                    .unwrap()
            ),
            ["db.write"]
        );
        assert_eq!(
            names(
                registry
                    .query_tools_json(&json!({ "idempotent": true }))
                    .unwrap()
            ),
            ["db.write"]
        );
        assert!(matches!(
            registry.query_tools_json(&json!({ "no_such_key": 1 })),
            Err(ToolError::BadFilter(_))
        ));
        assert!(matches!(
            registry.query_tools_json(&json!([1, 2])),
            Err(ToolError::BadFilter(_))
        ));
    }

    #[test]
    fn change_listeners_see_registration_and_unregistration() {
        let registry = ToolRegistry::new();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let _listener = registry.on_change({
            let seen = Arc::clone(&seen);
            move |event: &ToolEvent| {
                seen.lock()
                    .unwrap()
                    .push((event.kind().to_owned(), event.tool_name().to_owned()));
                Ok(())
            }
        });

        let handle = registry.register_tool(echo_tool("foo")).unwrap();
        handle.remove();

        let seen = seen.lock().unwrap();
        assert_eq!(
            *seen,
            [
                ("tool_registered".to_owned(), "foo".to_owned()),
                ("tool_unregistered".to_owned(), "foo".to_owned())
            ]
        );
        assert_eq!(
            ToolEvent::Registered("foo".into()).to_payload(),
            json!({ "event": "tool_registered", "name": "foo" })
        );
    }

    #[test]
    fn dropping_the_listener_handle_stops_notifications() {
        let registry = ToolRegistry::new();
        let seen = Arc::new(Mutex::new(0usize));
        let listener = registry.on_change({
            let seen = Arc::clone(&seen);
            move |_| {
                *seen.lock().unwrap() += 1;
                Ok(())
            }
        });

        let first = registry.register_tool(echo_tool("a")).unwrap();
        drop(listener);
        let _second = registry.register_tool(echo_tool("b")).unwrap();

        assert_eq!(*seen.lock().unwrap(), 1, "摘掉的监听不该再收到通知");
        drop(first);
    }

    #[test]
    fn duplicate_and_empty_names_are_errors() {
        let registry = ToolRegistry::new();
        let _handle = registry.register_tool(echo_tool("foo")).unwrap();
        assert!(matches!(
            registry.register_tool(echo_tool("foo")),
            Err(ToolError::AlreadyRegistered(_))
        ));
        assert!(matches!(
            registry.register_tool(echo_tool("")),
            Err(ToolError::EmptyName)
        ));
    }

    #[test]
    fn c_abi_round_trips_json() {
        let registry = ffi::registry();
        let handle = registry.register_tool(echo_tool("ffi.echo")).unwrap();

        let raw = ffi::fuyubot_tool_list_tools();
        assert!(!raw.is_null());
        // SAFETY: 返回值由本模块分配、以 NUL 结尾。
        let text = unsafe { CStr::from_ptr(raw) }
            .to_string_lossy()
            .into_owned();
        // SAFETY: 只释放一次。
        unsafe { ffi::fuyubot_tool_string_free(raw) };

        let parsed: Value = serde_json::from_str(&text).unwrap();
        assert!(
            parsed
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| tool["name"] == "ffi.echo")
        );

        let filter = CString::new(json!({ "name": "ffi" }).to_string()).unwrap();
        // SAFETY: filter 是合法的 NUL 结尾字符串。
        let raw = unsafe { ffi::fuyubot_tool_query_tools(filter.as_ptr()) };
        let text = unsafe { CStr::from_ptr(raw) }
            .to_string_lossy()
            .into_owned();
        unsafe { ffi::fuyubot_tool_string_free(raw) };
        let parsed: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed.as_array().unwrap().len(), 1);

        // 坏过滤条件返回 error JSON，而不是空指针。
        let bad = CString::new("{not json").unwrap();
        let raw = unsafe { ffi::fuyubot_tool_query_tools(bad.as_ptr()) };
        let text = unsafe { CStr::from_ptr(raw) }
            .to_string_lossy()
            .into_owned();
        unsafe { ffi::fuyubot_tool_string_free(raw) };
        assert!(serde_json::from_str::<Value>(&text).unwrap()["error"].is_string());

        handle.remove();
    }

    #[test]
    fn context_integration() {
        let ctx = Context::new();
        let _guard = install(&ctx).unwrap();

        let handle = ctx.register_tool(echo_tool("ctx.echo")).unwrap();
        let call = ctx
            .call_tool("ctx.echo", &json!({ "text": "hey" }))
            .unwrap();
        assert_eq!(call.output["echo"]["text"], json!("hey"));

        drop(handle);
        assert!(ctx.tools().unwrap().is_empty());

        // 没装注册表时给出的错误要能指路。
        let bare = Context::new();
        let text = bare.tools().map(|_| ()).unwrap_err().to_string();
        assert!(text.contains("fuyubot_tool::install"), "{text}");
    }
}
