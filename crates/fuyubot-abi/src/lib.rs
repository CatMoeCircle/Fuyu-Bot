//! 插件 ABI 契约：宿主 VTable、插件导出符号与 `define_plugin!` 宏。
//!
//! 本 crate **不依赖 `fuyubot-core`**（也不依赖任何机制 crate）：插件看见的只有 C ABI。
//! 所有跨边界的东西都是 `#[repr(C)]` 的 POD、不透明句柄或 NUL 结尾字符串。
//!
//! ## 这个 ABI 的两条硬规矩
//!
//! 1. **每个 VTable 函数都收 `host: *mut c_void` 作为第一个参数**。Rust 的裸函数指针没有
//!    捕获环境，`HostApiVTable` 里放不了闭包；要让函数找到宿主实例，只有「进程级全局」
//!    或「显式上下文参数」两条路。选后者：全局会让一个进程只能有一个宿主，测试也没法并行。
//!    插件作者不用管它——[`HostApi`] 会把这个指针带上。
//! 2. **函数指针字段一律是 `Option<...>`**。C 侧传 NULL 完全合法（老宿主没有新函数、
//!    插件不关心某个能力），而 Rust 里非 `Option` 的函数指针字段收到 NULL 就是 UB。
//!    `Option<fn>` 与裸指针同大小同布局（niche 优化），ABI 上没有任何代价。

use core::ffi::{c_char, c_void};
use std::ffi::{CStr, CString};

/// ABI 版本。字段只增不改，新增一律追加在尾部。
pub const ABI_VERSION: u32 = 2;

/// cbindgen 生成的头文件**原文**。
///
/// 嵌进来是为了让头文件跟着 crate 一起分发：C/C++ 插件作者（以及
/// `fuyubot-sdk init --lang c`）不用先跑一次构建才能拿到 `fuyubot_plugin.h`。
pub const PLUGIN_HEADER: &str = include_str!(concat!(env!("OUT_DIR"), "/fuyubot_plugin.h"));

/// 插件入口符号名。
pub const SYM_PLUGIN_INIT: &[u8] = b"plugin_init";
pub const SYM_PLUGIN_START: &[u8] = b"plugin_start";
pub const SYM_PLUGIN_STOP: &[u8] = b"plugin_stop";
pub const SYM_PLUGIN_FREE_STRING: &[u8] = b"plugin_free_string";

// ── 事件通道约定 ───────────────────────────────────────────────────
//
// 这三个名字是宿主与插件之间的约定，所以由 ABI crate 定，两边都从这里取。

/// 更新的**兜底通道**：每条更新都会同时发到「具体的 `@type` 通道」和这条上。
pub const UPDATE_EVENT: &str = "update";

/// 响应通道：负载是 TDLib 原始 JSON 加一个数值型 `request_id`。
/// 它只发给发起该请求的那个插件。
pub const RESPONSE_EVENT: &str = "response";

/// 过滤器里用来**选通道**的特殊键：它不参与负载匹配。
///
/// `wait_for_update` 靠它决定订阅哪条通道，例如
/// `{"event_type":"response","request_id":5}` 表示「等 request_id 为 5 的那条响应」。
pub const FILTER_EVENT_TYPE_KEY: &str = "event_type";

/// 日志级别，数值与宿主侧一致。
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Error = 1,
    Warn = 2,
    Info = 3,
    Debug = 4,
    Trace = 5,
}

impl LogLevel {
    /// 非法数值按 `Info` 处理：ABI 边界的垃圾值不该让插件炸。
    pub fn from_u32(value: u32) -> Self {
        match value {
            1 => Self::Error,
            2 => Self::Warn,
            4 => Self::Debug,
            5 => Self::Trace,
            _ => Self::Info,
        }
    }
}

/// 中间件挂载点。数值必须与 `fuyubot-middleware::Stage` 一一对应。
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Stage {
    PreUpdate = 0,
    PreRoute = 1,
    InPlugin = 2,
    PostResponse = 3,
    PreToolCall = 4,
    PostToolCall = 5,
}

impl Stage {
    pub const ALL: [Stage; 6] = [
        Stage::PreUpdate,
        Stage::PreRoute,
        Stage::InPlugin,
        Stage::PostResponse,
        Stage::PreToolCall,
        Stage::PostToolCall,
    ];

    /// 非法数值返回 `None`：调用方该报错而不是猜一个阶段。
    pub fn from_u32(value: u32) -> Option<Self> {
        match value {
            0 => Some(Self::PreUpdate),
            1 => Some(Self::PreRoute),
            2 => Some(Self::InPlugin),
            3 => Some(Self::PostResponse),
            4 => Some(Self::PreToolCall),
            5 => Some(Self::PostToolCall),
            _ => None,
        }
    }
}

/// ABI 层的不透明句柄。宿主拿到它只当编号用。
///
/// **这五个句柄必须写成显式 `struct`，不能用 `macro_rules!` 批量生成**：
/// cbindgen 不展开宏，宏里生成的类型不会出现在 C 头文件里，而 `HostApiVTable`
/// 的字段又用到它们——头文件就会「用了没声明的类型」，`-fsyntax-only` 直接报错。
/// 只有 impl 那一半仍走宏（impl 不进头文件，没这个问题）。
///
/// `#[repr(C)] + 单个 u64` 是硬要求：句柄要能按值跨 ABI（`const _` 里有断言）。
macro_rules! abi_handle_impls {
    ($($name:ident),* $(,)?) => {
        $(
            impl $name {
                /// 无效句柄：注册失败时返回它。
                pub const INVALID: Self = Self { id: 0 };

                pub fn is_valid(self) -> bool {
                    self.id != 0
                }
            }
        )*
    };
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct SubscriptionHandle {
    pub id: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct ToolHandle {
    pub id: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct ServiceHandle {
    pub id: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct MiddlewareHandle {
    pub id: u64,
}

/// 请求标识：异步/跨语言调用做关联用。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct RequestId {
    pub id: u64,
}

abi_handle_impls!(
    SubscriptionHandle,
    ToolHandle,
    ServiceHandle,
    MiddlewareHandle,
    RequestId,
);

impl std::fmt::Display for RequestId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "#{}", self.id)
    }
}

