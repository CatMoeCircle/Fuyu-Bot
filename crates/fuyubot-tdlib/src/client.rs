//! `TdClient`：`td_receive` 循环、`@extra` 注入与 `RequestId` 匹配。

use std::collections::HashMap;
use std::ffi::{CStr, CString, c_void};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use libloading::Library;
use serde_json::Value;

use crate::ffi::TdApi;
use crate::loader::{self, TdlibLoadError};

/// TDLib 用它把响应和请求对上。
pub const EXTRA_FIELD: &str = "@extra";

/// 请求标识：自增 u64，序列化成十进制字符串放进 `@extra`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RequestId(u64);

impl RequestId {
    pub fn as_u64(self) -> u64 {
        self.0
    }

    /// TDLib 的 `@extra` 是字符串，不是数字。
    pub fn to_extra(self) -> String {
        self.0.to_string()
    }

    pub fn from_extra(extra: &str) -> Option<Self> {
        extra.parse().ok().map(Self)
    }
}

impl std::fmt::Display for RequestId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "#{}", self.0)
    }
}

/// 已发出、还没收到响应的请求。
#[derive(Debug, Clone)]
pub struct PendingRequest {
    pub method: String,
    pub sent_at: Instant,
}

/// 带 `@extra` 的响应。
#[derive(Debug, Clone)]
pub struct TdResponse {
    pub id: RequestId,
    pub payload: Value,
}

impl TdResponse {
    /// TDLib 用 `@type: "error"` 表示失败。
    pub fn is_error(&self) -> bool {
        self.payload.get("@type").and_then(Value::as_str) == Some("error")
    }

    pub fn error_message(&self) -> Option<&str> {
        self.is_error()
            .then(|| self.payload.get("message").and_then(Value::as_str))
            .flatten()
    }
}

/// 不带 `@extra` 的更新。
#[derive(Debug, Clone)]
pub struct TdUpdate {
    pub kind: String,
    pub payload: Value,
}

/// 接收循环分流出来的东西。
#[derive(Debug, Clone)]
pub enum Routed {
    Response(TdResponse),
    Update(TdUpdate),
}

/// 传输层错误。
#[derive(Debug, thiserror::Error)]
pub enum TdlibError {
    #[error(transparent)]
    Load(#[from] TdlibLoadError),
    #[error("请求里含 NUL 字节，无法转成 C 字符串")]
    InteriorNul,
    #[error("TDLib 返回的不是合法 JSON: {0}")]
    BadResponse(String),
}

/// 把 `@extra` 注入请求。不是 JSON 对象就原样不动——TDLib 的请求必须是对象。
pub fn inject_extra(request: &mut Value, id: RequestId) {
    if let Value::Object(map) = request {
        map.insert(EXTRA_FIELD.to_owned(), Value::String(id.to_extra()));
    }
}

/// 把一条收到的 JSON 分流成响应或更新。
pub fn route(payload: Value) -> Routed {
    let extra = payload.get(EXTRA_FIELD).and_then(Value::as_str);
    if let Some(id) = extra.and_then(RequestId::from_extra) {
        return Routed::Response(TdResponse { id, payload });
    }
    let kind = payload
        .get("@type")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_owned();
    Routed::Update(TdUpdate { kind, payload })
}

/// 一个 TDLib json-client。
///
/// `raw` 由 TDLib 持有，`lib` 只是保证动态库不被卸载。
pub struct TdClient {
    api: TdApi,
    raw: *mut c_void,
    pending: Arc<Mutex<HashMap<RequestId, PendingRequest>>>,
    next_id: AtomicU64,
    /// 保持动态库存活；`None` 只出现在测试注入符号时。
    _lib: Option<Library>,
}

// SAFETY: TDLib 的 json-client 接口按文档是线程安全的（td_send / td_receive 可从任意线程调用）。
// 唯一的例外是 td_json_client_destroy：它必须等所有线程都不再使用该句柄后再调用，
// 所以 `Drop` 只在最后一个 `Arc` 归零时执行。
unsafe impl Send for TdClient {}
unsafe impl Sync for TdClient {}

impl TdClient {
    /// 按优先级解析并加载 tdjson，然后创建一个原生客户端。
    pub fn new() -> Result<Self, TdlibError> {
        let resolved = loader::resolve()?;
        let location = resolved.target.display().to_string();

        // SAFETY: target 来自 loader，且我们把 Library 一直留在结构体里。
        let lib = unsafe { loader::open_library(&resolved.target) }.map_err(|source| {
            TdlibLoadError::Dlopen {
                location: location.clone(),
                source,
            }
        })?;

        // SAFETY: lib 是刚加载的 TDLib，随后由 self 持有。
        let api = unsafe { loader::resolve_symbols(&lib) }?;
        // SAFETY: 符号已解析；客户端句柄由 TDLib 分配。
        let raw = unsafe { (api.create)() };
        if raw.is_null() {
            return Err(TdlibLoadError::NullClientHandle.into());
        }

        Ok(Self {
            api,
            raw,
            pending: Arc::new(Mutex::new(HashMap::new())),
            next_id: AtomicU64::new(1),
            _lib: Some(lib),
        })
    }

