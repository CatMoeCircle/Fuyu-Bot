//! 最小 Rust 插件：实现 [`Plugin`]，其余交给 `define_plugin!`。
//!
//! 它就是插件作者的抄写模板——构建产物 `fuyubot_plugin_demo.{dll,so,dylib}` 会被
//! `fuyubot-loader` 的集成测试真的 dlopen 起来，走一遍「VTable → 更新管线 → 响应」，
//! 所以这份代码不是摆设。
//!
//! 它做四件事：
//! 1. `init`/`start`/`stop` 各经 VTable 打一条日志（可被 `log_lines()` 断言）；
//! 2. 注册一个工具声明；
//! 3. 订阅 `updateNewMessage`，把收到的整条更新打日志（这样中间件改了什么一眼可见）；
//! 4. 发一条 `getMe`，并订阅它自己的响应，收到后打日志。

use std::ffi::CStr;

use fuyubot_abi::{HostApi, LogLevel, Plugin, define_plugin};

/// 回调私有数据：C 回调没有捕获环境，只能靠 `user_data` 传。
///
/// 存成整数放在插件状态里，`DemoPlugin` 就自动满足 `Plugin: Send`。
struct CallbackState {
    host: HostApi,
}

/// 插件自己的实例状态。`plugin_init` 会把它 Box 起来交给宿主透传。
struct DemoPlugin {
    host: HostApi,
    /// `Box<CallbackState>` 的地址；`stop` 时收回。
    state: usize,
}

impl Plugin for DemoPlugin {
    fn init(host: HostApi) -> Result<Self, String> {
        host.log(LogLevel::Info, "fuyubot.demo: init");
        Ok(Self { host, state: 0 })
    }

    fn start(&mut self, config_json: &str) -> Result<(), String> {
        self.host.log(
            LogLevel::Info,
            &format!("fuyubot.demo: start({config_json})"),
        );

        // 回调要用的状态：声明周期由我们自己扛（见 stop）。
        let state = Box::into_raw(Box::new(CallbackState { host: self.host }));
        self.state = state as usize;

        // 经 VTable 注册一个工具声明。宿主那边能观察到，集成测试就断言这个。
        let tool = self
            .host
            .register_tool_meta(r#"{"name":"demo.echo","description":"回显入参"}"#);
        if !tool.is_valid() {
            return Err("注册工具失败".to_owned());
        }

        // 订阅更新：回调里把整条 JSON 打日志，中间件改了什么都看得见。
        let subscription = self.host.on(
            "updateNewMessage",
            None,
            on_new_message,
            state as *mut std::ffi::c_void,
        );
        if !subscription.is_valid() {
            return Err("订阅 updateNewMessage 失败".to_owned());
        }

        // 发一条请求并订阅它自己的响应。过滤器的 event_type 键用来选通道。
        let request = self
            .host
            .td_invoke_simplified("getMe", "{}")
            .map_err(|e| e.to_string())?;
        let filter = format!(r#"{{"event_type":"response","request_id":{}}}"#, request.id);
        let response = self.host.on(
            "response",
            Some(&filter),
            on_response,
            state as *mut std::ffi::c_void,
        );
        if !response.is_valid() {
            return Err("订阅响应失败".to_owned());
        }

        // 让集成测试能验「插件 panic 被 define_plugin! 接住，而不是 abort 整个进程」。
        if config_json.contains("panic_on_start") {
            panic!("demo 插件按配置在 start 里 panic");
        }
        Ok(())
    }

    fn stop(&mut self) {
        self.host.log(LogLevel::Info, "fuyubot.demo: stop");
        if self.state != 0 {
            // SAFETY: 这个指针来自 start 里的 Box::into_raw，只在这里收回一次。
            drop(unsafe { Box::from_raw(self.state as *mut CallbackState) });
            self.state = 0;
        }
    }
}

extern "C-unwind" fn on_new_message(user_data: *mut std::ffi::c_void, payload: *const i8) -> i32 {
    // SAFETY: 宿主传的是它自己那段 NUL 结尾的临时字符串。
    let text = unsafe { CStr::from_ptr(payload) }.to_string_lossy();
    // SAFETY: user_data 是 start 里那个 CallbackState，插件卸载前一直有效。
    let Some(state) = (unsafe { (user_data as *const CallbackState).as_ref() }) else {
        return -1;
    };
    state.host.log(
        LogLevel::Info,
        &format!("fuyubot.demo: updateNewMessage {text}"),
    );
    0
}

extern "C-unwind" fn on_response(user_data: *mut std::ffi::c_void, payload: *const i8) -> i32 {
    // SAFETY: 同 on_new_message。
    let text = unsafe { CStr::from_ptr(payload) }.to_string_lossy();
    let Some(state) = (unsafe { (user_data as *const CallbackState).as_ref() }) else {
        return -1;
    };
    state
        .host
        .log(LogLevel::Info, &format!("fuyubot.demo: response {text}"));
    0
}

define_plugin!(DemoPlugin, "fuyubot.demo");
