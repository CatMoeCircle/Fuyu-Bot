//! `TdClient`：`td_receive` 循环、`@extra` 注入与 `RequestId` 匹配。

use std::collections::HashMap;
use std::ffi::{CStr, CString, c_void};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use libloading::Library;
use serde_json::Value;
use tokio::sync::oneshot;

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
    /// [`TdClient::send_and_wait`] 在预算内没等到这个请求的响应（缺口 17）。
    #[error("等 {id} 的响应超时（等了 {waited:?}）")]
    Timeout { id: RequestId, waited: Duration },
    /// 等待期间客户端被销毁：这个 `id` 的响应不会再有人送进来了。
    #[error("TDLib 客户端已销毁，{id} 的响应不会再来了")]
    ClientClosed { id: RequestId },
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

/// 谁在等哪个请求的响应（缺口 17）。
///
/// 与 [`TdClient::pending`] **不是一张表**：`pending` 是诊断（「已经发出去、还没回」），
/// 这里只说「**有人正在等**」。没有等待者的请求（`send`）不会进这张表，
/// 等待者超时 / 取消之后也不会留下悬空条目。
type Waiters = Arc<Mutex<HashMap<RequestId, oneshot::Sender<TdResponse>>>>;

/// 等待者的清理守卫：**future 被取消**（`select!` / `JoinHandle::abort`）时，
/// `send_and_wait` 后半段那两行清理根本执行不到，只有 `Drop` 会跑。
///
/// 正常路径（响应到达 / 超时）也会经过它，`remove` 是幂等的——响应到达时
/// [`TdClient::handle`] 已经摘走了，这里再摘一次返回 `None`，无害。
struct WaiterGuard {
    id: RequestId,
    waiters: Waiters,
}

impl Drop for WaiterGuard {
    fn drop(&mut self) {
        // **不能 panic**：这里也可能在 unwinding 里跑（双重 panic 直接 abort）。
        self.waiters
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.id);
    }
}

/// 把「等一个已经发出去的响应」变成 `Result`：超时与客户端销毁都要有明确错误，不许 panic。
async fn await_response(
    id: RequestId,
    rx: oneshot::Receiver<TdResponse>,
    timeout: Duration,
) -> Result<TdResponse, TdlibError> {
    match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(response)) => Ok(response),
        // Sender 被 drop：客户端销毁（`Drop` 会清空这张表）。绝不能让调用方永远挂着。
        Ok(Err(_)) => Err(TdlibError::ClientClosed { id }),
        Err(_) => Err(TdlibError::Timeout {
            id,
            waited: timeout,
        }),
    }
}

/// 一个 TDLib json-client。
///
/// `raw` 由 TDLib 持有，`lib` 只是保证动态库不被卸载。
pub struct TdClient {
    api: TdApi,
    raw: *mut c_void,
    pending: Arc<Mutex<HashMap<RequestId, PendingRequest>>>,
    /// 缺口 17：谁在等哪个请求的响应。响应到达时 [`handle`](Self::handle) 摘走并唤醒；
    /// 超时 / 取消由 [`WaiterGuard`] 摘走；客户端销毁时 [`Drop`] 整表清空。
    waiters: Waiters,
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

        // 日志详细度先压到默认值（**在 create 之前**）：TDLib 自己的默认太啰嗦，
        // 每轮 `td_receive` 的超时、`Begin to wait for updates…` 之类全打 stderr。
        // 这是全局设置，所以在这里设一次对所有客户端都生效；要更详细就随后调
        // `set_log_verbosity`（`[tdlib] log_verbosity`）。
        // SAFETY: 符号已解析，且这是 TDLib 允许在任何初始化之前调用的全局设置。
        unsafe { (api.set_log_verbosity)(crate::ffi::DEFAULT_LOG_VERBOSITY) };

        // SAFETY: 符号已解析；客户端句柄由 TDLib 分配。
        let raw = unsafe { (api.create)() };
        if raw.is_null() {
            return Err(TdlibLoadError::NullClientHandle.into());
        }

        Ok(Self {
            api,
            raw,
            pending: Arc::new(Mutex::new(HashMap::new())),
            waiters: Arc::new(Mutex::new(HashMap::new())),
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
            waiters: Arc::new(Mutex::new(HashMap::new())),
            next_id: AtomicU64::new(1),
            _lib: None,
        }
    }

    /// 覆盖日志详细度（默认 [`crate::ffi::DEFAULT_LOG_VERBOSITY`] = 1，在 [`new`](Self::new) 里设好）。
    ///
    /// **全局设置**，不带 client 句柄：调一次对所有客户端都生效。
    /// `0` = 只打致命错误，`1` = 加警告（默认），越大越啰嗦。
    pub fn set_log_verbosity(&self, level: i32) {
        // 负数没有意义（TDLib 会当 0 处理），但别静默吞掉调用方的意图——钳到 0。
        // SAFETY: 符号在 new() 里已解析；这是 TDLib 的全局日志设置。
        unsafe { (self.api.set_log_verbosity)(level.max(0)) };
    }

