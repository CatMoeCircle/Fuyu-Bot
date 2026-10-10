//! echo 示例插件：监听 `updateNewMessage`，把收到的正文**原样**回过去。
//!
//! 这是插件作者最短的一条闭环：订阅 → 解析负载 → 经 VTable 发请求。
//! 它同时是 `fuyubot run --replay` 的验证对象——不需要 Telegram 账号就能看它工作
//! （见 examples/README.md）。
//!
//! 语义上刻意做了两件「不起眼但必须」的事：
//! 1. **跳过自己发的消息**（`message.is_outgoing`），否则 echo 会自激成死循环；
//! 2. 正文空着就不回，免得对编辑消息、服务消息也回声。

use std::ffi::c_void;

use fuyubot_plugin_api::{HostApi, HostExt, Plugin, Subscription, define_plugin, parse_payload};
use serde_json::{Value, json};

/// 回调没有捕获环境，宿主的状态只能放进插件字段、经 `user_data` 传进去。
struct EchoPlugin {
    host: HostApi,
    /// `Box<HostApi>` 的地址；`stop` 时收回。
    state: usize,
    /// 订阅句柄必须活着：drop 就是退订。
    _updates: Option<Subscription>,
}

impl Plugin for EchoPlugin {
    fn init(host: HostApi) -> Result<Self, String> {
        host.info("echo: init");
        Ok(Self {
            host,
            state: 0,
            _updates: None,
        })
    }

    fn start(&mut self, config_json: &str) -> Result<(), String> {
        self.host.info(&format!("echo: start({config_json})"));
        let state = Box::into_raw(Box::new(self.host));
        self.state = state as usize;
        self._updates =
            self.host
                .subscribe_json("updateNewMessage", None, on_message, state as *mut c_void);
        Ok(())
    }

    fn stop(&mut self) {
        self.host.info("echo: stop");
        if self.state != 0 {
            // SAFETY: 这个指针来自 start 里的 Box::into_raw，只在这里收回一次。
            drop(unsafe { Box::from_raw(self.state as *mut HostApi) });
            self.state = 0;
        }
    }
}

/// 收到一条消息：正文原样回过去。
extern "C-unwind" fn on_message(user_data: *mut c_void, payload: *const std::ffi::c_char) -> i32 {
    // SAFETY: 宿主按 ABI 契约传的是临时 NUL 结尾字符串。
    let update = unsafe { parse_payload(payload) };
    // SAFETY: user_data 是 start 里那个 Box<HostApi>，插件卸载前一直有效。
    let Some(host) = (unsafe { (user_data as *const HostApi).as_ref() }) else {
        return -1;
    };
    let Some(message) = update.get("message") else {
        return 0;
    };

    let (Some(chat_id), text) = (
        message.get("chat_id").and_then(Value::as_i64),
        message
            .pointer("/content/text/text")
            .and_then(Value::as_str)
            .unwrap_or(""),
    ) else {
        return 0;
    };
    let outgoing = message
        .get("is_outgoing")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if outgoing || text.is_empty() || chat_id == 0 {
        return 0;
    }

    // 经 VTable 发出去；`td_invoke_simplified` 会给 `{}` 补上 `@type`。
    let params = json!({ "chat_id": chat_id, "text": text });
    match host.invoke_value("sendMessage", params) {
        Ok(request) => host.info(&format!("echo: 回了 #{} 一条消息", request.id)),
        Err(e) => host.error(&format!("echo: 发不出去：{e}")),
    }
    0
}

define_plugin!(EchoPlugin, "fuyubot.example.echo");
