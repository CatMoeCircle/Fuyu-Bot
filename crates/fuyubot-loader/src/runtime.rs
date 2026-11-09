//! 宿主运行时：把 ABI VTable 的每个函数桥到阶段 2/3 的机制上。
//!
//! 这里就是「宿主层做桥」那句话的落点：四个机制 crate 彼此不依赖，
//! 把它们组合起来的地方只能有一个，那就是这里。

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::{CStr, CString, c_char, c_void};
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use fuyubot_abi::{
    ABI_VERSION, EventCallbackFn, FILTER_EVENT_TYPE_KEY, HostApi, HostApiVTable,
    MiddlewareCallbackFn, MiddlewareHandle, RequestId, SIMPLIFY_OP_KEY, ServiceHandle,
    ServiceMethod, ServiceVTable, SubscriptionHandle, ToolHandle, WaitHandle,
};
use fuyubot_core::{Context, Effect, PluginManager, PluginReporter, Report};
use fuyubot_event::{EventBus, Filter, SubscriptionHandle as EventSubscription};
use fuyubot_middleware::{
    Middleware, MiddlewareContext, MiddlewareHandle as MwRegistration, Next, Pipeline,
    Stage as MwStage,
};
use fuyubot_service::{ServiceContextExt as _, ServiceHandle as SvcRegistration};
use fuyubot_simplify::{Simplify, ops};
use fuyubot_tdlib::{TdClient, TdlibError, schema::TdApiSchema};
use fuyubot_tool::{ToolDescriptor, ToolHandle as ToolRegistration, ToolHandlerFn, ToolRegistry};
use serde_json::{Value, json};

/// 插件日志级别。宿主调 [`HostRuntime::set_log_verbosity`] 时用它，
/// CLI 也从这里取，省得为了一个枚举去依赖 `fuyubot-abi`。
pub use fuyubot_abi::LogLevel;
/// 响应通道，与 [`fuyubot_abi::RESPONSE_EVENT`] 同一个名字。
pub use fuyubot_abi::RESPONSE_EVENT;
/// 更新的兜底通道，与 [`fuyubot_abi::UPDATE_EVENT`] 同一个名字。
pub use fuyubot_abi::UPDATE_EVENT;

/// 插件日志最多留这么多行（环形缓冲，旧的被挤掉）。
const LOG_CAPACITY: usize = 256;

/// 插件日志的默认门槛：`Info` 及以上才记（`Debug` / `Trace` 默认丢掉）。
///
/// 与 `[tdlib] log_verbosity` 一个思路——默认安静，要排查再调大。
/// 插件那边只要把 `[plugins].log_level` 写成 `"debug"` / `"trace"` 就能开。
pub const DEFAULT_LOG_VERBOSITY: LogLevel = LogLevel::Info;

/// 宿主自己注册的工具在 `list_tools` 的 `owner` 字段里的取值。
///
/// 用 `host` 而不是某个插件名：宿主的内置工具不属于任何插件，插件名一律是点分的
/// （`fuyubot.demo`），两者不会混。
pub const HOST_TOOL_OWNER: &str = "host";

/// 内置工具名：编译期烘焙的 TDLib 函数表（`td_api.tl` 的 `---functions---` 段）。
///
/// 入参 `{"filter": "sendMessage"}`（filter 可选），输出 `{"count":…,"functions":[…]}`。
/// 宿主侧（`HostRuntime::tools().call_tool`）直接拿到输出；**ABI 侧 `call_tool` 只回请求号**
/// ——输出回投不在本版 ABI 里（见缺口 13 的边界），插件目前只能用 `list_tools` 发现它。
pub const TD_FUNCTIONS_TOOL: &str = "fuyubot.td.functions";

/// 已发出请求的留档上限。
const REQUEST_LOG_CAPACITY: usize = 256;

/// 没接 TDLib 时用的合成请求号起点：躲开 TDLib `@extra` 的低位区间。
const SYNTHETIC_REQUEST_BASE: u64 = 1 << 48;

/// `simplify_invoke` 的操作号起点：再高一档，与 TDLib / 合成请求号都不重叠。
///
/// 它**不是**发给 TDLib 的请求号，只是「这次操作的回投地址」——
/// 结果走 [`RESPONSE_EVENT`] 回到发起插件，靠的就是这个号。
const SIMPLIFY_REQUEST_BASE: u64 = 1 << 49;

/// 同时跑着的 simplify 工作线程超过这个数就打一条警告。
///
/// 目前是**一个操作一个线程**（op 内部是阻塞的：等响应、等发送结局都要卡住）。
/// 不做硬上限是因为上限满了只能拒绝，而「消息没发出去」比「线程多一点」糟得多；
/// 真要把并发收住，正确做法是改成有界的等待器池，不是在这里砍请求。
const SIMPLIFY_BUSY_WORKERS: usize = 64;

/// 请求归属表（请求号 → 发起它的插件）的容量上限。
///
/// 等不到响应的请求会让这张表只增不减：离线模式的合成请求号**永远**不会有响应回来，
/// 真 TDLib 也有不回响应的情形。被挤掉的老请求只是不再定向投递，它的响应仍走全局通道。
const REQUEST_OWNER_CAPACITY: usize = 1024;

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
    /// 属于哪个子作用域（0 = 插件根作用域）。`rollback_scope` 按它细粒度撤销。
    scope: u64,
    kind: RegistrationKind,
}

/// 一个子作用域。中间插件为每个脚本建一个，卸载单个脚本只撤销它自己的作用域。
#[allow(dead_code)] // `parent` 暂未读：留给 enter/leave 配对校验与诊断用（设计文档 5.2）。
struct ScopeInfo {
    /// 继承自创建时的 owner：owner 整批回滚时连带把它撤掉。
    owner: u64,
    /// 父作用域句柄（0 = 插件根作用域，父就是 owner 本身）。
    parent: u64,
    /// 是否还有效（drop 之后即失效，句柄不得再用）。
    alive: bool,
}

/// 请求号 → 发起它的插件 owner，按插入顺序淘汰。
///
/// 响应回来时 [`take`](Self::take) 出 owner 做定向投递（见 [`HostRuntime::deliver_response`]）。
/// 容量到 [`REQUEST_OWNER_CAPACITY`] 就挤掉最老的：宁可少一次定向，也不要无界增长。
///
/// `order` 只记插入顺序、不跟踪删除，所以成功的响应会在它里面留下幽灵项；
/// 长到两倍容量时重建一次（重建后淘汰顺序退化为任意，但有界性仍然成立）。
#[derive(Default)]
struct RequestOwners {
    map: HashMap<u64, u64>,
    order: VecDeque<u64>,
}

impl RequestOwners {
    fn insert(&mut self, id: u64, owner: u64) {
        // 同一个 id 重复登记不该发生；真发生了就只覆盖，不重复排队。
        if self.map.insert(id, owner).is_none() {
            self.order.push_back(id);
        }
        while self.map.len() > REQUEST_OWNER_CAPACITY {
            match self.order.pop_front() {
                Some(oldest) => {
                    self.map.remove(&oldest);
                }
                None => break,
            }
        }
        if self.order.len() > REQUEST_OWNER_CAPACITY * 2 {
            self.order.clear();
            self.order.extend(self.map.keys().copied());
        }
    }

    /// 取走归属：一条请求只配一次响应。
    fn take(&mut self, id: u64) -> Option<u64> {
        self.map.remove(&id)
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.map.len()
    }
}

/// 一个 `wait_for_update` 等待位。
#[derive(Default)]
struct Waiter {
    result: Mutex<Option<Value>>,
    cancelled: AtomicBool,
    ready: Condvar,
}

/// 存在服务容器里的裸 vtable 指针 + 注册它的插件。
///
/// SAFETY: vtable 是只读的，且 ABI 契约要求提供方让它活到注销为止。
/// `owner` 用来在服务调用期间压一层「执行者」——被调方在方法里写配置时
/// 应该写**自己**的段，而不是调用方的（`set_config` 的同一条规则）。
#[derive(Clone, Copy)]
struct AbiService {
    vtable: *const ServiceVTable,
    owner: u64,
}

unsafe impl Send for AbiService {}
unsafe impl Sync for AbiService {}

impl std::fmt::Debug for AbiService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AbiService")
            .field("vtable", &self.vtable)
            .field("owner", &self.owner)
            .finish()
    }
}

/// 全局配置的运行时状态：一份 JSON（完整配置树）+ 落盘位置。
///
/// 配置是**三层叠出来的**（后者只填前者的缺失键）：
/// 包默认（`plugin.json` 的 `config`）< 运行时落盘（`<root>/plugin-config/<名>.toml`）
/// < 用户配置（`fuyubot.toml` 的 `[plugin.<名>]`）。
///
/// `fuyubot.toml` **运行时永不重写**：插件写自己的配置只动 `plugin-config/` 下那一个文件，
/// 所以用户的注释、段顺序、`api_hash` 都不会被卷进来。
#[derive(Default)]
struct ConfigState {
    /// 插件配置的运行时落盘目录（`<配置文件所在目录>/plugin-config/`）。`None` = 不可写。
    store: Option<PathBuf>,
    /// 完整配置树的 JSON（三层已经合成好；`get_config` 读的就是它）。
    root: Value,
    /// 用户层快照：`fuyubot.toml` 的 `plugin` 段。
    ///
    /// `set_config` 用它判断「这个键是不是用户钉住的」——钉住的键**直接拒绝**，
    /// 而不是写完等下次启动被静默盖回去。
    user_plugin: Value,
}

/// 内置 scope service 的名字。中间插件通过 `get_service("fuyubot.host.scope")` 拿到它，
/// 用它给每个脚本建子作用域（见设计文档 5.2 / 5.3）。
static SCOPE_SERVICE_NAME: &CStr = c"fuyubot.host.scope";

/// 全局配置里，插件各自的段所在的顶层键名：`[plugin.<插件名>]` 解析成
/// `{"plugin": {"<插件名>": {…}}}`。读顶层、写自己段都从这颗子树出发。
const CONFIG_PLUGIN_KEY: &str = "plugin";

/// 插件配置的运行时落盘目录（相对配置文件所在目录）：`plugin-config/<插件名>.toml`。
///
/// 与 `fuyubot.toml` 分开的理由：用户配置只读、注释永远不丢；插件写自己的记忆不会
/// 把整棵树重写一遍，也不会把凭据（`api_hash`）卷进来。文件名是插件名的**百分号编码**
/// （见 [`encode_plugin_file_name`]），文件内容就是那一段配置本身（不带 `[plugin.x]` 头）。
const CONFIG_STORE_DIR: &str = "plugin-config";

/// 宿主运行时。
///
/// 强制定制成 `Arc`：`host_ctx` 必须指向一个地址稳定的实例，而且插件可能
/// 比加载器活得久，所以每个 [`LoadedPlugin`](crate::LoadedPlugin) 都会持有一份克隆。
pub struct HostRuntime {
    ctx: Context,
    events: Arc<EventBus>,
    tools: Arc<ToolRegistry>,
    middleware: Arc<Pipeline>,
    td: OnceLock<Arc<TdClient>>,
    /// 装四个机制拿到的守卫，必须留住，否则机制会从 Context 里消失。
    _guards: Vec<Effect>,
    /// 宿主自己注册的工具（目前只有 TDLib 函数表）。句柄 drop 即注销，所以要活到进程结束。
    _builtin_tools: Vec<ToolRegistration>,
    /// 所有跨 ABI 的注册；插件卸载时按 owner 一次性回滚，子作用域按 scope 细粒度回滚。
    registrations: Mutex<Vec<Registration>>,
    next_id: AtomicU64,
    next_owner: AtomicU64,
    /// 子作用域表：中间插件为每个脚本建一个，卸载单个脚本只撤销它自己的。
    scopes: Mutex<HashMap<u64, ScopeInfo>>,
    next_scope: AtomicU64,
    /// owner → 这个插件的 **Context 子作用域**（服务可见性）。
    ///
    /// 与上面那个 `scopes` 是两件事：`scopes` 管「哪些注册归这个插件」（回滚粒度
    /// 与 `scope` 细分），这张表管「它注册的服务给谁看、它能看见谁的服务」。
    /// 插件在 `begin_plugin` 时拿到自己的 child，`rollback_owner` 时连同注册一起丢弃。
    plugin_contexts: Mutex<HashMap<u64, Context>>,
    /// 内置「插件清单」service 的 vtable（`context` 指向本实例）。按值存，地址稳定。
    plugins_vtable: ServiceVTable,
    waiters: Mutex<HashMap<u64, Arc<Waiter>>>,
    next_waiter: AtomicU64,
    /// 插件经 `log` 打进来的日志，环形缓冲、有上限。
    log_lines: Mutex<VecDeque<String>>,
    /// 插件日志的门槛：级别数值 **小于等于** 它的才记（`Error = 1 … Trace = 5`）。
    ///
    /// 默认 [`DEFAULT_LOG_VERBOSITY`]。没有它的时候 `api_log` 对每条日志都
    /// `eprintln!`——插件刷 Debug 就是实打实的 stderr 系统调用。
    log_verbosity: AtomicU32,
    /// 插件日志的来源白名单：`None` = 全收，`Some(set)` = 只收这些插件。
    ///
    /// 由 CLI 的 `--log-plugin` 设置（`None` / 空表都表示不过滤）。只作用于
    /// **插件经 VTable 打进来的**日志：宿主与管线日志不走 [`api_log`]，所以
    /// 过滤某个插件时框架自己的告警仍然看得见。
    log_plugins: Mutex<Option<HashSet<String>>>,
    /// 原始更新 / 响应的追踪开关（`--trace-updates`）：开了之后管线把 TDLib
    /// 发来的每一条原始 JSON 打到 stderr（含消息内容，只给调试用）。
    trace_updates: AtomicBool,
    /// `list_tools` 的序列化缓存：`(工具表版本号, owner 表版本号, JSON)`。两个都没变就复用。
    ///
    /// 为什么还要 owner 版本号：`owner` 是**算出来**的（工具注册完之后 `remember` 才记下
    /// 归属），只盯工具表版本号的话，可能把「暂时按宿主算」的那一版缓存住。
    tool_list_cache: Mutex<Option<(u64, u64, Arc<CString>)>>,
    /// 请求 id → 发起它的 owner。响应回来时按它决定投递给谁（有容量上限）。
    request_owners: Mutex<RequestOwners>,
    /// 没接 TDLib 时用的合成请求号。
    next_synthetic_request: AtomicU64,
    /// `simplify_invoke` 的操作号（不是 TDLib 请求号）。
    next_simplify_request: AtomicU64,
    /// 正在跑的 simplify 工作线程数（只用于告警，见 [`SIMPLIFY_BUSY_WORKERS`]）。
    simplify_workers: AtomicU64,
    /// 已经发出的请求留档（诊断 + 测试用）。
    sent_requests: Mutex<VecDeque<SentRequest>>,
    /// 内置 scope service 的 vtable（`context` 指向本实例）。按值存，地址稳定。
    scope_vtable: ServiceVTable,
    /// 内置「我是谁」service 的 vtable（同上）。
    identity_vtable: ServiceVTable,
    /// `getMe` 的结果（登录成功后由 CLI 用 [`HostRuntime::set_self_user`] 写进来）。
    /// `fuyubot.host.identity` service 读它；没登录 / 没接 TDLib 时是 `None`。
    self_user: Mutex<Option<Value>>,
    vtable: HostApiVTable,
    /// 全局配置（路径 + JSON 树）。`get_config` / `set_config` 读写它。
    config: Mutex<ConfigState>,
    /// 插件 owner id → 插件名（`set_config` 按它判定「这是谁的段」；`list_tools` 的
    /// `owner` 字段也从这里取）。
    owner_names: Mutex<HashMap<u64, String>>,
    /// `owner_names` / 归属表的版本号：每次登记或摘掉名字、每次 `remember` 都 +1。
    owner_revision: AtomicU64,
    /// **活的**插件报告来源（装了内核的 [`PluginReporter`] 之后才有）。
    ///
    /// 为什么是「来源」而不是「快照」：`HostRuntime` 只持有 `Context`，**刻意不持有
    /// `PluginManager`**——那会让 loader 反过来攥住内核的生命周期。但把算好的
    /// 快照存下来又要求调用方记得刷新，而没人记得（运行期插件上下线之后
    /// `/plugins` 还在报旧状态，说三个在跑、其实退了两个）。
    ///
    /// [`PluginReporter`] 两边都满足：它持 `Weak<Mutex<Vec<Fiber>>>`，管理器没了
    /// 就报空表，不会把内核吊住；而每次问都现算，**结构上不存在过期**。
    plugin_report: Mutex<Option<PluginReporter>>,
}

/// 一条已经发出的请求。
#[derive(Debug, Clone)]
pub struct SentRequest {
    pub id: u64,
    pub method: String,
    pub params: Value,
}

/// [`HostRuntime::td_request`] 的错误。
///
/// 与 [`fuyubot_tdlib::TdlibError`] 分开：那条只管「发不出去」，这条还要表达
/// 「没接 TDLib」「请求形状不对」「等了但没回来」——宿主（JNI / TUI）要能分别说清楚。
#[derive(Debug, thiserror::Error)]
pub enum TdRequestError {
    /// 这次运行没有接 TDLib（`--no-tdlib` / 回放，或客户端还没建好）。
    #[error("这次运行没有接 TDLib")]
    NoTdlib,
    /// 请求不是 JSON 对象。
    #[error("请求必须是 JSON 对象（例如 {{\"@type\":\"getMe\"}}）")]
    NotObject,
    /// 请求缺 `@type`：TDLib 的每条请求都靠它选方法。
    #[error("请求缺 @type")]
    MissingType,
    /// 交给 `td_send` 时失败（含 NUL 字节、库未就绪）。
    #[error("发不出去：{0}")]
    Send(#[from] TdlibError),
    /// 预算内没等到**它自己的**响应（没有接收循环 / TDLib 没回）。
    #[error("{id} 的响应没在 {waited:?} 内回来")]
    Timeout { id: u64, waited: Duration },
}

/// [`HostRuntime::td_request`] 的一次等待：请求号 → 响应（`None` = 还在等）。
///
/// 每次请求一份，用完即弃：这样并发调用之间不共享状态，也不会因为某次超时把
/// 别的请求的响应吃掉。
#[derive(Default)]
struct TdWaiters {
    slots: Mutex<HashMap<u64, Option<Value>>>,
    ready: Condvar,
}

impl TdWaiters {
    /// 响应通道的订阅者调它。只认**本表登记过**的请求号，别人的响应原样放过。
    fn deliver(&self, payload: &Value) {
        let Some(id) = payload.get("request_id").and_then(Value::as_u64) else {
            return;
        };
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(slot) = slots.get_mut(&id) {
            if slot.is_none() {
                *slot = Some(payload.clone());
                self.ready.notify_all();
            }
        }
    }

