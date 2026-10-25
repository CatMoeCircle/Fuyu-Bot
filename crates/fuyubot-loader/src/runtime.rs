//! 宿主运行时：把 ABI VTable 的每个函数桥到阶段 2/3 的机制上。
//!
//! 这里就是「宿主层做桥」那句话的落点：四个机制 crate 彼此不依赖，
//! 把它们组合起来的地方只能有一个，那就是这里。

use std::collections::{HashMap, VecDeque};
use std::ffi::{CStr, CString, c_char, c_void};
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use fuyubot_abi::{
    ABI_VERSION, EventCallbackFn, FILTER_EVENT_TYPE_KEY, HostApiVTable, LogLevel,
    MiddlewareCallbackFn, MiddlewareHandle, RequestId, ServiceHandle, ServiceVTable,
    SubscriptionHandle, ToolHandle, WaitHandle,
};
use fuyubot_core::{Context, Effect};
use fuyubot_event::{EventBus, Filter, SubscriptionHandle as EventSubscription};
use fuyubot_middleware::{
    Middleware, MiddlewareContext, MiddlewareHandle as MwRegistration, Next, Pipeline,
    Stage as MwStage,
};
use fuyubot_service::{ServiceContainer, ServiceHandle as SvcRegistration};
use fuyubot_tdlib::TdClient;
use fuyubot_tool::{ToolDescriptor, ToolHandle as ToolRegistration, ToolRegistry};
use serde_json::{Value, json};

/// 响应通道，与 [`fuyubot_abi::RESPONSE_EVENT`] 同一个名字。
pub use fuyubot_abi::RESPONSE_EVENT;
/// 更新的兜底通道，与 [`fuyubot_abi::UPDATE_EVENT`] 同一个名字。
pub use fuyubot_abi::UPDATE_EVENT;

/// 插件日志最多留这么多行（环形缓冲，旧的被挤掉）。
const LOG_CAPACITY: usize = 256;

/// 已发出请求的留档上限。
const REQUEST_LOG_CAPACITY: usize = 256;

/// 没接 TDLib 时用的合成请求号起点：躲开 TDLib `@extra` 的低位区间。
const SYNTHETIC_REQUEST_BASE: u64 = 1 << 48;

/// 宿主侧跨 ABI 的一次注册。
///
/// 句柄**故意不读**：留着它们就是为了让 `Drop` 在回滚时真的去注销。
#[allow(dead_code)]
enum RegistrationKind {
    Subscription(EventSubscription),
    Tool(ToolRegistration),
    Service(SvcRegistration),
    Middleware(MwRegistration),
}

#[allow(dead_code)]
struct Registration {
    id: u64,
    /// 谁注册的：插件 id（0 = 宿主自己）。卸载插件时按它整批回滚。
    owner: u64,
    kind: RegistrationKind,
}

/// 一个 `wait_for_update` 等待位。
#[derive(Default)]
struct Waiter {
    result: Mutex<Option<Value>>,
    cancelled: AtomicBool,
    ready: Condvar,
}

/// 存在服务容器里的裸 vtable 指针。
///
/// SAFETY: vtable 是只读的，且 ABI 契约要求提供方让它活到注销为止。
#[derive(Clone, Copy)]
struct AbiService(*const ServiceVTable);

unsafe impl Send for AbiService {}
unsafe impl Sync for AbiService {}

impl std::fmt::Debug for AbiService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("AbiService").field(&self.0).finish()
    }
}

/// 宿主运行时。
///
/// 强制定制成 `Arc`：`host_ctx` 必须指向一个地址稳定的实例，而且插件可能
/// 比加载器活得久，所以每个 [`LoadedPlugin`](crate::LoadedPlugin) 都会持有一份克隆。
pub struct HostRuntime {
    ctx: Context,
    events: Arc<EventBus>,
    services: Arc<ServiceContainer>,
    tools: Arc<ToolRegistry>,
    middleware: Arc<Pipeline>,
    td: OnceLock<Arc<TdClient>>,
    /// 装四个机制拿到的守卫，必须留住，否则机制会从 Context 里消失。
    _guards: Vec<Effect>,
    /// 所有跨 ABI 的注册；插件卸载时按 owner 一次性回滚。
    registrations: Mutex<Vec<Registration>>,
    next_id: AtomicU64,
    /// 当前正在加载的插件 id（0 = 宿主自己）。
    current_owner: AtomicU64,
    next_owner: AtomicU64,
    waiters: Mutex<HashMap<u64, Arc<Waiter>>>,
    next_waiter: AtomicU64,
    /// 插件经 `log` 打进来的日志，环形缓冲、有上限。
    log_lines: Mutex<VecDeque<String>>,
    /// 请求 id → 发起它的 owner。响应回来时按它决定投递给谁。
    request_owners: Mutex<HashMap<u64, u64>>,
    /// 没接 TDLib 时用的合成请求号。
    next_synthetic_request: AtomicU64,
    /// 已经发出的请求留档（诊断 + 测试用）。
    sent_requests: Mutex<VecDeque<SentRequest>>,
    vtable: HostApiVTable,
}

/// 一条已经发出的请求。
#[derive(Debug, Clone)]
pub struct SentRequest {
    pub id: u64,
    pub method: String,
    pub params: Value,
}

/// 作用域守卫：把 `current_owner` 换成某个插件，离开作用域自动还原。
struct OwnerGuard<'a> {
    runtime: &'a HostRuntime,
    previous: u64,
}

impl Drop for OwnerGuard<'_> {
    fn drop(&mut self) {
        self.runtime
            .current_owner
            .store(self.previous, Ordering::Relaxed);
    }
}

