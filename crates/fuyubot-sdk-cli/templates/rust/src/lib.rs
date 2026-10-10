//! __NAME__：fuyubot 插件（Rust）。
//!
//! 由 `fuyubot-sdk init` 生成。用你自己的工具链编它（`cargo build --release`），
//! 产物丢进插件目录就能被 `fuyubot run` 加载。

use fuyubot_plugin_api::{HostApi, HostExt, Plugin, Subscription, define_plugin};

struct MyPlugin {
    host: HostApi,
    /// 订阅句柄**必须活着**：它 drop 的那一刻就退订了。
    _updates: Option<Subscription>,
}

impl Plugin for MyPlugin {
    fn init(host: HostApi) -> Result<Self, String> {
        host.info("__NAME__: init");
        Ok(Self {
            host,
            _updates: None,
        })
    }

    fn start(&mut self, config_json: &str) -> Result<(), String> {
        self.host.info(&format!("__NAME__: start({config_json})"));
        // 这里订阅的是兜底通道（所有更新都会来）。要精确一点就换成
        // "updateNewMessage" 之类具体的 @type。
        self._updates = self
            .host
            .subscribe_json("update", None, on_update, std::ptr::null_mut());
        Ok(())
    }
}

/// 回调没有捕获环境；要状态就把它放进插件字段、经 `user_data` 传进来。
extern "C-unwind" fn on_update(
    _user: *mut std::ffi::c_void,
    payload: *const std::ffi::c_char,
) -> i32 {
    // SAFETY: 宿主按 ABI 契约传的是临时 NUL 结尾字符串。
    let update = unsafe { fuyubot_plugin_api::parse_payload(payload) };
    let update_type = update
        .get("@type")
        .and_then(|value| value.as_str())
        .unwrap_or("?")
        .to_owned();
    println!("__NAME__: 收到 {update_type}");
    0
}

define_plugin!(MyPlugin, "__NAME__");
