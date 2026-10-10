//! 插件作者门面：`fuyubot-abi` 的**安全包装层** + ABI 版本协商。
//!
//! 这个 crate 不含任何新契约——ABI 的形状只有 `fuyubot-abi` 一份实现，这里只是
//! 把它转成插件作者用着不疼的样子：
//!
//! - 重导出 [`Plugin`] / [`define_plugin`] / [`HostApi`] / [`HostApiVTable`]，
//!   插件只用依赖这一个 crate；
//! - [`HostExt`]：日志简写、订阅返回 **drop 即退订**的守卫、按 `serde_json::Value`
//!   收发（而不是每次手拼 JSON 字符串）；
//! - [`wait_for_update_json`](HostExt::wait_for_update_json)：把「只在下一次宿主调用前有效」
//!   的临时指针**当场**拷成 `Value`，不给插件作者留悬垂指针的机会；
//! - [`SDK_ABI_VERSION`] / [`is_compatible`]：本 SDK 声明的兼容 ABI 版本，
//!   宿主加载时会拿它与 [`HostApiVTable::version`] 比（见 `fuyubot-loader`）。
//!
//! ## 最小插件
//!
//! ```ignore
//! use fuyubot_plugin_api::{define_plugin, HostApi, HostExt, LogLevel, Plugin};
//!
//! struct Echo { host: HostApi }
//!
//! impl Plugin for Echo {
//!     fn init(host: HostApi) -> Result<Self, String> { Ok(Self { host }) }
//!     fn start(&mut self, _config: &str) -> Result<(), String> {
//!         self.host.info("echo: 起来了");
//!         let _sub = self.host.subscribe_json("updateNewMessage", None, |payload| { /* ... */ });
//!         Ok(())
//!     }
//! }
//!
//! define_plugin!(Echo, "fuyubot.echo");
//! ```

use core::ffi::c_void;

use serde_json::Value;

/// ABI 版本、句柄与回调类型都直接来自 `fuyubot-abi`——**只有一份定义**。
pub use fuyubot_abi::{
    ABI_VERSION, AbiError, EventCallbackFn, FILTER_EVENT_TYPE_KEY, HostApi, HostApiVTable,
    LogLevel, MiddlewareCallbackFn, MiddlewareHandle, PLUGIN_HEADER, Plugin, PluginFreeStringFn,
    PluginHandle, PluginInitFn, PluginStartFn, PluginStopFn, RESPONSE_EVENT, RequestId,
    ServiceHandle, ServiceMethod, ServiceMethodFn, ServiceVTable, Stage, SubscriptionHandle,
    ToolHandle, UPDATE_EVENT, WaitHandle, define_plugin,
};

/// 本 SDK 声明兼容的宿主 ABI 版本。
///
/// 插件用它换算出「我能在哪种宿主上跑」；宿主加载插件时比的是
/// [`PluginHandle::abi_version`]（由 [`define_plugin`] 填成这个值）。
pub const SDK_ABI_VERSION: u32 = ABI_VERSION;

/// 这个 SDK 认得的宿主 ABI 版本。字段只增不改，所以这里目前只有一档。
pub const COMPATIBLE_HOST_ABI_VERSIONS: &[u32] = &[ABI_VERSION];

/// 宿主 ABI 版本是否与本 SDK 兼容。
pub fn is_compatible(host_abi_version: u32) -> bool {
    COMPATIBLE_HOST_ABI_VERSIONS.contains(&host_abi_version)
}

/// 一条**活着的**订阅：drop 就是退订。
///
/// `HostApi::on` 返回的裸 [`SubscriptionHandle`] 忘了 `off` 就会一直收消息，
/// 而且插件卸载时宿主虽然会整批回滚，插件自己却已经泄漏了一份状态。
/// 这个守卫把「注册 → 注销」绑成一个值，和内核里 `Effect` 的语义一致。
#[must_use = "drop 掉守卫就会立刻退订"]
pub struct Subscription {
    api: HostApi,
    handle: SubscriptionHandle,
}

impl Subscription {
    /// 绑一个句柄；无效句柄不建守卫。
    pub fn new(api: HostApi, handle: SubscriptionHandle) -> Option<Self> {
        handle.is_valid().then_some(Self { api, handle })
    }

    pub fn handle(&self) -> SubscriptionHandle {
        self.handle
    }

    /// 主动退订，并吃掉守卫。
    pub fn unsubscribe(mut self) {
        self.api.off(self.handle);
        self.handle = SubscriptionHandle::INVALID;
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        if self.handle.is_valid() {
            self.api.off(self.handle);
        }
    }
}