/// [`HostApiVTable::wait_for_update`] 的返回值：既报告结果也带回负载。
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct WaitHandle {
    /// 0 表示这次等待没建立起来。
    pub id: u64,
    /// 1 = 等到了匹配的更新；0 = 超时或被取消。
    pub matched: u32,
    pub _reserved: u32,
    /// 匹配到的更新 JSON。**宿主持有**，只在下一次宿主调用之前有效，
    /// 插件要用就得自己拷走（不要试图释放它）。
    pub payload_json: *const c_char,
}

impl WaitHandle {
    pub const INVALID: Self = Self {
        id: 0,
        matched: 0,
        _reserved: 0,
        payload_json: core::ptr::null(),
    };

    pub fn is_valid(self) -> bool {
        self.id != 0
    }

    pub fn did_match(self) -> bool {
        self.matched != 0
    }
}

/// 事件回调：`payload_json` 是宿主临时的 JSON 字符串。返回 0 表示成功。
///
/// 用 `C-unwind` 而不是 `C`：`extern "C"` 下 panic 会在函数边界直接 abort，
/// 宿主**来不及** `catch_unwind`；`C-unwind` 允许 unwind 穿过边界，宿主就能把它接住、
/// 记一条日志、然后继续跑下一条更新。对 C/C++ 插件而言两者 ABI 一致。
pub type EventCallbackFn =
    unsafe extern "C-unwind" fn(user_data: *mut c_void, payload_json: *const c_char) -> i32;

/// 中间件回调：返回 0 表示继续往下走，非 0 表示中断本阶段。同样用 `C-unwind`。
pub type MiddlewareCallbackFn = unsafe extern "C-unwind" fn(
    user_data: *mut c_void,
    stage: u32,
    payload_json: *const c_char,
) -> i32;

/// 服务方法的实现：入参 JSON → 出参 JSON。
///
/// 返回的字符串由**提供方**持有，调用方读完即弃（不要 free 别人的指针）。
pub type ServiceMethodFn =
    unsafe extern "C" fn(context: *mut c_void, input_json: *const c_char) -> *const c_char;

/// 服务的一个方法。
///
/// `func` 的签名**故意内联**而不是写 `Option<ServiceMethodFn>`：
/// cbindgen 只在签名内联时才把 `Option<fn>` 生成成「可空的函数指针」，
/// 走类型别名它会退化成 `struct Option_ServiceMethodFn`——一个不完整的结构体
/// 类型被按值传参，C 编译器会直接报错。两种写法在 Rust 里是**同一个类型**，
/// 所以这不改变 ABI（头文件里仍然有 `ServiceMethodFn` 那个 typedef）。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ServiceMethod {
    /// 方法名，NUL 结尾 UTF-8。
    pub name: *const c_char,
    pub func: Option<
        unsafe extern "C" fn(context: *mut c_void, input_json: *const c_char) -> *const c_char,
    >,
}

/// 服务描述：一个名字 + 一组「名字 → C 函数」的方法。
///
/// 跨语言服务就是这么多：宿主不需要知道服务的 Rust 类型，按名字调方法即可。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ServiceVTable {
    pub name: *const c_char,
    pub version: u32,
    pub size: u32,
    /// 提供方的实例数据，会原样传给每个 [`ServiceMethodFn`]。
    pub context: *mut c_void,
    pub methods: *const ServiceMethod,
    pub method_count: usize,
}

// ── 宿主 VTable ────────────────────────────────────────────────────