impl HostRuntime {
    /// 在给定的 [`Context`] 上装好四个机制，并生成 VTable。
    pub fn new(ctx: Context) -> Result<Arc<Self>, String> {
        let mut guards = Vec::with_capacity(4);
        guards.push(fuyubot_event::install(&ctx).map_err(|e| e.to_string())?);
        guards.push(fuyubot_service::install(&ctx).map_err(|e| e.to_string())?);
        guards.push(fuyubot_tool::install(&ctx).map_err(|e| e.to_string())?);
        guards.push(fuyubot_middleware::install(&ctx).map_err(|e| e.to_string())?);

        let events = ctx.get::<EventBus>().map_err(|e| e.to_string())?;
        let services = ctx.get::<ServiceContainer>().map_err(|e| e.to_string())?;
        let tools = ctx.get::<ToolRegistry>().map_err(|e| e.to_string())?;
        let middleware = ctx.get::<Pipeline>().map_err(|e| e.to_string())?;

        Ok(Arc::new_cyclic(|weak| Self {
            ctx,
            events,
            services,
            tools,
            middleware,
            td: OnceLock::new(),
            _guards: guards,
            registrations: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(1),
            current_owner: AtomicU64::new(0),
            next_owner: AtomicU64::new(1),
            waiters: Mutex::new(HashMap::new()),
            next_waiter: AtomicU64::new(1),
            log_lines: Mutex::new(VecDeque::new()),
            request_owners: Mutex::new(HashMap::new()),
            next_synthetic_request: AtomicU64::new(SYNTHETIC_REQUEST_BASE),
            sent_requests: Mutex::new(VecDeque::new()),
            // `host_ctx` 指向这个 `Arc` 内部；地址稳定，插件活着期间一直有效。
            vtable: HostApiVTable {
                version: ABI_VERSION,
                size: core::mem::size_of::<HostApiVTable>() as u32,
                host_ctx: weak.as_ptr() as *mut c_void,
                on: Some(api_on),
                off: Some(api_off),
                emit: Some(api_emit),
                register_tool: Some(api_register_tool),
                call_tool: Some(api_call_tool),
                list_tools: Some(api_list_tools),
                provide_service: Some(api_provide_service),
                get_service: Some(api_get_service),
                td_invoke: Some(api_td_invoke),
                td_invoke_simplified: Some(api_td_invoke_simplified),
                td_execute: Some(api_td_execute),
                use_middleware: Some(api_use_middleware),
                wait_for_update: Some(api_wait_for_update),
                cancel_wait: Some(api_cancel_wait),
                log: Some(api_log),
            },
        }))
    }

    /// 交给插件的 VTable 指针。
    pub fn vtable_ptr(&self) -> *const HostApiVTable {
        &self.vtable
    }

    pub fn vtable(&self) -> &HostApiVTable {
        &self.vtable
    }

    pub fn context(&self) -> &Context {
        &self.ctx
    }

    pub fn events(&self) -> &Arc<EventBus> {
        &self.events
    }

    pub fn services(&self) -> &Arc<ServiceContainer> {
        &self.services
    }

    pub fn tools(&self) -> &Arc<ToolRegistry> {
        &self.tools
    }

    pub fn middleware(&self) -> &Arc<Pipeline> {
        &self.middleware
    }

    /// 接上 TDLib 客户端（可选）。没接时 `td_invoke*` 返回无效请求号。
    pub fn attach_tdlib(&self, client: Arc<TdClient>) -> Result<(), Arc<TdClient>> {
        self.td.set(client)
    }

    /// 把一条更新广播到事件总线。
    ///
    /// **两条通道都发**：具体的 `@type`（例如 `updateNewMessage`）和兜底通道 [`UPDATE_EVENT`]。
    /// 订阅者按 `@type` 能精确订阅，按 `"update"` 能收到全部。
    /// `Routed::Update` 的接线点就是这里。
    pub fn publish_update(&self, payload: &Value) -> fuyubot_core::Result<usize> {
        let mut total = self.events.emit(UPDATE_EVENT, payload)?;
        if let Some(update_type) = payload.get("@type").and_then(Value::as_str) {
            if update_type != UPDATE_EVENT {
                total += self.events.emit(update_type, payload)?;
            }
        }
        Ok(total)
    }

    /// 插件经 VTable 打进来的日志（最多 [`LOG_CAPACITY`] 行，最新的在末尾）。
    ///
    /// 宿主自己也可以接日志系统：这里留了一份，省得为了可观测性再引一个 crate。
    pub fn log_lines(&self) -> Vec<String> {
        self.log_lines
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .collect()
    }

    /// 已经发出的请求（最多 [`REQUEST_LOG_CAPACITY`] 条，最新的在末尾）。
    ///
    /// 没接 TDLib 时请求只留档不发出——插件与整条管线在没有真实 TDLib 的情况下
    /// 也能被完整跑通（集成测试就是这么用的）。
    pub fn sent_requests(&self) -> Vec<SentRequest> {
        self.sent_requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .collect()
    }

    /// 把一条 TDLib 响应投递给发起该请求的插件（事件类型 [`RESPONSE_EVENT`]）。
    ///
    /// 接收循环拿到 `Routed::Response` 时调它。找不到请求归属时按全局投递（`scope = 0`）。
    pub fn deliver_response(&self, payload: &Value) -> fuyubot_core::Result<usize> {
        let request_id = request_id_of(payload);
        let owner = match request_id {
            Some(id) => self
                .request_owners
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&id)
                .unwrap_or(0),
            None => 0,
        };

        // 原始 JSON 一个字段都不剥；只额外挂一个数值型的 request_id，方便插件过滤。
        let mut forwarded = payload.clone();
        if let (Some(id), Some(object)) = (request_id, forwarded.as_object_mut()) {
            object.insert("request_id".to_owned(), json!(id));
        }
        self.events.emit_scoped(owner, RESPONSE_EVENT, &forwarded)
    }

    /// 当前归属：0 = 宿主自己，其它 = 正在跑的那个插件。
    pub fn current_owner(&self) -> u64 {
        self.current_owner.load(Ordering::Relaxed)
    }

    /// 在作用域内把归属切成 `owner`；插件里再发起请求、再注册订阅都会算在它名下。
    fn enter_owner(&self, owner: u64) -> OwnerGuard<'_> {
        let previous = self.current_owner.swap(owner, Ordering::Relaxed);
        OwnerGuard {
            runtime: self,
            previous,
        }
    }

    fn record_request(&self, id: u64, owner: u64, method: String, params: &Value) {
        self.request_owners
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, owner);

        let mut log = self.sent_requests.lock().unwrap_or_else(|e| e.into_inner());
        if log.len() >= REQUEST_LOG_CAPACITY {
            log.pop_front();
        }
        log.push_back(SentRequest {
            id,
            method,
            params: params.clone(),
        });
    }

    // ── 插件作用域 ─────────────────────────────────────────────

    fn begin_plugin(&self) -> u64 {
        let id = self.next_owner.fetch_add(1, Ordering::Relaxed);
        self.current_owner.store(id, Ordering::Relaxed);
        id
    }

    fn end_plugin(&self) {
        self.current_owner.store(0, Ordering::Relaxed);
    }

    /// 把某个插件注册的一切都撤掉（注册顺序的倒序由各自的 Drop 保证）。
    pub(crate) fn rollback_owner(&self, owner: u64) {
        let removed = {
            let mut registrations = self.registrations();
            let mut removed = Vec::new();
            let mut index = 0;
            while index < registrations.len() {
                if registrations[index].owner == owner {
                    removed.push(registrations.swap_remove(index));
                } else {
                    index += 1;
                }
            }
            removed
        };
        // 在锁外回滚：句柄的 Drop 会回头去动事件总线 / 工具表。
        drop(removed);
    }

    fn remember(&self, kind: RegistrationKind) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let owner = self.current_owner.load(Ordering::Relaxed);
        self.registrations().push(Registration { id, owner, kind });
        id
    }

    fn forget(&self, id: u64) {
        let removed = {
            let mut registrations = self.registrations();
            let position = registrations.iter().position(|r| r.id == id);
            position.map(|position| registrations.swap_remove(position))
        };
        drop(removed);
    }

    fn warn(&self, message: &str) {
        eprintln!("[fuyubot-host] {message}");
    }

    fn registrations(&self) -> MutexGuard<'_, Vec<Registration>> {
        self.registrations.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl std::fmt::Debug for HostRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostRuntime")
            .field("abi_version", &self.vtable.version)
            .field("registered", &self.registrations().len())
            .finish_non_exhaustive()
    }
}