/// 把「回调收到的临时 JSON 指针」读成 `Value` 的辅助函数。
///
/// # Safety
/// `payload` 必须来自宿主（ABI 契约保证它是临时 NUL 结尾字符串）。
pub unsafe fn parse_payload(payload: *const core::ffi::c_char) -> Value {
    // SAFETY: 由调用方保证。
    let text = unsafe { std::ffi::CStr::from_ptr(payload) }.to_string_lossy();
    serde_json::from_str(&text).unwrap_or(Value::Null)
}

/// 插件作者视角的宿主能力。
///
/// 扩展 trait 而不是给 [`HostApi`] 加固有方法：`HostApi` 在 `fuyubot-abi` 里，
/// 是 ABI 层；这里是可以跟着 SDK 版本演进的便利层，两者生命周期不同。
pub trait HostExt {
    /// 打一条日志（三档简写）。
    fn info(&self, message: &str);
    fn warn(&self, message: &str);
    fn error(&self, message: &str);

    /// 订阅事件，用 [`Value`] 取过滤条件；返回 drop 即退订的守卫。
    ///
    /// `filter` 里的 `event_type` 键是**选通道**用的（见 [`FILTER_EVENT_TYPE_KEY`]），
    /// 其余键做负载字段匹配。
    fn subscribe_json(
        &self,
        event_type: &str,
        filter: Option<Value>,
        callback: EventCallbackFn,
        user_data: *mut c_void,
    ) -> Option<Subscription>;

    /// 发一条 TDLib 请求，参数是 [`Value`]。
    fn invoke_value(&self, method: &str, params: Value) -> Result<RequestId, AbiError>;

    /// 等一条匹配的更新，并把结果当场拷成 [`Value`]（宿主的缓冲区早就失效了）。
    ///
    /// 返回 `None` 表示超时或被取消。
    fn wait_for_update_json(&self, filter: Value, timeout_ms: u64) -> Option<Value>;
}

impl HostExt for HostApi {
    fn info(&self, message: &str) {
        self.log(LogLevel::Info, message);
    }

    fn warn(&self, message: &str) {
        self.log(LogLevel::Warn, message);
    }

    fn error(&self, message: &str) {
        self.log(LogLevel::Error, message);
    }

    fn subscribe_json(
        &self,
        event_type: &str,
        filter: Option<Value>,
        callback: EventCallbackFn,
        user_data: *mut c_void,
    ) -> Option<Subscription> {
        let filter = filter.map(|value| value.to_string());
        Subscription::new(
            self.clone(),
            self.on(event_type, filter.as_deref(), callback, user_data),
        )
    }

    fn invoke_value(&self, method: &str, params: Value) -> Result<RequestId, AbiError> {
        let params = if params.is_object() {
            params.to_string()
        } else {
            "{}".to_owned()
        };
        self.td_invoke_simplified(method, &params)
    }