/// 宿主传给插件的函数表。
///
/// 版本协商靠 `version`；`size` 让插件判断宿主的表有没有自己需要的那几个字段
/// （老宿主 + 新插件时，读越界字段就是 UB，先比 `size` 再读才安全）。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct HostApiVTable {
    /// 第一个字段，必须等于 [`ABI_VERSION`]。
    pub version: u32,
    /// `size_of::<HostApiVTable>()`，由宿主填。
    pub size: u32,
    /// 宿主实例指针，每个函数都原样回传。
    pub host_ctx: *mut c_void,

    // 回调参数内联签名，理由见 `ServiceMethod::func`（cbindgen 的 `Option<fn>` 限制）。
    pub on: Option<
        unsafe extern "C" fn(
            host: *mut c_void,
            update_type: *const c_char,
            filter_json: *const c_char,
            callback: Option<unsafe extern "C-unwind" fn(*mut c_void, *const c_char) -> i32>,
            user_data: *mut c_void,
        ) -> SubscriptionHandle,
    >,
    pub off: Option<unsafe extern "C" fn(host: *mut c_void, handle: SubscriptionHandle)>,
    pub emit: Option<
        unsafe extern "C" fn(
            host: *mut c_void,
            event_type: *const c_char,
            payload_json: *const c_char,
        ) -> i32,
    >,
    pub register_tool: Option<
        unsafe extern "C" fn(host: *mut c_void, descriptor_json: *const c_char) -> ToolHandle,
    >,
    pub call_tool: Option<
        unsafe extern "C" fn(
            host: *mut c_void,
            name: *const c_char,
            input_json: *const c_char,
        ) -> RequestId,
    >,
    pub list_tools: Option<unsafe extern "C" fn(host: *mut c_void) -> *const c_char>,
    pub provide_service: Option<
        unsafe extern "C" fn(
            host: *mut c_void,
            name: *const c_char,
            vtable: *const ServiceVTable,
        ) -> ServiceHandle,
    >,
    pub get_service: Option<
        unsafe extern "C" fn(host: *mut c_void, name: *const c_char) -> *const ServiceVTable,
    >,
    pub td_invoke:
        Option<unsafe extern "C" fn(host: *mut c_void, method_json: *const c_char) -> RequestId>,
    pub td_invoke_simplified: Option<
        unsafe extern "C" fn(
            host: *mut c_void,
            method_name: *const c_char,
            params_json: *const c_char,
        ) -> RequestId,
    >,
    /// **同步**执行一个不需要网络的 TDLib 方法（`td_json_client_execute`），返回响应 JSON。
    ///
    /// 与 `td_invoke*` 的区别是「不等响应通道」：调用当场就有结果，所以可以在**回调里**
    /// 安全使用（`td_invoke` + `wait_for_update` 那套会卡住投递线程）。
    /// TDLib 允许这么调的方法只限不碰服务器的那些——`parseTextEntities`、
    /// `getTextEntities`、`getFileMimeType` 之类；发消息、编辑消息**不能**走它。
    ///
    /// 返回的字符串是宿主线程局部缓冲（见 [`HostApi::td_execute`]），读完即弃。
    pub td_execute: Option<
        unsafe extern "C" fn(host: *mut c_void, request_json: *const c_char) -> *const c_char,
    >,
    pub use_middleware: Option<
        unsafe extern "C" fn(
            host: *mut c_void,
            stage: u32,
            callback: Option<unsafe extern "C-unwind" fn(*mut c_void, u32, *const c_char) -> i32>,
            user_data: *mut c_void,
        ) -> MiddlewareHandle,
    >,
    pub wait_for_update: Option<
        unsafe extern "C" fn(
            host: *mut c_void,
            filter_json: *const c_char,
            timeout_ms: u64,
        ) -> WaitHandle,
    >,
    pub cancel_wait: Option<unsafe extern "C" fn(host: *mut c_void, handle: WaitHandle)>,
    pub log: Option<unsafe extern "C" fn(host: *mut c_void, level: u32, message: *const c_char)>,
}

/// VTable 就是「两个 u32 + 16 个指针」；布局一改，这里立刻编译不过。
const _: () = {
    use core::mem::{offset_of, size_of};

    assert!(offset_of!(HostApiVTable, version) == 0);
    assert!(offset_of!(HostApiVTable, size) == 4);
    assert!(offset_of!(HostApiVTable, host_ctx) == 8);
    assert!(size_of::<Option<unsafe extern "C" fn()>>() == size_of::<*const c_void>());
    assert!(size_of::<HostApiVTable>() == 2 * size_of::<u32>() + 16 * size_of::<*const c_void>());

    // 句柄要能按值跨 ABI：必须恰好一个 u64。
    assert!(size_of::<SubscriptionHandle>() == size_of::<u64>());
    assert!(size_of::<ToolHandle>() == size_of::<u64>());
    assert!(size_of::<ServiceHandle>() == size_of::<u64>());
    assert!(size_of::<MiddlewareHandle>() == size_of::<u64>());
    assert!(size_of::<RequestId>() == size_of::<u64>());

    assert!(offset_of!(ServiceVTable, name) == 0);
    assert!(offset_of!(PluginHandle, abi_version) == 0);
    // 失败的 PluginHandle 就是全零版本号，判据必须稳。
    assert!(PluginHandle::FAILED.abi_version == 0);
};

/// 边界校验失败的原因。
#[derive(Debug, thiserror::Error)]
pub enum AbiError {
    #[error("宿主传了空 VTable 指针")]
    NullVTable,
    #[error("ABI 版本不匹配：插件按 {expected} 编译，宿主是 {found}")]
    VersionMismatch { expected: u32, found: u32 },
    #[error("宿主 VTable 太小：需要 {needed} 字节，实际只有 {found} 字节")]
    VTableTooSmall { needed: u32, found: u32 },
    #[error("宿主 VTable 缺了必需的函数 {0}")]
    MissingFunction(&'static str),
    #[error("字符串不是合法的 NUL 结尾 UTF-8")]
    BadString,
    #[error("宿主调用失败（返回值 {0}）")]
    HostCall(i32),
}

// ── 插件侧的宿主门面 ───────────────────────────────────────────────

/// 插件侧的宿主句柄。
///
/// 内部是**已经解包过**的函数指针：构造时校验一次，之后每次调用都不再碰 `Option`，
/// 也就没有「校验漏了导致 panic」的路径。
#[derive(Clone, Copy)]
pub struct HostApi {
    host_ctx: *mut c_void,
    version: u32,
    on: unsafe extern "C" fn(
        *mut c_void,
        *const c_char,
        *const c_char,
        Option<EventCallbackFn>,
        *mut c_void,
    ) -> SubscriptionHandle,
    off: unsafe extern "C" fn(*mut c_void, SubscriptionHandle),
    emit: unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> i32,
    register_tool: unsafe extern "C" fn(*mut c_void, *const c_char) -> ToolHandle,
    call_tool: unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> RequestId,
    list_tools: unsafe extern "C" fn(*mut c_void) -> *const c_char,
    provide_service:
        unsafe extern "C" fn(*mut c_void, *const c_char, *const ServiceVTable) -> ServiceHandle,
    get_service: unsafe extern "C" fn(*mut c_void, *const c_char) -> *const ServiceVTable,
    td_invoke: unsafe extern "C" fn(*mut c_void, *const c_char) -> RequestId,
    td_invoke_simplified:
        unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> RequestId,
    td_execute: unsafe extern "C" fn(*mut c_void, *const c_char) -> *const c_char,
    use_middleware: unsafe extern "C" fn(
        *mut c_void,
        u32,
        Option<MiddlewareCallbackFn>,
        *mut c_void,
    ) -> MiddlewareHandle,
    wait_for_update: unsafe extern "C" fn(*mut c_void, *const c_char, u64) -> WaitHandle,
    cancel_wait: unsafe extern "C" fn(*mut c_void, WaitHandle),
    log: unsafe extern "C" fn(*mut c_void, u32, *const c_char),
}

impl std::fmt::Debug for HostApi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostApi")
            .field("version", &self.version)
            .field("host_ctx", &self.host_ctx)
            .finish_non_exhaustive()
    }
}