// SAFETY: `vtable.host_ctx` 是指向本实例的裸指针，构造之后不再改动；
// 其余字段要么是 `Arc` / `OnceLock`，要么在 `Mutex` 后面，本身都是 `Send + Sync`。
// 插件必须能从任意宿主线程调用这些桥接函数，所以这个断言是必要的。
unsafe impl Send for HostRuntime {}
unsafe impl Sync for HostRuntime {}

// ── 边界小工具 ─────────────────────────────────────────────────────

/// # Safety
/// `host` 必须来自 [`HostRuntime::vtable_ptr`]，且运行时仍然活着。
unsafe fn runtime<'a>(host: *mut c_void) -> Option<&'a HostRuntime> {
    if host.is_null() {
        None
    } else {
        // SAFETY: 由调用方保证。
        Some(unsafe { &*(host as *const HostRuntime) })
    }
}

/// # Safety
/// `ptr` 必须是宿主给的 NUL 结尾合法 UTF-8（或 NULL）。
unsafe fn cstr<'a>(ptr: *const c_char) -> Option<&'a str> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: 由调用方保证。
    unsafe { CStr::from_ptr(ptr) }.to_str().ok()
}

thread_local! {
    /// 宿主返回给插件的字符串：**宿主持有**，下一次宿主调用前有效。
    /// 与插件侧 `plugin_free_string` 的分工：谁分配谁释放，宿主不碰插件的指针。
    static HOST_STRING: std::cell::RefCell<CString> = std::cell::RefCell::new(CString::default());
}

fn host_string(text: &str) -> *const c_char {
    HOST_STRING.with(|slot| {
        *slot.borrow_mut() = CString::new(text).unwrap_or_default();
        slot.borrow().as_ptr()
    })
}

fn parse_json(raw: *const c_char) -> Option<Value> {
    // SAFETY: 调用方按 ABI 契约保证指针合法。
    let text = unsafe { cstr(raw) }?;
    if text.trim().is_empty() {
        return Some(Value::Null);
    }
    serde_json::from_str(text).ok()
}

/// 从响应里取请求号。
///
/// TDLib 会把 `@extra` 原样回显（是**字符串**），我们自己也接受数值型的 `request_id`——
/// 后者是 [`HostRuntime::deliver_response`] 补上去的，方便插件用过滤器直接匹配。
fn request_id_of(payload: &Value) -> Option<u64> {
    payload
        .get("request_id")
        .and_then(Value::as_u64)
        .or_else(|| {
            payload
                .get("@extra")
                .and_then(Value::as_str)
                .and_then(|extra| extra.parse().ok())
        })
}

/// 过滤器分成两部分：`event_type` 键用来选**通道**（不参与负载匹配），其余键做负载匹配。
///
/// 返回 `(通道名, 负载过滤器, 客户端需要匹配的负载键)`——负载过滤器里已经把 `event_type` 摘掉了，
/// 但 `wait_for_update` 还要拿原始 payload 再确认一次通道键（见 [`api_wait_for_update`]）。
fn parse_filter(raw: *const c_char) -> Result<Option<Filter>, String> {
    Ok(filter_parts(raw)?.1)
}

/// 拆过滤器：`(通道, 负载过滤器)`。通道默认 [`UPDATE_EVENT`]。
fn filter_parts(raw: *const c_char) -> Result<(String, Option<Filter>), String> {
    // SAFETY: 调用方按 ABI 契约保证指针合法。
    let Some(text) = (unsafe { cstr(raw) }) else {
        return Ok((UPDATE_EVENT.to_owned(), None));
    };
    if text.trim().is_empty() || text.trim() == "null" {
        return Ok((UPDATE_EVENT.to_owned(), None));
    }
    let value: Value =
        serde_json::from_str(text).map_err(|e| format!("过滤条件不是合法 JSON: {e}"))?;
    let object = value
        .as_object()
        .ok_or_else(|| "过滤条件必须是 JSON 对象".to_owned())?;

    let channel = object
        .get(FILTER_EVENT_TYPE_KEY)
        .and_then(Value::as_str)
        .unwrap_or(UPDATE_EVENT)
        .to_owned();

    let mut filter = Filter::new();
    for (path, expected) in object {
        if path == FILTER_EVENT_TYPE_KEY {
            continue;
        }
        filter = filter.field(path, expected.clone());
    }
    Ok((channel, Some(filter)))
}

fn map_stage(stage: u32) -> Option<MwStage> {
    Some(match fuyubot_abi::Stage::from_u32(stage)? {
        fuyubot_abi::Stage::PreUpdate => MwStage::PreUpdate,
        fuyubot_abi::Stage::PreRoute => MwStage::PreRoute,
        fuyubot_abi::Stage::InPlugin => MwStage::InPlugin,
        fuyubot_abi::Stage::PostResponse => MwStage::PostResponse,
        fuyubot_abi::Stage::PreToolCall => MwStage::PreToolCall,
        fuyubot_abi::Stage::PostToolCall => MwStage::PostToolCall,
    })
}

/// 插件的中间件回调包成 [`Middleware`]。
struct CallbackMiddleware {
    name: String,
    /// 宿主运行时指针（存成整数，结构体就自动是 `Send + Sync`）。
    host: usize,
    /// 注册它的插件。
    owner: u64,
    callback: MiddlewareCallbackFn,
    /// 插件的私有数据，同样存成整数。
    user_data: usize,
}

impl Middleware for CallbackMiddleware {
    fn name(&self) -> &str {
        &self.name
    }