    /// 用现成的符号表构造，供测试注入替代实现。`raw` 必须是 TDLib 给的句柄。
    #[doc(hidden)]
    pub fn from_parts(api: TdApi, raw: *mut c_void) -> Self {
        Self {
            api,
            raw,
            pending: Arc::new(Mutex::new(HashMap::new())),
            next_id: AtomicU64::new(1),
            _lib: None,
        }
    }

    /// 发起请求：注入 `@extra`、登记 pending、交给 `td_send`。
    ///
    /// `td_send` 在返回前就把字符串拷走了，所以临时 `CString` 可以立即释放。
    pub fn send(&self, mut request: Value) -> Result<RequestId, TdlibError> {
        let id = RequestId(self.next_id.fetch_add(1, Ordering::Relaxed));
        inject_extra(&mut request, id);

        let method = request
            .get("@type")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_owned();
        let json = CString::new(request.to_string()).map_err(|_| TdlibError::InteriorNul)?;

        self.pending.lock().unwrap().insert(
            id,
            PendingRequest {
                method,
                sent_at: Instant::now(),
            },
        );
        // SAFETY: raw 是有效的客户端句柄，json 在调用期间存活。
        unsafe { (self.api.send)(self.raw, json.as_ptr()) };
        Ok(id)
    }

    /// 同步执行无状态方法，不经过接收循环。
    pub fn execute(&self, mut request: Value) -> Result<Value, TdlibError> {
        // TDLib 的 execute 是同步的，`@extra` 由我们自己加上去就行。
        let id = RequestId(self.next_id.fetch_add(1, Ordering::Relaxed));
        inject_extra(&mut request, id);
        let json = CString::new(request.to_string()).map_err(|_| TdlibError::InteriorNul)?;

        // SAFETY: raw 是有效的客户端句柄；返回指针由 TDLib 持有，只在下次调用前有效。
        let raw = unsafe { (self.api.execute)(self.raw, json.as_ptr()) };
        if raw.is_null() {
            return Ok(Value::Null);
        }
        let text = unsafe { CStr::from_ptr(raw) }.to_string_lossy();
        serde_json::from_str(&text).map_err(|_| TdlibError::BadResponse(text.into_owned()))
    }

    /// 拉一次更新并分流；`None` 表示超时（TDLib 那边没数据）。
    pub fn receive(&self, timeout: Duration) -> Option<Routed> {
        // SAFETY: raw 有效；timeout 是秒。
        let raw = unsafe { (self.api.receive)(self.raw, timeout.as_secs_f64()) };
        if raw.is_null() {
            return None;
        }
        let text = unsafe { CStr::from_ptr(raw) }.to_string_lossy();
        let payload: Value = serde_json::from_str(&text).ok()?;
        Some(self.handle(payload))
    }

    /// 分流并把对应的 pending 请求摘掉。
    fn handle(&self, payload: Value) -> Routed {
        let routed = route(payload);
        if let Routed::Response(response) = &routed {
            self.pending.lock().unwrap().remove(&response.id);
        }
        routed
    }

    /// 起一个阻塞线程跑接收循环，把每条结果交给 `callback`。
    ///
    /// 循环不会退出（也不该退出）：TDLib 的接收线程生命周期等于进程。
    pub fn spawn_receive_loop<F>(self: &Arc<Self>, mut callback: F) -> tokio::task::JoinHandle<()>
    where
        F: FnMut(Routed) + Send + 'static,
    {
        let this = Arc::clone(self);
        // ponytail: 固定 1s 轮询；要低延迟就换 TDLib 的 update callback 注册
        tokio::task::spawn_blocking(move || {
            loop {
                if let Some(routed) = this.receive(Duration::from_secs(1)) {
                    callback(routed);
                }
            }
        })
    }