    /// 等到 `id` 的响应或 `deadline`。返回 `None` = 超时。
    ///
    /// 调用方必须先 `insert(id, None)` 再调它：先占位、后等待，才不会丢掉「占位之前
    /// 就回来」的响应（`deliver` 只填已经存在的槽）。
    fn wait(&self, id: u64, deadline: Instant) -> Option<Value> {
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(value) = slots.get(&id).and_then(|slot| slot.as_ref()) {
                return Some(value.clone());
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            let (next, _) = self
                .ready
                .wait_timeout(slots, deadline - now)
                .unwrap_or_else(|e| e.into_inner());
            slots = next;
        }
    }
}

// 当前作用域栈：栈顶是「当前 (owner, scope)」。栈底永远是 `(0, 0)`（宿主根）。
//
// 用 **thread-local** 而不是全局量：中间插件要**并行**加载多个脚本，每个加载线程
// 各自 enter 自己的作用域、互不干扰；全局量会让「pump 线程 enter 期间宿主恰在加载
// 别的插件」发生串号。硬约束（见设计文档 5.4）：注册必须发生在某个显式作用域内，
// 离开作用域后不得再使用作用域内取得的句柄。
thread_local! {
    static SCOPE_STACK: std::cell::RefCell<Vec<(u64, u64)>> =
        std::cell::RefCell::new(vec![(0, 0)]);
}

/// 作用域守卫：把一层 (owner, scope) 压进 thread-local 栈，drop 时弹出还原。
struct ScopeGuard;

impl Drop for ScopeGuard {
    fn drop(&mut self) {
        SCOPE_STACK.with(|stack| {
            stack.borrow_mut().pop();
        });
    }
}

// 调用者栈（ABI v6）：宿主经 `api_call_service` 中转服务调用时，把**调用方**的 owner
// 压进来；被调方在方法里问 `current_plugin` 读到的就是它。
//
// 为什么不能直接读 `SCOPE_STACK`：那个栈只记「宿主最后把控制权交给了谁」。插件在自己
// 起的线程（tokio task / wait_for_update 循环）上调服务时那条栈是空的，而调用方手里的
// `host_ctx` 明明白白写着它是谁——所以调用者栈的**值**取自 `PluginHost.owner`，
// 只是借线程局部变量做一个「调用期间有效」的作用域。
//
// 也正因为值来自宿主发放的 `host_ctx`，被调方无法自报：它没有机会写这张栈。
thread_local! {
    static CALLER_STACK: std::cell::RefCell<Vec<u64>> = std::cell::RefCell::new(Vec::new());
}

/// 调用者守卫：drop 时弹出调用者栈（panic 也照样弹，不然整条线程的归属都会串）。
struct CallerGuard;

impl Drop for CallerGuard {
    fn drop(&mut self) {
        CALLER_STACK.with(|stack| {
            stack.borrow_mut().pop();
        });
    }
}

// 执行者栈：「现在这段代码跑在哪个插件名下」。服务调用期间是**提供方**，
// 其余时候等于线程作用域栈顶。
//
// 它和调用者栈是两个问题：`current_plugin` 要的是「谁在调我」，而
// `set_config` / 日志要的是「这段代码算谁的」。服务方法里写配置必须写提供方自己的段，
// 不然就会写进调用方的配置里（v5 一直如此，v6 有了服务表上的 owner 才能修）。
thread_local! {
    static EXECUTOR_STACK: std::cell::RefCell<Vec<u64>> = std::cell::RefCell::new(Vec::new());
}

/// 执行者守卫：drop 时弹出执行者栈。
struct ExecutorGuard;

impl Drop for ExecutorGuard {
    fn drop(&mut self) {
        EXECUTOR_STACK.with(|stack| {
            stack.borrow_mut().pop();
        });
    }
}

// 「上一次调用为什么失败」：ABI 上 `call_service` 只能回 NULL，
// 细节靠 VTable 的 `last_error` 带回去（线程局部，下一次失败前一直有效）。
thread_local! {
    static SERVICE_ERROR: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

impl HostRuntime {
    /// 在给定的 [`Context`] 上装好四个机制，并生成 VTable。
    pub fn new(ctx: Context) -> Result<Arc<Self>, String> {
        let mut guards = Vec::with_capacity(5);
        guards.push(fuyubot_event::install(&ctx).map_err(|e| e.to_string())?);
        guards.push(fuyubot_service::install(&ctx).map_err(|e| e.to_string())?);
        guards.push(fuyubot_tool::install(&ctx).map_err(|e| e.to_string())?);
        guards.push(fuyubot_middleware::install(&ctx).map_err(|e| e.to_string())?);

        let events = ctx.get::<EventBus>().map_err(|e| e.to_string())?;
        let tools = ctx.get::<ToolRegistry>().map_err(|e| e.to_string())?;
        let middleware = ctx.get::<Pipeline>().map_err(|e| e.to_string())?;

        // 先把编译期烘焙的 `td_api.tl` 元数据解析掉（缺口 13）。拖到第一次 `td_send`
        // 才解析的话，**第一条**请求会凭空多出一次几十毫秒的 JSON 解析——那正是
        // 「刚起来那几条更新」最容易被漏掉的窗口（`download_file` 的进度等待就被它踩中过）。
        // 进程内只解析一次，之后每次校验都只是查一张内存里的表。
        let _ = TdApiSchema::compiled();

        // 工具注册 / 注销 → 事件总线（缺口 9）。
        //
        // 机制 crate 之间不许互相依赖（硬约束 12），`ToolRegistry` 只认自己的监听器，
        // 所以这条桥只能由唯一同时认识两边的东西来搭——就是这里。通道名直接用
        // `ToolEvent::kind()`（`tool_registered` / `tool_unregistered`），不再另起一套命名。
        //
        // 监听器在工具表的锁**外面**被调用，所以回调里再 `emit` 不会自锁；`emit` 的回调
        // 也可能反过来查工具表，这条链是安全的。
        let bus = Arc::clone(&events);
        guards.push(
            tools.on_change(move |event| bus.emit(event.kind(), &event.to_payload()).map(|_| ())),
        );

        // 内置工具 `fuyubot.td.functions`（缺口 13③）：把编译期烘焙的
        // `td_api.tl` 函数表变成工具表里**第一个带真实处理器**的工具，而不是只躺在库里的
        // 死数据（宿主 `call_tool` 能直接拿到结果；ABI 侧的输出回投是另一条 ABI 决定）。
        // 它同时也给 schema 找到了生产消费者——`check_request` 那条在 `td_send` 里。
        let mut descriptor = ToolDescriptor::new(
            TD_FUNCTIONS_TOOL,
            "列出这版 TDLib 的 `---functions---` 方法（含参数、返回类型与说明）",
            json!({
                "type": "object",
                "properties": {
                    "filter": {
                        "type": "string",
                        "description": "只看名字里含这段文字的方法（忽略大小写）"
                    }
                },
                "additionalProperties": false
            }),
            Some(td_functions_handler),
        );
        descriptor.idempotent = true;
        let builtin = tools
            .register_tool(descriptor)
            .map_err(|e| format!("注册内置工具 {TD_FUNCTIONS_TOOL} 失败: {e}"))?;

        // 预置内置 scope service（owner = 0 宿主自己，永远不会被回滚）。
        let rt = Arc::new_cyclic(|weak| Self {
            ctx,
            events,
            tools,
            middleware,
            td: OnceLock::new(),
            _guards: guards,
            _builtin_tools: vec![builtin],
            registrations: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(1),
            next_owner: AtomicU64::new(1),
            scopes: Mutex::new(HashMap::new()),
            plugin_contexts: Mutex::new(HashMap::new()),
            next_scope: AtomicU64::new(1),
            waiters: Mutex::new(HashMap::new()),
            next_waiter: AtomicU64::new(1),
            log_lines: Mutex::new(VecDeque::new()),
            log_verbosity: AtomicU32::new(DEFAULT_LOG_VERBOSITY as u32),
            trace_updates: AtomicBool::new(false),
            tool_list_cache: Mutex::new(None),
            request_owners: Mutex::new(RequestOwners::default()),
            next_synthetic_request: AtomicU64::new(SYNTHETIC_REQUEST_BASE),
            next_simplify_request: AtomicU64::new(SIMPLIFY_REQUEST_BASE),
            simplify_workers: AtomicU64::new(0),
            sent_requests: Mutex::new(VecDeque::new()),
            // scope service 的 `context` 指向这个 Arc 内部（和 host_ctx 一个道理）。
            scope_vtable: ServiceVTable {
                name: SCOPE_SERVICE_NAME.as_ptr(),
                version: 1,
                size: core::mem::size_of::<ServiceVTable>() as u32,
                context: weak.as_ptr() as *mut c_void,
                methods: SCOPE_METHODS.0.as_ptr(),
                method_count: SCOPE_METHODS.0.len(),
            },
            plugins_vtable: ServiceVTable {
                name: PLUGIN_REPORT_SERVICE_C.as_ptr(),
                version: 1,
                size: core::mem::size_of::<ServiceVTable>() as u32,
                context: weak.as_ptr() as *mut c_void,
                methods: PLUGIN_METHODS.0.as_ptr(),
                method_count: PLUGIN_METHODS.0.len(),
            },
            identity_vtable: ServiceVTable {
                name: IDENTITY_SERVICE_C.as_ptr(),
                version: 1,
                size: core::mem::size_of::<ServiceVTable>() as u32,
                context: weak.as_ptr() as *mut c_void,
                methods: IDENTITY_METHODS.0.as_ptr(),
                method_count: IDENTITY_METHODS.0.len(),
            },
            self_user: Mutex::new(None),
            config: Mutex::new(ConfigState::default()),
            log_plugins: Mutex::new(None),
            owner_names: Mutex::new(HashMap::new()),
            owner_revision: AtomicU64::new(0),
            plugin_report: Mutex::new(None),
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
                call_tool_result: Some(api_call_tool_result),
                list_tools: Some(api_list_tools),
                list_plugins: Some(api_list_plugins),
                provide_service: Some(api_provide_service),
                get_service: Some(api_get_service),
                td_invoke: Some(api_td_invoke),
                td_invoke_simplified: Some(api_td_invoke_simplified),
                td_execute: Some(api_td_execute),
                use_middleware: Some(api_use_middleware),
                wait_for_update: Some(api_wait_for_update),
                cancel_wait: Some(api_cancel_wait),
                log: Some(api_log),
                get_config: Some(api_get_config),
                set_config: Some(api_set_config),
                provide_service_shared: Some(api_provide_service_shared),
                simplify_invoke: Some(api_simplify_invoke),
                simplify_invoke_result: Some(api_simplify_invoke_result),
                current_plugin: Some(api_current_plugin),
                call_service: Some(api_call_service),
                last_error: Some(api_last_error),
                list_services: Some(api_list_services),
                set_config_section: Some(api_set_config_section),
            },
        });
        rt.provide_scope_service();
        rt.provide_plugins_service();
        rt.provide_identity_service();
        Ok(rt)
    }

    /// 把内置 scope service 预置进服务表。
    ///
    /// 句柄故意 `forget`：它要活到进程结束，不能跟着谁 drop 就注销。
    fn provide_scope_service(&self) {
        let vtable = &self.scope_vtable as *const ServiceVTable;
        match self.ctx.provide_named(
            SCOPE_SERVICE_NAME.to_str().unwrap_or_default(),
            AbiService { vtable, owner: 0 },
        ) {
            Ok(effect) => std::mem::forget(effect),
            Err(e) => self.warn(&format!("预置 scope service 失败: {e}")),
        }
    }

    /// 把内置的「插件清单」service 预置进服务表。
    ///
    /// 名字 [`PLUGIN_REPORT_SERVICE`]，方法 `report`（无参）返回最近一次快照的 JSON。
    /// 插件用现成的 `get_service` + 方法调用就能读，**不需要动 ABI**——
    /// 这正是缺口 31 里「不碰 ABI 的那一半」。
    fn provide_plugins_service(&self) {
        let vtable = &self.plugins_vtable as *const ServiceVTable;
        match self
            .ctx
            .provide_named(PLUGIN_REPORT_SERVICE, AbiService { vtable, owner: 0 })
        {
            // 句柄故意 `forget`：它要活到进程结束，不能跟着谁 drop 就注销。
            Ok(effect) => std::mem::forget(effect),
            Err(e) => self.warn(&format!("预置插件清单 service 失败: {e}")),
        }
    }

    /// 把内置的「我是谁」service 预置进服务表。
    ///
    /// 名字 [`IDENTITY_SERVICE`]，方法 `usernames`（无参）返回自身活跃用户名的 JSON 数组。
    /// `fuyubot-simplify` 的 `on_command` 靠它判断「这条命令点名的是不是我」——
    /// 插件在**命令回调里**就能读（纯内存，不是 TDLib 请求，不会卡接收循环）。
    fn provide_identity_service(&self) {
        let vtable = &self.identity_vtable as *const ServiceVTable;
        match self
            .ctx
            .provide_named(IDENTITY_SERVICE, AbiService { vtable, owner: 0 })
        {
            // 句柄故意 `forget`：它要活到进程结束，不能跟着谁 drop 就注销。
            Ok(effect) => std::mem::forget(effect),
            Err(e) => self.warn(&format!("预置身份 service 失败: {e}")),
        }
    }

    /// 记下 `getMe` 的结果（CLI 在登录成功之后调）。
    ///
    /// 时机正好：插件早就加载好了，但接收循环还没开（`run` 的第 ⑦ 步），
    /// 所以插件不可能在 `start()` 里同步问 TDLib——身份只能由宿主喂进来。
    pub fn set_self_user(&self, me: Value) {
        *self
            .self_user
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(me);
    }

    /// 自己的全部活跃用户名（`usernames.active_usernames`，字段路径见约束 42）。
    ///
    /// 还没登录 / 没接 TDLib 时是空表——调用方据此判断「身份未知」，别拿空表当「谁都不匹配」。
    pub fn self_usernames(&self) -> Vec<String> {
        let self_user = self.self_user.lock().unwrap_or_else(|e| e.into_inner());
        let Some(me) = self_user.as_ref() else {
            return Vec::new();
        };
        me.get("usernames")
            .and_then(|usernames| usernames.get("active_usernames"))
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// 装进全局配置：`path` 是用户配置文件（启动时读、运行时**不写**），
    /// `None` = 这次启动不可写（`set_config` 直接报错），
    /// `root` 是完整配置树的 JSON。必须在插件加载之前调（插件 `start` 里就可能读）。
    ///
    /// 这里顺手把**运行时落盘**（`<path 的目录>/plugin-config/*.toml`）并进来，只填缺失键：
    /// 用户配置优先，插件上次存的值排在它下面。
    pub fn install_config(&self, path: Option<PathBuf>, root: Value) {
        let store = path
            .as_ref()
            .and_then(|path| path.parent())
            .map(|dir| dir.join(CONFIG_STORE_DIR));
        let mut root = root;
        // 用户层快照要在合并落盘**之前**取：`set_config` 靠它判断「键被用户钉住了吗」。
        let user_plugin = root
            .get(CONFIG_PLUGIN_KEY)
            .cloned()
            .unwrap_or_else(|| json!({}));
        let mut warnings = Vec::new();
        if let Some(dir) = store.as_ref() {
            merge_store_into(&mut root, dir, &mut warnings);
        }
        for warning in warnings {
            self.warn(&warning);
        }
        let mut config = self.config.lock().unwrap_or_else(|e| e.into_inner());
        config.store = store;
        config.root = root;
        config.user_plugin = user_plugin;
    }

    /// 合并插件包自带的默认配置（`plugin.json` 的 `config`），**只填缺失键**。
    ///
    /// 优先级最低：用户配置与运行时落盘已有的键一律不动。loader 在包探测完、
    /// 插件进内核之前调（`DylibPlugin::new`），所以 `inject` / `soft_inject` 兜底
    /// 也能读到默认值。
    pub fn apply_plugin_defaults(&self, name: &str, defaults: Value) {
        if !defaults.is_object() {
            return;
        }
        let mut config = self.config.lock().unwrap_or_else(|e| e.into_inner());
        if !config.root.is_object() {
            // 没装配置（纯测试 / 嵌入方）：给一个空树，默认值照样读得到。
            config.root = json!({});
        }
        let Some(root) = config.root.as_object_mut() else {
            return;
        };
        let plugin = root
            .entry(CONFIG_PLUGIN_KEY.to_owned())
            .or_insert_with(|| json!({}));
        if let Ok(own) = ensure_leaf(plugin, name) {
            merge_missing(own, &defaults);
        }
    }

    /// 登记「插件 owner id → 插件名」。loader 在 `plugin_init` 之后、`plugin_start` 之前调，
    /// 这样插件在 `start` 里（以及之后的回调里）调 `set_config` 时，宿主能据此判定它是谁。
    pub(crate) fn register_owner_name(&self, owner: u64, name: &str) {
        self.owner_names
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(owner, name.to_owned());
        self.owner_revision.fetch_add(1, Ordering::Relaxed);
    }

    /// 卸载插件时摘掉它的 owner 映射（owner id 会复用吗？不会——`next_owner` 只增不减，
    /// 但摘掉仍能省内存，也避免误判）。loader 在 `LoadedPlugin::drop` 回滚时顺带调。
    pub(crate) fn unregister_owner_name(&self, owner: u64) {
        self.owner_names
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&owner);
        self.owner_revision.fetch_add(1, Ordering::Relaxed);
    }

    /// owner id → 插件名。查不到（极早期 / 已经注销）返回 `None`，日志来源标成
    /// `plugin`，不冒充宿主。
    fn owner_name(&self, owner: u64) -> Option<String> {
        self.owner_names
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&owner)
            .cloned()
    }

    /// 读配置：`section` 为空 = 读顶层；否则把 `section` 当**点路径**在 `plugin` 表里下钻
    /// （插件名是点分的，如 `fuyubot.manage` → `plugin.fuyubot.manage`）。
    /// `key` 为空 = 整段/整个顶层。命中返回 `Some(JSON)`，否则 `None`。
    fn read_config(&self, section: &str, key: &str) -> Option<Value> {
        let config = self.config.lock().unwrap_or_else(|e| e.into_inner());
        let node = if section.is_empty() {
            &config.root
        } else {
            let plugin = config.root.get(CONFIG_PLUGIN_KEY)?;
            lookup_path(plugin, section)?
        };
        if key.is_empty() {
            return Some(node.clone());
        }
        lookup_path(node, key).cloned()
    }

    /// 写配置：默认只写「当前执行者」自己名下的段；`target = Some(名字)` 时写**指定插件**的段。
    ///
    /// 越权判定：没有 `target` 时靠 [`HostRuntime::current_executor`] + `owner_names` 映射，
    /// 插件无法伪造（`set_config` 根本没有 section 参数）。跨插件写是**显式**的
    /// （`set_config_section`），调用方必须自己写出目标插件名，所以也谈不上冒充。
    ///
    /// 落盘只写**目标那一个文件**：`<root>/plugin-config/<插件名>.toml`。
    /// `fuyubot.toml` 运行时永不重写，所以用户写在里面的注释、其它段、`api_hash` 都安全。
    fn write_config(
        &self,
        target: Option<&str>,
        key: &str,
        value_json: &str,
    ) -> Result<(), String> {
        let executor = self.current_executor();
        let name = match target {
            // 显式目标：跨插件写。名字为空等于「没指定」，拒绝比猜一个安全。
            Some(section) if !section.is_empty() => section.to_owned(),
            Some(_) => return Err("目标插件名不能为空".to_owned()),
            // 没给目标：写执行者那段。名字没登记（宿主自己 / 还没走完 init）就不能写。
            None => self
                .executor_name()
                .ok_or_else(|| "当前执行者没有登记的插件名，不能写配置".to_owned())?,
        };

        // 跨插件写留一笔：出问题时要能回答「是谁改了别人的配置」。
        if target.is_some() {
            let who = self
                .owner_name(executor)
                .unwrap_or_else(|| "宿主".to_owned());
            if who != name {
                self.info(&format!("{who} 改写插件 {name} 的配置"));
            }
        }

        let value: Value =
            serde_json::from_str(value_json).map_err(|e| format!("值不是合法 JSON: {e}"))?;

        // 锁住配置，先改内存，落盘成功才算数（落盘失败要还原，不能留半截状态）。
        let mut config = self.config.lock().unwrap_or_else(|e| e.into_inner());

        // 用户钉住的键：直接拒绝。写进去下次启动照样被用户配置盖回去，
        // 那正是「set_config 返回成功、值却没留住」这类最难查的 bug（P0 修过一次）。
        if user_pins(&config.user_plugin, &name, key) {
            return Err(user_pin_message(&name, key));
        }

        // 落盘位置先拷出来，避免下面持有 `config` 可变借用的同时再读它。
        let store = config.store.clone();
        // 保证 `plugin.<name路径>` 这颗子树存在。
        let old = {
            let root = config
                .root
                .as_object_mut()
                .ok_or_else(|| "配置根不是 JSON 对象".to_owned())?;
            let plugin = root
                .entry(CONFIG_PLUGIN_KEY.to_owned())
                .or_insert_with(|| json!({}));
            // 插件名是点分的：在 plugin 表里按路径下钻到自己的叶子表。
            let own = ensure_leaf(plugin, &name)?;

            let old = own.clone();
            if key.is_empty() {
                *own = value.clone();
            } else {
                set_path(own, key, value.clone())?;
            }
            old
        };

        // 落盘：没有 store 目录 = 这次启动没装配置文件——**报错**而不是静默成功：
        // 插件会以为已经保存，下次启动又是旧值（Android 上一度就是这样）。
        let failure = match store.as_ref() {
            Some(dir) => persist_plugin_section(dir, &name, key, &value).err(),
            None => Some(
                "这次启动没有可回写的配置文件（`config_path` 为空），`set_config` 无处落盘"
                    .to_owned(),
            ),
        };
        if let Some(e) = failure {
            // 落盘失败：还原内存，报错。
            if let Some(plugin) = config.root.get_mut(CONFIG_PLUGIN_KEY) {
                if let Ok(own) = ensure_leaf(plugin, &name) {
                    *own = old;
                }
            }
            return Err(e);
        }
        Ok(())
    }

    /// 全局 VTable 指针（测试 / 宿主内部用；**正式加载插件用 [`Self::plugin_host`]**）。
    pub fn vtable_ptr(&self) -> *const HostApiVTable {
        &self.vtable
    }

    pub fn vtable(&self) -> &HostApiVTable {
        &self.vtable
    }

    /// 造一份「指向本运行时 + 这个插件」的 [`PluginHost`]，把其中的 vtable 交给 `plugin_init`。
    ///
    /// 每个插件一份（地址稳定）：插件可能在**自己起的线程**、或阻塞式
    /// `wait_for_update` 循环里调 `host.log`，那些线程上没有宿主的作用域栈——归属
    /// 只能从它传回来的 `host_ctx` 里认。
    pub(crate) fn plugin_host(&self, owner: u64) -> Box<PluginHost> {
        let mut host = Box::new(PluginHost {
            magic: PLUGIN_HOST_MAGIC,
            runtime: self as *const HostRuntime,
            owner,
            vtable: self.vtable,
        });
        let ptr = host.as_mut() as *mut PluginHost as *mut c_void;
        host.vtable.host_ctx = ptr;
        host
    }

    pub fn context(&self) -> &Context {
        &self.ctx
    }

    pub fn events(&self) -> &Arc<EventBus> {
        &self.events
    }

    /// 读某个插件**自己那一段**配置里的一个键：也就是
    /// `[plugin.<插件名>]` 下的 `key`（插件名是点分的，见约束 52）。
    ///
    /// 宿主读依赖声明 `inject` 走这里。这不是绕过 `get_config` 的权限模型——
    /// 那条是给**插件**用的（插件只能写自己的段），这条是宿主自己读，给谁看由它决定。
    pub fn plugin_config_value(&self, plugin: &str, key: &str) -> Option<Value> {
        self.read_config(plugin, key)
    }

    /// 某个能力名是不是已经有了，**不看类型**。
    ///
    /// 这是 `inject` 依赖判定的一边：插件等着的能力能不能在报表格里找到。
    pub fn has_service(&self, name: &str) -> bool {
        self.ctx.has(name)
    }

    /// 已注册的**命名**服务清单，已排序。
    ///
    /// 现在直接读内核 `Context`：命名服务和类型化服务是同一张表了，
    /// 宿主侧不必再为一个专用容器留字段。
    pub fn service_names(&self) -> Vec<String> {
        self.ctx.names()
    }

    /// 当前调用者**看得见**的服务清单（JSON 数组）：名字、版本、方法名、提供方。
    ///
    /// 从调用方的作用域链查，所以私有服务也在里面；只报元数据、不交 vtable 指针
    /// （想调还是走 `call_service`，那样才有身份与错误细节）。
    fn service_catalog(&self) -> Value {
        let scope = self.caller_context();
        let mut out = Vec::new();
        for name in scope.names() {
            let Ok(entry) = scope.resolve::<AbiService>(&name) else {
                continue;
            };
            // SAFETY: 表项活着（上面攥着 Arc），vtable 由提供方保证活到注销。
            let vtable = unsafe { &*entry.vtable };
            let mut methods = Vec::new();
            for index in 0..vtable.method_count {
                // SAFETY: method_count 由注册方保证，methods 指向同等数量的连续项。
                let method = unsafe { &*vtable.methods.add(index) };
                if method.name.is_null() {
                    continue;
                }
                // SAFETY: 方法名是 NUL 结尾 UTF-8。
                if let Ok(text) = unsafe { CStr::from_ptr(method.name) }.to_str() {
                    methods.push(text.to_owned());
                }
            }
            out.push(json!({
                "name": name,
                "version": vtable.version,
                "methods": methods,
                // `null` = 宿主预置的服务（scope / plugins / identity）。
                "plugin": self.owner_name(entry.owner),
            }));
        }
        Value::Array(out)
    }

    pub fn tools(&self) -> &Arc<ToolRegistry> {
        &self.tools
    }

    pub fn middleware(&self) -> &Arc<Pipeline> {
        &self.middleware
    }

    /// 装上内核的**活报告来源**。装一次就够，之后不必再管。
    ///
    /// 与旧接口（`install_plugin_report(&Report)`）的区别：那个要调用方在**每次**
    /// 状态变化后主动刷，实际没人刷得到；这个交出去的是计算能力，问一次算一次。
    pub fn install_plugin_reporter(&self, reporter: PluginReporter) {
        *self.plugin_report.lock().unwrap_or_else(|e| e.into_inner()) = Some(reporter);
    }

    /// 把内核的插件生命周期事件桥到事件总线（缺口 12 的宿主消费者）。
    ///
    /// 通道名直接用 `LifecycleEvent::kind()`（`plugin_loaded` / `plugin_activated` /
    /// `plugin_deactivated` / `plugin_failed` / `plugin_unloaded`），负载是
    /// `{"event": …, "name": …}`，失败时多一个 `reason`。
    ///
    /// 返回的 [`Effect`] 要留住（drop 即退订）。和 [`install_plugin_reporter`] 一样，
    /// 这只是把内核的能力接出来——`HostRuntime` 仍然**不持有** `PluginManager`。
    ///
    /// [`install_plugin_reporter`]: Self::install_plugin_reporter
    pub fn install_lifecycle_bridge(&self, manager: &PluginManager) -> Effect {
        let bus = Arc::clone(&self.events);
        manager.on_lifecycle(move |event| {
            let mut payload = json!({ "event": event.kind(), "name": event.name() });
            if let Some(reason) = event.reason() {
                payload["reason"] = Value::String(reason.to_owned());
            }
            let _ = bus.emit(event.kind(), &payload);
        })
    }