    fn handle(&self, ctx: &mut MiddlewareContext, next: Next<'_>) -> fuyubot_core::Result<()> {
        let Ok(payload) = CString::new(ctx.payload.to_string()) else {
            return next.run(ctx);
        };
        let stage = ctx.stage as u32;

        // SAFETY: ABI 契约保证 host 来自本运行时。
        let Some(rt) = (unsafe { runtime(self.host as *mut c_void) }) else {
            return next.run(ctx);
        };
        let _scope = rt.enter_owner(self.owner);

        // 插件中间件 panic：接住、记日志、当作没挂这一层继续往下走。
        let code = match panic::catch_unwind(AssertUnwindSafe(|| unsafe {
            (self.callback)(self.user_data as *mut c_void, stage, payload.as_ptr())
        })) {
            Ok(code) => code,
            Err(_) => {
                rt.warn(&format!("插件中间件 {} panic，已捕获；管线继续", self.name));
                return next.run(ctx);
            }
        };

        if code == 0 {
            next.run(ctx)
        } else {
            Err(format!("插件中间件返回 {code}").into())
        }
    }
}

// ── VTable 实现 ────────────────────────────────────────────────────

unsafe extern "C" fn api_log(host: *mut c_void, level: u32, message: *const c_char) {
    // SAFETY: ABI 契约保证 message 合法。
    let Some(text) = (unsafe { cstr(message) }) else {
        return;
    };
    let level = LogLevel::from_u32(level);
    // SAFETY: ABI 契约保证 host 来自本运行时。
    if let Some(rt) = unsafe { runtime(host) } {
        let mut lines = rt.log_lines.lock().unwrap_or_else(|e| e.into_inner());
        if lines.len() >= LOG_CAPACITY {
            lines.pop_front();
        }
        lines.push_back(format!("{level:?} {text}"));
    }
    eprintln!("[plugin/{level:?}] {text}");
}

unsafe extern "C" fn api_on(
    host: *mut c_void,
    update_type: *const c_char,
    filter_json: *const c_char,
    callback: Option<EventCallbackFn>,
    user_data: *mut c_void,
) -> SubscriptionHandle {
    // SAFETY: ABI 契约保证 host 来自本运行时。
    let Some(rt) = (unsafe { runtime(host) }) else {
        return SubscriptionHandle::INVALID;
    };
    // SAFETY: 同上。
    let (Some(event_type), Some(callback)) = (unsafe { cstr(update_type) }, callback) else {
        return SubscriptionHandle::INVALID;
    };
    let filter = match parse_filter(filter_json) {
        Ok(filter) => filter,
        Err(e) => {
            rt.warn(&e);
            return SubscriptionHandle::INVALID;
        }
    };

    let user_data = user_data as usize;
    // 注册时的归属烧进闭包：插件在自己回调里再注册的东西仍然算它的。
    let owner = rt.current_owner();
    let host = host as usize;
    let handle = rt
        .events
        .on_scoped(owner, event_type.to_owned(), filter, move |payload| {
            let Ok(text) = CString::new(payload.to_string()) else {
                return Ok(());
            };
            // SAFETY: ABI 契约保证 host 来自本运行时，且它比订阅活得久。
            let Some(rt) = (unsafe { runtime(host as *mut c_void) }) else {
                return Ok(());
            };
            let _scope = rt.enter_owner(owner);

            // 插件 panic 在这里被接住：不传播、不 abort，也不打断其余订阅者。
            match panic::catch_unwind(AssertUnwindSafe(|| unsafe {
                callback(user_data as *mut c_void, text.as_ptr())
            })) {
                Ok(0) => Ok(()),
                Ok(code) => Err(format!("插件事件回调返回 {code}").into()),
                Err(_) => {
                    rt.warn("插件事件回调 panic，已捕获；管线继续");
                    Ok(())
                }
            }
        });

    SubscriptionHandle {
        id: rt.remember(RegistrationKind::Subscription(handle)),
    }
}

unsafe extern "C" fn api_off(host: *mut c_void, handle: SubscriptionHandle) {
    // SAFETY: ABI 契约保证 host 来自本运行时。
    if let Some(rt) = unsafe { runtime(host) } {
        rt.forget(handle.id);
    }
}

unsafe extern "C" fn api_emit(
    host: *mut c_void,
    event_type: *const c_char,
    payload_json: *const c_char,
) -> i32 {
    // SAFETY: ABI 契约保证 host 来自本运行时。
    let Some(rt) = (unsafe { runtime(host) }) else {
        return -1;
    };
    // SAFETY: 同上。
    let Some(event_type) = (unsafe { cstr(event_type) }) else {
        return -1;
    };
    let payload = parse_json(payload_json).unwrap_or(Value::Null);
    // 插件自己发的事件只发给它自己 + 全局订阅者。
    match rt
        .events
        .emit_scoped(rt.current_owner(), event_type, &payload)
    {
        Ok(_) => 0,
        Err(e) => {
            rt.warn(&e.to_string());
            -1
        }
    }
}

unsafe extern "C" fn api_register_tool(
    host: *mut c_void,
    descriptor_json: *const c_char,
) -> ToolHandle {
    // SAFETY: ABI 契约保证 host 来自本运行时。
    let Some(rt) = (unsafe { runtime(host) }) else {
        return ToolHandle::INVALID;
    };
    // SAFETY: 同上。
    let Some(text) = (unsafe { cstr(descriptor_json) }) else {
        return ToolHandle::INVALID;
    };
    let descriptor = match descriptor_from_json(text) {
        Ok(descriptor) => descriptor,
        Err(e) => {
            rt.warn(&format!("工具描述不合法: {e}"));
            return ToolHandle::INVALID;
        }
    };
    match rt.tools.register_tool(descriptor) {
        Ok(handle) => ToolHandle {
            id: rt.remember(RegistrationKind::Tool(handle)),
        },
        Err(e) => {
            rt.warn(&format!("注册工具失败: {e}"));
            ToolHandle::INVALID
        }
    }
}

unsafe extern "C" fn api_call_tool(
    host: *mut c_void,
    name: *const c_char,
    input_json: *const c_char,
) -> RequestId {
    // SAFETY: ABI 契约保证 host 来自本运行时。
    let Some(rt) = (unsafe { runtime(host) }) else {
        return RequestId::INVALID;
    };
    // SAFETY: 同上。
    let Some(name) = (unsafe { cstr(name) }) else {
        return RequestId::INVALID;
    };
    let input = parse_json(input_json).unwrap_or(Value::Null);
    match rt.tools.call_tool(name, &input) {
        Ok(call) => RequestId {
            id: call.request_id.as_u64(),
        },
        Err(e) => {
            rt.warn(&format!("调用工具 {name} 失败: {e}"));
            RequestId::INVALID
        }
    }
}