// SAFETY: 函数指针本身是 Send + Sync；唯一的非 Send 字段是不透明的 `host_ctx`，
// 由宿主保证其线程安全（宿主运行时就是设计成多线程共享的）。插件把 `HostApi`
// 放进自己的状态、在宿主的回调线程里调用它，是这个 ABI 的正常用法。
unsafe impl Send for HostApi {}
unsafe impl Sync for HostApi {}

impl HostApi {
    /// 校验并拆开宿主传来的 VTable。
    ///
    /// # Safety
    /// `vtable` 必须指向一个活着的、由宿主构造的 [`HostApiVTable`]。
    pub unsafe fn from_raw(vtable: *const HostApiVTable) -> Result<Self, AbiError> {
        if vtable.is_null() {
            return Err(AbiError::NullVTable);
        }
        // SAFETY: 调用方保证指针有效；这里只读头两个字段。
        let head = unsafe { &*vtable };
        if head.version != ABI_VERSION {
            return Err(AbiError::VersionMismatch {
                expected: ABI_VERSION,
                found: head.version,
            });
        }
        let needed = core::mem::size_of::<HostApiVTable>() as u32;
        if head.size < needed {
            return Err(AbiError::VTableTooSmall {
                needed,
                found: head.size,
            });
        }

        // 解包每个字段；缺一个就是宿主实现不完整，在这里就报出来。
        macro_rules! required {
            ($field:ident) => {
                head.$field
                    .ok_or(AbiError::MissingFunction(stringify!($field)))?
            };
        }

        Ok(Self {
            host_ctx: head.host_ctx,
            version: head.version,
            on: required!(on),
            off: required!(off),
            emit: required!(emit),
            register_tool: required!(register_tool),
            call_tool: required!(call_tool),
            list_tools: required!(list_tools),
            provide_service: required!(provide_service),
            get_service: required!(get_service),
            td_invoke: required!(td_invoke),
            td_invoke_simplified: required!(td_invoke_simplified),
            td_execute: required!(td_execute),
            use_middleware: required!(use_middleware),
            wait_for_update: required!(wait_for_update),
            cancel_wait: required!(cancel_wait),
            log: required!(log),
        })
    }

    pub fn version(&self) -> u32 {
        self.version
    }

    /// 宿主实例指针。一般用不到，每个方法都会自动带上它。
    pub fn host_ctx(&self) -> *mut c_void {
        self.host_ctx
    }

    pub fn log(&self, level: LogLevel, message: &str) {
        let Ok(text) = CString::new(message) else {
            return;
        };
        // SAFETY: 构造时已校验非空；host_ctx 由宿主保证有效。
        unsafe { (self.log)(self.host_ctx, level as u32, text.as_ptr()) };
    }

    /// 订阅事件。`filter_json` 是 `{"字段路径": 期望值}`，`None` 表示全收。
    pub fn on(
        &self,
        event_type: &str,
        filter_json: Option<&str>,
        callback: EventCallbackFn,
        user_data: *mut c_void,
    ) -> SubscriptionHandle {
        let Ok(event_type) = CString::new(event_type) else {
            return SubscriptionHandle::INVALID;
        };
        let filter = json_cstring(filter_json);
        // SAFETY: 同 log。
        unsafe {
            (self.on)(
                self.host_ctx,
                event_type.as_ptr(),
                filter.as_ref().map_or(core::ptr::null(), |f| f.as_ptr()),
                Some(callback),
                user_data,
            )
        }
    }

    pub fn off(&self, handle: SubscriptionHandle) {
        // SAFETY: 句柄由宿主发放。
        unsafe { (self.off)(self.host_ctx, handle) };
    }

    pub fn emit(&self, event_type: &str, payload_json: &str) -> Result<(), AbiError> {
        let event_type = CString::new(event_type).map_err(|_| AbiError::BadString)?;
        let payload = CString::new(payload_json).map_err(|_| AbiError::BadString)?;
        // SAFETY: 同 log。
        let code = unsafe { (self.emit)(self.host_ctx, event_type.as_ptr(), payload.as_ptr()) };
        if code == 0 {
            Ok(())
        } else {
            Err(AbiError::HostCall(code))
        }
    }