    /// 此刻的插件报告（JSON）。没装来源就是空表。
    pub fn plugin_report(&self) -> String {
        let reporter = self
            .plugin_report
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        match reporter {
            Some(reporter) => report_json(&reporter.report()),
            None => report_json(&Report {
                plugins: Vec::new(),
            }),
        }
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
    ///
    /// 顺带维护「我是谁」：`getMe` 只是**登录那一刻**的快照，用户名 / 名字之后还会变
    /// （`updateUser`），所以每条 `updateUser` 都先过一遍 [`Self::absorb_self_update`]，
    /// 广播出去的就是新值——[`IDENTITY_SERVICE`] 与面板看到的都不会是死的。
    pub fn publish_update(&self, payload: &Value) -> fuyubot_core::Result<usize> {
        self.absorb_self_update(payload);
        let mut total = self.events.emit(UPDATE_EVENT, payload)?;
        if let Some(update_type) = payload.get("@type").and_then(Value::as_str) {
            if update_type != UPDATE_EVENT {
                total += self.events.emit(update_type, payload)?;
            }
        }
        Ok(total)
    }

    /// `updateUser` 里是不是**我自己**：是就把存的那份换成新的。
    ///
    /// 判据只看 id（`getMe` 的结果里那个 `user.id`）；别人的 `updateUser` 一概不碰。
    /// 还没登录（`self_user` 是 `None`）时也不碰——那时连「我是谁」都还不知道。
    fn absorb_self_update(&self, payload: &Value) {
        if payload.get("@type").and_then(Value::as_str) != Some("updateUser") {
            return;
        }
        let Some(user) = payload.get("user") else {
            return;
        };
        let Some(incoming) = user.get("id").and_then(Value::as_i64) else {
            return;
        };
        let mut slot = self.self_user.lock().unwrap_or_else(|e| e.into_inner());
        let Some(current) = slot.as_ref() else {
            return;
        };
        if current.get("id").and_then(Value::as_i64) != Some(incoming) {
            return;
        }
        *slot = Some(user.clone());
    }

    /// 宿主眼里的「我自己」（最近一次 `getMe` / `updateUser` 的 `user` JSON）。
    ///
    /// 没登录 / 没接 TDLib 时是 `None`。要用户名请用 [`Self::self_usernames`]。
    pub fn self_user(&self) -> Option<Value> {
        self.self_user
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// 读当前插件日志门槛（语义见上方 `log_verbosity` 字段）。
    pub fn log_verbosity(&self) -> u32 {
        self.log_verbosity.load(Ordering::Relaxed)
    }

    /// 设门槛。低于门槛的插件日志**既**不进 [`log_lines`](Self::log_lines) **也**不写 stderr。
    ///
    /// 只是省掉格式化与系统调用，**不改变**插件侧的行为：插件照样能调 `log`。
    pub fn set_log_verbosity(&self, level: LogLevel) {
        self.log_verbosity.store(level as u32, Ordering::Relaxed);
    }

    /// 只看这些插件打出来的日志（`--log-plugin`，可重复）。
    ///
    /// 传空表 = 取消过滤。名字按**原样**比较（插件名是 manifest 里的标识，大小写敏感）；
    /// 也接受逗号分隔的写法（`--log-plugin a,b`）。只影响插件经 VTable 打进来的日志，
    /// 宿主 / 管线日志不受影响。
    pub fn set_log_plugins(&self, names: Vec<String>) {
        let filter: HashSet<String> = names
            .iter()
            .flat_map(|raw| raw.split(','))
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .collect();
        let mut slot = self.log_plugins.lock().unwrap_or_else(|e| e.into_inner());
        *slot = if filter.is_empty() {
            None
        } else {
            Some(filter)
        };
    }

    /// 当前来源白名单（`None` = 不过滤）。给测试与诊断用。
    pub fn log_plugins(&self) -> Option<Vec<String>> {
        let slot = self.log_plugins.lock().unwrap_or_else(|e| e.into_inner());
        slot.as_ref().map(|names| {
            let mut names: Vec<String> = names.iter().cloned().collect();
            names.sort();
            names
        })
    }

    /// 这条插件日志该不该留下。没有白名单时全收；没有登记名字的 owner 也放行
    /// （`plugin_init` 阶段，宁可多打一行，也别把启动期的错误吞掉）。
    fn plugin_log_enabled(&self, name: &str) -> bool {
        let slot = self.log_plugins.lock().unwrap_or_else(|e| e.into_inner());
        slot.as_ref().is_none_or(|names| names.contains(name))
    }

    /// 打开 / 关闭原始更新追踪（`--trace-updates`）。
    pub fn set_trace_updates(&self, enabled: bool) {
        self.trace_updates.store(enabled, Ordering::Relaxed);
    }

    pub fn trace_updates(&self) -> bool {
        self.trace_updates.load(Ordering::Relaxed)
    }

    /// `list_tools` 的 JSON，按工具表 / owner 表版本号缓存。
    ///
    /// 每条工具多一个 `owner` 字段（缺口 31）：注册它的**插件名**，宿主内置工具是
    /// [`HOST_TOOL_OWNER`]。ABI 不用改——JSON 加字段向后兼容；插件靠它回答
    /// 「这个工具到底是谁的」。
    ///
    /// 工具表变更不频繁，而列工具是热路径：没缓存时每调一次都要 `BTreeMap` 全表克隆
    /// → 逐条 `to_json` → 序列化 → 再拷进线程局部缓冲。版本号没变就直接复用上次的
    /// `CString`（用 `Arc` 存，所以缓存被替换后插件手里那个旧指针**依然有效**）。
    fn tools_json(&self) -> Arc<CString> {
        let revision = self.tools.revision();
        let owner_revision = self.owner_revision.load(Ordering::Relaxed);
        let mut cache = self
            .tool_list_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        match cache.as_ref() {
            Some((cached_revision, cached_owner_revision, json))
                if *cached_revision == revision && *cached_owner_revision == owner_revision =>
            {
                Arc::clone(json)
            }
            _ => {
                let mut list = self.tools.list_tools_json();
                let owners = self.tool_owners();
                if let Some(items) = list.as_array_mut() {
                    for item in items {
                        let name = item
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned();
                        let owner = owners
                            .get(&name)
                            .map(String::as_str)
                            .unwrap_or(HOST_TOOL_OWNER);
                        item["owner"] = Value::String(owner.to_owned());
                    }
                }
                let json = Arc::new(CString::new(list.to_string()).unwrap_or_default());
                *cache = Some((revision, owner_revision, Arc::clone(&json)));
                json
            }
        }
    }

    /// 工具名 → 注册它的插件名（宿主自己 = [`HOST_TOOL_OWNER`]）。
    ///
    /// 从**注册表**算，而不是另存一张影子表：影子表要在「注销 / 回滚 / 加载失败」三处
    /// 同步，漏一处就会报错主人。注册表是唯一真实来源——工具不在表里，owner 也不存在。
    fn tool_owners(&self) -> HashMap<String, String> {
        // 先拷名字再拿注册表锁：两把锁不嵌套，免得以后有人反向加锁。
        let names = self
            .owner_names
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mut owners = HashMap::new();
        for registration in self.registrations().iter() {
            let RegistrationKind::Tool(handle) = &registration.kind else {
                continue;
            };
            let owner = names
                .get(&registration.owner)
                .cloned()
                .unwrap_or_else(|| HOST_TOOL_OWNER.to_owned());
            owners.insert(handle.name().to_owned(), owner);
        }
        owners
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
                .take(id)
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

    /// 发一条**原始 td_api JSON** 并等它自己的响应（宿主侧：JNI / TUI / 测试共用）。
    ///
    /// 与插件那条路（`td_invoke*` → 按归属回投）的区别：这条**不经过插件归属**，
    /// 也不需要 `@request_id` 约定——就是 TDLib 自己的「发一条 JSON、收一条 JSON」。
    ///
    /// 规则：
    /// - `request` 必须是对象且带 `@type`；`@extra` 由 [`TdClient::send`] 填，传了也会被覆盖。
    /// - **阻塞**到响应回来或 `timeout` 到期；超时返回 [`TdRequestError::Timeout`]。
    /// - 响应原样返回（`@type: "error"` 也是 `Ok`），由调用方自己判。
    /// - ⚠️ **必须有东西在收**：分发循环在跑，或者登录中的 `Session::login` 自己那条接收
    ///   循环（它把杂项经 `bypass` 转给管线）。两者都没有时只会等到超时。
    /// - ⚠️ 别在接收循环自己的回调里调它：那等于「收的人等自己要收的东西」，会一直等到超时。
    ///
    /// 等待表是**按调用**建的：每次请求一份槽位 + 一条临时订阅（退出时随句柄退订），
    /// 所以并发调用互不串号，超时也不会留下悬空条目。
    pub fn td_request(&self, request: Value, timeout: Duration) -> Result<Value, TdRequestError> {
        if !request.is_object() {
            return Err(TdRequestError::NotObject);
        }
        if request.get("@type").and_then(Value::as_str).is_none() {
            return Err(TdRequestError::MissingType);
        }
        let client = self.td.get().ok_or(TdRequestError::NoTdlib)?.clone();

        let waiters = Arc::new(TdWaiters::default());
        // 先订阅、再发送：响应可能在 `send` 返回之前就到达接收循环。
        let _subscription = {
            let waiters = Arc::clone(&waiters);
            self.events.on(RESPONSE_EVENT, None, move |payload| {
                waiters.deliver(payload);
                Ok(())
            })
        };

        // 占位与发送**在同一把锁里**：退出这段之后槽位已经在了，`deliver` 不会漏。
        // 持锁期间只做一次 `td_send`（不阻塞、不等待），不会把接收线程卡住。
        let id = {
            let mut slots = waiters.slots.lock().unwrap_or_else(|e| e.into_inner());
            let id = client.send(request)?.as_u64();
            slots.insert(id, None);
            id
        };

        match waiters.wait(id, Instant::now() + timeout) {
            Some(payload) => Ok(payload),
            None => Err(TdRequestError::Timeout {
                id,
                waited: timeout,
            }),
        }
    }

    /// 当前归属：0 = 宿主自己，其它 = 正在跑的那个插件（本线程作用域栈顶）。
    pub fn current_owner(&self) -> u64 {
        SCOPE_STACK.with(|stack| stack.borrow().last().map(|(owner, _)| *owner).unwrap_or(0))
    }

    /// 本线程当前压在调用者栈顶的 owner（不在服务调用里时为 `None`）。
    fn current_caller(&self) -> Option<u64> {
        CALLER_STACK.with(|stack| stack.borrow().last().copied())
    }

    /// 把「谁在调」压进调用者栈；离开作用域（或 panic）时自动弹出。
    ///
    /// 只在 `owner != 0` 时压：压一个 0 会把「宿主自己调」伪装成一次有身份的调用，
    /// 反而遮住内层本该沿作用域栈查到的信息。
    fn enter_caller(&self, owner: u64) -> Option<CallerGuard> {
        if owner == 0 {
            return None;
        }
        CALLER_STACK.with(|stack| stack.borrow_mut().push(owner));
        Some(CallerGuard)
    }

    /// 当前「这段代码算谁的」：执行者栈顶 → 线程作用域栈顶。
    ///
    /// 服务方法里是**提供方**（`set_config` 因此写自己的段），其余时候是宿主交给的插件。
    fn current_executor(&self) -> u64 {
        EXECUTOR_STACK
            .with(|stack| stack.borrow().last().copied())
            .unwrap_or_else(|| self.current_owner())
    }

    /// 压一层执行者（服务调用期间用）；`owner == 0`（宿主预置服务）不压。
    fn enter_executor(&self, owner: u64) -> Option<ExecutorGuard> {
        if owner == 0 {
            return None;
        }
        EXECUTOR_STACK.with(|stack| stack.borrow_mut().push(owner));
        Some(ExecutorGuard)
    }

    /// 当前执行者的插件名（没登记就是 `None`：宿主自己 / `init` 极早期）。
    ///
    /// 读配置的「空 section = 自己」与写配置的「写谁的段」共用这一处判定，
    /// 两边不会各算一套。
    fn executor_name(&self) -> Option<String> {
        let owner = self.current_executor();
        self.owner_names
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&owner)
            .cloned()
    }

    /// 记下/清掉「上一次调用为什么失败」（本线程）。
    fn set_service_error(&self, message: Option<String>) {
        SERVICE_ERROR.with(|slot| *slot.borrow_mut() = message);
    }

    fn last_service_error(&self) -> Option<String> {
        SERVICE_ERROR.with(|slot| slot.borrow().clone())
    }

    /// 当前子作用域（本线程作用域栈顶；0 = 插件根作用域 / 宿主根）。
    fn current_scope(&self) -> u64 {
        SCOPE_STACK.with(|stack| stack.borrow().last().map(|(_, scope)| *scope).unwrap_or(0))
    }

    /// 在作用域内把归属切成 `(owner, scope)`；插件再发起请求、再注册订阅都会算在它名下。
    fn enter_scope(&self, owner: u64, scope: u64) -> ScopeGuard {
        SCOPE_STACK.with(|stack| stack.borrow_mut().push((owner, scope)));
        ScopeGuard
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

    /// 登记一条 **simplify 操作号**的归属：结果回来时按它定向投递。
    ///
    /// 与 [`Self::record_request`] 的区别：这个号**不是**发给 TDLib 的请求，
    /// 所以不进 `sent_requests`（那条留档是「宿主替插件发出去了什么」），
    /// 但仍然要进归属表——不然结果只能全局广播，别人也收得到。
    fn record_simplify(&self, id: u64, owner: u64) {
        self.request_owners
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, owner);
    }

    /// 下一个 simplify 操作号。
    fn next_simplify_id(&self) -> u64 {
        self.next_simplify_request.fetch_add(1, Ordering::Relaxed)
    }

    // ── 插件作用域 ─────────────────────────────────────────────

    fn begin_plugin(&self) -> u64 {
        let id = self.next_owner.fetch_add(1, Ordering::Relaxed);
        // 每个插件一份 Context 子作用域：它注册的服务默认只自己（和自己的子作用域）看得见，
        // 查找仍沿父链走到根，所以宿主那四个机制照样能用。要跨插件共享走
        // `provide_service_shared`（直接写根）。
        self.plugin_contexts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, self.ctx.child());
        // 插件加载作用域：scope = 0（插件根作用域），压到本线程栈顶。
        SCOPE_STACK.with(|stack| stack.borrow_mut().push((id, 0)));
        id
    }

    /// 把某个 owner 临时压回作用域栈顶。卸载时跑 `plugin_stop` 用：那时加载作用域
    /// 已经弹掉了，不压回去的话插件在 `stop` 里打的日志会退化成 `plugin`、
    /// `set_config` 也会被当成宿主写的。
    pub(crate) fn enter_owner(&self, owner: u64) -> OwnerGuard<'_> {
        SCOPE_STACK.with(|stack| stack.borrow_mut().push((owner, 0)));
        OwnerGuard { _runtime: self }
    }

    fn end_plugin(&self) {
        SCOPE_STACK.with(|stack| {
            stack.borrow_mut().pop();
        });
    }

    /// 把某个插件注册的一切都撤掉，**后注册的先撤销**。
    ///
    /// 子作用域的 owner 继承自父，所以 owner 整批回滚**连带**子作用域（设计文档 5.2）：
    /// 它们的注册一起撤，条目也从作用域表里摘掉。
    pub(crate) fn rollback_owner(&self, owner: u64) {
        let removed = self.take_registrations(|r| r.owner == owner);
        // 在锁外回滚：句柄的 Drop 会回头去动事件总线 / 工具表。
        // 倒序撤销：与「卸载是加载顺序的倒序」一致，后注册的东西先撤。
        for registration in removed.into_iter().rev() {
            drop(registration);
        }
        // 这个 owner 建的子作用域一并失效并从表里摘掉，否则作用域表只增不减。
        self.scopes().retain(|_, info| info.owner != owner);
        // Context 子作用域也是：插件没了，它那层服务表就没人该看见了。
        // （注册本身已经在上面的倒序撤销里撤掉了，这里只是把表项丢掉。）
        self.plugin_contexts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&owner);
    }

    /// 当前调用者该往哪张服务表写、从哪张查。
    ///
    /// 有 owner 且登记过 child → 用它（插件的私有作用域）；否则退回根
    /// （宿主自己调的，或者 `plugin_init` 之前的早期调用）。
    fn caller_context(&self) -> Context {
        let owner = self.current_owner();
        self.plugin_contexts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&owner)
            .cloned()
            .unwrap_or_else(|| self.ctx.clone())
    }

    /// 摘走满足 `predicate` 的全部注册（**保持注册顺序**），剩下的写回表。
    ///
    /// 不用 `swap_remove`：它会搬动元素、打乱顺序，"按注册倒序撤销"就无从谈起。
    fn take_registrations(&self, predicate: impl Fn(&Registration) -> bool) -> Vec<Registration> {
        let mut registrations = self.registrations();
        let (matching, rest): (Vec<Registration>, Vec<Registration>) =
            registrations.drain(..).partition(|r| predicate(r));
        registrations.extend(rest);
        matching
    }

    /// 撤销某个子作用域累积的全部注册，并把它的句柄标记为失效。
    ///
    /// 只影响这个 scope，同 owner 的其他作用域不受影响——这是「单个脚本独立卸载」的落点。
    pub(crate) fn rollback_scope(&self, scope: u64) {
        if let Some(info) = self.scopes().get_mut(&scope) {
            info.alive = false;
        }
        let removed = self.take_registrations(|r| r.scope == scope);
        for registration in removed.into_iter().rev() {
            drop(registration);
        }
        // 摘掉条目：中间插件反复建/撤作用域（脚本热加载）时不该让这张表只增不减。
        self.scopes().remove(&scope);
    }

    fn remember(&self, kind: RegistrationKind) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        // 记的是「栈顶作用域的 owner + scope」：中间插件 enter 到哪个脚本的作用域，
        // 那个脚本在宿主侧的注册就落到那个 scope。
        let (owner, scope) =
            SCOPE_STACK.with(|stack| stack.borrow().last().copied().unwrap_or((0, 0)));
        // 只有工具注册才改变 `list_tools` 的归属——订阅 / 服务 / 中间件不用让缓存失效。
        let is_tool = matches!(&kind, RegistrationKind::Tool(_));
        self.registrations().push(Registration {
            id,
            owner,
            scope,
            kind,
        });
        if is_tool {
            self.owner_revision.fetch_add(1, Ordering::Relaxed);
        }
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

    /// 宿主侧的告警输出。`pub(crate)`：`loader` 的探测阶段也要报「自报名对不上」这类事。
    pub(crate) fn warn(&self, message: &str) {
        tracing::warn!(target: "fuyubot::host", "{message}");
    }

    /// 宿主侧的一般信息。与 [`Self::warn`] 走同一条通道（`tracing` 的 `fuyubot::host`
    /// target），区别只在级别：这条不是「出事了」，是「记一笔」。
    pub(crate) fn info(&self, message: &str) {
        tracing::info!(target: "fuyubot::host", "{message}");
    }

    fn registrations(&self) -> MutexGuard<'_, Vec<Registration>> {
        self.registrations.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn scopes(&self) -> MutexGuard<'_, HashMap<u64, ScopeInfo>> {
        self.scopes.lock().unwrap_or_else(|e| e.into_inner())
    }

    // ── 子作用域（供内置 scope service 调用）──────────────────────

    /// 新建一个子作用域：继承当前 owner，父 = 当前 scope。返回句柄。
    fn scope_create(&self) -> u64 {
        let handle = self.next_scope.fetch_add(1, Ordering::Relaxed);
        let (owner, parent) =
            SCOPE_STACK.with(|stack| stack.borrow().last().copied().unwrap_or((0, 0)));
        self.scopes().insert(
            handle,
            ScopeInfo {
                owner,
                parent,
                alive: true,
            },
        );
        handle
    }

    /// 进入一个作用域（压到本线程栈顶）。返回 `false` 表示句柄无效或已失效。
    fn scope_enter(&self, handle: u64) -> bool {
        let owner = {
            let scopes = self.scopes();
            let Some(info) = scopes.get(&handle) else {
                return false;
            };
            if !info.alive {
                return false;
            }
            info.owner
        };
        SCOPE_STACK.with(|stack| stack.borrow_mut().push((owner, handle)));
        true
    }

    /// 离开当前作用域（弹一层）。栈空时是空操作，不会下探到宿主根之下。
    fn scope_leave(&self) {
        SCOPE_STACK.with(|stack| {
            let mut stack = stack.borrow_mut();
            if stack.len() > 1 {
                stack.pop();
            }
        });
    }

    /// 撤销一个作用域的全部注册并失效句柄。返回 `false` 表示句柄无效。
    fn scope_drop(&self, handle: u64) -> bool {
        let valid = self
            .scopes()
            .get(&handle)
            .map(|info| info.alive)
            .unwrap_or(false);
        if !valid {
            return false;
        }
        self.rollback_scope(handle);
        true
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

/// 每个插件一份的宿主上下文（`host_ctx` 指向它，而不是全局运行时）。
///
/// 为什么需要它：日志来源原来只靠**线程局部**的作用域栈；插件在自己起的线程、
/// 或阻塞式 `wait_for_update` 循环里调 `host.log` 时栈是空的，只能标成 `plugin`。
/// 把归属烧进 `host_ctx` 之后，插件从**任何线程**调进来都认得出是谁。
#[repr(C)]
pub(crate) struct PluginHost {
    /// 与「运行时指针」区分用的魔数。
    magic: u64,
    /// 真正的运行时（`Arc<HostRuntime>` 内层地址，插件活着期间一直有效）。
    runtime: *const HostRuntime,
    /// 这个插件在宿主侧的归属 id。
    owner: u64,
    /// 交给 `plugin_init` 的 vtable 副本；在 `Box` 里地址固定，`host_ctx` 指回自己。
    vtable: HostApiVTable,
}

impl PluginHost {
    pub(crate) fn vtable_ptr(&self) -> *const HostApiVTable {
        &self.vtable
    }
}

/// 一个不可能与堆指针撞上的魔数（内存里是 `FUYUPLUG`）。
const PLUGIN_HOST_MAGIC: u64 = 0x4755_4C50_5559_5546;

/// 把 `host_ctx` 认成插件上下文；不是（老调用 / 测试用的全局 vtable）返回 `None`。
///
/// # Safety
/// `host` 必须来自 [`HostRuntime::vtable_ptr`] 或 [`HostRuntime::plugin_host`]，
/// 且宿主仍然活着（读前 8 字节判魔数，两边的内存都由宿主持有）。
unsafe fn plugin_host<'a>(host: *mut c_void) -> Option<&'a PluginHost> {
    if host.is_null() {
        return None;
    }
    // SAFETY: 由调用方保证；两个来源的首地址都是对齐且已初始化的。
    let candidate = unsafe { &*(host as *const PluginHost) };
    (candidate.magic == PLUGIN_HOST_MAGIC).then_some(candidate)
}

/// # Safety
/// `host` 必须来自 [`HostRuntime::vtable_ptr`] / [`HostRuntime::plugin_host`]，
/// 且运行时仍然活着。
unsafe fn runtime<'a>(host: *mut c_void) -> Option<&'a HostRuntime> {
    if host.is_null() {
        return None;
    }
    if let Some(ctx) = unsafe { plugin_host(host) } {
        // SAFETY: `PluginHost` 里的运行时指针由宿主写死，插件活着期间有效。
        return Some(unsafe { &*ctx.runtime });
    }
    // SAFETY: 由调用方保证（全局 vtable 的 `host_ctx` 就是运行时地址）。
    Some(unsafe { &*(host as *const HostRuntime) })
}

/// 把 `host_ctx` 换成一份**自己持有**的 [`Arc`]，好交给跨线程的工作函数。
///
/// 存在的理由：simplify 的操作要阻塞等 TDLib 应答，不能在调用它的那个线程上跑
/// （那通常是投递更新的线程，等下去就是死锁）。工作线程得自己持有一份强引用，
/// 否则宿主可能先被释放。
///
/// # Safety
/// 同 [`runtime`]，且此刻强计数必须 > 0（调用来自插件，插件自己就持有一份）。
unsafe fn runtime_arc(host: *mut c_void) -> Option<Arc<HostRuntime>> {
    if host.is_null() {
        return None;
    }
    // 插件上下文里包着真正的运行时地址；全局 vtable 的 `host_ctx` 本身就是它。
    let ptr = match unsafe { plugin_host(host) } {
        Some(ctx) => ctx.runtime,
        None => host as *const HostRuntime,
    };
    if ptr.is_null() {
        return None;
    }
    // SAFETY: `ptr` 是 `HostRuntime::new` 里 `Arc::new_cyclic` 给的地址
    // （`weak.as_ptr()`），也就是 Arc 内层的地址，所以按 Arc 加计数是合法的。
    unsafe { Arc::increment_strong_count(ptr) };
    // SAFETY: 同上；刚刚加过一次计数，这份 Arc 由本函数返回给调用方持有。
    Some(unsafe { Arc::from_raw(ptr) })
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
    static HOST_STRING: std::cell::RefCell<Arc<CString>> =
        std::cell::RefCell::new(Arc::new(CString::default()));
}

fn host_string(text: &str) -> *const c_char {
    HOST_STRING.with(|slot| {
        *slot.borrow_mut() = Arc::new(CString::new(text).unwrap_or_default());
        slot.borrow().as_ptr()
    })
}

/// 把**已经分配好**的字符串放进线程局部缓冲（不拷贝）。
///
/// 缓存命中时走这条（见 [`HostRuntime::tools_json`]）：缓冲里存的是 `Arc`，
/// 所以缓存被替换之后，插件手里那个旧指针**依然有效**——不会悬垂。
fn host_shared_string(text: Arc<CString>) -> *const c_char {
    HOST_STRING.with(|slot| {
        *slot.borrow_mut() = text;
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

/// 按点路径在 JSON 对象里查一个值（只下钻对象，路径里有数组/非对象就返回 `None`）。
fn lookup_path<'a>(node: &'a Value, path: &str) -> Option<&'a Value> {
    let mut current = node;
    for part in path.split('.') {
        if part.is_empty() {
            continue;
        }
        current = current.get(part)?;
    }
    Some(current)
}

/// 按点路径在 JSON 对象里写一个值（自动创建沿途对象）。`path` 为空由调用方处理。
fn set_path(node: &mut Value, path: &str, value: Value) -> Result<(), String> {
    let parts: Vec<&str> = path.split('.').filter(|p| !p.is_empty()).collect();
    if parts.is_empty() {
        return Err("写配置的 key 是空的".to_owned());
    }
    let mut current = node;
    for (index, part) in parts.iter().enumerate() {
        let is_last = index == parts.len() - 1;
        if is_last {
            current[part] = value;
            return Ok(());
        }
        // 中间层必须是对象（或不存在，那就建一个）。
        current = current
            .as_object_mut()
            .ok_or_else(|| format!("路径里的 {part} 不是对象，无法继续下钻"))?
            .entry((*part).to_owned())
            .or_insert_with(|| json!({}));
    }
    Ok(())
}

/// 按点路径下钻到一个**叶子对象**（沿途对象不存在就创建，已存在但非对象则报错）。
/// 用于把插件名（如 `fuyubot.manage`）在 `plugin` 表里定位到它自己的那段。
fn ensure_leaf<'a>(node: &'a mut Value, path: &str) -> Result<&'a mut Value, String> {
    let parts: Vec<&str> = path.split('.').filter(|p| !p.is_empty()).collect();
    if parts.is_empty() {
        return Err("插件名是空的".to_owned());
    }
    let mut current = node;
    for part in parts {
        current = current
            .as_object_mut()
            .ok_or_else(|| format!("路径里的 {part} 不是对象，无法下钻"))?
            .entry(part.to_owned())
            .or_insert_with(|| json!({}));
    }
    Ok(current)
}