unsafe extern "C" fn api_list_tools(host: *mut c_void) -> *const c_char {
    // SAFETY: ABI 契约保证 host 来自本运行时。
    let Some(rt) = (unsafe { runtime(host) }) else {
        return core::ptr::null();
    };
    host_string(&rt.tools.list_tools_json().to_string())
}

unsafe extern "C" fn api_provide_service(
    host: *mut c_void,
    name: *const c_char,
    vtable: *const ServiceVTable,
) -> ServiceHandle {
    // SAFETY: ABI 契约保证 host 来自本运行时。
    let Some(rt) = (unsafe { runtime(host) }) else {
        return ServiceHandle::INVALID;
    };
    if vtable.is_null() {
        rt.warn("provide_service 收到了空 vtable");
        return ServiceHandle::INVALID;
    }
    // SAFETY: 同上。
    let Some(name) = (unsafe { cstr(name) }) else {
        return ServiceHandle::INVALID;
    };
    match rt.services.provide(name, AbiService(vtable)) {
        Ok(handle) => ServiceHandle {
            id: rt.remember(RegistrationKind::Service(handle)),
        },
        Err(e) => {
            rt.warn(&format!("注册服务 {name} 失败: {e}"));
            ServiceHandle::INVALID
        }
    }
}

unsafe extern "C" fn api_get_service(
    host: *mut c_void,
    name: *const c_char,
) -> *const ServiceVTable {
    // SAFETY: ABI 契约保证 host 来自本运行时。
    let Some(rt) = (unsafe { runtime(host) }) else {
        return core::ptr::null();
    };
    // SAFETY: 同上。
    let Some(name) = (unsafe { cstr(name) }) else {
        return core::ptr::null();
    };
    match rt.services.get::<AbiService>(name) {
        Some(service) => service.0,
        None => core::ptr::null(),
    }
}

unsafe extern "C" fn api_td_invoke(host: *mut c_void, method_json: *const c_char) -> RequestId {
    // SAFETY: ABI 契约保证 host 来自本运行时。
    let Some(rt) = (unsafe { runtime(host) }) else {
        return RequestId::INVALID;
    };
    let Some(request) = parse_json(method_json) else {
        rt.warn("td_invoke 收到了非法 JSON");
        return RequestId::INVALID;
    };
    rt.td_send(request)
}

unsafe extern "C" fn api_td_invoke_simplified(
    host: *mut c_void,
    method_name: *const c_char,
    params_json: *const c_char,
) -> RequestId {
    // SAFETY: ABI 契约保证 host 来自本运行时。
    let Some(rt) = (unsafe { runtime(host) }) else {
        return RequestId::INVALID;
    };
    // SAFETY: 同上。
    let Some(method) = (unsafe { cstr(method_name) }) else {
        return RequestId::INVALID;
    };
    let mut params = parse_json(params_json).unwrap_or(Value::Null);
    if let Value::Object(map) = &mut params {
        map.insert("@type".to_owned(), Value::String(method.to_owned()));
    } else {
        params = serde_json::json!({ "@type": method });
    }
    rt.td_send(params)
}

/// 同步执行（`td_json_client_execute`）：当场返回响应 JSON，不走接收循环。
///
/// 只在**不碰服务器**的方法上成立（`parseTextEntities` 之类）。宿主没接 TDLib 时返回 NULL，
/// 调用方退回默认值即可——这也是离线测试下这条桥的行为。
unsafe extern "C" fn api_td_execute(
    host: *mut c_void,
    request_json: *const c_char,
) -> *const c_char {
    // SAFETY: ABI 契约保证 host 来自本运行时。
    let Some(rt) = (unsafe { runtime(host) }) else {
        return core::ptr::null();
    };
    let request = parse_json(request_json).unwrap_or(Value::Null);
    match rt.td_execute(request) {
        Some(response) => host_string(&response.to_string()),
        None => core::ptr::null(),
    }
}

unsafe extern "C" fn api_use_middleware(
    host: *mut c_void,
    stage: u32,
    callback: Option<MiddlewareCallbackFn>,
    user_data: *mut c_void,
) -> MiddlewareHandle {
    // SAFETY: ABI 契约保证 host 来自本运行时。
    let Some(rt) = (unsafe { runtime(host) }) else {
        return MiddlewareHandle::INVALID;
    };
    let (Some(callback), Some(stage)) = (callback, map_stage(stage)) else {
        rt.warn("use_middleware 收到了空回调或非法阶段号");
        return MiddlewareHandle::INVALID;
    };
    let middleware = CallbackMiddleware {
        name: format!("plugin:{}", stage.name()),
        host: host as usize,
        owner: rt.current_owner(),
        callback,
        user_data: user_data as usize,
    };
    let handle = rt.middleware.use_middleware(stage, middleware);
    MiddlewareHandle {
        id: rt.remember(RegistrationKind::Middleware(handle)),
    }
}

unsafe extern "C" fn api_wait_for_update(
    host: *mut c_void,
    filter_json: *const c_char,
    timeout_ms: u64,
) -> WaitHandle {
    // SAFETY: ABI 契约保证 host 来自本运行时。
    let Some(rt) = (unsafe { runtime(host) }) else {
        return WaitHandle::INVALID;
    };
    // 过滤器里的 `event_type` 键选通道：等更新用默认的 "update"，等响应用 "response"。
    let (channel, filter) = match filter_parts(filter_json) {
        Ok(parts) => parts,
        Err(e) => {
            rt.warn(&e);
            return WaitHandle::INVALID;
        }
    };

    let waiter = Arc::new(Waiter::default());
    let id = rt.next_waiter.fetch_add(1, Ordering::Relaxed);
    rt.waiters
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(id, Arc::clone(&waiter));

    let subscription = rt.events.on(channel, filter, {
        let waiter = Arc::clone(&waiter);
        move |payload| {
            let mut slot = waiter.result.lock().unwrap_or_else(|e| e.into_inner());
            if slot.is_none() {
                *slot = Some(payload.clone());
            }
            drop(slot);
            waiter.ready.notify_all();
            Ok(())
        }
    });

    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let matched = {
        let mut slot = waiter.result.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if slot.is_some() {
                break true;
            }
            if waiter.cancelled.load(Ordering::SeqCst) {
                break false;
            }
            let now = Instant::now();
            if timeout_ms == 0 || now >= deadline {
                break false;
            }
            let (guard, _) = waiter
                .ready
                .wait_timeout(slot, deadline - now)
                .unwrap_or_else(|e| e.into_inner());
            slot = guard;
        }
    };

    drop(subscription);
    rt.waiters
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&id);

    let payload = waiter
        .result
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    WaitHandle {
        id,
        matched: u32::from(matched),
        _reserved: 0,
        payload_json: payload
            .as_ref()
            .map_or(core::ptr::null(), |value| host_string(&value.to_string())),
    }
}

