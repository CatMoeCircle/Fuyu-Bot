//! 内置插件：`/start` 回一句欢迎语。
//!
//! 写法就是「插件作者从这儿抄」的最小样板：`fuyubot_simplify::on_command` 订阅
//! `updateNewMessage`，命中 `/start` 就把回复按 `reply_to` 发回同一个会话。
//! 产物 `fuyubot_plugin_start.{dll,so,dylib}` 跟可执行文件一起打包，
//! `fuyubot init` / `run` 会把它复制进项目的 `plugins/`。

use fuyubot_abi::{HostApi, LogLevel, Plugin, define_plugin};
use fuyubot_simplify::{
    Command, CommandSubscription, SendMessageParams, Simplify, Text, on_command,
};

/// 欢迎语。Markdown 的语法以 TDLib 为准（粗体是 `*x*`，不是 `**x**`）。
const WELCOME: &str = "👋 你好！我是 *FuyuBot*。\n\n发 /help 看看我能做什么。";

struct StartPlugin {
    host: HostApi,
    subscription: Option<CommandSubscription>,
}

impl Plugin for StartPlugin {
    fn init(host: HostApi) -> Result<Self, String> {
        Ok(Self {
            host,
            subscription: None,
        })
    }

    fn start(&mut self, _config_json: &str) -> Result<(), String> {
        let host = self.host;
        let subscription = on_command(host, move |command: &Command| {
            if !command.is("start") {
                return;
            }
            let params = SendMessageParams {
                text: Some(Text::markdown(WELCOME)),
                reply_to_message_id: Some(command.message_id),
                ..SendMessageParams::default()
            };
            if let Err(error) = Simplify::new(host).send_message(command.chat_id, &params) {
                host.log(LogLevel::Error, &format!("start 插件回复失败: {error}"));
            }
        });
        let subscription = subscription.ok_or_else(|| "订阅 updateNewMessage 失败".to_owned())?;
        self.subscription = Some(subscription);
        Ok(())
    }

    fn stop(&mut self) {
        // 守卫 drop 就退订（宿主卸载时也会整批回滚，这里只是早一点松手）。
        self.subscription = None;
    }
}

define_plugin!(StartPlugin, "fuyubot.start");