// ── 插件配置：三层合并 + 段级落盘 ──────────────────────────────────────
//
// 读：包默认（`plugin.json` 的 `config`）< 运行时落盘（`plugin-config/<名>.toml`）
//     < 用户配置（`fuyubot.toml` 的 `[plugin.<名>]`），后者只填前者的缺失键。
// 写：只写 `plugin-config/<名>.toml` 这一个文件（原子替换），`fuyubot.toml` 永不重写。

/// 把 `<dir>/*.toml` 逐个并进 `root.plugin.<名>`，**只填缺失键**（用户配置优先）。
///
/// 目录不存在 = 还没人写过配置，正常；坏文件只告警、跳过（一个插件写坏了不该拖垮启动）。
fn merge_store_into(root: &mut Value, dir: &Path, warnings: &mut Vec<String>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        Err(error) => {
            warnings.push(format!("读不了插件配置目录 {}：{error}", dir.display()));
            return;
        }
    };
    let Some(root) = root.as_object_mut() else {
        warnings.push("配置根不是对象，跳过插件运行时配置".to_owned());
        return;
    };
    let plugin = root
        .entry(CONFIG_PLUGIN_KEY.to_owned())
        .or_insert_with(|| json!({}));
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("toml") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        let Some(name) = decode_plugin_file_name(stem) else {
            warnings.push(format!(
                "跳过插件配置 {}：文件名不是合法的插件名编码",
                path.display()
            ));
            continue;
        };
        let value = match read_plugin_section(&path) {
            Ok(value) => value,
            Err(error) => {
                warnings.push(error);
                continue;
            }
        };
        match ensure_leaf(plugin, &name) {
            Ok(own) => merge_missing(own, &value),
            Err(error) => warnings.push(format!("插件配置 {} 用不上：{error}", path.display())),
        }
    }
}

/// 读一个插件配置文件：内容就是**那一段配置本身**（不带 `[plugin.<名>]` 头）。
fn read_plugin_section(path: &Path) -> Result<Value, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("读不了插件配置 {}：{error}", path.display()))?;
    let value = toml::from_str::<toml::Value>(&text)
        .map_err(|error| format!("插件配置 {} 不是合法 TOML：{error}", path.display()))?;
    let table = value
        .as_table()
        .ok_or_else(|| format!("插件配置 {} 的顶层不是表", path.display()))?;
    serde_json::to_value(table)
        .map_err(|error| format!("插件配置 {} 转不成 JSON：{error}", path.display()))
}

/// 把一次写操作落到**这个插件自己的文件**上，原子替换。
///
/// 只应用这次操作（`key` 空 = 整段替换），不是把合成后的整段写回去——包默认与用户配置
/// 不该被「冻结」进插件自己的记忆里，否则插件升级后新默认值就再也进不来了。
fn persist_plugin_section(dir: &Path, name: &str, key: &str, value: &Value) -> Result<(), String> {
    let path = dir.join(format!("{}.toml", encode_plugin_file_name(name)));
    let mut section = if path.is_file() {
        read_plugin_section(&path)?
    } else {
        json!({})
    };
    if key.is_empty() {
        section = value.clone();
    } else {
        set_path(&mut section, key, value.clone())?;
    }
    if !section.is_object() {
        return Err(format!(
            "{} 的顶层不是对象，没法写成一段配置",
            path.display()
        ));
    }
    let rendered =
        toml::Value::try_from(section).map_err(|error| format!("插件配置转不了 TOML：{error}"))?;
    let text = toml::to_string_pretty(&rendered)
        .map_err(|error| format!("插件配置序列化失败：{error}"))?;
    std::fs::create_dir_all(dir)
        .map_err(|error| format!("建不了插件配置目录 {}：{error}", dir.display()))?;
    write_atomic(&path, &text)
}

/// 先写同目录的临时文件再 `rename`：掉电 / 写一半时不会留下半截配置。
///
/// 同目录是必须的（跨卷 `rename` 不原子）；写入由配置锁串行化，所以固定临时名就够。
fn write_atomic(path: &Path, text: &str) -> Result<(), String> {
    let temporary = path.with_extension("toml.tmp");
    std::fs::write(&temporary, text)
        .map_err(|error| format!("写不了 {}：{error}", temporary.display()))?;
    if let Err(error) = std::fs::rename(&temporary, path) {
        let _ = std::fs::remove_file(&temporary);
        return Err(format!("替换 {} 失败：{error}", path.display()));
    }
    Ok(())
}

/// 深合并：`src` 只补 `dst` 里**缺席**（或为 null）的键，已有的值一律不动。
fn merge_missing(dst: &mut Value, src: &Value) {
    match (dst, src) {
        (Value::Object(dst), Value::Object(src)) => {
            for (key, value) in src {
                match dst.get_mut(key) {
                    Some(slot) => merge_missing(slot, value),
                    None => {
                        dst.insert(key.clone(), value.clone());
                    }
                }
            }
        }
        (dst, src) if dst.is_null() => *dst = src.clone(),
        _ => {}
    }
}

/// 这个键是不是用户钉住的（`fuyubot.toml` 的 `[plugin.<名>]` 里写了它）。
///
/// `key` 空 = 整段：用户段里有任何键就算钉住（否则「覆盖整段」会把用户写的键一起抹掉）。
fn user_pins(user_plugin: &Value, name: &str, key: &str) -> bool {
    let Some(section) = lookup_path(user_plugin, name) else {
        return false;
    };
    if key.is_empty() {
        return section.as_object().is_some_and(|table| !table.is_empty());
    }
    lookup_path(section, key).is_some()
}

/// 写被用户钉住的键时的错误文案：要能告诉用户「改哪个文件、怎么放行」。
fn user_pin_message(name: &str, key: &str) -> String {
    if key.is_empty() {
        return format!(
            "[plugin.{name}] 整段在 fuyubot.toml 里由用户固定，插件不能覆盖它（改用户配置，或删掉整段）"
        );
    }
    format!(
        "配置项 {key:?} 在 fuyubot.toml 的 [plugin.{name}] 里由用户固定，插件不能覆盖它（改用户配置，或删掉该键）"
    )
}

