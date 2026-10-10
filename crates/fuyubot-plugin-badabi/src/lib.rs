//! 手写导出的插件：**不用** `define_plugin!`，同时故意报一个不兼容的 ABI 版本。
//!
//! 两个用处：
//! 1. `fuyubot-loader` 的集成测试用它验证「版本不兼容 → 返回错误，不 panic」；
//! 2. 它是 C/C++/Zig 插件作者手写导出时该长什么样的对照——四个符号，一个不能少。

use core::ffi::{c_char, c_void};

use fuyubot_abi::{ABI_VERSION, HostApiVTable, PluginHandle};

#[unsafe(no_mangle)]
pub unsafe extern "C" fn plugin_init(_host: *const HostApiVTable) -> PluginHandle {
    // 故意比宿主高 100：加载器必须拒绝，而不是照着这个 ABI 乱调函数指针。
    PluginHandle {
        abi_version: ABI_VERSION + 100,
        size: core::mem::size_of::<PluginHandle>() as u32,
        // 真分配一块内存：加载器在版本不匹配时不会调 plugin_stop，
        // 这块内存就留在那儿了——这是 ABI 契约的已知代价，不是 bug。
        context: Box::into_raw(Box::new(0u8)) as *mut c_void,
        name: c"fuyubot.bad-abi".as_ptr(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn plugin_start(_handle: PluginHandle, _config_json: *const c_char) -> i32 {
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn plugin_stop(_handle: PluginHandle) {}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn plugin_free_string(ptr: *mut c_char) {
    if !ptr.is_null() {
        // SAFETY: 契约要求 ptr 来自本插件的 CString::into_raw，且只释放一次。
        drop(unsafe { std::ffi::CString::from_raw(ptr) });
    }
}