unsafe extern "C" fn api_cancel_wait(host: *mut c_void, handle: WaitHandle) {
    // SAFETY: ABI 契约保证 host 来自本运行时。
    let Some(rt) = (unsafe { runtime(host) }) else {
        return;
    };
    let waiter = rt
        .waiters
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&handle.id)
        .cloned();
    if let Some(waiter) = waiter {
        waiter.cancelled.store(true, Ordering::SeqCst);
        waiter.ready.notify_all();
    }
}

impl HostRuntime {
    fn td_send(&self, request: Value) -> RequestId {
        let method = request
            .get("@type")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_owned();
        let owner = self.current_owner();

        let id = match self.td.get() {
            Some(client) => match client.send(request.clone()) {
                Ok(id) => id.as_u64(),
                Err(e) => {
                    self.warn(&format!("td_invoke({method}) 失败: {e}"));
                    return RequestId::INVALID;
                }
            },
            None => {
                // 没接 TDLib：请求只留档不发出，但仍然发一个合成请求号，
                // 这样插件与管线在没有真实 TDLib 时也能被完整跑通。
                let id = self.next_synthetic_request.fetch_add(1, Ordering::Relaxed);
                self.warn(&format!(
                    "宿主没接 TDLib，{method} 只记录了没发出（合成 id={id}）"
                ));
                id
            }
        };

        self.record_request(id, owner, method, &request);
        RequestId { id }
    }

    /// **同步**执行一个不碰网络的 TDLib 方法（`TdClient::execute`）。
    ///
    /// 和 [`td_send`](Self::td_send) 刻意的区别：这里**不**留档、不分配请求号、不等响应
    /// ——它当场就有结果，所以插件在事件回调里调它不会把投递线程卡住。
    /// 没接 TDLib（离线测试）时返回 `None`，调用方自己退回默认值。
    fn td_execute(&self, request: Value) -> Option<Value> {
        let client = self.td.get()?;
        match client.execute(request) {
            Ok(response) => Some(response),
            Err(e) => {
                self.warn(&format!("td_execute 失败: {e}"));
                None
            }
        }
    }

    /// 给 [`crate::PluginLoader`] 用：一次插件加载的作用域。
    pub(crate) fn plugin_scope(&self) -> PluginScope<'_> {
        PluginScope {
            runtime: self,
            owner: self.begin_plugin(),
            finished: false,
        }
    }
}

/// 一次插件加载的作用域：正常结束时保留注册，失败或提前返回时整批回滚。
pub(crate) struct PluginScope<'a> {
    runtime: &'a HostRuntime,
    owner: u64,
    finished: bool,
}

impl PluginScope<'_> {
    pub(crate) fn owner(&self) -> u64 {
        self.owner
    }

    /// 保留这次加载产生的注册。
    pub(crate) fn commit(mut self) {
        self.finished = true;
        self.runtime.end_plugin();
    }
}

impl Drop for PluginScope<'_> {
    fn drop(&mut self) {
        if !self.finished {
            self.runtime.rollback_owner(self.owner);
        }
        self.runtime.end_plugin();
    }
}