/// 插件名 → 配置文件名（不含扩展名）：只保留 `[A-Za-z0-9._-]`，其余字节按 `%XX` 编码。
///
/// 插件名来自不受控的 `plugin_init`，可能带 `/`、`..`、非 ASCII——直接拼路径就是一个
/// 目录穿越 / 建不出文件的坑。Windows 保留名（`CON` / `NUL` / `COM1` …）把首字节编码掉。
fn encode_plugin_file_name(name: &str) -> String {
    let mut encoded = String::with_capacity(name.len());
    for &byte in name.as_bytes() {
        let safe = byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-');
        if safe {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    if is_reserved_windows_name(&encoded) {
        let mut bytes = encoded.into_bytes();
        let first = bytes.remove(0);
        encoded = format!("%{first:02X}{}", String::from_utf8_lossy(&bytes));
    }
    encoded
}

/// [`encode_plugin_file_name`] 的逆运算。不合法（空、空段、坏 UTF-8、坏 `%`）返回 `None`。
fn decode_plugin_file_name(stem: &str) -> Option<String> {
    let bytes = stem.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = stem.get(index + 1..index + 3)?;
            decoded.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    let name = String::from_utf8(decoded).ok()?;
    if name.is_empty() {
        return None;
    }
    Some(name)
}

/// `CON` / `PRN` / `AUX` / `NUL` / `COM1-9` / `LPT1-9`（Windows 上带扩展名也建不出来）。
fn is_reserved_windows_name(name: &str) -> bool {
    const FIXED: [&str; 4] = ["CON", "PRN", "AUX", "NUL"];
    let upper = name.to_ascii_uppercase();
    if FIXED.contains(&upper.as_str()) {
        return true;
    }
    let numbered = upper
        .strip_prefix("COM")
        .or_else(|| upper.strip_prefix("LPT"));
    matches!(numbered, Some(rest) if rest.len() == 1 && matches!(rest.as_bytes()[0], b'1'..=b'9'))
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
    /// 注册它的子作用域（回调里再注册的东西仍算这个 scope）。
    scope: u64,
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
        let _scope = rt.enter_scope(self.owner, self.scope);

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

// ── 内置 scope service 的实现 ───────────────────────────────────────
//
// 四个方法以 `ServiceMethodFn` 的形态暴露（`context` = 宿主运行时指针），
// JSON 协议：`scope_create` 入 `{}` 出 `{"handle":N}`；其余入 `{"handle":N}`，
// 成功出 `null`，失败出 `{"error":"..."}`。

unsafe extern "C" fn api_scope_create(
    context: *mut c_void,
    _input: *const c_char,
) -> *const c_char {
    // SAFETY: context 由本运行时在构造 scope_vtable 时写入。
    let Some(rt) = (unsafe { runtime(context) }) else {
        return core::ptr::null();
    };
    let handle = rt.scope_create();
    host_string(&json!({ "handle": handle }).to_string())
}

unsafe extern "C" fn api_scope_enter(context: *mut c_void, input: *const c_char) -> *const c_char {
    let Some(rt) = (unsafe { runtime(context) }) else {
        return core::ptr::null();
    };
    let Some(handle) = parse_json(input).and_then(|v| v.get("handle").and_then(Value::as_u64))
    else {
        return host_string(&json!({ "error": "scope_enter 需要 {\"handle\":N}" }).to_string());
    };
    if rt.scope_enter(handle) {
        core::ptr::null()
    } else {
        host_string(&json!({ "error": "无效或已失效的作用域" }).to_string())
    }
}

unsafe extern "C" fn api_scope_leave(context: *mut c_void, _input: *const c_char) -> *const c_char {
    let Some(rt) = (unsafe { runtime(context) }) else {
        return core::ptr::null();
    };
    rt.scope_leave();
    core::ptr::null()
}

unsafe extern "C" fn api_scope_drop(context: *mut c_void, input: *const c_char) -> *const c_char {
    let Some(rt) = (unsafe { runtime(context) }) else {
        return core::ptr::null();
    };
    let Some(handle) = parse_json(input).and_then(|v| v.get("handle").and_then(Value::as_u64))
    else {
        return host_string(&json!({ "error": "scope_drop 需要 {\"handle\":N}" }).to_string());
    };
    if rt.scope_drop(handle) {
        core::ptr::null()
    } else {
        host_string(&json!({ "error": "无效或已失效的作用域" }).to_string())
    }
}

/// scope service 的方法表。
///
/// 包一层 newtype 再 `unsafe impl Sync`：`ServiceMethod` 里的 `name` 是裸指针，
/// 编译器不认为它 `Sync`；但这里四个 `name` 都指向 `'static` 的字符串字面量，
/// 安全跨线程共享，`func` 是纯函数指针，同样安全。
struct ScopeMethods([ServiceMethod; 4]);

// SAFETY: 见 `ScopeMethods` 文档。
unsafe impl Sync for ScopeMethods {}

static SCOPE_METHODS: ScopeMethods = ScopeMethods([
    ServiceMethod {
        name: c"scope_create".as_ptr(),
        func: Some(api_scope_create),
    },
    ServiceMethod {
        name: c"scope_enter".as_ptr(),
        func: Some(api_scope_enter),
    },
    ServiceMethod {
        name: c"scope_leave".as_ptr(),
        func: Some(api_scope_leave),
    },
    ServiceMethod {
        name: c"scope_drop".as_ptr(),
        func: Some(api_scope_drop),
    },
]);

/// 内置「插件清单」service 的名字。插件用 ABI 的 `get_service` 查它。
pub const PLUGIN_REPORT_SERVICE: &str = "fuyubot.host.plugins";

/// 同一个名字的 `CString` 版（`ServiceVTable.name` 要 NUL 结尾的指针）。
const PLUGIN_REPORT_SERVICE_C: &core::ffi::CStr = c"fuyubot.host.plugins";

/// 「插件清单」service 的方法表。
///
/// 现在只有一个方法 `report`：无参，返回宿主最近一次快照的 JSON。
/// 加方法只要在这张表里追加一项——插件侧本来就是按名字调的。
struct PluginMethods([ServiceMethod; 1]);

// SAFETY: 与 `ScopeMethods` 同理：`name` 指向 `'static` 字面量，`func` 是纯函数指针。
unsafe impl Sync for PluginMethods {}

static PLUGIN_METHODS: PluginMethods = PluginMethods([ServiceMethod {
    name: c"report".as_ptr(),
    func: Some(api_plugin_report),
}]);

/// `report` 方法的实现：无参，返回快照 JSON。
///
/// 用 `host_string`（线程局部缓冲）而不是自己分配：契约是「提供方持有返回值、
/// 调用方读完即弃」，和 `api_list_tools` 一个套路。
unsafe extern "C" fn api_plugin_report(
    context: *mut c_void,
    _input: *const c_char,
) -> *const c_char {
    // SAFETY: ABI 契约保证 context 来自本运行时。
    let Some(rt) = (unsafe { runtime(context) }) else {
        return core::ptr::null();
    };
    host_string(&rt.plugin_report())
}

/// 内置「我是谁」service 的名字。插件用 ABI 的 `get_service` 查它；
/// `fuyubot-simplify` 的 `on_command` 正是这么判断「命令点名的是不是我」的。
pub const IDENTITY_SERVICE: &str = "fuyubot.host.identity";

/// 同一个名字的 `CString` 版（`ServiceVTable.name` 要 NUL 结尾的指针）。
const IDENTITY_SERVICE_C: &core::ffi::CStr = c"fuyubot.host.identity";

/// 「我是谁」service 的方法表。
///
/// - `usernames`：无参，返回自身全部活跃用户名的 JSON 数组（没登录时 `[]`）；
/// - `me`：无参，返回 `getMe` 的原始 `user` JSON（没登录时 `null`）。
///
/// 加方法只要在这张表里追加一项——插件侧本来就是按名字调的。
struct IdentityMethods([ServiceMethod; 2]);

// SAFETY: 与 `ScopeMethods` 同理：`name` 指向 `'static` 字面量，`func` 是纯函数指针。
unsafe impl Sync for IdentityMethods {}

static IDENTITY_METHODS: IdentityMethods = IdentityMethods([
    ServiceMethod {
        name: c"usernames".as_ptr(),
        func: Some(api_identity_usernames),
    },
    ServiceMethod {
        name: c"me".as_ptr(),
        func: Some(api_identity_me),
    },
]);

/// `usernames`：自身全部活跃用户名的 JSON 数组。
unsafe extern "C" fn api_identity_usernames(
    context: *mut c_void,
    _input: *const c_char,
) -> *const c_char {
    // SAFETY: ABI 契约保证 context 来自本运行时。
    let Some(rt) = (unsafe { runtime(context) }) else {
        return core::ptr::null();
    };
    host_string(&json!(rt.self_usernames()).to_string())
}

/// `me`：`getMe` 的原始 JSON（没登录时 `null`）。
unsafe extern "C" fn api_identity_me(context: *mut c_void, _input: *const c_char) -> *const c_char {
    // SAFETY: ABI 契约保证 context 来自本运行时。
    let Some(rt) = (unsafe { runtime(context) }) else {
        return core::ptr::null();
    };
    let me = rt
        .self_user
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    host_string(&me.clone().unwrap_or(Value::Null).to_string())
}

// ── VTable 实现 ────────────────────────────────────────────────────

unsafe extern "C" fn api_log(host: *mut c_void, level: u32, message: *const c_char) {
    // SAFETY: ABI 契约保证 message 合法。
    let Some(text) = (unsafe { cstr(message) }) else {
        return;
    };
    let level = LogLevel::from_u32(level);
    // SAFETY: ABI 契约保证 host 来自本运行时。
    let Some(rt) = (unsafe { runtime(host) }) else {
        return;
    };
    // 门槛过滤：低于门槛的既不入环形缓冲也不写 stderr（省掉格式化与系统调用）。
    if level as u32 > rt.log_verbosity() {
        return;
    }
    // 来源：优先 `host_ctx` 里烧好的归属——插件自己起的线程、阻塞式 `wait_for_update`
    // 循环、以及「A 调 B 的 service」这几种情况都没有/只有调用方的作用域栈，
    // 真正说话的是持有这份 vtable 的插件。没有插件上下文时（测试用的全局 vtable）
    // 才退回线程作用域栈。
    let owner = match unsafe { plugin_host(host) } {
        Some(ctx) => ctx.owner,
        None => rt.current_owner(),
    };
    // 名字还没登记（理论上只会在 init 极早期）时标成 `plugin`，不冒充宿主。
    let plugin = rt.owner_name(owner);
    if let Some(name) = plugin.as_deref() {
        if !rt.plugin_log_enabled(name) {
            return;
        }
    }
    let source = plugin.as_deref().unwrap_or("plugin");
    {
        let mut lines = rt.log_lines.lock().unwrap_or_else(|e| e.into_inner());
        if lines.len() >= LOG_CAPACITY {
            lines.pop_front();
        }
        // 环形缓冲也带来源：`--replay` 打出来的是「谁说的」，而不只是「说了什么」。
        lines.push_back(format!("{source} {level:?} {text}"));
    }
    // 动态级别没法走 `event!(level, …)`（callsite 要求常量级别），所以逐级 match；
    // 每个分支各有一个静态 callsite，开销与直接写 `info!` 一样。
    match level {
        LogLevel::Error => tracing::error!(target: "fuyubot::plugin", plugin = %source, "{text}"),
        LogLevel::Warn => tracing::warn!(target: "fuyubot::plugin", plugin = %source, "{text}"),
        LogLevel::Info => tracing::info!(target: "fuyubot::plugin", plugin = %source, "{text}"),
        LogLevel::Debug => tracing::debug!(target: "fuyubot::plugin", plugin = %source, "{text}"),
        LogLevel::Trace => tracing::trace!(target: "fuyubot::plugin", plugin = %source, "{text}"),
    }
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
    // 注册时的归属烧进闭包：插件在自己回调里再注册的东西仍然算它的（同一个子作用域）。
    let owner = rt.current_owner();
    let scope = rt.current_scope();
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
            let _scope = rt.enter_scope(owner, scope);

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
    handler: Option<ToolHandlerFn>,
    user_data: *mut c_void,
) -> ToolHandle {
    // SAFETY: ABI 契约保证 host 来自本运行时。
    let Some(rt) = (unsafe { runtime(host) }) else {
        return ToolHandle::INVALID;
    };
    // SAFETY: 同上。
    let Some(text) = (unsafe { cstr(descriptor_json) }) else {
        return ToolHandle::INVALID;
    };
    let mut descriptor = match descriptor_from_json(text) {
        Ok(descriptor) => descriptor,
        Err(e) => {
            rt.warn(&format!("工具描述不合法: {e}"));
            return ToolHandle::INVALID;
        }
    };
    // ABI 传来的处理器 + user_data 直接塞进描述（签名已对齐）。
    descriptor.handler = handler;
    descriptor.user_data = user_data;
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

/// 同步工具调用：直接返回结果 JSON（失败是 `{"error": …}`）。
///
/// 与 `api_call_tool` 的区别：那个只回请求号，**没有结果回投通道**；这个当场执行并把
/// `ToolRegistry::call_tool` 的 `output` 交给调用方。这样脚本桥 / 原生插件都能真的拿到
/// 工具结果，而不是一个永远等不到事件的 id。
unsafe extern "C" fn api_call_tool_result(
    host: *mut c_void,
    name: *const c_char,
    input_json: *const c_char,
) -> *const c_char {
    // SAFETY: ABI 契约保证 host 来自本运行时。
    let Some(rt) = (unsafe { runtime(host) }) else {
        return core::ptr::null();
    };
    // SAFETY: 同上。
    let Some(name) = (unsafe { cstr(name) }) else {
        return core::ptr::null();
    };
    let input = parse_json(input_json).unwrap_or(Value::Null);
    match rt.tools.call_tool(name, &input) {
        Ok(call) => host_string(&call.output.to_string()),
        Err(error) => {
            rt.warn(&format!("调用工具 {name} 失败: {error}"));
            host_string(&json!({ "error": error.to_string() }).to_string())
        }
    }
}

unsafe extern "C" fn api_list_tools(host: *mut c_void) -> *const c_char {
    // SAFETY: ABI 契约保证 host 来自本运行时。
    let Some(rt) = (unsafe { runtime(host) }) else {
        return core::ptr::null();
    };
    host_shared_string(rt.tools_json())
}

unsafe extern "C" fn api_list_plugins(host: *mut c_void) -> *const c_char {
    // SAFETY: ABI 契约保证 host 来自本运行时。
    let Some(rt) = (unsafe { runtime(host) }) else {
        return core::ptr::null();
    };
    host_string(&rt.plugin_report())
}

/// 当前调用者是谁：宿主自己回 NULL，插件回它的名字（线程局部缓冲，读完即弃）。
///
/// 身份优先级：
/// 1. **调用者栈顶**——宿主经 `api_call_service` 中转服务调用时压的那一层。这是
///    被调方真正想要的那个答案，而且插件在自己的线程上调进来也成立；
/// 2. 否则退回本线程作用域栈顶（[`HostRuntime::current_owner`]）——宿主把控制权交给
///    某个插件（`start` / 回调 / 中间件 / `stop`）时的情形。
///
/// 两个来源最终都查 `owner_names` —— 与 `set_config` 的越权判定是同一份映射，
/// 插件伪造不了（它既写不了调用者栈，也拿不到别人的 `host_ctx`）。
unsafe extern "C" fn api_current_plugin(host: *mut c_void) -> *const c_char {
    // SAFETY: ABI 契约保证 host 来自本运行时。
    let Some(rt) = (unsafe { runtime(host) }) else {
        return core::ptr::null();
    };
    let owner = rt.current_caller().unwrap_or_else(|| rt.current_owner());
    match rt.owner_name(owner) {
        Some(name) => host_string(&name),
        // 宿主自己（owner = 0）或名字还没登记：NULL = 「不是插件」。
        None => core::ptr::null(),
    }
}

/// 注册到**进程级**：所有插件可见。与 [`api_provide_service`] 的唯一区别就是落点。
unsafe extern "C" fn api_provide_service_shared(
    host: *mut c_void,
    name: *const c_char,
    vtable: *const ServiceVTable,
) -> ServiceHandle {
    // SAFETY: ABI 契约保证 host 来自本运行时。
    let Some(rt) = (unsafe { runtime(host) }) else {
        return ServiceHandle::INVALID;
    };
    if vtable.is_null() {
        rt.warn("provide_service_shared 收到了空 vtable");
        return ServiceHandle::INVALID;
    }
    // SAFETY: 同上。
    let Some(name) = (unsafe { cstr(name) }) else {
        return ServiceHandle::INVALID;
    };
    // 故意写根：这就是「共享」的全部含义。同名会和别人撞，所以调用方得是真的想共享。
    match rt.ctx.provide_service(
        name,
        AbiService {
            vtable,
            owner: rt.current_owner(),
        },
    ) {
        Ok(handle) => ServiceHandle {
            id: rt.remember(RegistrationKind::Service(handle)),
        },
        Err(e) => {
            rt.warn(&format!("注册共享服务 {name} 失败: {e}"));
            ServiceHandle::INVALID
        }
    }
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
    match rt.caller_context().provide_service(
        name,
        AbiService {
            vtable,
            owner: rt.current_owner(),
        },
    ) {
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
    match rt.caller_context().resolve::<AbiService>(name) {
        Ok(service) => service.vtable,
        Err(_) => core::ptr::null(),
    }
}

impl HostRuntime {
    /// 按名字调一个**插件提供的** service 的方法（宿主侧的 Rust 入口）。
    ///
    /// 存在的理由：`AbiService` 是私有类型，而宿主里的 Rust 代码想查插件托管的
    /// 清单时绕不开它——`fuyubot plugin list` 要把两个中间插件管着的脚本摊开打印，
    /// 就是走这里（约定：名字以 `fuyubot.host.scripts` 开头的 service 都有一个
    /// `list` 方法，返回 `{"lang":…,"scripts":[…]}`）。
    ///
    /// 返回的 JSON 是**当场拷走**的：ABI 要求提供方持有返回字符串、调用方读完即弃，
    /// 所以这里不等调用方看完就已经复制成 `String`。
    ///
    /// 出错（服务不存在 / 没这个方法 / 入参带 NUL）返回 `Err`，不 panic——
    /// 插件给的 service 表理论上什么都有。
    pub fn call_service(
        &self,
        service: &str,
        method: &str,
        input_json: &str,
    ) -> Result<String, String> {
        // 宿主自己调：从**根**查（它没有「自己的作用域」，也看不到插件的私有服务）。
        self.invoke_service(&self.ctx, None, service, method, input_json)
    }

    /// 服务调用的共用实现：`scope` 决定从哪张表查，`caller` 决定调用期间「谁在调我」。
    ///
    /// - 宿主自己调 → `scope = &self.ctx`、`caller = None`；
    /// - 插件经 vtable 调 → `scope = caller_context()`（调用方自己的作用域链）、
    ///   `caller = Some(owner)`，被调方因此能问出**真实调用方**（ABI v6）。
    fn invoke_service(
        &self,
        scope: &Context,
        caller: Option<u64>,
        service: &str,
        method: &str,
        input_json: &str,
    ) -> Result<String, String> {
        // 查到的 Arc 要一直持有到调用结束：它是对应表项的唯一强引用，
        // 中间被注销就会让下面的裸指针悬空。
        let Ok(entry) = scope.resolve::<AbiService>(service) else {
            return Err(format!("没有名为 {service} 的 service"));
        };
        // SAFETY: 表项活着（上面攥着 Arc），指针来自 provide_service 时校验过的 vtable。
        let vtable = unsafe { &*entry.vtable };
        // 提供方的 owner：调用期间要拿它当「执行者」。
        let provider = entry.owner;
        let input = CString::new(input_json).map_err(|_| "入参含 NUL".to_owned())?;
        for index in 0..vtable.method_count {
            // SAFETY: method_count 由注册方保证，methods 指向同等数量的连续项。
            let entry = unsafe { &*vtable.methods.add(index) };
            if entry.name.is_null() {
                continue;
            }
            // SAFETY: 方法名是 NUL 结尾 UTF-8。
            if unsafe { CStr::from_ptr(entry.name) }.to_string_lossy() != method {
                continue;
            }
            let Some(func) = entry.func else {
                return Err(format!("service {service} 的方法 {method} 没有实现"));
            };
            // 调用期间把「谁在调」压在栈顶：被调方的 `current_plugin` 读它。
            // 守卫在这里建、在函数返回（含提前 return 与 panic）时弹，不会漏层。
            let _caller = caller.and_then(|owner| self.enter_caller(owner));
            // 同时把「这段代码算谁的」切成提供方：服务方法里 `set_config` 要写自己的段。
            let _executor = self.enter_executor(provider);
            // SAFETY: 契约要求返回字符串由提供方持有、本线程下一次调用前有效——
            // 这里立刻拷走，不把裸指针交出去。
            let raw = unsafe { func(vtable.context, input.as_ptr()) };
            if raw.is_null() {
                // 与 scope service 的约定一致：null = 成功但没有输出。
                return Ok(String::new());
            }
            // SAFETY: raw 是提供方的 NUL 结尾字符串。
            return Ok(unsafe { CStr::from_ptr(raw) }
                .to_string_lossy()
                .into_owned());
        }
        Err(format!("service {service} 没有方法 {method}"))
    }
}

/// **经过宿主**调服务（ABI v6）：插件侧的 `call_service` 落到这里。
///
/// 与「插件自己取 vtable 再直调」的区别只有一处，但正是缺的那一处：**宿主在场**，
/// 于是它能用 `host_ctx`（这份 vtable 属于哪个插件，见 [`plugin_host`]）记下调用者，
/// 压进调用者栈。插件在自己的线程上调进来也认得出——那条路径上作用域栈是空的。
///
/// 返回：NULL = 失败（原因只进日志，ABI 不传细节）；空串 = 成功但对方没输出。
unsafe extern "C" fn api_call_service(
    host: *mut c_void,
    service: *const c_char,
    method: *const c_char,
    input_json: *const c_char,
) -> *const c_char {
    // SAFETY: ABI 契约保证 host 来自本运行时。
    let Some(rt) = (unsafe { runtime(host) }) else {
        return core::ptr::null();
    };
    // SAFETY: 三个字符串的参数契约同 `get_service` / `td_invoke`。
    let (Some(service), Some(method)) = (unsafe { cstr(service) }, unsafe { cstr(method) }) else {
        return core::ptr::null();
    };
    let input = unsafe { cstr(input_json) }.unwrap_or("");
    // 调用者 = 这份 vtable 的归属。插件可能在自己的线程上调进来（作用域栈是空的），
    // 那时只有 `host_ctx` 知道它是谁；只有测试用的全局 vtable 才退回作用域栈。
    let caller = match unsafe { plugin_host(host) } {
        Some(ctx) => ctx.owner,
        None => rt.current_owner(),
    };
    // 查服务要用**调用方**的作用域链：插件的私有服务也对它自己可见。
    match rt.invoke_service(&rt.caller_context(), Some(caller), service, method, input) {
        // 注意 `host_string("")` 返回的是**非空**指针：空串 = 成功但没有输出。
        Ok(text) => {
            rt.set_service_error(None);
            host_string(&text)
        }
        Err(error) => {
            rt.warn(&format!("call_service({service}.{method}) 失败: {error}"));
            // 失败原因留给 `last_error`：插件侧因此能报出「哪一步不对」，而不是一句笼统的
            // 「调用失败」。
            rt.set_service_error(Some(format!("{service}.{method}: {error}")));
            core::ptr::null()
        }
    }
}

/// 上一次被拒绝的调用为什么失败（本线程、宿主线程局部缓冲，读完即弃）。
unsafe extern "C" fn api_last_error(host: *mut c_void) -> *const c_char {
    // SAFETY: ABI 契约保证 host 来自本运行时。
    let Some(rt) = (unsafe { runtime(host) }) else {
        return core::ptr::null();
    };
    match rt.last_service_error() {
        Some(message) => host_string(&message),
        None => core::ptr::null(),
    }
}

/// 当前插件看得见的服务清单（JSON 数组；含自己的私有服务与整条父链）。
///
/// 只报**名字 + 方法名 + 版本 + 提供方**，不报 vtable 指针：想调还是走 `call_service`。
unsafe extern "C" fn api_list_services(host: *mut c_void) -> *const c_char {
    // SAFETY: ABI 契约保证 host 来自本运行时。
    let Some(rt) = (unsafe { runtime(host) }) else {
        return core::ptr::null();
    };
    host_string(&rt.service_catalog().to_string())
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

/// **跨语言**的 simplify 操作（ABI v5）：起一个工作线程跑它，结果走 `"response"` 通道回给发起插件。
///
/// 不在调用线程上跑的理由：op 内部要阻塞等 TDLib 应答（等响应、等发送结局），
/// 而调用方往往正站在**投递更新**的那个线程上——在那儿等就是自己等自己。
///
/// 工作线程的归属是**宿主自己**（`owner = 0`），所以它顺手发出的那些 TDLib 请求
/// 的中间响应只会到全局订阅者，不会漏给插件；插件最终只收到一条
/// `{"@type":"simplifyResult",…,"request_id":<本函数返回的号>}`。
unsafe extern "C" fn api_simplify_invoke(host: *mut c_void, op_json: *const c_char) -> RequestId {
    // SAFETY: ABI 契约保证 host 来自本运行时；这里要一份能跨线程的强引用。
    let Some(rt) = (unsafe { runtime_arc(host) }) else {
        return RequestId::INVALID;
    };
    let Some(request) = parse_json(op_json) else {
        rt.warn("simplify_invoke 收到了非法 JSON");
        return RequestId::INVALID;
    };
    let op = request
        .get(SIMPLIFY_OP_KEY)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    if op.is_empty() {
        rt.warn("simplify_invoke 的 op JSON 里没有 op 名");
        return RequestId::INVALID;
    }
    if ops::is_sync_op(&op) {
        rt.warn(&format!(
            "simplify op `{op}` 是同步操作，回调里该走 simplify_invoke_result（照跑，只是白占一个线程）"
        ));
    }

    let owner = rt.current_owner();
    let id = rt.next_simplify_id();
    rt.record_simplify(id, owner);

    let busy = rt.simplify_workers.fetch_add(1, Ordering::Relaxed) + 1;
    if busy >= SIMPLIFY_BUSY_WORKERS as u64 {
        rt.warn(&format!(
            "同时跑着的 simplify 操作有 {busy} 个，检查一下插件是不是在刷"
        ));
    }
    // spawn 的闭包拿走 `rt` / `op` / `request` 的所有权；失败的分支里要用 `op`，先留一份。
    let spawn = std::thread::Builder::new()
        .name(format!("fy-simplify-{op}"))
        .spawn({
            let rt = Arc::clone(&rt);
            let op = op.clone();
            let request = request.clone();
            move || rt.run_simplify(id, &op, &request)
        });
    if let Err(e) = spawn {
        rt.warn(&format!("simplify op `{op}` 起不了工作线程: {e}"));
        rt.simplify_workers.fetch_sub(1, Ordering::Relaxed);
        return RequestId::INVALID;
    }
    RequestId { id }
}

/// **同步**的 simplify 操作（ABI v5）：只收不碰网络的 op，当场返回结果 JSON。
///
/// 同步 op 底层是 `td_execute` 或纯本地计算，所以在**更新回调里也安全**——
/// 不像 [`api_simplify_invoke`] 那样会卡住投递线程。
unsafe extern "C" fn api_simplify_invoke_result(
    host: *mut c_void,
    op_json: *const c_char,
) -> *const c_char {
    // SAFETY: ABI 契约保证 host 来自本运行时。
    let Some(rt) = (unsafe { runtime(host) }) else {
        return core::ptr::null();
    };
    let request = parse_json(op_json).unwrap_or(Value::Null);
    let op = request
        .get(SIMPLIFY_OP_KEY)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    // 认不出的 op 也放进去跑：`run` 会给出 `unknown_op`，比在这里猜「它大概是异步的」准确。
    let outcome = if ops::is_sync_op(&op) || !ops::known_ops().contains(&op.as_str()) {
        ops::run(&rt.simplify(), &request)
    } else {
        Err(ops::OpError::new(
            "async_op",
            format!(
                "`{op}` 要等 TDLib 应答，不能在这里同步跑；请用 simplify_invoke 并等 `response` 事件"
            ),
        ))
    };
    host_string(&ops::to_result_json(&op, outcome).to_string())
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
        scope: rt.current_scope(),
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

/// 读配置：`section` 空 = **自己**那段（与 `set_config` 同一个默认），
/// 非空 = 那个插件名（点路径，`fuyubot.manage` → `plugin.fuyubot.manage`）。
///
/// 顶层（`[bot]` / `[telegram]`）**不通过插件 API 暴露**：插件要的永远是插件自己的段，
/// 读宿主凭据既不必要也不安全。宿主内部仍可直接 `read_config("")`。
unsafe extern "C" fn api_get_config(
    host: *mut c_void,
    section: *const c_char,
    key: *const c_char,
) -> *const c_char {
    // SAFETY: ABI 契约保证 host 来自本运行时。
    let Some(rt) = (unsafe { runtime(host) }) else {
        return core::ptr::null();
    };
    // SAFETY: 同上；NULL 当空串（= 自己）。
    let section = unsafe { cstr(section) }.unwrap_or("");
    let key = unsafe { cstr(key) }.unwrap_or("");
    // 空 section = 读自己：名字由宿主判定，插件没法借这个默认值读别人的段。
    let section = if section.is_empty() {
        match rt.executor_name() {
            Some(name) => name,
            // 宿主自己调、或名字还没登记：没有「自己」可指，按「找不到」处理。
            None => return core::ptr::null(),
        }
    } else {
        section.to_owned()
    };
    match rt.read_config(&section, key) {
        Some(value) => host_string(&value.to_string()),
        None => core::ptr::null(),
    }
}

unsafe extern "C" fn api_set_config(
    host: *mut c_void,
    key: *const c_char,
    value_json: *const c_char,
) -> i32 {
    // SAFETY: ABI 契约保证 host 来自本运行时。
    let Some(rt) = (unsafe { runtime(host) }) else {
        return -1;
    };
    // SAFETY: 同上；key 空串 = 覆盖整段。
    let key = unsafe { cstr(key) }.unwrap_or("");
    let value = unsafe { cstr(value_json) }.unwrap_or("null");
    match rt.write_config(None, key, value) {
        Ok(()) => {
            rt.set_service_error(None);
            0
        }
        Err(e) => {
            rt.warn(&format!("写配置被拒: {e}"));
            rt.set_service_error(Some(format!("set_config: {e}")));
            -1
        }
    }
}

/// 写**指定插件**的配置段（ABI v6 内追加）。
///
/// 与 `api_set_config` 的差别只有「目标是谁」：那个由宿主按执行者判定，这个由调用方
/// 显式写出插件名。两条路共用同一套规则（用户钉住的键照样拒绝、只落
/// `plugin-config/<目标>.toml`、`fuyubot.toml` 不重写）。
unsafe extern "C" fn api_set_config_section(
    host: *mut c_void,
    section: *const c_char,
    key: *const c_char,
    value_json: *const c_char,
) -> i32 {
    // SAFETY: ABI 契约保证 host 来自本运行时。
    let Some(rt) = (unsafe { runtime(host) }) else {
        return -1;
    };
    // SAFETY: 同上；section 空串 = 没指定目标（拒绝，不猜）。
    let section = unsafe { cstr(section) }.unwrap_or("");
    let key = unsafe { cstr(key) }.unwrap_or("");
    let value = unsafe { cstr(value_json) }.unwrap_or("null");
    match rt.write_config(Some(section), key, value) {
        Ok(()) => {
            rt.set_service_error(None);
            0
        }
        Err(e) => {
            rt.warn(&format!("跨插件写配置被拒（目标 {section}）: {e}"));
            rt.set_service_error(Some(format!("set_config_section({section}): {e}")));
            -1
        }
    }
}

impl HostRuntime {
    /// 一份对着**宿主自己** vtable 的 [`Simplify`]。
    ///
    /// 宿主跑 simplify op 用的就是它——与 Rust 插件里那个 `Simplify` 是**同一个实现**，
    /// 所以「Rust 的人话方法」和「别的语言的 op JSON」行为完全一致，差别只在入参。
    fn simplify(&self) -> Simplify {
        // SAFETY: vtable 由本实例持有，调用期间一定活着。
        let host = unsafe { HostApi::from_raw(self.vtable_ptr()) }
            .expect("宿主自己的 vtable 一定版本自洽、字段齐全");
        Simplify::new(host)
    }

    /// 在工作线程里跑一个 simplify op，把结果投递回发起它的插件。
    ///
    /// 工作线程**不压作用域栈**，所以 `current_owner()` 是 0：op 内部顺手发出的
    /// TDLib 请求算宿主的，它们的中间响应只到全局订阅者，插件看不见。
    fn run_simplify(self: Arc<Self>, id: u64, op: &str, request: &Value) {
        let outcome = ops::run(&self.simplify(), request);
        let mut payload = ops::to_result_json(op, outcome);
        if let Value::Object(map) = &mut payload {
            map.insert("request_id".to_owned(), json!(id));
        }
        // 归属在 `api_simplify_invoke` 里就登记过了，这里只管投递。
        if let Err(e) = self.deliver_response(&payload) {
            self.warn(&format!("simplify op `{op}` 的结果没投出去: {e}"));
        }
        self.simplify_workers.fetch_sub(1, Ordering::Relaxed);
    }

    fn td_send(&self, request: Value) -> RequestId {
        let method = request
            .get("@type")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_owned();
        // 真 schema 的**生产消费者**（缺口 13①）：方法名在不在 `---functions---` 里、
        // 参数名认不认得，发之前先校一遍。只警告不拦截——schema 只是同一版
        // `td_api.tl` 的编译期快照，最终裁判仍是 TDLib；硬拒绝会让 schema 与真库
        // 不同步时挡住合法请求（用户 2026 拍板：警告照发）。
        if let Some(reason) = schema_rejection(&method, &request) {
            self.warn(&format!(
                "请求 {method} 没过 schema 校验（照发，真库会再判一次）: {reason}"
            ));
        }
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

/// [`HostRuntime::enter_owner`] 的守卫：drop 时弹回作用域栈。
pub(crate) struct OwnerGuard<'a> {
    _runtime: &'a HostRuntime,
}

impl Drop for OwnerGuard<'_> {
    fn drop(&mut self) {
        SCOPE_STACK.with(|stack| {
            stack.borrow_mut().pop();
        });
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
        // **不要**在这里 `end_plugin()`：`self` 紧接着就被 Drop，那边统一收尾。
        // 早先两处都弹一次，栈就被弹穿了（单层加载时 pop 空栈看不出来，
        // 中间插件嵌套加载脚本时会把外层的作用域弹掉）。
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

/// 把内核的报告序列化成 JSON。
///
/// 形状故意**稳定**（插件会按字段名读它，改字段就是改契约）：
///
/// ```json
/// {
///   "count": 2,
///   "plugins": [
///     {"name": "a", "state": "active"},
///     {"name": "b", "state": "pending", "missing": ["database"]}
///   ]
/// }
/// ```
///
/// 只写有意义的字段：`missing` 只在 pending 时出现，`reason` 只在 failed 时出现。
/// 空数组一律写出来（`"missing": []` 会被省掉）——调用方按 `state` 判断即可，
/// 不必猜「字段不在」是什么意思。
fn report_json(report: &Report) -> String {
    let plugins: Vec<Value> = report
        .plugins
        .iter()
        .map(|entry| {
            let mut item = json!({
                "name": entry.name,
                "state": entry.state.as_str(),
            });
            let object = item.as_object_mut().expect("刚造的对象");
            if !entry.missing.is_empty() {
                object.insert("missing".to_owned(), json!(entry.missing));
            }
            if let Some(reason) = &entry.reason {
                object.insert("reason".to_owned(), json!(reason));
            }
            item
        })
        .collect();
    json!({ "count": report.plugins.len(), "plugins": plugins }).to_string()
}

/// 发请求前的本地校验：方法名与参数名是否出现在**编译期烘焙**的 `td_api.tl` 里。
///
/// 返回第一处不认得的名字（给日志用）；`None` = 过了。抽成纯函数是为了能直接测
/// 判据本身——宿主警告只走 stderr，没有可断言的环形缓冲。
fn schema_rejection(method: &str, request: &Value) -> Option<String> {
    TdApiSchema::compiled()
        .check_request(method, request)
        .err()
        .map(|error| error.to_string())
}

/// 内置工具 `fuyubot.td.functions` 的处理器：返回函数表（可按名字子串过滤）。
unsafe extern "C-unwind" fn td_functions_handler(
    _user_data: *mut c_void,
    input_json: *const c_char,
) -> *const c_char {
    // SAFETY: ABI 契约保证入参是 NUL 结尾字符串、活到调用结束。
    let input = unsafe { cstr(input_json) }.unwrap_or("{}");
    let filter = serde_json::from_str::<Value>(input).ok().and_then(|value| {
        value
            .get("filter")
            .and_then(Value::as_str)
            .map(str::to_ascii_lowercase)
    });
    let entries = match (td_functions_json().as_array(), filter.as_deref()) {
        (Some(entries), None) => entries.clone(),
        (Some(entries), Some(needle)) => entries
            .iter()
            .filter(|entry| {
                entry["name"]
                    .as_str()
                    .is_some_and(|name| name.to_ascii_lowercase().contains(needle))
            })
            .cloned()
            .collect(),
        _ => Vec::new(),
    };
    let payload = json!({ "count": entries.len(), "functions": entries });
    fuyubot_tool::reply(&payload.to_string())
}

/// 函数表只从 schema 取一次：三千多条带说明，每次调工具都重算太浪费。
fn td_functions_json() -> &'static Value {
    static CACHE: OnceLock<Value> = OnceLock::new();
    CACHE.get_or_init(|| TdApiSchema::compiled().functions_json())
}

/// 把 ABI 的 JSON 描述转成 [`ToolDescriptor`]。
///
/// 处理器由 `api_register_tool` 单独填（ABI v4 起 `register_tool` 带函数指针，
/// 见 `api_register_tool`）；这里只负责元数据部分。
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

    /// 没接 TDLib 时立刻报 `NoTdlib`，不能傻等到超时。
    #[test]
    fn td_request_without_a_client_reports_no_tdlib() {
        let rt = runtime();
        let started = Instant::now();
        let error = rt
            .td_request(json!({ "@type": "getMe" }), Duration::from_secs(5))
            .unwrap_err();
        assert!(matches!(error, TdRequestError::NoTdlib), "{error}");
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    /// 形状校验在「有没有客户端」之前：参数错误要报参数错误。
    #[test]
    fn td_request_checks_the_request_shape_first() {
        let rt = runtime();
        assert!(matches!(
            rt.td_request(json!([1, 2]), Duration::from_secs(1)),
            Err(TdRequestError::NotObject)
        ));
        assert!(matches!(
            rt.td_request(json!({ "foo": 1 }), Duration::from_secs(1)),
            Err(TdRequestError::MissingType)
        ));
    }

    /// 等待位被**它自己的**响应叫醒；别人的响应只该被放过。
    #[test]
    fn a_registered_request_is_woken_by_its_own_response() {
        let waiters = Arc::new(TdWaiters::default());
        waiters
            .slots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(11, None);

        // 先喂一条别人的响应：不能把 11 叫醒。
        waiters.deliver(&json!({"@type": "ok", "request_id": 99}));

        let waiter = Arc::clone(&waiters);
        let handle =
            std::thread::spawn(move || waiter.wait(11, Instant::now() + Duration::from_secs(5)));
        std::thread::sleep(Duration::from_millis(20));
        waiters.deliver(&json!({"@type": "ok", "request_id": 11}));

        assert_eq!(
            handle.join().unwrap(),
            Some(json!({"@type": "ok", "request_id": 11}))
        );
    }

    /// 没人登记的请求号不该在表里长出来（否则并发请求会互相吃响应）。
    #[test]
    fn responses_for_unknown_ids_are_ignored() {
        let waiters = TdWaiters::default();
        waiters.deliver(&json!({"@type": "ok", "request_id": 7}));
        assert!(
            waiters
                .slots
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty()
        );
    }

    /// 插件自己起的线程上没有宿主的作用域栈——归属必须从 `host_ctx` 里认出来，
    /// 不能退化成 `plugin`（这正是「脚本 / 异步插件日志没有名字」的根因）。
    #[test]
    fn a_foreign_thread_still_logs_under_the_plugin_name() {
        let rt = runtime();
        let owner = rt.begin_plugin();
        rt.register_owner_name(owner, "demo.threaded");
        let host = rt.plugin_host(owner);
        let ptr = host.vtable_ptr() as usize;

        std::thread::spawn(move || {
            let api = unsafe { HostApi::from_raw(ptr as *const HostApiVTable) }
                .expect("插件上下文里的 vtable 应该自洽");
            api.log(LogLevel::Info, "from a plugin thread");
        })
        .join()
        .unwrap();

        rt.end_plugin();
        let lines = rt.log_lines();
        assert!(
            lines
                .iter()
                .any(|line| line.contains("demo.threaded") && line.contains("from a plugin thread")),
            "外来线程的日志要带插件名：{lines:?}"
        );
        rt.unregister_owner_name(owner);
        drop(host);
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
        assert!(vtable.call_tool_result.is_some());
        assert!(vtable.list_tools.is_some());
        assert!(vtable.list_plugins.is_some());
        assert!(vtable.provide_service.is_some());
        assert!(vtable.get_service.is_some());
        assert!(vtable.td_invoke.is_some());
        assert!(vtable.td_invoke_simplified.is_some());
        assert!(vtable.td_execute.is_some());
        assert!(vtable.use_middleware.is_some());
        assert!(vtable.wait_for_update.is_some());
        assert!(vtable.cancel_wait.is_some());
        assert!(vtable.log.is_some());
        assert!(vtable.get_config.is_some());
        assert!(vtable.set_config.is_some());
    }

    /// 每个测试一个干净的目录（`target/test-tmp/`，别用系统 TEMP）。
    fn scratch(name: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-tmp/fuyubot-loader/config")
            .join(name)
            .join(std::process::id().to_string());
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 配置读写桥：读顶层 / 读插件段 / 写自己段（含点路径）/ 越权写别人段被拒 / 只写自己的文件。
    #[test]
    fn config_is_readable_and_writable_only_own_section() {
        let rt = runtime();
        let dir = scratch("read-write");
        let path = dir.join("fuyubot.toml");
        // 用户的配置文件：插件写配置**一个字节都不能动**（注释、凭据都在里面）。
        let original = "# 用户手写\n[telegram]\napi_id = 123\napi_hash = \"saved-hash\"\n\n[plugin.a.bot]\ncity = \"上海\"\n";
        std::fs::write(&path, original).unwrap();
        rt.install_config(
            Some(path.clone()),
            serde_json::json!({
                "bot": { "name": "demo" },
                "plugin": {
                    "a": { "bot": { "city": "上海" } },
                    "other": { "bot": { "secret": "x" } },
                    // 用户没钉任何键的段：整段覆盖要走得通。
                    "fresh": { "one": {} }
                }
            }),
        );

        // SAFETY: rt 活着。
        let api = unsafe { fuyubot_abi::HostApi::from_raw(rt.vtable_ptr()) }.unwrap();

        // 空 section = 自己：宿主没有「自己」那段，所以读不到（顶层不再对插件暴露）。
        assert!(
            api.get_config("", "bot.name").is_none(),
            "插件 API 不暴露 [bot] / [telegram] 这类顶层配置"
        );
        // 读别人的段：显式给插件名。
        let whole = api.get_config("other.bot", "").unwrap();
        assert!(whole.contains("secret"), "{whole}");
        // 不存在的段 / 键返回 None。
        assert!(api.get_config("ghost", "").is_none());
        assert!(api.get_config("a.bot", "nope").is_none());

        {
            let scope = rt.plugin_scope();
            rt.register_owner_name(scope.owner(), "a.bot");

            // 写自己段里用户没钉住的键（新键 + 点路径）都能落盘。
            assert_eq!(api.set_config("greeting", "\"hi\"").unwrap(), ());
            assert_eq!(api.get_config("a.bot", "greeting").unwrap(), "\"hi\"");
            assert_eq!(api.set_config("nested.k", "2").unwrap(), ());
            assert_eq!(api.get_config("a.bot", "nested.k").unwrap(), "2");
            // 「默认读自己」：空 section 现在指自己那段，与 set_config 的默认一致。
            assert_eq!(api.get_config("", "greeting").unwrap(), "\"hi\"");
            assert_eq!(api.get_config("", "nested.k").unwrap(), "2");

            // 用户钉住的键：拒绝，不写。
            assert!(
                api.set_config("city", "\"北京\"").is_err(),
                "fuyubot.toml 里写了的键，插件不能覆盖"
            );
            assert_eq!(api.get_config("a.bot", "city").unwrap(), "\"上海\"");

            // 越权写别人的段：owner 是 a.bot，只会落进 a.bot 段。
            api.set_config("secret", "\"hacked\"").unwrap();
            assert_eq!(api.get_config("other.bot", "secret").unwrap(), "\"x\"");

            scope.commit();
        }

        // 没有用户键的段：整段覆盖照旧可用。
        {
            let scope = rt.plugin_scope();
            rt.register_owner_name(scope.owner(), "fresh.one");
            assert_eq!(api.set_config("", "{\"x\":1}").unwrap(), ());
            assert!(api.get_config("fresh.one", "").unwrap().contains("\"x\":1"));
            scope.commit();
        }

        // 用户配置文件**原样不动**（这是 P2 的核心承诺：注释和凭据永远不会被插件卷走）。
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);

        // 插件自己的落盘：`plugin-config/<名>.toml`，内容是那一段本身（没有 [plugin.x] 头）。
        let store = dir.join(CONFIG_STORE_DIR);
        let own = std::fs::read_to_string(store.join("a.bot.toml")).unwrap();
        assert!(own.contains("greeting = \"hi\""), "{own}");
        assert!(own.contains("secret = \"hacked\""), "{own}");
        assert!(
            !own.contains("city"),
            "用户钉住的键不该被复制进插件自己的文件: {own}"
        );
        // 原子写不留临时文件。
        assert!(
            !store.join("a.bot.toml.tmp").exists(),
            "临时文件必须已经 rename 掉"
        );

        // 回到宿主根作用域（owner 0），没有名字 → 写失败。
        assert!(api.set_config("k", "1").is_err());
    }

    /// 跨插件写：`set_config_for` 显式点名目标，落到**对方**的段与文件；
    /// 用户配置里钉住的键照样拒绝，`fuyubot.toml` 照样一个字节不动。
    #[test]
    fn a_plugin_can_write_another_plugins_section() {
        let rt = runtime();
        let dir = scratch("cross-write");
        let path = dir.join("fuyubot.toml");
        let original = "[plugin.a.bot]\ncity = \"上海\"\n\n[plugin.b.bot]\nmode = \"user\"\n";
        std::fs::write(&path, original).unwrap();
        rt.install_config(
            Some(path.clone()),
            serde_json::json!({
                "plugin": {
                    "a": { "bot": { "city": "上海" } },
                    "b": { "bot": { "mode": "user" } }
                }
            }),
        );

        // SAFETY: rt 活着。
        let api = unsafe { fuyubot_abi::HostApi::from_raw(rt.vtable_ptr()) }.unwrap();

        {
            let scope = rt.plugin_scope();
            rt.register_owner_name(scope.owner(), "a.bot");

            // 新键：写进 b.bot 的段。
            api.set_config_for("b.bot", "extra", "\"from-a\"")
                .expect("跨插件写应当成功");
            // 用户钉住的键：跨插件也照样拒绝（这条规则不因为「换了个入口」就放松）。
            let error = api
                .set_config_for("b.bot", "mode", "\"hacked\"")
                .unwrap_err();
            assert!(error.to_string().contains("mode"), "{error}");

            scope.commit();
        }

        assert_eq!(api.get_config("b.bot", "extra").unwrap(), "\"from-a\"");
        assert_eq!(api.get_config("b.bot", "mode").unwrap(), "\"user\"");
        assert!(
            api.get_config("a.bot", "extra").is_none(),
            "不该写进调用方那段"
        );

        // 落盘进**目标**的文件；用户配置原样不动。
        let stored =
            std::fs::read_to_string(dir.join(CONFIG_STORE_DIR).join("b.bot.toml")).unwrap();
        assert!(stored.contains("extra = \"from-a\""), "{stored}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);

        // 目标为空等于没指定：拒绝而不是猜。
        assert!(api.set_config_for("", "k", "1").is_err());
    }

    /// 服务方法里写配置，写的是**提供方自己**的段，不是调用方的。
    ///
    /// v5 只能靠线程作用域栈判「谁的段」，于是 A 调 B 的服务、B 写配置会落进 A 的段。
    /// v6 在服务表上记了提供方 owner，调用期间把执行者切成它。
    #[test]
    fn a_service_method_writes_its_own_config_section() {
        let rt = runtime();
        let dir = scratch("executor-config");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("fuyubot.toml");
        std::fs::write(&path, "").unwrap();
        rt.install_config(
            Some(path),
            serde_json::json!({ "plugin": { "a": { "bot": {} }, "b": { "bot": {} } } }),
        );

        unsafe extern "C" fn write_own(
            context: *mut c_void,
            _input: *const c_char,
        ) -> *const c_char {
            // SAFETY: 测试把 `context` 指向下面自持的 `HostApi`。
            let api = unsafe { &*(context as *const fuyubot_abi::HostApi) };
            match api.set_config("mark", "\"b\"") {
                Ok(()) => host_string("ok"),
                Err(error) => host_string(&format!("err:{error}")),
            }
        }

        struct Methods([ServiceMethod; 1]);
        // SAFETY: name 指向 'static 字面量，func 是纯函数指针。
        unsafe impl Sync for Methods {}
        static METHODS: Methods = Methods([ServiceMethod {
            name: c"mark".as_ptr(),
            func: Some(write_own),
        }]);
        let name = CString::new("demo.writer").unwrap();
        // SAFETY: rt 活着；vtable / api 都比注册活得久。
        let api = unsafe { fuyubot_abi::HostApi::from_raw(rt.vtable_ptr()) }.unwrap();
        let vtable = ServiceVTable {
            name: name.as_ptr(),
            version: 1,
            size: core::mem::size_of::<ServiceVTable>() as u32,
            context: (&api as *const fuyubot_abi::HostApi)
                .cast_mut()
                .cast::<c_void>(),
            methods: METHODS.0.as_ptr(),
            method_count: METHODS.0.len(),
        };

        // 由 b.bot 注册这个服务（owner 记在服务表上）。
        let handle = {
            let scope = rt.plugin_scope();
            rt.register_owner_name(scope.owner(), "b.bot");
            // SAFETY: 契约同上；写根，a.bot 才查得到。
            let handle = unsafe { api.provide_service_shared("demo.writer", &vtable) };
            assert!(handle.is_valid());
            scope.commit();
            handle
        };

        // a.bot 调它：执行者是 b.bot，配置就落 b.bot。
        {
            let scope = rt.plugin_scope();
            rt.register_owner_name(scope.owner(), "a.bot");
            assert_eq!(api.call_service("demo.writer", "mark", "{}").unwrap(), "ok");
            scope.commit();
        }

        assert_eq!(api.get_config("b.bot", "mark").unwrap(), "\"b\"");
        assert!(
            api.get_config("a.bot", "mark").is_none(),
            "提供方的写入不该跑到调用方段里"
        );

        drop(handle);
    }

    /// 服务发现：清单里有名字、版本、方法名与提供方；宿主预置的 `plugin` 是 `null`。
    #[test]
    fn plugins_can_list_the_services_they_can_see() {
        let rt = runtime();
        // SAFETY: rt 活着。
        let api = unsafe { fuyubot_abi::HostApi::from_raw(rt.vtable_ptr()) }.unwrap();
        let listed: serde_json::Value = serde_json::from_str(&api.list_services()).unwrap();
        let items = listed.as_array().expect("必须是数组");

        let scope = items
            .iter()
            .find(|item| item["name"] == "fuyubot.host.scope")
            .expect("宿主预置的 scope service 应当在清单里");
        assert!(scope["plugin"].is_null(), "宿主预置的服务没有提供方插件");
        let methods: Vec<&str> = scope["methods"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|method| method.as_str())
            .collect();
        assert!(methods.contains(&"scope_create"), "{methods:?}");
        assert!(methods.contains(&"scope_enter"), "{methods:?}");
    }

    /// 调用失败要能说出**为什么**，而不是一句笼统的「调用失败」。
    #[test]
    fn a_failed_service_call_reports_why() {
        let rt = runtime();
        // SAFETY: rt 活着。
        let api = unsafe { fuyubot_abi::HostApi::from_raw(rt.vtable_ptr()) }.unwrap();
        let error = api.call_service("nope.svc", "m", "{}").unwrap_err();
        let text = error.to_string();
        assert!(text.contains("nope.svc"), "{text}");
        assert!(text.contains("没有名为"), "{text}");
    }

    /// 插件自己落盘的文件下次启动要读回来，但**用户配置优先**（只填缺失键）。
    #[test]
    fn runtime_store_is_loaded_below_the_user_config() {
        let rt = runtime();
        let dir = scratch("store-load");
        let store = dir.join(CONFIG_STORE_DIR);
        std::fs::create_dir_all(&store).unwrap();
        std::fs::write(
            store.join("a.bot.toml"),
            "city = \"from-store\"\nkept = 7\n",
        )
        .unwrap();

        rt.install_config(
            Some(dir.join("fuyubot.toml")),
            serde_json::json!({ "plugin": { "a": { "bot": { "city": "from-user" } } } }),
        );
        // SAFETY: rt 活着。
        let api = unsafe { fuyubot_abi::HostApi::from_raw(rt.vtable_ptr()) }.unwrap();

        assert_eq!(
            api.get_config("a.bot", "city").unwrap(),
            "\"from-user\"",
            "用户配置压过运行时落盘"
        );
        assert_eq!(
            api.get_config("a.bot", "kept").unwrap(),
            "7",
            "用户没写的键由插件自己的文件兜底"
        );
    }

    /// 包默认配置：只填**缺失**键，用户配置与运行时落盘优先；重复合并不覆盖已有值。
    #[test]
    fn plugin_defaults_fill_only_missing_keys() {
        let rt = runtime();
        let dir = scratch("defaults");
        let store = dir.join(CONFIG_STORE_DIR);
        std::fs::create_dir_all(&store).unwrap();
        std::fs::write(store.join("a.bot.toml"), "city = \"from-store\"\n").unwrap();

        rt.install_config(
            Some(dir.join("fuyubot.toml")),
            serde_json::json!({ "plugin": { "a": { "bot": { "lang": "zh" } } } }),
        );

        rt.apply_plugin_defaults(
            "a.bot",
            serde_json::json!({ "city": "default", "lang": "en", "level": 1 }),
        );
        // SAFETY: rt 活着。
        let api = unsafe { fuyubot_abi::HostApi::from_raw(rt.vtable_ptr()) }.unwrap();

        assert_eq!(
            api.get_config("a.bot", "lang").unwrap(),
            "\"zh\"",
            "用户配置最优先"
        );
        assert_eq!(
            api.get_config("a.bot", "city").unwrap(),
            "\"from-store\"",
            "运行时落盘压过包默认"
        );
        assert_eq!(
            api.get_config("a.bot", "level").unwrap(),
            "1",
            "缺的键用默认值补"
        );

        // 再合一次（插件重新加载）不能把已有的值盖掉。
        rt.apply_plugin_defaults("a.bot", serde_json::json!({ "level": 2, "lang": "en" }));
        assert_eq!(api.get_config("a.bot", "level").unwrap(), "1");
    }

    /// 没有落盘目录 = **不可写**：`set_config` 必须报错并把内存还原（不能留半截状态）。
    ///
    /// 以前这里返回成功、只改内存：插件以为保存了，重启后又变回旧值——Android 上踩过。
    #[test]
    fn set_config_without_a_config_file_fails_and_rolls_back() {
        let rt = runtime();
        let dir = scratch("no-store");
        // 让「建 plugin-config 目录」失败：用同名文件占住那个位置。
        std::fs::write(dir.join(CONFIG_STORE_DIR), b"not a directory").unwrap();
        rt.install_config(
            Some(dir.join("fuyubot.toml")),
            // 插件名是点分的：树里必须是嵌套表（`"a.bot"` 字面键查不到，见 `with_inject`）。
            serde_json::json!({ "plugin": { "a": { "bot": {} } } }),
        );
        // SAFETY: rt 活着。
        let api = unsafe { fuyubot_abi::HostApi::from_raw(rt.vtable_ptr()) }.unwrap();

        let scope = rt.plugin_scope();
        rt.register_owner_name(scope.owner(), "a.bot");

        assert!(api.set_config("k", "\"new\"").is_err(), "写不进去就该报错");
        assert!(
            api.get_config("a.bot", "k").is_none(),
            "落盘失败必须还原内存，别留下「内存里有、磁盘上没有」的状态"
        );
        scope.commit();
    }

    /// 没有配置文件（`config_path = None`）时同样报错，而不是静默只写内存。
    #[test]
    fn set_config_without_any_config_installed_fails() {
        let rt = runtime();
        rt.install_config(None, serde_json::json!({ "plugin": {} }));
        // SAFETY: rt 活着。
        let api = unsafe { fuyubot_abi::HostApi::from_raw(rt.vtable_ptr()) }.unwrap();
        let scope = rt.plugin_scope();
        rt.register_owner_name(scope.owner(), "a.bot");
        assert!(api.set_config("k", "1").is_err());
        scope.commit();
    }

    /// 插件名 → 文件名编码：路径分隔符、`..`、非 ASCII 都要能安全往返。
    #[test]
    fn store_file_names_are_encoded_and_decoded() {
        for name in [
            "a.bot",
            "fuyubot.manage",
            "../escape",
            "a/b",
            "名字",
            "CON",
            "LPT9",
        ] {
            let encoded = encode_plugin_file_name(name);
            assert!(
                !encoded.contains('/') && !encoded.contains('\\'),
                "{name} 编码后仍是路径：{encoded}"
            );
            assert_ne!(encoded, "CON", "Windows 保留名要编码掉");
            assert_eq!(
                decode_plugin_file_name(&encoded).as_deref(),
                Some(name),
                "编码必须可逆"
            );
        }
        assert!(decode_plugin_file_name("").is_none(), "空文件名不是插件");
        assert!(decode_plugin_file_name("%zz").is_none(), "坏 % 转义要拒绝");
    }

    /// 日志门槛：低于门槛的**既**不留档**也**不写 stderr；调高门槛后又能收到。
    #[test]
    fn log_verbosity_filters_out_quieter_levels() {
        let rt = runtime();
        assert_eq!(rt.log_verbosity(), DEFAULT_LOG_VERBOSITY as u32);

        // SAFETY: vtable 活着。
        let api = unsafe { fuyubot_abi::HostApi::from_raw(rt.vtable_ptr()) }.unwrap();

        // 默认 Info：Debug 被丢掉，Info / Error 留下。
        api.log(LogLevel::Debug, "debug-should-be-dropped");
        api.log(LogLevel::Info, "info-should-stay");
        api.log(LogLevel::Error, "error-should-stay");
        let lines = rt.log_lines();
        assert!(!lines.iter().any(|line| line.contains("debug-should")));
        assert!(lines.iter().any(|line| line.contains("info-should-stay")));
        assert!(lines.iter().any(|line| line.contains("error-should-stay")));

        // 调成 Trace 之后 Debug 也进来了。
        rt.set_log_verbosity(LogLevel::Trace);
        api.log(LogLevel::Debug, "debug-after-raising");
        assert!(
            rt.log_lines()
                .iter()
                .any(|line| line.contains("debug-after-raising"))
        );

        // 调成 Error 之后连 Info 都进不来（只影响留档，插件照样能调）。
        rt.set_log_verbosity(LogLevel::Error);
        api.log(LogLevel::Info, "info-after-lowering");
        assert!(
            !rt.log_lines()
                .iter()
                .any(|line| line.contains("info-after-lowering"))
        );
    }

    /// 来源过滤：只留白名单里的插件；空表恢复全收；环形缓冲带来源。
    #[test]
    fn log_plugin_filter_keeps_only_selected_sources() {
        let rt = runtime();
        let scope = rt.plugin_scope();
        rt.register_owner_name(scope.owner(), "fuyubot.demo");
        // SAFETY: vtable 活着；scope 已把 owner 压到栈顶，api.log 会记到它名下。
        let api = unsafe { fuyubot_abi::HostApi::from_raw(rt.vtable_ptr()) }.unwrap();

        rt.set_log_plugins(vec!["fuyubot.demo".to_owned()]);
        assert_eq!(rt.log_plugins(), Some(vec!["fuyubot.demo".to_owned()]));
        api.log(LogLevel::Info, "kept");
        rt.set_log_plugins(vec!["other".to_owned()]);
        api.log(LogLevel::Info, "dropped");

        let lines = rt.log_lines();
        assert!(lines.iter().any(|line| line.contains("kept")), "{lines:?}");
        assert!(
            !lines.iter().any(|line| line.contains("dropped")),
            "白名单外的插件日志不该进环形缓冲：{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("fuyubot.demo") && line.contains("kept")),
            "环形缓冲要带来源：{lines:?}"
        );

        // 逗号写法也行（`--log-plugin a,b`）；空白会被吃掉。
        rt.set_log_plugins(vec![" fuyubot.demo , other ".to_owned()]);
        assert_eq!(
            rt.log_plugins(),
            Some(vec!["fuyubot.demo".to_owned(), "other".to_owned()])
        );

        // 空表 = 取消过滤（不是「谁都不留」）。
        rt.set_log_plugins(Vec::new());
        assert_eq!(rt.log_plugins(), None);
        api.log(LogLevel::Info, "back");
        assert!(
            rt.log_lines().iter().any(|line| line.contains("back")),
            "清空白名单后应恢复全收"
        );
    }

    /// 原始更新追踪开关（`--trace-updates`）：默认关，开了就开，可以再关掉。
    #[test]
    fn trace_updates_toggles() {
        let rt = runtime();
        assert!(!rt.trace_updates(), "默认不该开着");
        rt.set_trace_updates(true);
        assert!(rt.trace_updates());
        rt.set_trace_updates(false);
        assert!(!rt.trace_updates());
    }

    /// `list_tools` 的缓存：工具表没变就复用，注册 / 注销后必须失效。
    #[test]
    fn list_tools_json_cache_follows_the_table_revision() {
        let rt = runtime();
        let first = rt.tools_json();
        assert_eq!(first, rt.tools_json(), "没变更时该复用同一个缓冲");
        // 复用意味着连指针都一样（Arc 里是同一个 CString）。
        assert!(core::ptr::eq(first.as_ptr(), rt.tools_json().as_ptr()));

        let descriptor = fuyubot_tool::ToolDescriptor::new(
            "cache.probe",
            "探测缓存",
            serde_json::json!({"type": "object"}),
            None,
        );
        let handle = rt.tools.register_tool(descriptor).unwrap();
        let after_register = rt.tools_json();
        assert_ne!(after_register, first);
        assert!(after_register.to_string_lossy().contains("cache.probe"));

        drop(handle);
        let after_unregister = rt.tools_json();
        assert!(
            !after_unregister.to_string_lossy().contains("cache.probe"),
            "注销后缓存必须失效：{}",
            after_unregister.to_string_lossy()
        );
    }

    /// 工具注册 / 注销必须同时出现在事件总线上（缺口 9 的验收）。
    ///
    /// 以前只有 `ToolRegistry` 的本地监听器能看见，插件订阅 `tool_registered`
    /// 永远收不到——`ToolEvent::kind()` 那套命名等于是死代码。
    #[test]
    fn tool_registration_is_broadcast_on_the_event_bus() {
        let rt = runtime();
        let seen: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let _subscription = rt.events().on("tool_registered", None, move |payload| {
            sink.lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(payload.clone());
            Ok(())
        });
        let unregistered: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&unregistered);
        let _subscription = rt.events().on("tool_unregistered", None, move |payload| {
            sink.lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(payload.clone());
            Ok(())
        });

        let handle = rt
            .tools()
            .register_tool(fuyubot_tool::ToolDescriptor::new(
                "bus.probe",
                "总线探针",
                serde_json::json!({"type": "object"}),
                None,
            ))
            .unwrap();

        {
            let seen = seen.lock().unwrap_or_else(|e| e.into_inner());
            assert_eq!(seen.len(), 1, "注册只该发一条：{seen:?}");
            assert_eq!(seen[0]["event"], "tool_registered");
            assert_eq!(seen[0]["name"], "bus.probe");
        }

        drop(handle);
        let unregistered = unregistered.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(unregistered.len(), 1, "注销只该发一条：{unregistered:?}");
        assert_eq!(unregistered[0]["event"], "tool_unregistered");
        assert_eq!(unregistered[0]["name"], "bus.probe");
    }

    /// 缺口 12 的宿主侧消费者：内核生命周期事件必须出现在事件总线上。
    #[test]
    fn lifecycle_events_reach_the_event_bus() {
        struct Probe;
        impl fuyubot_core::Plugin for Probe {
            fn name(&self) -> &str {
                "lifecycle.probe"
            }
            fn apply(
                &self,
                _ctx: &Context,
            ) -> std::result::Result<Effect, fuyubot_core::KernelError> {
                Ok(Effect::none())
            }
        }

        let ctx = Context::new();
        let rt = HostRuntime::new(ctx.clone()).unwrap();
        let mut manager = PluginManager::new(ctx.clone());
        let _bridge = rt.install_lifecycle_bridge(&manager);

        let seen: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let _sub = rt.events().on("plugin_activated", None, move |payload| {
            sink.lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(payload.clone());
            Ok(())
        });
        let unloaded: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&unloaded);
        let _sub = rt.events().on("plugin_unloaded", None, move |payload| {
            sink.lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(payload.clone());
            Ok(())
        });

        manager.load(Probe).unwrap();
        manager.unload("lifecycle.probe").unwrap();

        let activated = seen.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(activated.len(), 1, "{activated:?}");
        assert_eq!(activated[0]["event"], "plugin_activated");
        assert_eq!(activated[0]["name"], "lifecycle.probe");
        let unloaded = unloaded.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(unloaded.len(), 1, "{unloaded:?}");
        assert_eq!(unloaded[0]["name"], "lifecycle.probe");
    }

    /// 缺口 10 的 async 形态走**真宿主**（不是 fake vtable）：插件作用域 → 发请求 →
    /// 另一个线程投递响应 → `await` 被唤醒。验证归属订阅、`request_id` 与 waker 三件事。
    #[tokio::test]
    async fn response_router_awaits_a_real_host_response() {
        let rt = runtime();
        // SAFETY: rt 活着。
        let api = unsafe { fuyubot_abi::HostApi::from_raw(rt.vtable_ptr()) }.unwrap();
        let scope = rt.plugin_scope();
        let router = fuyubot_abi::ResponseRouter::new(api).expect("注册响应订阅");
        // 关键：在**插件自己的线程**上发请求（没有宿主作用域）。路由器会用 scope service
        // 把归属切回插件，否则响应会按 owner 0 全局投递、这里的 scoped 订阅收不到。
        let (id, future) = {
            let router = router.clone();
            std::thread::spawn(move || router.invoke_simplified("getMe", "{}"))
                .join()
                .expect("发送线程不能 panic")
                .expect("发送请求")
        };

        let responder = {
            let rt = Arc::clone(&rt);
            std::thread::spawn(move || {
                rt.deliver_response(&json!({
                    "@type": "ok",
                    "@extra": id.id.to_string(),
                }))
                .expect("投递响应");
            })
        };

        let value = tokio::time::timeout(Duration::from_secs(2), future)
            .await
            .expect("宿主响应没叫醒 future（waker 没被调用？）")
            .expect("响应应该成功");
        responder.join().unwrap();
        assert_eq!(value["@type"], "ok");
        assert_eq!(value["request_id"], id.id);

        drop(router);
        scope.commit();
    }

    /// 同一路由器的更新等待：真 HostRuntime 的 `publish_update` → `await` 返回。
    #[tokio::test]
    async fn event_router_awaits_a_real_update() {
        let rt = runtime();
        // SAFETY: rt 活着。
        let api = unsafe { fuyubot_abi::HostApi::from_raw(rt.vtable_ptr()) }.unwrap();
        let scope = rt.plugin_scope();
        let router = fuyubot_abi::ResponseRouter::new(api).expect("注册订阅");
        let update = router
            .wait_for_update(json!({"@type": "updateNewMessage", "message.chat.id": -7}))
            .expect("等更新");

        let publisher = {
            let rt = Arc::clone(&rt);
            std::thread::spawn(move || {
                rt.publish_update(&json!({
                    "@type": "updateNewMessage",
                    "message": { "chat": { "id": -7 } },
                }))
                .expect("发布更新");
            })
        };

        let value = tokio::time::timeout(Duration::from_secs(2), update)
            .await
            .expect("更新没叫醒 future")
            .expect("更新应该成功");
        publisher.join().unwrap();
        assert_eq!(value["@type"], "updateNewMessage");
        assert_eq!(value["message"]["chat"]["id"], -7);

        drop(router);
        scope.commit();
    }

    /// 缺口 13③：函数表是**能调**的工具，不是躺在库里的死数据。
    #[test]
    fn builtin_td_functions_tool_is_callable() {
        let rt = runtime();
        let all = rt
            .tools()
            .call_tool(TD_FUNCTIONS_TOOL, &json!({}))
            .expect("内置工具必须能调");
        let all = &all.output;
        assert!(all["count"].as_u64().unwrap() > 100, "{all}");
        assert_eq!(
            all["functions"].as_array().unwrap().len(),
            all["count"].as_u64().unwrap() as usize
        );

        // 过滤走名字子串，大小写不敏感。
        let filtered = rt
            .tools()
            .call_tool(TD_FUNCTIONS_TOOL, &json!({ "filter": "SENDMESSAGEALBUM" }))
            .expect("带过滤也能调")
            .output;
        assert_eq!(filtered["count"], 1, "{filtered}");
        assert_eq!(filtered["functions"][0]["name"], "sendMessageAlbum");
        assert!(
            filtered["functions"][0]["params"]
                .as_array()
                .is_some_and(|params| !params.is_empty()),
            "函数条目要带参数：{filtered}"
        );

        let none = rt
            .tools()
            .call_tool(TD_FUNCTIONS_TOOL, &json!({ "filter": "zzz-nothing" }))
            .expect("空结果也是成功")
            .output;
        assert_eq!(none["count"], 0, "{none}");
    }

    /// 缺口 31：`list_tools` 要说清每个工具是**谁**注册的。
    ///
    /// 宿主内置工具报 [`HOST_TOOL_OWNER`]；插件注册的报插件名；回滚后工具与归属
    /// 一起消失（归属是从注册表算的，不是另一张会留残影的表）。
    #[test]
    fn list_tools_reports_who_registered_each_tool() {
        let rt = runtime();
        // SAFETY: rt 活着。
        let api = unsafe { fuyubot_abi::HostApi::from_raw(rt.vtable_ptr()) }.unwrap();

        let scope = rt.plugin_scope();
        let owner = scope.owner();
        rt.register_owner_name(owner, "demo.owner");
        let tool = api.register_tool_meta(
            r#"{"name":"owned.tool","description":"归属测试","input_schema":{"type":"object"}}"#,
        );
        assert!(tool.is_valid());

        let listed: Value =
            serde_json::from_str(&rt.tools_json().to_string_lossy()).expect("list_tools 是 JSON");
        let find = |name: &str| {
            listed
                .as_array()
                .expect("工具表是数组")
                .iter()
                .find(|item| item["name"] == name)
                .cloned()
                .unwrap_or_else(|| panic!("list_tools 里没有 {name}：{listed}"))
        };
        assert_eq!(find("owned.tool")["owner"], "demo.owner");
        assert_eq!(find(TD_FUNCTIONS_TOOL)["owner"], HOST_TOOL_OWNER);

        drop(scope);
        rt.unregister_owner_name(owner);
        let after = rt.tools_json().to_string_lossy().into_owned();
        assert!(
            !after.contains("owned.tool"),
            "回滚后不该留下归属残影：{after}"
        );
    }

    /// 缺口 13①：真 schema 的判据本身——方法名、参数名、合法请求三条都钉住。
    ///
    /// 宿主警告只走 stderr，没有可断言的环形缓冲，所以测判据（`schema_rejection`）+
    /// 「错拼请求仍然发得出去」这两半，合起来就是 td_send 的全部行为。
    #[test]
    fn td_send_flags_requests_the_real_schema_rejects() {
        let typo = json!({"@type":"sendMesssage","chat_id":1,"text":"hi"});
        let reason = schema_rejection("sendMesssage", &typo).expect("方法名拼错必须被认出来");
        assert!(reason.contains("sendMesssage"), "{reason}");

        let wrong_param = json!({"@type":"sendMessage","chat_id":1,"textt":"hi"});
        let reason = schema_rejection("sendMessage", &wrong_param).expect("参数名拼错必须被认出来");
        assert!(reason.contains("textt"), "{reason}");

        // 合法请求不许误报——不然日志天天狼来了，没人会看。
        assert_eq!(schema_rejection("getMe", &json!({"@type":"getMe"})), None);
        assert_eq!(
            schema_rejection(
                "sendMessage",
                &json!({"@type":"sendMessage","chat_id":1,"input_message_content":{}})
            ),
            None
        );

        // 只警告不拦截：错拼的请求照样拿到请求号、照样留档。
        let rt = runtime();
        // SAFETY: rt 活着。
        let api = unsafe { fuyubot_abi::HostApi::from_raw(rt.vtable_ptr()) }.unwrap();
        let id = api
            .td_invoke(r#"{"@type":"sendMesssage","chat_id":1,"text":"hi"}"#)
            .unwrap();
        assert!(id.is_valid(), "照发，所以请求号必须有效");
        assert!(
            rt.sent_requests()
                .iter()
                .any(|request| request.id == id.id && request.method == "sendMesssage"),
            "{:?}",
            rt.sent_requests()
        );
    }

    /// simplify 的**同步**入口（ABI v5）：纯计算类的 op 当场返回结果。
    #[test]
    fn simplify_invoke_result_runs_the_sync_ops_inline() {
        let rt = runtime();
        // SAFETY: vtable 活着。
        let api = unsafe { HostApi::from_raw(rt.vtable_ptr()) }.unwrap();

        // 没接 TDLib 时 `formatted` 退回纯文本，所以这条离线也成立。
        let parsed: Value = serde_json::from_str(&api.simplify_invoke_result(
            r#"{"op":"parse_text_entities","args":{"text":"*hi*","parse_mode":"markdown"}}"#,
        ))
        .unwrap();
        assert_eq!(parsed["ok"], true, "{parsed}");
        assert_eq!(parsed["result"]["@type"], "formattedText");
        assert_eq!(parsed["result"]["text"], "*hi*");

        // 富消息：本地 Markdown → `inputRichMessage`，这是别的语言唯一能拿到它的路子。
        let rich: Value = serde_json::from_str(&api.simplify_invoke_result(
            r##"{"op":"rich_from_markdown","args":{"markdown":"# t\n\nbody"}}"##,
        ))
        .unwrap();
        assert_eq!(rich["ok"], true, "{rich}");
        assert_eq!(rich["result"]["@type"], "inputMessageRichText");
        assert_eq!(rich["result"]["rich_message"]["@type"], "inputRichMessage");

        // 自描述：操作表本身可以问出来
        let listed: Value =
            serde_json::from_str(&api.simplify_invoke_result(r#"{"op":"list_ops"}"#)).unwrap();
        assert_eq!(listed["ok"], true, "{listed}");
        assert!(
            listed["result"]["async"]
                .as_array()
                .is_some_and(|ops| ops.iter().any(|op| op == "send_message")),
            "异步表里有 send_message：{listed}"
        );

        // 认不出的 op：明确报错，不静默
        let unknown: Value =
            serde_json::from_str(&api.simplify_invoke_result(r#"{"op":"nope"}"#)).unwrap();
        assert_eq!(unknown["ok"], false, "{unknown}");
        assert_eq!(unknown["error"]["code"], "unknown_op");

        // 异步 op 不许走同步入口（会卡住投递线程）
        let blocked: Value = serde_json::from_str(
            &api.simplify_invoke_result(r#"{"op":"send_message","args":{"chat_id":1}}"#),
        )
        .unwrap();
        assert_eq!(blocked["ok"], false, "{blocked}");
        assert_eq!(blocked["error"]["code"], "async_op");
    }

    /// simplify 的**异步**入口（ABI v5）：op 在宿主的工作线程里跑，结果按 `request_id` 回到发起者。
    ///
    /// 这条测试同时钉住两件事：① op 内部发出的 TDLib 请求**照旧**进 `sent_requests`
    /// （说明它真的走了宿主那条路）；② 结果只以 `simplifyResult` 的形状回来一次。
    #[test]
    fn simplify_invoke_hands_the_result_back_through_the_response_channel() {
        let rt = runtime();
        // SAFETY: vtable 活着。
        let api = unsafe { HostApi::from_raw(rt.vtable_ptr()) }.unwrap();

        let id = api
            .simplify_invoke(
                r#"{"op":"delete_messages","args":{"chat_id":1,"message_ids":[7],
                    "revoke":true,"timeout_ms":5000}}"#,
            )
            .unwrap();
        assert!(id.is_valid(), "操作号必须有效");

        // 模拟 TDLib：看到 `deleteMessages` 就回一条 ok
        let responder = {
            let rt = Arc::clone(&rt);
            std::thread::spawn(move || {
                for _ in 0..200 {
                    std::thread::sleep(Duration::from_millis(10));
                    let target = rt
                        .sent_requests()
                        .into_iter()
                        .find(|request| request.method == "deleteMessages");
                    if let Some(request) = target {
                        let _ = rt.deliver_response(
                            &json!({"@type":"ok","@extra": request.id.to_string()}),
                        );
                        return;
                    }
                }
            })
        };

        let filter =
            json!({ FILTER_EVENT_TYPE_KEY: RESPONSE_EVENT, "request_id": id.id }).to_string();
        let handle = api.wait_for_update(&filter, 5000);
        responder.join().unwrap();
        assert!(handle.did_match(), "结果必须回到 response 通道");
        // SAFETY: 宿主保证这个指针在下一次宿主调用之前有效，这里立刻读完。
        let text = unsafe { CStr::from_ptr(handle.payload_json) }.to_string_lossy();
        let payload: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(payload["op"], "delete_messages", "{payload}");
        assert_eq!(payload["ok"], true, "{payload}");
        assert_eq!(payload["result"]["@type"], "ok");
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
        let tool = api.register_tool_meta(
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

    // ── 子作用域 ───────────────────────────────────────────────────

    /// 中间件计数回调，`user_data` 当 `AtomicUsize` 用。
    unsafe extern "C-unwind" fn count_mw(
        user_data: *mut c_void,
        _stage: u32,
        _payload: *const c_char,
    ) -> i32 {
        let counter = unsafe { &*(user_data as *const AtomicUsize) };
        counter.fetch_add(1, Ordering::SeqCst);
        0
    }

    /// 通过 VTable 的 `get_service` 拿内置 scope service。
    fn scope_service(rt: &HostRuntime) -> *const ServiceVTable {
        // SAFETY: rt 活着。
        let api = unsafe { fuyubot_abi::HostApi::from_raw(rt.vtable_ptr()) }.unwrap();
        let svc = api.get_service("fuyubot.host.scope");
        assert!(!svc.is_null(), "内置 scope service 必须预置");
        svc
    }

    /// 按名字调 scope service 的一个方法。`None` = 成功（无输出），
    /// `Some(text)` = 有输出（scope_create 的 handle，或错误 JSON）。
    fn call_scope(svc: *const ServiceVTable, name: &str, input: &str) -> Option<String> {
        // SAFETY: svc 来自 get_service，方法表有效。
        let vtable = unsafe { &*svc };
        let input = CString::new(input).unwrap();
        for i in 0..vtable.method_count {
            // SAFETY: method_count 由宿主保证。
            let method = unsafe { &*vtable.methods.add(i) };
            // SAFETY: 方法名是 NUL 结尾的合法 UTF-8。
            let mname = unsafe { CStr::from_ptr(method.name) }.to_string_lossy();
            if mname != name {
                continue;
            }
            let func = method.func.expect("scope 方法都有实现");
            // SAFETY: context 是宿主运行时指针，input 是临时 C 字符串。
            let raw = unsafe { func(vtable.context, input.as_ptr()) };
            if raw.is_null() {
                return None;
            }
            // SAFETY: raw 是宿主线程局部缓冲的 NUL 结尾字符串。
            return Some(
                unsafe { CStr::from_ptr(raw) }
                    .to_string_lossy()
                    .into_owned(),
            );
        }
        panic!("scope service 没有方法 {name}");
    }

    fn scope_create(svc: *const ServiceVTable) -> u64 {
        let out = call_scope(svc, "scope_create", "{}").expect("create 应返回 handle");
        serde_json::from_str::<Value>(&out).unwrap()["handle"]
            .as_u64()
            .unwrap()
    }

    fn scope_enter(svc: *const ServiceVTable, handle: u64) {
        let input = format!(r#"{{"handle":{handle}}}"#);
        assert!(
            call_scope(svc, "scope_enter", &input).is_none(),
            "enter 应成功"
        );
    }

    fn scope_leave(svc: *const ServiceVTable) {
        assert!(call_scope(svc, "scope_leave", "{}").is_none());
    }

    fn scope_drop(svc: *const ServiceVTable, handle: u64) {
        let input = format!(r#"{{"handle":{handle}}}"#);
        assert!(
            call_scope(svc, "scope_drop", &input).is_none(),
            "drop 应成功"
        );
    }

    /// 验收（设计文档 5.2）：`scope_drop` 后该作用域的订阅/工具/中间件全消失，
    /// 同 owner 的其他作用域不受影响。
    #[test]
    fn scope_drop_removes_its_own_registrations_but_not_others() {
        let rt = runtime();
        let _owner = rt.begin_plugin();
        // SAFETY: rt 活着。
        let api = unsafe { fuyubot_abi::HostApi::from_raw(rt.vtable_ptr()) }.unwrap();
        let svc = scope_service(&rt);

        let scope_a = scope_create(svc);
        let scope_b = scope_create(svc);

        // 在 scope_a 里注册：订阅 + 工具 + 中间件。
        scope_enter(svc, scope_a);
        let hits_a = AtomicUsize::new(0);
        let _sub_a = api.on("notice", None, count_event, as_user_data(&hits_a));
        let _tool_a = api.register_tool_meta(r#"{"name":"a.tool"}"#);
        let mw_hits = AtomicUsize::new(0);
        let _mw_a = api.use_middleware(
            fuyubot_abi::Stage::PreUpdate,
            count_mw,
            as_user_data(&mw_hits),
        );
        scope_leave(svc);

        // 在 scope_b 里只注册一个工具。
        scope_enter(svc, scope_b);
        let _tool_b = api.register_tool_meta(r#"{"name":"b.tool"}"#);
        scope_leave(svc);

        // 撤销 scope_a。
        scope_drop(svc, scope_a);

        // scope_a 的订阅收不到事件了。
        api.emit("notice", r#"{"x":1}"#).unwrap();
        assert_eq!(
            hits_a.load(Ordering::SeqCst),
            0,
            "scope_a 的订阅应随 drop 撤销"
        );

        // scope_a 的中间件不跑了。
        let mut ctx = MiddlewareContext::new(MwStage::PreUpdate, Value::Null);
        rt.middleware().run(MwStage::PreUpdate, &mut ctx).unwrap();
        assert_eq!(
            mw_hits.load(Ordering::SeqCst),
            0,
            "scope_a 的中间件应随 drop 撤销"
        );

        // 工具列表：a.tool 没了，b.tool 还在。
        let listed = api.list_tools();
        assert!(!listed.contains("a.tool"), "{listed}");
        assert!(
            listed.contains("b.tool"),
            "scope_b 的工具不受影响：{listed}"
        );

        // 重复 drop 同一个 scope 应失败（已失效）。
        let input = format!(r#"{{"handle":{scope_a}}}"#);
        assert!(
            call_scope(svc, "scope_drop", &input).is_some(),
            "已失效的作用域再 drop 应报错"
        );

        rt.end_plugin();
    }

    /// 验收（设计文档 5.2）：owner 整批回滚仍连带子作用域。
    #[test]
    fn owner_rollback_also_removes_child_scopes() {
        let rt = runtime();
        let owner = rt.begin_plugin();
        // SAFETY: rt 活着。
        let api = unsafe { fuyubot_abi::HostApi::from_raw(rt.vtable_ptr()) }.unwrap();
        let svc = scope_service(&rt);

        // 建一个子作用域，往里注册一个工具，但不显式 drop。
        let scope = scope_create(svc);
        scope_enter(svc, scope);
        let _tool = api.register_tool_meta(r#"{"name":"child.tool"}"#);
        scope_leave(svc);

        // 直接回滚 owner（模拟插件被卸载），子作用域的注册应连带消失。
        rt.rollback_owner(owner);

        let listed = api.list_tools();
        assert!(
            !listed.contains("child.tool"),
            "owner 整批回滚应连带子作用域：{listed}"
        );

        rt.end_plugin();
    }

    /// owner 整批回滚时，它的子作用域**条目**也要从表里摘掉（不然只增不减）。
    #[test]
    fn owner_rollback_drops_child_scope_entries() {
        let rt = runtime();
        let owner = rt.begin_plugin();
        let svc = scope_service(&rt);

        let _scope = scope_create(svc);
        assert_eq!(rt.scopes().len(), 1);

        rt.rollback_owner(owner);
        assert_eq!(
            rt.scopes().len(),
            0,
            "owner 回滚后它的子作用域条目不该留在表里"
        );

        rt.end_plugin();
    }

    /// `scope_drop` 之后条目要被摘掉：中间插件反复建/撤作用域（脚本热加载）时不该膨胀。
    #[test]
    fn scope_drop_removes_the_entry_too() {
        let rt = runtime();
        let _owner = rt.begin_plugin();
        let svc = scope_service(&rt);

        let a = scope_create(svc);
        let _b = scope_create(svc);
        assert_eq!(rt.scopes().len(), 2);

        scope_drop(svc, a);
        assert_eq!(rt.scopes().len(), 1, "drop 后条目应被摘掉");

        rt.end_plugin();
    }

    /// 请求归属表必须有界：等不到响应的请求（离线合成号、TDLib 不回的那些）
    /// 不该让它无限增长。
    #[test]
    fn request_owner_table_is_bounded() {
        let rt = runtime();
        let total = REQUEST_OWNER_CAPACITY * 3;
        // 直接登记，绕过 TDLib：这些 id 永远等不到响应，正是会泄漏的那种情形。
        for index in 0..total {
            rt.record_request(index as u64, 1, "test.method".to_owned(), &Value::Null);
        }

        let mut owners = rt.request_owners.lock().unwrap_or_else(|e| e.into_inner());
        assert!(
            owners.len() <= REQUEST_OWNER_CAPACITY,
            "归属表应有界（上限 {REQUEST_OWNER_CAPACITY}），实际 {} 条",
            owners.len()
        );
        assert!(owners.take(0).is_none(), "最老的请求应被挤掉");
        assert!(
            owners.take((total - 1) as u64).is_some(),
            "最新的请求应还在表里"
        );
    }

    /// 撤销是注册顺序的倒序：后注册的先撤（与「卸载是加载顺序的倒序」一致）。
    #[test]
    fn rollback_undoes_registrations_in_reverse_order() {
        use fuyubot_tool::ToolEvent;

        let rt = runtime();
        let owner = rt.begin_plugin();
        // SAFETY: rt 活着。
        let api = unsafe { fuyubot_abi::HostApi::from_raw(rt.vtable_ptr()) }.unwrap();

        let removed = Arc::new(Mutex::new(Vec::new()));
        let _listener = rt.tools().on_change({
            let removed = Arc::clone(&removed);
            move |event| {
                if let ToolEvent::Unregistered(name) = event {
                    removed
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(name.clone());
                }
                Ok(())
            }
        });

        for name in ["t1", "t2", "t3"] {
            let handle = api.register_tool_meta(&format!(r#"{{"name":"{name}"}}"#));
            assert!(handle.is_valid(), "{name} 应注册成功");
        }

        rt.rollback_owner(owner);

        assert_eq!(
            *removed.lock().unwrap_or_else(|e| e.into_inner()),
            vec!["t3".to_owned(), "t2".to_owned(), "t1".to_owned()],
            "撤销顺序应是注册顺序的倒序"
        );

        rt.end_plugin();
    }

    /// `call_service`：宿主侧按名字调插件提供的 service。
    ///
    /// 这条入口是 `fuyubot plugin list` 展平脚本清单的依赖项——`AbiService` 是私有
    /// 类型，CLI 只能走这里。
    #[test]
    fn call_service_invokes_a_plugin_provided_method() {
        let rt = runtime();

        unsafe extern "C" fn echo(_context: *mut c_void, input: *const c_char) -> *const c_char {
            // 把入参回显成 `{"echo":<入参>}`：调用方要能读到返回值。
            let raw = unsafe { CStr::from_ptr(input) }.to_string_lossy();
            host_string(&format!(r#"{{"echo":{raw}}}"#))
        }

        // ServiceMethod 里的 name 是裸指针，编译器不认为它 Sync——包一层
        // 显式声明（与上面的 SCOPE_METHODS 同一套路）：字面量地址稳定、只读。
        struct EchoMethods([ServiceMethod; 1]);
        // SAFETY: name 指向 'static 字面量，func 是纯函数指针。
        unsafe impl Sync for EchoMethods {}
        static METHODS: EchoMethods = EchoMethods([ServiceMethod {
            name: c"echo".as_ptr(),
            func: Some(echo),
        }]);
        let name = CString::new("demo.echo").unwrap();
        let vtable = ServiceVTable {
            name: name.as_ptr(),
            version: 1,
            size: core::mem::size_of::<ServiceVTable>() as u32,
            context: core::ptr::null_mut(),
            methods: METHODS.0.as_ptr(),
            method_count: METHODS.0.len(),
        };
        {
            // SAFETY: rt 活着；vtable 比注册活得久（本作用域内一直有效）。
            let api = unsafe { fuyubot_abi::HostApi::from_raw(rt.vtable_ptr()) }.unwrap();
            let handle = unsafe { api.provide_service("demo.echo", &vtable) };
            assert!(handle.is_valid());

            // 入参 JSON 原样进、返回值当场拷成 String。
            let out = rt
                .call_service("demo.echo", "echo", r#"{"x":1}"#)
                .expect("echo 必须调得通");
            assert_eq!(out, r#"{"echo":{"x":1}}"#);

            // 不存在的服务 / 不存在的方法：报错指路，不 panic。
            let error = rt.call_service("nope", "echo", "{}").unwrap_err();
            assert!(error.contains("nope"), "{error}");
            let error = rt.call_service("demo.echo", "nope", "{}").unwrap_err();
            assert!(error.contains("nope"), "{error}");
        }
        // vtable 是局部变量，活到本函数末尾——比所有调用都久。
    }

    /// 服务方法能回问「谁在调我」：宿主按本线程作用域栈顶给出**权威**插件名。
    ///
    /// `/help` 的命令归属就靠这条路——`register` 里问一句 `current_plugin()`，
    /// 插件伪造不成别人（与 `set_config` 的越权判定是同一份映射）。
    #[test]
    fn service_method_can_ask_who_is_calling() {
        let rt = runtime();
        let owner = rt.begin_plugin();
        rt.register_owner_name(owner, "demo.caller");

        unsafe extern "C" fn whoami(context: *mut c_void, _input: *const c_char) -> *const c_char {
            // SAFETY: 测试把 `context` 指向下面自持的 `HostApi`，活得比调用久。
            let api = unsafe { &*(context as *const fuyubot_abi::HostApi) };
            host_string(api.current_plugin().as_deref().unwrap_or(""))
        }

        struct WhoamiMethods([ServiceMethod; 1]);
        // SAFETY: name 指向 'static 字面量，func 是纯函数指针。
        unsafe impl Sync for WhoamiMethods {}
        static METHODS: WhoamiMethods = WhoamiMethods([ServiceMethod {
            name: c"who".as_ptr(),
            func: Some(whoami),
        }]);
        let name = CString::new("demo.whoami").unwrap();
        // SAFETY: rt 活着；下面的 vtable / api 都比注册活得久。
        let api = unsafe { fuyubot_abi::HostApi::from_raw(rt.vtable_ptr()) }.unwrap();
        let vtable = ServiceVTable {
            name: name.as_ptr(),
            version: 1,
            size: core::mem::size_of::<ServiceVTable>() as u32,
            context: (&api as *const fuyubot_abi::HostApi)
                .cast_mut()
                .cast::<c_void>(),
            methods: METHODS.0.as_ptr(),
            method_count: METHODS.0.len(),
        };
        // SAFETY: 契约同上；共享注册落在根上，`HostRuntime::call_service` 才查得到。
        let handle = unsafe { api.provide_service_shared("demo.whoami", &vtable) };
        assert!(handle.is_valid());

        // 当前线程栈顶是 `demo.caller`：服务方法问到的必须是它，而不是宿主自己。
        assert_eq!(
            rt.call_service("demo.whoami", "who", "{}").unwrap(),
            "demo.caller"
        );

        drop(handle);
        rt.rollback_owner(owner);
        rt.end_plugin();
        rt.unregister_owner_name(owner);
    }

    /// ABI v6：服务调用经过宿主，**调用方自己的线程**上也认得出它是谁。
    ///
    /// 这正是 v5 的缺口：`current_plugin` 只读线程作用域栈，而插件自己起的线程上没有
    /// 栈顶，于是被调方（`/help` 的 `register`）只拿得到 `{"ok":true,"plugin":null}`。
    /// v6 让宿主中转这次调用，用调用方那份 `host_ctx`（`PluginHost.owner`）记下身份，
    /// 与线程无关、也不可能被调用方自报。
    #[test]
    fn a_service_call_from_the_callers_own_thread_is_still_attributed() {
        let rt = runtime();
        let owner = rt.begin_plugin();
        rt.register_owner_name(owner, "demo.caller");

        unsafe extern "C" fn whoami(context: *mut c_void, _input: *const c_char) -> *const c_char {
            // SAFETY: 测试把 `context` 指向下面自持的 `HostApi`，活得比调用久。
            let api = unsafe { &*(context as *const fuyubot_abi::HostApi) };
            host_string(api.current_plugin().as_deref().unwrap_or(""))
        }

        struct WhoamiMethods([ServiceMethod; 1]);
        // SAFETY: name 指向 'static 字面量，func 是纯函数指针。
        unsafe impl Sync for WhoamiMethods {}
        static METHODS: WhoamiMethods = WhoamiMethods([ServiceMethod {
            name: c"who".as_ptr(),
            func: Some(whoami),
        }]);
        let name = CString::new("demo.whoami.v6").unwrap();
        // SAFETY: rt 活着；下面的 vtable / api 都比注册活得久。
        let api_provider = unsafe { fuyubot_abi::HostApi::from_raw(rt.vtable_ptr()) }.unwrap();
        let vtable = ServiceVTable {
            name: name.as_ptr(),
            version: 1,
            size: core::mem::size_of::<ServiceVTable>() as u32,
            context: (&api_provider as *const fuyubot_abi::HostApi)
                .cast_mut()
                .cast::<c_void>(),
            methods: METHODS.0.as_ptr(),
            method_count: METHODS.0.len(),
        };
        // SAFETY: 契约同上；共享注册落在根上，调用方沿父链查得到。
        let handle = unsafe { api_provider.provide_service_shared("demo.whoami.v6", &vtable) };
        assert!(handle.is_valid());

        // 调用方拿**自己那份** `host_ctx`：里面烧着它的 owner，与线程无关。
        let caller_host = rt.plugin_host(owner);
        // SAFETY: caller_host 活到线程 join 之后。
        let api_caller =
            unsafe { fuyubot_abi::HostApi::from_raw(caller_host.vtable_ptr()) }.unwrap();

        // 换一条线：`begin_plugin` 压的作用域栈是**本线程**的，新线程上没有那一层。
        let reply =
            std::thread::spawn(move || api_caller.call_service("demo.whoami.v6", "who", "{}"))
                .join()
                .expect("调用线程不该 panic");
        assert_eq!(
            reply.expect("服务该调得通"),
            "demo.caller",
            "v6：身份来自 host_ctx，不依赖线程作用域栈"
        );

        drop(handle);
        rt.rollback_owner(owner);
        rt.end_plugin();
        rt.unregister_owner_name(owner);
    }

    /// 身份 service：登录前是空的，`set_self_user` 之后给出用户名。
    ///
    /// 名字必须与 `fuyubot-simplify` 里 `on_command` 用的那个常量一致——两边各写一份
    /// 字符串字面量，漏改一边就是「命令过滤静默失效」。
    #[test]
    fn identity_service_reports_the_logged_in_user() {
        let rt = runtime();
        assert_eq!(IDENTITY_SERVICE, fuyubot_simplify::IDENTITY_SERVICE);

        // 还没登录 / 没接 TDLib：空表 + null（调用方据此当「身份未知」）。
        assert_eq!(
            rt.call_service(IDENTITY_SERVICE, "usernames", "{}")
                .unwrap(),
            "[]"
        );
        assert_eq!(
            rt.call_service(IDENTITY_SERVICE, "me", "{}").unwrap(),
            "null"
        );

        // 登录之后：用户名在 `usernames.active_usernames`（约束 42）。
        rt.set_self_user(json!({
            "id": 7,
            "usernames": {
                "@type": "usernames",
                "active_usernames": ["FuyuBot", "FuyuBotBackup"],
            },
            "type": { "@type": "userTypeBot" },
        }));
        assert_eq!(
            rt.call_service(IDENTITY_SERVICE, "usernames", "{}")
                .unwrap(),
            r#"["FuyuBot","FuyuBotBackup"]"#
        );
        assert!(
            rt.call_service(IDENTITY_SERVICE, "me", "{}")
                .unwrap()
                .contains(r#""id":7"#)
        );
    }

    /// `getMe` 只是登录那一刻的快照：之后的 `updateUser`（id 是自己那条）要刷新它，
    /// 别人的 `updateUser` 一概不碰。
    #[test]
    fn self_user_follows_update_user_updates() {
        let rt = runtime();
        rt.set_self_user(json!({
            "id": 7,
            "usernames": { "@type": "usernames", "active_usernames": ["OldName"] },
        }));
        assert_eq!(rt.self_usernames(), vec!["OldName".to_owned()]);

        // 别人的：不动。
        rt.publish_update(&json!({
            "@type": "updateUser",
            "user": { "id": 8, "usernames": { "active_usernames": ["SomeoneElse"] } },
        }))
        .unwrap();
        assert_eq!(rt.self_usernames(), vec!["OldName".to_owned()]);

        // 自己的：换成新值（改用户名 / 改名字都走这条）。
        rt.publish_update(&json!({
            "@type": "updateUser",
            "user": {
                "id": 7,
                "first_name": "New",
                "usernames": { "active_usernames": ["NewName"] },
            },
        }))
        .unwrap();
        assert_eq!(rt.self_usernames(), vec!["NewName".to_owned()]);
        assert_eq!(
            rt.self_user().unwrap().pointer("/first_name"),
            Some(&json!("New"))
        );

        // 没登录时收到 `updateUser`：没有「我自己」可比，什么都不做。
        let fresh = runtime();
        fresh
            .publish_update(&json!({
                "@type": "updateUser",
                "user": { "id": 7, "usernames": { "active_usernames": ["X"] } },
            }))
            .unwrap();
        assert_eq!(fresh.self_user(), None);
    }

    /// 作用域的两种落点：`provide_service` 写调用者自己的 child，`provide_service_shared` 写根。
    ///
    /// 验四件事：两个插件注册同名服务**不冲突**；私有服务落在 child 里、根上看不见；
    /// 共享服务落在根上；child 沿父链能看见共享的那份。
    ///
    /// 只验落点（provide 写哪张表）。**真实可见性**由集成测试
    /// `a_plugin_reads_back_the_service_it_just_provided` 验——那条得真 dlopen 一个
    /// 插件，因为 owner 栈是线程局部的、由加载路径上的 `plugin_scope()` 压入。
    /// 手工在这里叠两层 scope 会和那个栈打架（查过，不是产品路径的问题）。
    #[test]
    fn plugin_services_are_private_until_shared() {
        unsafe extern "C" fn echo(_ctx: *mut c_void, input: *const c_char) -> *const c_char {
            input
        }
        let method = CString::new("echo").unwrap();
        let name = CString::new("demo.svc").unwrap();
        let methods = [ServiceMethod {
            name: method.as_ptr(),
            func: Some(echo),
        }];
        // 用 `version` 区分两份 vtable，指针比较才有意义。
        let service = |tag: u32| ServiceVTable {
            name: name.as_ptr(),
            version: tag,
            size: core::mem::size_of::<ServiceVTable>() as u32,
            context: core::ptr::null_mut(),
            methods: methods.as_ptr(),
            method_count: methods.len(),
        };
        let service_a = service(1);
        let service_b = service(2);
        let shared = service(3);

        let rt = HostRuntime::new(Context::new()).unwrap();
        let host = rt.vtable.host_ctx;
        let provide = rt.vtable.provide_service.unwrap();
        let provide_shared = rt.vtable.provide_service_shared.unwrap();

        let scope_a = rt.plugin_scope();
        assert_eq!(
            SCOPE_STACK.with(|s| s.borrow().len()),
            2,
            "begin_plugin 应当往底(0,0)之上压一层"
        );
        // SAFETY: host 来自本运行时；几个 vtable 都活到函数末尾。
        let handle_a = unsafe { provide(host, name.as_ptr(), &service_a) };
        assert_ne!(handle_a.id, 0, "插件 A 注册私有服务应当成功");

        // 第二个插件注册**同名**服务：落点不同，所以不该冲突。
        // 下划线前缀不是「不用」——是刻意持有到函数末尾，提前 drop 会把栈弹掉。
        let _scope_b = rt.plugin_scope();
        // SAFETY: 同上。
        let handle_b = unsafe { provide(host, name.as_ptr(), &service_b) };
        assert_ne!(
            handle_b.id, 0,
            "两个插件注册同名服务不该冲突——它们落在各自的作用域里"
        );

        // 私有：落在插件自己的 child 上，**根上看不见**。
        let child = rt
            .plugin_contexts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&scope_a.owner())
            .cloned()
            .expect("begin_plugin 应当给这个 owner 建 child");
        assert!(
            child.resolve::<AbiService>("demo.svc").is_ok(),
            "私有服务应当落在插件自己的 child 里"
        );
        assert!(
            rt.ctx.resolve::<AbiService>("demo.svc").is_err(),
            "私有服务不该漏到根上——这正是「默认私有」的全部含义"
        );

        // 共享：显式写根，child 沿父链也看得见。
        // SAFETY: host 来自本运行时；vtable 活到函数末尾。
        let handle_shared = unsafe { provide_shared(host, c"demo.shared".as_ptr(), &shared) };
        assert_ne!(handle_shared.id, 0, "共享注册应当成功");
        assert!(
            rt.ctx.resolve::<AbiService>("demo.shared").is_ok(),
            "共享服务应当在根上"
        );
        assert!(
            child.resolve::<AbiService>("demo.shared").is_ok(),
            "child 沿父链也该看见共享服务"
        );
    }

    /// ABI v4 核心：`register_tool` 带 handler 之后 `call_tool` 真的调到它。
    ///
    /// 这是缺口 6 的正解——之前 `register_tool` 只带元数据，`call_tool` 一律报
    /// `NoHandler`。现在 handler 是 C 函数指针 + user_data，宿主侧包装后存进
    /// `ToolDescriptor`，`call_tool` 直接调。
    #[test]
    fn a_registered_handler_is_invoked_by_call_tool() {
        let rt = runtime();

        // handler 把入参回显成 `{"echo":<入参>}`。
        unsafe extern "C-unwind" fn echo(
            user_data: *mut c_void,
            input: *const c_char,
        ) -> *const c_char {
            let counter = unsafe { &*(user_data as *const AtomicUsize) };
            counter.fetch_add(1, Ordering::SeqCst);
            let raw = unsafe { CStr::from_ptr(input) }.to_string_lossy();
            host_string(&format!(r#"{{"echo":{raw}}}"#))
        }

        let counter = Arc::new(AtomicUsize::new(0));
        let handle = unsafe {
            (rt.vtable.register_tool.unwrap())(
                rt.vtable.host_ctx,
                c"{\"name\":\"v4.echo\",\"description\":\"回显\",\"input_schema\":{\"type\":\"object\"}}".as_ptr(),
                Some(echo),
                Arc::as_ptr(&counter) as *mut c_void,
            )
        };
        assert!(handle.is_valid(), "带 handler 的注册应该成功");

        // 走 VTable 调用：handler 必须被调到。
        let request_id = unsafe {
            (rt.vtable.call_tool.unwrap())(
                rt.vtable.host_ctx,
                c"v4.echo".as_ptr(),
                c"{\"x\":1}".as_ptr(),
            )
        };
        assert!(request_id.is_valid(), "call_tool 应该发出请求");
        assert_eq!(counter.load(Ordering::SeqCst), 1, "handler 必须被调到一次");

        // 同步结果：同一个工具，`call_tool_result` 当场把 output 交回来。
        let raw = unsafe {
            (rt.vtable.call_tool_result.unwrap())(
                rt.vtable.host_ctx,
                c"v4.echo".as_ptr(),
                c"{\"x\":1}".as_ptr(),
            )
        };
        assert!(!raw.is_null(), "结果不能是 NULL");
        let text = unsafe { CStr::from_ptr(raw) }.to_string_lossy();
        let value: Value = serde_json::from_str(&text).expect("结果是 JSON");
        assert_eq!(value["echo"]["x"], 1, "{value}");
        assert_eq!(counter.load(Ordering::SeqCst), 2, "同步调用也要走 handler");

        // 不存在的工具：返回 `{"error": …}`，不是 NULL。
        let raw = unsafe {
            (rt.vtable.call_tool_result.unwrap())(
                rt.vtable.host_ctx,
                c"no.such.tool".as_ptr(),
                c"{}".as_ptr(),
            )
        };
        let text = unsafe { CStr::from_ptr(raw) }.to_string_lossy();
        let value: Value = serde_json::from_str(&text).expect("错误也是 JSON");
        assert!(value["error"].as_str().is_some(), "{value}");
    }

    /// `list_plugins` 返回宿主那份报告（与 `fuyubot.host.plugins` service 同源）。
    #[test]
    fn list_plugins_returns_the_host_report() {
        let rt = runtime();
        let raw = unsafe { (rt.vtable.list_plugins.unwrap())(rt.vtable.host_ctx) };
        assert!(!raw.is_null(), "list_plugins 该返回 JSON");
        let text = unsafe { CStr::from_ptr(raw) }.to_string_lossy();
        assert!(text.contains("plugins"), "该是插件报告 JSON：{text}");
    }
}