    /// 发起请求：注入 `@extra`、登记 pending、交给 `td_send`。
    ///
    /// `td_send` 在返回前就把字符串拷走了，所以临时 `CString` 可以立即释放。
    ///
    /// 这是**不等响应**的那条路：响应回来还是走接收循环的回调/管线。要「发出去并等它自己的
    /// 响应」用 [`send_and_wait`](Self::send_and_wait)（缺口 17）。
    pub fn send(&self, request: Value) -> Result<RequestId, TdlibError> {
        let id = RequestId(self.next_id.fetch_add(1, Ordering::Relaxed));
        self.send_with_id(request, id)?;
        Ok(id)
    }

    /// [`send`](Self::send) 的主体，`id` 由调用方给。
    ///
    /// [`send_and_wait`](Self::send_and_wait) 需要**先登记等待者、再发**：接收循环在另一个
    /// 线程上，响应完全可能在登记之前就回来。所以把「注入 + 登记 + 发送」这段抽出来，
    /// 两条路共用同一份实现（只差谁先谁后）。
    fn send_with_id(&self, mut request: Value, id: RequestId) -> Result<(), TdlibError> {
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
        Ok(())
    }

    /// 发一条请求，等**它自己的**响应（gaps 缺口 17）。
    ///
    /// 响应照样由**接收循环**收进来（[`spawn_receive_loop`](Self::spawn_receive_loop) /
    /// `UpdatePipeline::spawn_dispatch`），[`handle`](Self::handle) 在走原有回调/管线路径
    /// **之外**再唤醒这里的等待者——两条路互不影响。
    ///
    /// 因此调用时**必须有东西在收**：接收循环已经起来，或者另一条线程在跑
    /// [`receive`](Self::receive)。没有的话只会等到超时（不会死锁，也不会 panic）。
    /// ⚠️ 别在接收循环的 `callback` 里 `.await` 它：那等于让收的人等自己要等的东西。
    ///
    /// 返回值是**原样的** [`TdResponse`]：TDLib 的业务错误（`@type: "error"`）也是 `Ok`，
    /// 用 [`TdResponse::is_error`] 判（和 `log_in_with` 的处理方式一致）。
    ///
    /// 清理约定（都不留悬空条目）：
    /// - 响应到达 → [`handle`](Self::handle) 摘走等待者；
    /// - 超时 → [`TdlibError::Timeout`]，等待者摘掉；请求仍留在 [`pending`](Self::pending) 里
    ///   （它确实还在途，响应真回来时照常摘除——诊断说的是「在途」，不是「还有人等」）；
    /// - 客户端销毁 → [`TdlibError::ClientClosed`]；
    /// - future 被取消（`select!` / `abort`）→ [`WaiterGuard`] 的 `Drop` 摘掉。
    pub async fn send_and_wait(
        &self,
        request: Value,
        timeout: Duration,
    ) -> Result<TdResponse, TdlibError> {
        let id = RequestId(self.next_id.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = oneshot::channel();
        self.waiters.lock().unwrap().insert(id, tx);
        // 守卫在函数返回/被取消时都会跑；响应先到的情形它再 remove 一次是幂等的。
        let _guard = WaiterGuard {
            id,
            waiters: Arc::clone(&self.waiters),
        };
        // 登记之后才发：否则响应可能比等待者先到，谁也叫不醒。
        self.send_with_id(request, id)?;
        await_response(id, rx, timeout).await
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

    /// 分流、摘掉对应的 pending 请求，并唤醒等这个响应的等待者。
    ///
    /// 「唤醒」放在这里（而不是 `receive` 里）是因为 `handle` 是所有接收路径的共同出口：
    /// 接收循环、登录状态机自己的同步 `receive`、集成测试的轮询都经过它。
    fn handle(&self, payload: Value) -> Routed {
        let routed = route(payload);
        if let Routed::Response(response) = &routed {
            self.pending.lock().unwrap().remove(&response.id);
            // 等待者可能已经超时 / 被取消（`send` 返回 `Err`），那不是错误；
            // 回调/管线那条路已经拿到了 `routed`，这里只负责唤醒。
            // 先把 sender 摘出来再 `send`：唤醒可能让等待方立刻跑起来，别攥着锁。
            let waiter = self.waiters.lock().unwrap().remove(&response.id);
            if let Some(waiter) = waiter {
                let _ = waiter.send(response.clone());
            }
        }
        routed
    }

    /// 起一个阻塞线程跑接收循环，把每条结果交给 `callback`，返回**停止把手**。
    ///
    /// 循环会一直跑到 [`ReceiveLoop::stop`] 被调用为止（gaps 缺口 8：以前它没有退出条件，
    /// 于是「重启 TDLib」「退回登录」「Ctrl-C 退出」都做不到）。
    #[must_use = "不收下这个把手就永远停不掉接收循环"]
    pub fn spawn_receive_loop<F>(self: &Arc<Self>, mut callback: F) -> ReceiveLoop
    where
        F: FnMut(Routed) + Send + 'static,
    {
        let this = Arc::clone(self);
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        // 固定 1s 轮询；要低延迟就换 TDLib 的 update callback 注册。
        // 代价是 stop() 之后最长还要 1s 才真的停下（见 ReceiveLoop::stop）。
        let handle = tokio::task::spawn_blocking(move || {
            while !flag.load(Ordering::Relaxed) {
                if let Some(routed) = this.receive(Duration::from_secs(1)) {
                    callback(routed);
                }
            }
        });
        ReceiveLoop { stop, handle }
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

/// 接收循环的停止把手（gaps 缺口 8）。
///
/// **停止是显式的 [`stop`](Self::stop)，不是 drop 即停** —— 这点刻意和 `Effect` /
/// `Subscription` 那套「drop 即注销」相反：后台任务被静默丢掉时，调用方很容易误以为
/// 「还在收更新」，而一个明确的 `stop()` 让「谁在什么时候停的」在代码里看得见。
#[derive(Debug)]
pub struct ReceiveLoop {
    stop: Arc<AtomicBool>,
    handle: tokio::task::JoinHandle<()>,
}

impl ReceiveLoop {
    /// 请求循环退出。**最多再等一个轮询周期（1s）**才真正停下——`stop()` 返回不代表已经停了，
    /// 要确认就紧接着 `join().await`。
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    /// 等循环真的退出。`stop()` 之后调；**没 stop 就 await 会一直等下去**。
    pub async fn join(self) {
        let _ = self.handle.await;
    }
}

impl Drop for TdClient {
    fn drop(&mut self) {
        // 先把等待者放掉（缺口 17）：还在 `send_and_wait` 里的调用方会立刻拿到
        // `TdlibError::ClientClosed`，而不是一直等一个永远不会再来的响应。
        // 守卫随后还会 remove 一次，表已经空了，无害。
        self.waiters
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
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

    unsafe extern "C" fn noop_set_log_verbosity(_: i32) {}

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
            set_log_verbosity: noop_set_log_verbosity,
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

    // ── 缺口 17：`send_and_wait` 的 oneshot 路径 ────────────────────────────
    //
    // 下面这些测试用 `Capture` 那套假 `TdApi`，**直接调 `handle`** 模拟接收循环
    // （`handle` 就是所有接收路径的共同出口），不碰真库、不需要账号。

    /// 造一个把请求抓进 `sent` 的假客户端。
    ///
    /// `Capture` 故意泄漏到测试进程结束：`TdClient` 的 `raw` 一直指着它，
    /// 提前释放就是悬空指针（`noop_destroy` 什么都不做，也没有地方回收）。
    fn fake_client() -> (Arc<TdClient>, Arc<StdMutex<Vec<String>>>) {
        let sent = Arc::new(StdMutex::new(Vec::new()));
        let capture = Box::new(Capture {
            sent: Arc::clone(&sent),
        });
        let raw = Box::into_raw(capture) as *mut c_void;
        let api = TdApi {
            create: noop_create,
            destroy: noop_destroy,
            send: capture_send,
            receive: noop_receive,
            execute: noop_execute,
            set_log_verbosity: noop_set_log_verbosity,
        };
        (Arc::new(TdClient::from_parts(api, raw)), sent)
    }

    /// 等到至少 `expected` 条请求在途，返回 `(id, 方法名)` 列表（按 id 升序）。
    async fn wait_until_pending(client: &TdClient, expected: usize) -> Vec<(RequestId, String)> {
        for _ in 0..10_000 {
            let pending = client.pending();
            if pending.len() >= expected {
                return pending
                    .into_iter()
                    .map(|(id, method, _)| (id, method))
                    .collect();
            }
            tokio::task::yield_now().await;
        }
        panic!("等不到 {expected} 条在途请求");
    }

    fn id_of(pending: &[(RequestId, String)], method: &str) -> RequestId {
        pending
            .iter()
            .find(|(_, name)| name == method)
            .unwrap_or_else(|| panic!("在途请求里没有 {method}"))
            .0
    }

    /// 正常唤醒：响应到达时既走原来的 `Routed` 出口，也唤醒等这个 id 的人。
    #[tokio::test]
    async fn send_and_wait_wakes_up_on_its_own_response() {
        let (client, _sent) = fake_client();
        let waiting = tokio::spawn({
            let client = Arc::clone(&client);
            async move {
                client
                    .send_and_wait(json!({ "@type": "getMe" }), Duration::from_secs(5))
                    .await
            }
        });

        let pending = wait_until_pending(&client, 1).await;
        let id = id_of(&pending, "getMe");

        // 模拟接收循环：`handle` 的返回值照旧交给回调/管线，同时唤醒等待者。
        let routed = client.handle(json!({
            "@type": "ok",
            "@extra": id.to_extra(),
            "value": 7,
        }));
        assert!(matches!(routed, Routed::Response(_)));

        let response = waiting.await.unwrap().unwrap();
        assert_eq!(response.id, id);
        assert_eq!(response.payload["value"], json!(7));
        assert!(client.pending().is_empty());
        assert!(client.waiters.lock().unwrap().is_empty());
    }

    /// 超时：明确错误、等待者被摘掉，不能 panic、不能留下悬空条目。
    #[tokio::test]
    async fn a_timeout_returns_an_error_and_cleans_the_waiter() {
        let (client, _sent) = fake_client();
        let error = client
            .send_and_wait(json!({ "@type": "getMe" }), Duration::from_millis(20))
            .await
            .unwrap_err();
        assert!(matches!(error, TdlibError::Timeout { .. }), "{error}");
        assert!(
            client.waiters.lock().unwrap().is_empty(),
            "超时之后等待者必须被摘掉"
        );
        // 请求本身仍然算在途（响应可能还会来，届时 `handle` 照常摘掉 pending）：
        // 诊断说的是「发出去、还没回」，不是「还有人等」。
        assert_eq!(client.pending().len(), 1);
    }

    /// 响应不属于自己时不受影响：先回 beta，只有 beta 被唤醒，alpha 继续等自己的。
    #[tokio::test]
    async fn a_response_for_someone_else_does_not_wake_this_waiter() {
        let (client, _sent) = fake_client();
        let alpha = tokio::spawn({
            let client = Arc::clone(&client);
            async move {
                client
                    .send_and_wait(json!({ "@type": "alpha" }), Duration::from_secs(5))
                    .await
            }
        });
        let beta = tokio::spawn({
            let client = Arc::clone(&client);
            async move {
                client
                    .send_and_wait(json!({ "@type": "beta" }), Duration::from_secs(5))
                    .await
            }
        });

        let pending = wait_until_pending(&client, 2).await;
        let alpha_id = id_of(&pending, "alpha");
        let beta_id = id_of(&pending, "beta");

        client.handle(json!({ "@type": "ok", "@extra": beta_id.to_extra(), "who": "beta" }));
        let response = beta.await.unwrap().unwrap();
        assert_eq!(response.payload["who"], json!("beta"));
        // alpha 还在等：它的 id 不能被 beta 的响应顺手摘掉。
        let still_pending = client.pending();
        assert_eq!(still_pending.len(), 1);
        assert_eq!(still_pending[0].0, alpha_id);

        client.handle(json!({ "@type": "ok", "@extra": alpha_id.to_extra(), "who": "alpha" }));
        let response = alpha.await.unwrap().unwrap();
        assert_eq!(response.payload["who"], json!("alpha"));
        assert!(client.pending().is_empty());
        assert!(client.waiters.lock().unwrap().is_empty());
    }

    /// 客户端销毁：等待者被唤醒（收到错误），而不是一直等到超时。
    #[tokio::test]
    async fn dropping_the_client_wakes_waiters_with_an_error() {
        let (client, _sent) = fake_client();
        let (tx, rx) = oneshot::channel();
        let id = RequestId(41);
        client.waiters.lock().unwrap().insert(id, tx);

        drop(client);

        let error = await_response(id, rx, Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(matches!(error, TdlibError::ClientClosed { .. }), "{error}");
    }

    /// future 被取消（`abort`）时也要摘掉等待者——`Drop` 是唯一会跑到的清理点。
    #[tokio::test]
    async fn a_cancelled_waiter_does_not_leak() {
        let (client, _sent) = fake_client();
        let task = tokio::spawn({
            let client = Arc::clone(&client);
            async move {
                client
                    .send_and_wait(json!({ "@type": "getMe" }), Duration::from_secs(5))
                    .await
            }
        });
        wait_until_pending(&client, 1).await;
        assert_eq!(client.waiters.lock().unwrap().len(), 1);

        task.abort();
        let _ = task.await;

        for _ in 0..10_000 {
            if client.waiters.lock().unwrap().is_empty() {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("被取消的等待者没被摘掉");
    }
}