    /// 已发出、还没收到响应的请求（`RequestId`、方法名、等了多久）。
    pub fn pending(&self) -> Vec<(RequestId, String, Duration)> {
        let now = Instant::now();
        let mut list: Vec<_> = self
            .pending
            .lock()
            .unwrap()
            .iter()
            .map(|(id, req)| (*id, req.method.clone(), now.duration_since(req.sent_at)))
            .collect();
        list.sort_by_key(|(id, _, _)| *id);
        list
    }
}

impl Drop for TdClient {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            // SAFETY: raw 由 TDLib 创建，且本结构体是唯一持有者。
            unsafe { (self.api.destroy)(self.raw) };
            self.raw = std::ptr::null_mut();
        }
        // self.lib 随后自然 drop，动态库在该句柄使用完之后卸载。
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::ffi::c_char;
    use std::sync::Mutex as StdMutex;

    /// 抓取 `td_send` 实际收到的 C 字符串。
    struct Capture {
        sent: Arc<StdMutex<Vec<String>>>,
    }

    unsafe extern "C" fn capture_send(ctx: *mut c_void, request: *const c_char) {
        // SAFETY: ctx 是 &Capture，由测试保证活到 drop 之后。
        let capture = unsafe { &*(ctx as *const Capture) };
        let text = unsafe { CStr::from_ptr(request) }
            .to_string_lossy()
            .into_owned();
        capture.sent.lock().unwrap().push(text);
    }

    unsafe extern "C" fn noop_destroy(_: *mut c_void) {}
    unsafe extern "C" fn noop_create() -> *mut c_void {
        std::ptr::null_mut()
    }
    unsafe extern "C" fn noop_receive(_: *mut c_void, _: f64) -> *const c_char {
        std::ptr::null()
    }
    unsafe extern "C" fn noop_execute(_: *mut c_void, _: *const c_char) -> *const c_char {
        std::ptr::null()
    }

    #[test]
    fn send_injects_and_records_extra() {
        let sent = Arc::new(StdMutex::new(Vec::new()));
        let capture = Box::new(Capture {
            sent: Arc::clone(&sent),
        });
        let api = TdApi {
            create: noop_create,
            destroy: noop_destroy,
            send: capture_send,
            receive: noop_receive,
            execute: noop_execute,
        };
        let client = TdClient::from_parts(api, capture.as_ref() as *const Capture as *mut c_void);

        let id = client.send(json!({ "@type": "getMe" })).unwrap();

        let captured = sent.lock().unwrap();
        assert_eq!(captured.len(), 1);
        let parsed: Value = serde_json::from_str(&captured[0]).unwrap();
        assert_eq!(parsed["@type"], json!("getMe"));
        assert_eq!(parsed[EXTRA_FIELD], json!(id.to_extra()));

        // 请求已登记，等响应回来。
        let pending = client.pending();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].1, "getMe");

        // 收到带同一个 @extra 的响应后，pending 应该被摘掉。
        let routed = client.handle(json!({ "@type": "ok", "@extra": id.to_extra() }));
        match routed {
            Routed::Response(response) => assert_eq!(response.id, id),
            Routed::Update(_) => panic!("带 @extra 的必须是响应"),
        }
        assert!(client.pending().is_empty());
    }

    #[test]
    fn route_splits_responses_from_updates() {
        match route(json!({ "@type": "updateNewMessage", "@extra": "7" })) {
            Routed::Response(response) => {
                assert_eq!(response.id, RequestId(7));
                assert!(!response.is_error());
            }
            Routed::Update(_) => panic!("带 @extra 的必须是响应"),
        }

        match route(json!({ "@type": "updateNewMessage" })) {
            Routed::Update(update) => assert_eq!(update.kind, "updateNewMessage"),
            Routed::Response(_) => panic!("不带 @extra 的必须是更新"),
        }

        // @extra 不是数字时按更新处理，不能 panic。
        match route(json!({ "@type": "ok", "@extra": "not-a-number" })) {
            Routed::Update(update) => assert_eq!(update.kind, "ok"),
            Routed::Response(_) => panic!("无法解析的 @extra 应退化为更新"),
        }
    }

    #[test]
    fn error_responses_are_recognisable() {
        let routed = route(
            json!({ "@type": "error", "@extra": "3", "code": 401, "message": "unauthorized" }),
        );
        let Routed::Response(response) = routed else {
            panic!("期望响应");
        };
        assert!(response.is_error());
        assert_eq!(response.error_message(), Some("unauthorized"));
    }
}