    fn wait_for_update_json(&self, filter: Value, timeout_ms: u64) -> Option<Value> {
        let handle = self.wait_for_update(&filter.to_string(), timeout_ms);
        if !handle.did_match() {
            return None;
        }
        // SAFETY: 宿主保证 payload 在本线程下一次宿主调用前有效——这里立刻拷走。
        Some(unsafe { parse_payload(handle.payload_json) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::ffi::c_char;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static HITS: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C-unwind" fn counter(_user: *mut c_void, _payload: *const c_char) -> i32 {
        HITS.fetch_add(1, Ordering::SeqCst);
        0
    }

    /// 用 `fuyubot-loader` 会构造的那种 VTable 形状，验「守卫 drop 即退订」。
    ///
    /// 这里不引 loader（插件侧不该依赖宿主层），只搭一个够用的假宿主。
    mod fake_host {
        use super::*;
        use fuyubot_abi::{ABI_VERSION, HostApiVTable, SubscriptionHandle};

        static ACTIVE: AtomicUsize = AtomicUsize::new(0);

        pub fn active() -> usize {
            ACTIVE.load(Ordering::SeqCst)
        }

        unsafe extern "C" fn on(
            _host: *mut c_void,
            _event: *const c_char,
            _filter: *const c_char,
            _cb: Option<EventCallbackFn>,
            _ud: *mut c_void,
        ) -> SubscriptionHandle {
            ACTIVE.fetch_add(1, Ordering::SeqCst);
            SubscriptionHandle { id: 1 }
        }

        unsafe extern "C" fn off(_host: *mut c_void, _handle: SubscriptionHandle) {
            ACTIVE.fetch_sub(1, Ordering::SeqCst);
        }
        unsafe extern "C" fn log(_host: *mut c_void, _l: u32, _m: *const c_char) {}
        unsafe extern "C" fn emit(_h: *mut c_void, _t: *const c_char, _p: *const c_char) -> i32 {
            0
        }
        unsafe extern "C" fn register_tool(_h: *mut c_void, _d: *const c_char) -> ToolHandle {
            ToolHandle::INVALID
        }
        unsafe extern "C" fn call_tool(
            _h: *mut c_void,
            _n: *const c_char,
            _i: *const c_char,
        ) -> RequestId {
            RequestId::INVALID
        }
        unsafe extern "C" fn list_tools(_h: *mut c_void) -> *const c_char {
            core::ptr::null()
        }
        unsafe extern "C" fn provide(
            _h: *mut c_void,
            _n: *const c_char,
            _v: *const ServiceVTable,
        ) -> ServiceHandle {
            ServiceHandle::INVALID
        }
        unsafe extern "C" fn get(_h: *mut c_void, _n: *const c_char) -> *const ServiceVTable {
            core::ptr::null()
        }
        unsafe extern "C" fn td(_h: *mut c_void, _m: *const c_char) -> RequestId {
            RequestId::INVALID
        }
        unsafe extern "C" fn td2(
            _h: *mut c_void,
            _n: *const c_char,
            _p: *const c_char,
        ) -> RequestId {
            RequestId { id: 42 }
        }
        unsafe extern "C" fn middleware(
            _h: *mut c_void,
            _s: u32,
            _c: Option<MiddlewareCallbackFn>,
            _u: *mut c_void,
        ) -> MiddlewareHandle {
            MiddlewareHandle::INVALID
        }
        unsafe extern "C" fn wait(_h: *mut c_void, _f: *const c_char, _t: u64) -> WaitHandle {
            WaitHandle::INVALID
        }
        unsafe extern "C" fn cancel(_h: *mut c_void, _w: WaitHandle) {}

        pub fn api() -> HostApi {
            // 每次调都新建一份静态 VTable：地址稳定，插件侧只读。
            let vtable: &'static HostApiVTable = Box::leak(Box::new(HostApiVTable {
                version: ABI_VERSION,
                size: core::mem::size_of::<HostApiVTable>() as u32,
                host_ctx: core::ptr::null_mut(),
                on: Some(on),
                off: Some(off),
                emit: Some(emit),
                register_tool: Some(register_tool),
                call_tool: Some(call_tool),
                list_tools: Some(list_tools),
                provide_service: Some(provide),
                get_service: Some(get),
                td_invoke: Some(td),
                td_invoke_simplified: Some(td2),
                use_middleware: Some(middleware),
                wait_for_update: Some(wait),
                cancel_wait: Some(cancel),
                log: Some(log),
            }));
            // SAFETY: vtable 是 Box::leak 出来的，地址稳定且布局正确。
            unsafe { HostApi::from_raw(vtable) }.expect("假宿主必须能被解包")
        }
    }

    #[test]
    fn version_negotiation_is_one_place() {
        assert_eq!(SDK_ABI_VERSION, ABI_VERSION);
        assert!(is_compatible(ABI_VERSION));
        assert!(!is_compatible(ABI_VERSION + 1));
        assert!(COMPATIBLE_HOST_ABI_VERSIONS.contains(&ABI_VERSION));
    }

    #[test]
    fn subscription_guard_unsubscribes_on_drop() {
        let api = fake_host::api();
        assert_eq!(fake_host::active(), 0);
        {
            let _sub = api
                .subscribe_json("updateNewMessage", None, counter, core::ptr::null_mut())
                .expect("订阅必须建立起来");
            assert_eq!(fake_host::active(), 1);
        }
        assert_eq!(fake_host::active(), 0, "守卫 drop 之后必须退订");
    }

    #[test]
    fn invoke_value_forwards_params() {
        let api = fake_host::api();
        let id = api
            .invoke_value("getMe", serde_json::json!({"x": 1}))
            .unwrap();
        assert_eq!(id.id, 42);
        // 非对象参数退化成空对象，而不是把 `null` 拼进请求。
        assert_eq!(api.invoke_value("getMe", Value::Null).unwrap().id, 42);
    }

    #[test]
    fn transient_payload_is_copied_out() {
        let payload = std::ffi::CString::new(r#"{"@type":"user","id":7}"#).unwrap();
        // SAFETY: 上面构造的字符串就是宿主会给的那种临时字符串。
        let value = unsafe { parse_payload(payload.as_ptr()) };
        assert_eq!(value["id"], 7);
        // 不是 JSON 也不 panic，退化成 Null。
        let bad = std::ffi::CString::new("not json").unwrap();
        // SAFETY: 同上。
        assert_eq!(unsafe { parse_payload(bad.as_ptr()) }, Value::Null);
    }
}