/// 把 ABI 的 JSON 描述转成 [`ToolDescriptor`]。
///
/// 处理器一律是 `None`：本版 ABI 的 `register_tool` 不带函数指针，
/// 插件只能登记「我是个什么工具」，没法让宿主回调进来执行它。
fn descriptor_from_json(text: &str) -> Result<ToolDescriptor, String> {
    let value: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    ToolDescriptor::from_json(&value).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// 计数回调把 `user_data` 当 `AtomicUsize` 用。
    ///
    /// **不能共用一个 `static` 计数器**：测试是并行跑的，共用 static 会让断言互相干扰。
    /// 类型必须是 `C-unwind`——宿主靠在边界外 `catch_unwind` 接住插件的 panic。
    unsafe extern "C-unwind" fn count_event(user_data: *mut c_void, payload: *const c_char) -> i32 {
        // SAFETY: 宿主保证 payload 是临时 NUL 结尾字符串；user_data 由调用方给。
        assert!(!payload.is_null());
        let counter = unsafe { &*(user_data as *const AtomicUsize) };
        counter.fetch_add(1, Ordering::SeqCst);
        0
    }

    /// 一被调到就 panic 的回调，用来验证宿主把它接住了。
    unsafe extern "C-unwind" fn panicking_event(
        _user: *mut c_void,
        _payload: *const c_char,
    ) -> i32 {
        panic!("插件回调故意 panic");
    }

    /// 把一个计数器借成回调的 `user_data`。调用方要保证它活得比订阅久。
    fn as_user_data(counter: &AtomicUsize) -> *mut c_void {
        counter as *const AtomicUsize as *mut c_void
    }

    fn runtime() -> Arc<HostRuntime> {
        HostRuntime::new(Context::new()).expect("运行时应该建得起来")
    }

    /// VTable 必须每个字段都填满，且版本/尺寸自洽。
    #[test]
    fn vtable_is_fully_populated() {
        let rt = runtime();
        let vtable = rt.vtable();
        assert_eq!(vtable.version, ABI_VERSION);
        assert_eq!(vtable.size as usize, core::mem::size_of::<HostApiVTable>());
        assert!(!vtable.host_ctx.is_null());
        assert_eq!(vtable.host_ctx as usize, Arc::as_ptr(&rt) as usize);
        assert!(vtable.on.is_some());
        assert!(vtable.off.is_some());
        assert!(vtable.emit.is_some());
        assert!(vtable.register_tool.is_some());
        assert!(vtable.call_tool.is_some());
        assert!(vtable.list_tools.is_some());
        assert!(vtable.provide_service.is_some());
        assert!(vtable.get_service.is_some());
        assert!(vtable.td_invoke.is_some());
        assert!(vtable.td_invoke_simplified.is_some());
        assert!(vtable.td_execute.is_some());
        assert!(vtable.use_middleware.is_some());
        assert!(vtable.wait_for_update.is_some());
        assert!(vtable.cancel_wait.is_some());
        assert!(vtable.log.is_some());
    }

    /// 用插件视角的 `HostApi` 走一遍全部能力，证明桥接真的通。
    #[test]
    fn host_api_round_trips_through_the_bridge() {
        let rt = runtime();
        // SAFETY: vtable 活着。
        let api = unsafe { fuyubot_abi::HostApi::from_raw(rt.vtable_ptr()) }.unwrap();
        assert_eq!(api.version(), ABI_VERSION);

        api.log(LogLevel::Info, "hi from the test");
        assert!(
            rt.log_lines()
                .iter()
                .any(|line| line.contains("hi from the test")),
            "log 必须经 VTable 落到宿主上：{:?}",
            rt.log_lines()
        );

        // 事件：订阅 → 广播 → 退订
        let hits = AtomicUsize::new(0);
        let subscription = api.on("notice", None, count_event, as_user_data(&hits));
        assert!(subscription.is_valid());
        api.emit("notice", r#"{"text":"x"}"#).unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        api.off(subscription);
        api.emit("notice", r#"{"text":"x"}"#).unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 1, "退订后不该再收到");

        // 中间件：注册 → 跑一个阶段
        static MW_HITS: AtomicUsize = AtomicUsize::new(0);
        unsafe extern "C-unwind" fn mw_callback(
            _u: *mut c_void,
            stage: u32,
            _p: *const c_char,
        ) -> i32 {
            assert_eq!(stage, fuyubot_abi::Stage::PreUpdate as u32);
            MW_HITS.fetch_add(1, Ordering::SeqCst);
            0
        }
        let middleware = api.use_middleware(
            fuyubot_abi::Stage::PreUpdate,
            mw_callback,
            core::ptr::null_mut(),
        );
        assert!(middleware.is_valid());
        let mut ctx = MiddlewareContext::new(MwStage::PreUpdate, Value::Null);
        rt.middleware().run(MwStage::PreUpdate, &mut ctx).unwrap();
        assert_eq!(MW_HITS.load(Ordering::SeqCst), 1);

        // 工具：注册声明 → 列表 → 调用（没有处理器，应报 NoHandler 并返回无效请求号）
        let tool = api.register_tool(
            r#"{"name":"demo.echo","description":"回显","input_schema":{"type":"object"}}"#,
        );
        assert!(tool.is_valid());
        let listed = api.list_tools();
        assert!(listed.contains("demo.echo"), "{listed}");
        assert_eq!(api.call_tool("demo.echo", "{}").unwrap().is_valid(), false);

        // 服务：提供一个 vtable → 取回同一个指针
        let service_name = CString::new("demo.service").unwrap();
        let vtable = ServiceVTable {
            name: service_name.as_ptr(),
            version: 1,
            size: core::mem::size_of::<ServiceVTable>() as u32,
            context: core::ptr::null_mut(),
            methods: core::ptr::null(),
            method_count: 0,
        };
        // SAFETY: vtable 比注册活得久（在这个测试作用域里）。
        let service = unsafe { api.provide_service("demo.service", &vtable) };
        assert!(service.is_valid());
        assert_eq!(api.get_service("demo.service"), &vtable as *const _);
        assert!(api.get_service("nope").is_null());

        // 没接 TDLib：请求只留档、拿一个合成请求号，插件与管线照旧能跑通
        let first = api.td_invoke(r#"{"@type":"getMe"}"#).unwrap();
        assert!(first.is_valid(), "没接 TDLib 时也该给合成请求号");
        let second = api
            .td_invoke_simplified("getChatHistory", r#"{"chat_id":1}"#)
            .unwrap();
        assert!(second.is_valid());

        let sent = rt.sent_requests();
        assert_eq!(sent.len(), 2, "{sent:?}");
        assert_eq!(sent[0].method, "getMe");
        assert_eq!(sent[1].method, "getChatHistory");
        assert_eq!(sent[1].params["@type"], "getChatHistory");
        assert_eq!(sent[1].params["chat_id"], 1);

        // 同步 `td_execute`：没接 TDLib 时宿主返回 NULL（调用方退回默认值），
        // 而且它**不**进请求留档——它不是「发出去的请求」。
        assert!(
            api.td_execute(r#"{"@type":"parseTextEntities"}"#).is_none(),
            "没接 TDLib 就该返回 None"
        );
        assert_eq!(rt.sent_requests().len(), 2, "td_execute 不该留档");
    }

    #[test]
    fn wait_for_update_wakes_up_on_a_matching_update() {
        let rt = runtime();
        let publisher = Arc::clone(&rt);
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            publisher
                .publish_update(&serde_json::json!({ "kind": "message", "id": 7 }))
                .unwrap();
        });

        let waiter = rt.plugin_scope();
        // SAFETY: rt 活着。
        let api = unsafe { fuyubot_abi::HostApi::from_raw(rt.vtable_ptr()) }.unwrap();
        let result = api.wait_for_update(r#"{"kind":"message"}"#, 2_000);
        assert!(result.is_valid());
        assert!(result.did_match(), "应该等到匹配的更新");
        // SAFETY: 宿主保证 payload 在本线程下一次宿主调用前有效。
        let payload = unsafe { CStr::from_ptr(result.payload_json) }.to_string_lossy();
        assert!(payload.contains("\"id\":7"), "{payload}");
        waiter.commit();
        handle.join().unwrap();
    }

    #[test]
    fn wait_for_update_times_out() {
        let rt = runtime();
        // SAFETY: rt 活着。
        let api = unsafe { fuyubot_abi::HostApi::from_raw(rt.vtable_ptr()) }.unwrap();
        let started = Instant::now();
        let result = api.wait_for_update("{}", 60);
        assert!(result.is_valid());
        assert!(!result.did_match(), "没人发更新就该超时");
        assert!(result.payload_json.is_null());
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "不该等到超时之外"
        );
    }

    #[test]
    fn cancel_wait_wakes_the_waiter_up() {
        let rt = runtime();
        let waiting = Arc::clone(&rt);
        let started = Instant::now();

        let handle = std::thread::spawn(move || {
            // SAFETY: rt 在这个线程 join 之前一直活着。
            let api = unsafe { fuyubot_abi::HostApi::from_raw(waiting.vtable_ptr()) }.unwrap();
            api.wait_for_update("{}", 30_000).did_match()
        });

        // 等它把等待位挂上，然后取消。这里直接翻内部表取 id——测试就在同模块里。
        let id = loop {
            if let Some(id) = rt.waiters.lock().unwrap().keys().copied().max() {
                break id;
            }
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "等待位一直没建起来"
            );
            std::thread::sleep(Duration::from_millis(5));
        };
        // SAFETY: 句柄由本运行时发放。
        unsafe {
            (rt.vtable().cancel_wait.unwrap())(
                rt.vtable().host_ctx,
                WaitHandle {
                    id,
                    matched: 0,
                    _reserved: 0,
                    payload_json: core::ptr::null(),
                },
            )
        };

        assert!(!handle.join().unwrap(), "被取消的等待不该报成匹配");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "取消应该立刻唤醒，而不是等满 30 秒"
        );
    }

    #[test]
    fn plugin_registrations_roll_back_together() {
        let rt = runtime();
        // SAFETY: rt 活着。
        let api = unsafe { fuyubot_abi::HostApi::from_raw(rt.vtable_ptr()) }.unwrap();
        let hits = AtomicUsize::new(0);

        let scope = rt.plugin_scope();
        let subscription = api.on("notice", None, count_event, as_user_data(&hits));
        assert!(subscription.is_valid());
        api.emit("notice", "{}").unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        // 不 commit —— 作用域结束就是「这次加载失败」，注册要整批撤掉。
        drop(scope);
        api.emit("notice", "{}").unwrap();
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "回滚后插件注册的订阅必须消失"
        );
    }

    #[test]
    fn host_side_registrations_survive_a_plugin_rollback() {
        let rt = runtime();
        // SAFETY: rt 活着。
        let api = unsafe { fuyubot_abi::HostApi::from_raw(rt.vtable_ptr()) }.unwrap();
        let hits = AtomicUsize::new(0);

        // owner = 0：宿主自己注册的
        let host_side = api.on("notice", None, count_event, as_user_data(&hits));
        let scope = rt.plugin_scope();
        let plugin_side = api.on("notice", None, count_event, as_user_data(&hits));
        drop(scope);

        api.emit("notice", "{}").unwrap();
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "只有宿主自己的那份订阅该活下来"
        );
        api.off(host_side);
        api.off(plugin_side);
        api.emit("notice", "{}").unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn stage_numbers_match_the_middleware_crate() {
        for stage in fuyubot_abi::Stage::ALL {
            let mapped = map_stage(stage as u32).expect("每个 ABI 阶段都要能映射");
            assert_eq!(
                mapped as u32, stage as u32,
                "ABI 的 {:?} 与 middleware 的 {:?} 数值不一致",
                stage, mapped
            );
        }
        assert!(map_stage(99).is_none());
    }

    #[test]
    fn bad_filters_and_bad_json_do_not_panic() {
        let rt = runtime();

        // 过滤条件不是对象
        let bad = CString::new("[1,2,3]").unwrap();
        let hits = AtomicUsize::new(0);
        // SAFETY: 上面构造的字符串是合法 NUL 结尾。
        let handle = unsafe {
            (rt.vtable().on.unwrap())(
                rt.vtable().host_ctx,
                CString::new("x").unwrap().as_ptr(),
                bad.as_ptr(),
                Some(count_event),
                as_user_data(&hits),
            )
        };
        assert!(!handle.is_valid());
        assert!(matches!(parse_filter(bad.as_ptr()), Err(_)));
    }

    #[test]
    fn panicking_callbacks_are_caught_and_do_not_break_the_broadcast() {
        let rt = runtime();
        // SAFETY: rt 活着。
        let api = unsafe { fuyubot_abi::HostApi::from_raw(rt.vtable_ptr()) }.unwrap();
        let hits = AtomicUsize::new(0);

        // 先挂一个会 panic 的，再挂一个正常的：后者必须照样被调到。
        let _bad = api.on("notice", None, panicking_event, core::ptr::null_mut());
        let _good = api.on("notice", None, count_event, as_user_data(&hits));

        // 把 panic 输出吞掉，别污染测试输出。
        let previous = panic::take_hook();
        panic::set_hook(Box::new(|_| {}));
        let emitted = api.emit("notice", "{}");
        panic::set_hook(previous);

        assert!(emitted.is_ok(), "插件 panic 不该让 emit 报错：{emitted:?}");
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "panic 的插件不该挡住后面的订阅者"
        );
    }

    #[test]
    fn responses_go_only_to_the_requesting_plugin() {
        let rt = runtime();
        // SAFETY: rt 活着。
        let api = unsafe { fuyubot_abi::HostApi::from_raw(rt.vtable_ptr()) }.unwrap();
        let mine = AtomicUsize::new(0);
        let theirs = AtomicUsize::new(0);

        // 宿主（scope = 0）自己订一份
        let host_side = api.on(RESPONSE_EVENT, None, count_event, as_user_data(&theirs));

        // 插件 A 发起请求并订阅响应（ABI 这层的过滤器是 JSON 字符串）
        let scope = rt.plugin_scope();
        let request = api.td_invoke(r#"{"@type":"getMe"}"#).unwrap();
        assert!(request.is_valid());
        let filter = format!(r#"{{"request_id":{}}}"#, request.id);
        let plugin_side = api.on(
            RESPONSE_EVENT,
            Some(&filter),
            count_event,
            as_user_data(&mine),
        );
        scope.commit();

        // 模拟 td_receive 把响应交回来
        let response = json!({ "@type": "user", "@extra": request.id.to_string(), "id": 7 });
        assert_eq!(
            rt.deliver_response(&response).unwrap(),
            2,
            "插件 + 宿主各一份"
        );

        assert_eq!(mine.load(Ordering::SeqCst), 1, "发起请求的插件必须收到");
        assert_eq!(
            theirs.load(Ordering::SeqCst),
            1,
            "全局订阅者也收得到（透明，不拦截）"
        );

        api.off(host_side);
        api.off(plugin_side);
        // 第二次投递：请求归属已经消费掉了，只剩全局订阅者
        assert_eq!(rt.deliver_response(&response).unwrap(), 0);
        assert_eq!(mine.load(Ordering::SeqCst), 1);
    }

    /// 宿主自己也能直接往总线上推更新，plugins 的 `wait_for_update` 靠的就是它。
    #[test]
    fn publish_update_reaches_subscribers() {
        let rt = runtime();
        let hits = AtomicUsize::new(0);
        // SAFETY: rt 活着。
        let api = unsafe { fuyubot_abi::HostApi::from_raw(rt.vtable_ptr()) }.unwrap();
        let subscription = api.on(UPDATE_EVENT, None, count_event, as_user_data(&hits));
        assert_eq!(
            rt.publish_update(&serde_json::json!({ "a": 1 })).unwrap(),
            1
        );
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        api.off(subscription);
    }
}