    /// 注册工具。`descriptor_json` 是 `fuyubot-tool::ToolDescriptor` 的 JSON 形态，
    /// 但**不含处理器指针**：本版 ABI 只能登记声明，没法让宿主回调进插件执行工具。
    pub fn register_tool(&self, descriptor_json: &str) -> ToolHandle {
        let Ok(descriptor) = CString::new(descriptor_json) else {
            return ToolHandle::INVALID;
        };
        // SAFETY: 同 log。
        unsafe { (self.register_tool)(self.host_ctx, descriptor.as_ptr()) }
    }

    pub fn call_tool(&self, name: &str, input_json: &str) -> Result<RequestId, AbiError> {
        let name = CString::new(name).map_err(|_| AbiError::BadString)?;
        let input = CString::new(input_json).map_err(|_| AbiError::BadString)?;
        // SAFETY: 同 log。
        Ok(unsafe { (self.call_tool)(self.host_ctx, name.as_ptr(), input.as_ptr()) })
    }

    /// 全部工具的元数据 JSON 数组。宿主临时的字符串，读完即弃。
    pub fn list_tools(&self) -> String {
        // SAFETY: 同 log；宿主保证返回的指针在读完之前有效。
        let raw = unsafe { (self.list_tools)(self.host_ctx) };
        unsafe { cstr_to_string(raw) }
    }

    /// # Safety
    /// `vtable` 必须在注销之前一直有效（生命周期由调用方保证）。
    pub unsafe fn provide_service(
        &self,
        name: &str,
        vtable: *const ServiceVTable,
    ) -> ServiceHandle {
        let Ok(name) = CString::new(name) else {
            return ServiceHandle::INVALID;
        };
        // SAFETY: 调用方保证 vtable 存活。
        unsafe { (self.provide_service)(self.host_ctx, name.as_ptr(), vtable) }
    }

    pub fn get_service(&self, name: &str) -> *const ServiceVTable {
        let Ok(name) = CString::new(name) else {
            return core::ptr::null();
        };
        // SAFETY: 同 log。
        unsafe { (self.get_service)(self.host_ctx, name.as_ptr()) }
    }

    pub fn td_invoke(&self, method_json: &str) -> Result<RequestId, AbiError> {
        let method = CString::new(method_json).map_err(|_| AbiError::BadString)?;
        // SAFETY: 同 log。
        Ok(unsafe { (self.td_invoke)(self.host_ctx, method.as_ptr()) })
    }

    pub fn td_invoke_simplified(
        &self,
        method_name: &str,
        params_json: &str,
    ) -> Result<RequestId, AbiError> {
        let name = CString::new(method_name).map_err(|_| AbiError::BadString)?;
        let params = CString::new(params_json).map_err(|_| AbiError::BadString)?;
        // SAFETY: 同 log。
        Ok(unsafe { (self.td_invoke_simplified)(self.host_ctx, name.as_ptr(), params.as_ptr()) })
    }

    /// **同步**执行一个不碰网络的 TDLib 方法，返回响应 JSON 文本。
    ///
    /// 宿主没接 TDLib、或 TDLib 自己返回空时是 `None`。
    /// 结果放在宿主的**线程局部缓冲**里，所以这里当场拷成 `String`——
    /// 别想着把返回的指针留到下一次调用之后。
    pub fn td_execute(&self, request_json: &str) -> Option<String> {
        let request = CString::new(request_json).ok()?;
        // SAFETY: 构造时已校验非空；host_ctx 由宿主保证有效。
        let raw = unsafe { (self.td_execute)(self.host_ctx, request.as_ptr()) };
        if raw.is_null() {
            return None;
        }
        // SAFETY: 宿主保证这个指针在下次宿主调用前有效，这里立刻读完。
        Some(
            unsafe { CStr::from_ptr(raw) }
                .to_string_lossy()
                .into_owned(),
        )
    }

    pub fn use_middleware(
        &self,
        stage: Stage,
        callback: MiddlewareCallbackFn,
        user_data: *mut c_void,
    ) -> MiddlewareHandle {
        // SAFETY: 同 log。
        unsafe { (self.use_middleware)(self.host_ctx, stage as u32, Some(callback), user_data) }
    }

    /// 等一条匹配 `filter_json` 的更新。**阻塞当前线程**，直到匹配、超时或被
    /// [`cancel_wait`](Self::cancel_wait) 取消。`timeout_ms` 为 0 表示只检查当前状态。
    pub fn wait_for_update(&self, filter_json: &str, timeout_ms: u64) -> WaitHandle {
        let Ok(filter) = CString::new(filter_json) else {
            return WaitHandle::INVALID;
        };
        // SAFETY: 同 log。
        unsafe { (self.wait_for_update)(self.host_ctx, filter.as_ptr(), timeout_ms) }
    }

    pub fn cancel_wait(&self, handle: WaitHandle) {
        // SAFETY: 句柄由宿主发放。
        unsafe { (self.cancel_wait)(self.host_ctx, handle) };
    }
}

fn json_cstring(text: Option<&str>) -> Option<CString> {
    text.and_then(|text| CString::new(text).ok())
}

/// # Safety
/// `raw` 必须是宿主给的、NUL 结尾的合法 UTF-8 指针（或 NULL）。
unsafe fn cstr_to_string(raw: *const c_char) -> String {
    if raw.is_null() {
        return String::new();
    }
    // SAFETY: 由调用方保证。
    unsafe { CStr::from_ptr(raw) }
        .to_string_lossy()
        .into_owned()
}

// ── 插件侧导出 ─────────────────────────────────────────────────────

