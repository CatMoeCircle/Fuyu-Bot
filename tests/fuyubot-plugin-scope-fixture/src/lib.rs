//! 测试夹具：在 `plugin_start` 里走一遍**服务作用域**的完整路径。
//!
//! 它验证的是「插件运行时真的能读写自己的作用域」——这条路径靠单元测试测不出来，
//! 因为 `api_provide_service` / `api_get_service` 读的是**线程局部的 owner 栈**，
//! 而那个栈由 `plugin_scope()` 在加载路径上压入。只有真 dlopen 一个插件才复现得了。
//!
//! 三件事，任何一件失败就让 `start` 返回非零（宿主会判加载失败）：
//! 1. `provide_service` 注册私有服务成功；
//! 2. `get_service` 立刻取回来的是**自己那份**（不是别人的、也不是 NULL）；
//! 3. 根上的公共服务（`fuyubot.host.plugins`）沿父链仍然可见。

use core::ffi::{c_char, c_void};

use fuyubot_abi::{HostApi, LogLevel, Plugin, ServiceMethod, ServiceVTable, define_plugin};

/// 服务的唯一方法：把入参原样吐回来（不会真被调到，只为让 vtable 形状合法）。
unsafe extern "C" fn echo(_ctx: *mut c_void, input: *const c_char) -> *const c_char {
    input
}

struct ScopeFixture {
    host: HostApi,
}

impl Plugin for ScopeFixture {
    fn init(host: HostApi) -> Result<Self, String> {
        Ok(Self { host })
    }

    fn start(&mut self, _config_json: &str) -> Result<(), String> {
        // 泄漏两份小结构：服务在注销前必须一直有效，夹具活到进程结束，泄漏最省事。
        let methods = Box::leak(Box::new([ServiceMethod {
            name: c"echo".as_ptr(),
            func: Some(echo),
        }]));
        let vtable: &'static ServiceVTable = Box::leak(Box::new(ServiceVTable {
            name: c"fixture.svc".as_ptr(),
            version: 1,
            size: core::mem::size_of::<ServiceVTable>() as u32,
            context: core::ptr::null_mut(),
            methods: methods.as_ptr(),
            method_count: methods.len(),
        }));

        // SAFETY: vtable 是 `'static` 的泄漏内存，活到进程结束。
        let handle = unsafe { self.host.provide_service("fixture.svc", vtable) };
        if !handle.is_valid() {
            self.host
                .log(LogLevel::Error, "fixture: provide_service 失败");
            return Err("provide_service 失败".to_owned());
        }

        // 关键一步：刚注册的东西，自己能不能立刻取回来。
        let got = self.host.get_service("fixture.svc");
        if got != vtable as *const ServiceVTable {
            self.host.log(
                LogLevel::Error,
                &format!("fixture: get_service 没拿回自己那份（got={got:p}）"),
            );
            return Err("get_service 没拿回自己那份".to_owned());
        }
        self.host
            .log(LogLevel::Info, "fixture: get_service 拿回了自己注册的那份");

        // 根上的公共服务应当仍然可见：查找沿父链向上。
        if self.host.get_service("fuyubot.host.plugins").is_null() {
            self.host
                .log(LogLevel::Error, "fixture: 看不见根上的公共服务，父链断了");
            return Err("父链断了".to_owned());
        }
        self.host
            .log(LogLevel::Info, "fixture: 根上的公共服务仍然可见");

        Ok(())
    }
}

define_plugin!(ScopeFixture, "fuyubot.scope-fixture");
