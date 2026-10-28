//! 手写导出的插件：**不用** `define_plugin!`，同时故意报一个不兼容的 ABI 版本。
//!
//! 三个用处：
//! 1. `fuyubot-loader` 的集成测试用它验证「版本不兼容 → 返回错误，不 panic」；
//! 2. 验证版本不匹配时宿主会走 `plugin_free_handle` 把 init 分配的状态还回来
//!    （ABI v4 的缺口 7 就是靠这个符号销账的）；
//! 3. 它是 C/C++/Zig 插件作者手写导出时该长什么样的对照。

use core::ffi::{c_char, c_void};
use std::sync::atomic::{AtomicUsize, Ordering};

use fuyubot_abi::{ABI_VERSION, HostApiVTable, PluginHandle};

/// `plugin_free_handle` 被调过几次。
///
/// 宿主**没有**别的办法观测这件事：版本不匹配时 VTable 一样不能信，
/// 所以这里走一个独立的导出函数（见 [`badabi_free_calls`]），
/// 测试用 libloading 再读一次符号就能断言「状态真被还回来了」。
static FREED: AtomicUsize = AtomicUsize::new(0);

#[unsafe(no_mangle)]
pub unsafe extern "C" fn plugin_init(_host: *const HostApiVTable) -> PluginHandle {
    // 故意比宿主高 100：加载器必须拒绝，而不是照着这个 ABI 乱调函数指针。
    PluginHandle {
        abi_version: ABI_VERSION + 100,
        size: core::mem::size_of::<PluginHandle>() as u32,
        // 真分配一块内存：宿主在这里**不能**调 plugin_stop（版本不匹配），
        // 只能走 plugin_free_handle 归还——这正是本插件要验的那条路。
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

/// ABI v4 的出口：**只接 handle 本身、不碰 VTable**，所以在任何版本上都安全。
///
/// 签名是永久冻结的（AGENTS 约束 57）——宿主正是靠这一点，才敢在一个
/// 「版本对不上、VTable 一律不可信」的插件上调用它。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn plugin_free_handle(handle: PluginHandle) {
    if handle.context.is_null() {
        return;
    }
    // SAFETY: context 来自本插件 plugin_init 的 Box::into_raw，只还一次。
    drop(unsafe { Box::from_raw(handle.context as *mut u8) });
    FREED.fetch_add(1, Ordering::SeqCst);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn plugin_free_string(ptr: *mut c_char) {
    if !ptr.is_null() {
        // SAFETY: 契约要求 ptr 来自本插件的 CString::into_raw，且只释放一次。
        drop(unsafe { std::ffi::CString::from_raw(ptr) });
    }
}

/// 供测试读：`plugin_free_handle` 被调过几次。
///
/// 用函数而不是直接暴露 static，是因为从 cdylib 里读 static 要先拿
/// `*const AtomicUsize` 再解引用，多一层就多一个出错的地方；返回 usize 最省事。
#[unsafe(no_mangle)]
pub extern "C" fn badabi_free_calls() -> usize {
    FREED.load(Ordering::SeqCst)
}