/// 插件初始化返回值。
///
/// `abi_version == 0` 表示 init 失败——返回值不是指针，没有 NULL 可用，就约定这一条。
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct PluginHandle {
    pub abi_version: u32,
    pub size: u32,
    /// 插件自己的实例数据。宿主只透传给 `plugin_start` / `plugin_stop`，从不解引用。
    pub context: *mut c_void,
    /// 插件名，NUL 结尾，**插件持有**，框架不释放。
    pub name: *const c_char,
}

impl PluginHandle {
    pub const FAILED: Self = Self {
        abi_version: 0,
        size: core::mem::size_of::<Self>() as u32,
        context: core::ptr::null_mut(),
        name: core::ptr::null(),
    };

    pub fn new(name: *const c_char, context: *mut c_void) -> Self {
        Self {
            abi_version: ABI_VERSION,
            size: core::mem::size_of::<Self>() as u32,
            context,
            name,
        }
    }

    pub fn is_valid(self) -> bool {
        self.abi_version != 0 && !self.context.is_null()
    }
}

pub type PluginInitFn = unsafe extern "C" fn(host: *const HostApiVTable) -> PluginHandle;
/// 返回 0 表示启动成功。
pub type PluginStartFn =
    unsafe extern "C" fn(handle: PluginHandle, config_json: *const c_char) -> i32;
pub type PluginStopFn = unsafe extern "C" fn(handle: PluginHandle);
/// 释放**插件自己**分配的字符串。
pub type PluginFreeStringFn = unsafe extern "C" fn(ptr: *mut c_char);

// ── 插件作者视角 ───────────────────────────────────────────────────

/// Rust 插件作者只需要实现它，剩下的 C ABI 导出交给 [`define_plugin!`](crate::define_plugin)。
pub trait Plugin: Sized + Send + 'static {
    /// 拿到宿主 VTable 时构造自己；返回 `Err` 表示拒绝被加载。
    fn init(host: HostApi) -> Result<Self, String>;

    /// 宿主调用 `plugin_start`。`config_json` 是宿主传入的配置（可能是空串）。
    fn start(&mut self, config_json: &str) -> Result<(), String>;

    /// 宿主调用 `plugin_stop`（或插件被卸载）。默认什么都不做。
    fn stop(&mut self) {}
}

/// 为 `$ty` 生成 4 个 C ABI 导出符号（`plugin_init` / `plugin_start` / `plugin_stop` /
/// `plugin_free_string`）。
///
/// ```ignore
/// struct MyPlugin { host: fuyubot_abi::HostApi }
/// impl fuyubot_abi::Plugin for MyPlugin { ... }
///
/// fuyubot_abi::define_plugin!(MyPlugin, "my.plugin");
/// ```
///
/// 为什么是 `macro_rules!` 而不是 `#[my_macro] mod foo;` 的 proc-macro 形式：
/// **属性宏看不到外部文件的内容**——实测 `#[mark] mod inner;` 只收到 `mod inner;` 那 10 个字符，
/// `inner.rs` 根本不在宏的输入里。proc-macro 形式只对「把所有代码内联在模块里」有效，
/// 而插件天然是一文件一模块。用 `macro_rules!` 既没这个陷阱，也不用引进
/// `syn`/`quote`/proc-macro crate。
///
/// **一个 crate 只能展开一次**：四个导出符号名是 ABI 定死的（`plugin_init` 等），
/// 同一个 crate 里展开两次就是重复定义、链接必然失败。这与 ABI 的设计一致——
/// 一个 cdylib 就是一个插件。
#[macro_export]
macro_rules! define_plugin {
    ($ty:ty, $name:literal) => {
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn plugin_init(
            host: *const $crate::HostApiVTable,
        ) -> $crate::PluginHandle {
            const NAME: &str = ::core::concat!($name, "\0");
            // SAFETY: 契约要求宿主传一个活着的 VTable 指针。
            let Ok(api) = (unsafe { $crate::HostApi::from_raw(host) }) else {
                return $crate::PluginHandle::FAILED;
            };
            // 插件的 panic 在这里就被挡住：`extern "C"` 边界上逃出去的 panic 是 abort，
            // 那会连宿主一起带走，所以必须自己接。
            let started = ::std::panic::catch_unwind(::std::panic::AssertUnwindSafe(|| {
                <$ty as $crate::Plugin>::init(api)
            }));
            match started {
                Ok(Ok(state)) => $crate::PluginHandle::new(
                    NAME.as_ptr() as *const ::core::ffi::c_char,
                    ::std::boxed::Box::into_raw(::std::boxed::Box::new(state))
                        as *mut ::core::ffi::c_void,
                ),
                Ok(Err(_)) | Err(_) => $crate::PluginHandle::FAILED,
            }
        }

        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn plugin_start(
            handle: $crate::PluginHandle,
            config_json: *const ::core::ffi::c_char,
        ) -> i32 {
            if handle.abi_version != $crate::ABI_VERSION || handle.context.is_null() {
                return -1;
            }
            // SAFETY: context 由 plugin_init 里的 Box::into_raw 产生，只在这里取回。
            let state = unsafe { &mut *(handle.context as *mut $ty) };
            let config = if config_json.is_null() {
                ::std::string::String::new()
            } else {
                // SAFETY: 契约要求宿主传 NUL 结尾的合法字符串。
                unsafe { ::std::ffi::CStr::from_ptr(config_json) }
                    .to_string_lossy()
                    .into_owned()
            };
            match ::std::panic::catch_unwind(::std::panic::AssertUnwindSafe(|| {
                <$ty as $crate::Plugin>::start(state, &config)
            })) {
                Ok(Ok(())) => 0,
                Ok(Err(_)) | Err(_) => -1,
            }
        }

        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn plugin_stop(handle: $crate::PluginHandle) {
            if handle.context.is_null() {
                return;
            }
            // SAFETY: 同 plugin_start；这里顺带把状态收回 Box 释放掉。
            let mut state = unsafe { ::std::boxed::Box::from_raw(handle.context as *mut $ty) };
            let _ = ::std::panic::catch_unwind(::std::panic::AssertUnwindSafe(|| {
                <$ty as $crate::Plugin>::stop(&mut state);
            }));
        }

        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn plugin_free_string(ptr: *mut ::core::ffi::c_char) {
            if ptr.is_null() {
                return;
            }
            // SAFETY: 契约要求 ptr 来自本插件的 CString::into_raw，且只释放一次。
            drop(unsafe { ::std::ffi::CString::from_raw(ptr) });
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static LOG_CALLS: AtomicUsize = AtomicUsize::new(0);
    static LAST_LEVEL: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn test_log(_host: *mut c_void, level: u32, message: *const c_char) {
        LOG_CALLS.fetch_add(1, Ordering::SeqCst);
        LAST_LEVEL.store(level as usize, Ordering::SeqCst);
        // SAFETY: 宿主保证 message 是 NUL 结尾的合法字符串。
        let text = unsafe { CStr::from_ptr(message) }.to_string_lossy();
        assert!(
            text.contains("plugin says hi") || text.contains("stopping"),
            "{text}"
        );
    }

    // 只为了过 `from_raw` 的校验：其余字段都给空实现。
    unsafe extern "C" fn noop_on(
        _: *mut c_void,
        _: *const c_char,
        _: *const c_char,
        _: Option<EventCallbackFn>,
        _: *mut c_void,
    ) -> SubscriptionHandle {
        SubscriptionHandle::INVALID
    }
    unsafe extern "C" fn noop_off(_: *mut c_void, _: SubscriptionHandle) {}
    unsafe extern "C" fn noop_emit(_: *mut c_void, _: *const c_char, _: *const c_char) -> i32 {
        0
    }
    unsafe extern "C" fn noop_register_tool(_: *mut c_void, _: *const c_char) -> ToolHandle {
        ToolHandle::INVALID
    }
    unsafe extern "C" fn noop_call_tool(
        _: *mut c_void,
        _: *const c_char,
        _: *const c_char,
    ) -> RequestId {
        RequestId::INVALID
    }
    unsafe extern "C" fn noop_list_tools(_: *mut c_void) -> *const c_char {
        core::ptr::null()
    }
    unsafe extern "C" fn noop_provide(
        _: *mut c_void,
        _: *const c_char,
        _: *const ServiceVTable,
    ) -> ServiceHandle {
        ServiceHandle::INVALID
    }
    unsafe extern "C" fn noop_get(_: *mut c_void, _: *const c_char) -> *const ServiceVTable {
        core::ptr::null()
    }
    unsafe extern "C" fn noop_td(_: *mut c_void, _: *const c_char) -> RequestId {
        RequestId::INVALID
    }
    unsafe extern "C" fn noop_td2(_: *mut c_void, _: *const c_char, _: *const c_char) -> RequestId {
        RequestId::INVALID
    }
    unsafe extern "C" fn noop_execute(_: *mut c_void, _: *const c_char) -> *const c_char {
        core::ptr::null()
    }
    unsafe extern "C" fn noop_middleware(
        _: *mut c_void,
        _: u32,
        _: Option<MiddlewareCallbackFn>,
        _: *mut c_void,
    ) -> MiddlewareHandle {
        MiddlewareHandle::INVALID
    }
    unsafe extern "C" fn noop_wait(_: *mut c_void, _: *const c_char, _: u64) -> WaitHandle {
        WaitHandle::INVALID
    }
    unsafe extern "C" fn noop_cancel(_: *mut c_void, _: WaitHandle) {}

    fn test_vtable() -> HostApiVTable {
        HostApiVTable {
            version: ABI_VERSION,
            size: core::mem::size_of::<HostApiVTable>() as u32,
            host_ctx: core::ptr::null_mut(),
            on: Some(noop_on),
            off: Some(noop_off),
            emit: Some(noop_emit),
            register_tool: Some(noop_register_tool),
            call_tool: Some(noop_call_tool),
            list_tools: Some(noop_list_tools),
            provide_service: Some(noop_provide),
            get_service: Some(noop_get),
            td_invoke: Some(noop_td),
            td_invoke_simplified: Some(noop_td2),
            td_execute: Some(noop_execute),
            use_middleware: Some(noop_middleware),
            wait_for_update: Some(noop_wait),
            cancel_wait: Some(noop_cancel),
            log: Some(test_log),
        }
    }

    /// 被宏包裹的测试插件：`start` 里通过 VTable 打日志。
    struct TestPlugin {
        host: HostApi,
    }

    impl Plugin for TestPlugin {
        fn init(host: HostApi) -> Result<Self, String> {
            Ok(Self { host })
        }

        fn start(&mut self, config_json: &str) -> Result<(), String> {
            self.host
                .log(LogLevel::Debug, &format!("plugin says hi ({config_json})"));
            Ok(())
        }

        fn stop(&mut self) {
            self.host.log(LogLevel::Info, "plugin stopping");
        }
    }

    define_plugin!(TestPlugin, "test.plugin");

    unsafe extern "C-unwind" fn noop_callback(_: *mut c_void, _: *const c_char) -> i32 {
        0
    }

    unsafe extern "C-unwind" fn noop_middleware_callback(
        _: *mut c_void,
        _: u32,
        _: *const c_char,
    ) -> i32 {
        0
    }

    /// 回调必须是 `C-unwind`：不然宿主没法把插件的 panic 接住。
    /// 这个测试是编译期断言——类型不对就编译不过（闭包不会强制转成 `C-unwind`，所以要用 fn 项）。
    #[test]
    fn callbacks_use_the_unwind_capable_abi() {
        let callback: EventCallbackFn = noop_callback;
        let middleware: MiddlewareCallbackFn = noop_middleware_callback;
        assert_eq!(
            core::mem::size_of_val(&callback),
            core::mem::size_of::<*const c_void>()
        );
        assert_eq!(
            core::mem::size_of_val(&middleware),
            core::mem::size_of::<*const c_void>()
        );
    }

    #[test]
    fn from_raw_rejects_bad_vtables() {
        // SAFETY: 传空指针正是 `from_raw` 要拒绝的情况之一。
        assert!(matches!(
            unsafe { HostApi::from_raw(core::ptr::null()) },
            Err(AbiError::NullVTable)
        ));

        let mut vtable = test_vtable();
        vtable.version = ABI_VERSION + 1;
        // SAFETY: vtable 是本地合法结构。
        let error = unsafe { HostApi::from_raw(&vtable) }.unwrap_err();
        assert!(matches!(error, AbiError::VersionMismatch { .. }), "{error}");

        let mut vtable = test_vtable();
        vtable.size = 4;
        // SAFETY: 同上。
        let error = unsafe { HostApi::from_raw(&vtable) }.unwrap_err();
        assert!(matches!(error, AbiError::VTableTooSmall { .. }), "{error}");

        let mut vtable = test_vtable();
        vtable.log = None;
        // SAFETY: 同上。
        let error = unsafe { HostApi::from_raw(&vtable) }.unwrap_err();
        assert!(matches!(error, AbiError::MissingFunction("log")), "{error}");
    }

    /// 宏生成的导出确实能跑通：init → start（经 VTable 调 log）→ stop。
    #[test]
    fn macro_generated_exports_round_trip() {
        let vtable = test_vtable();
        LOG_CALLS.store(0, Ordering::SeqCst);

        // SAFETY: vtable 在本次调用期间存活。
        let handle = unsafe { plugin_init(&vtable) };
        assert!(handle.is_valid(), "init 应该成功");
        assert_eq!(handle.abi_version, ABI_VERSION);
        assert!(!handle.name.is_null());
        // SAFETY: name 是宏里那个 NUL 结尾的静态字符串。
        let name = unsafe { CStr::from_ptr(handle.name) }.to_string_lossy();
        assert_eq!(name, "test.plugin");

        let config = CString::new(r#"{"a":1}"#).unwrap();
        // SAFETY: handle 来自上面的 init。
        let code = unsafe { plugin_start(handle, config.as_ptr()) };
        assert_eq!(code, 0);
        assert_eq!(LOG_CALLS.load(Ordering::SeqCst), 1);
        assert_eq!(LAST_LEVEL.load(Ordering::SeqCst), LogLevel::Debug as usize);

        // SAFETY: 同一个 handle，stop 收走并释放状态。
        unsafe { plugin_stop(handle) };
        assert_eq!(LOG_CALLS.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn macro_export_refuses_a_mismatched_host() {
        let mut vtable = test_vtable();
        vtable.version = ABI_VERSION + 99;
        // SAFETY: vtable 存活。
        let handle = unsafe { plugin_init(&vtable) };
        assert!(!handle.is_valid(), "版本不对时 init 必须返回失败句柄");
    }

    #[test]
    fn log_levels_and_stages_map_safely() {
        assert_eq!(LogLevel::from_u32(1), LogLevel::Error);
        assert_eq!(LogLevel::from_u32(5), LogLevel::Trace);
        assert_eq!(LogLevel::from_u32(999), LogLevel::Info);
        assert_eq!(LogLevel::from_u32(0), LogLevel::Info);

        for stage in Stage::ALL {
            assert_eq!(Stage::from_u32(stage as u32), Some(stage));
        }
        assert_eq!(Stage::from_u32(6), None);
    }

    #[test]
    fn service_vtable_is_usable_from_rust() {
        unsafe extern "C" fn echo(_ctx: *mut c_void, input: *const c_char) -> *const c_char {
            input
        }
        let method_name = CString::new("echo").unwrap();
        let service_name = CString::new("demo").unwrap();
        let methods = [ServiceMethod {
            name: method_name.as_ptr(),
            func: Some(echo),
        }];
        let vtable = ServiceVTable {
            name: service_name.as_ptr(),
            version: 1,
            size: core::mem::size_of::<ServiceVTable>() as u32,
            context: core::ptr::null_mut(),
            methods: methods.as_ptr(),
            method_count: methods.len(),
        };

        // SAFETY: methods 指向一个元素，长度也是 1。
        let first = unsafe { *vtable.methods };
        let text = CString::new("hello").unwrap();
        // SAFETY: func 非空、context 为 null（echo 不用它）。
        let out = unsafe { (first.func.unwrap())(vtable.context, text.as_ptr()) };
        // SAFETY: echo 原样返回入参指针。
        assert_eq!(unsafe { CStr::from_ptr(out) }, text.as_c_str());
    }
}
