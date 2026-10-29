//! 内置插件：`/start` 回一句欢迎语。
//!
//! 写法就是「插件作者从这儿抄」的最小样板：`fuyubot_simplify::on_command` 订阅
//! `updateNewMessage`，命中 `/start` 就把回复按 `reply_to` 发回同一个会话。
//! 产物 `fuyubot_plugin_start.{dll,so,dylib}` 跟可执行文件一起打包，
//! `fuyubot init` / `run` 会把它复制进项目的 `plugins/`。

use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use fuyubot_abi::{HostApi, LogLevel, Plugin, define_plugin};
use fuyubot_simplify::{
    Command, CommandSubscription, SendMessageParams, Simplify, Text, on_command,
};

/// 欢迎语。Markdown 的语法以 TDLib 为准（粗体是 `*x*`，不是 `**x**`）。
const WELCOME: &str = "👋 你好！我是 *FuyuBot*。\n\n发 /help 看看我能做什么。";

struct StartPlugin {
    host: HostApi,
    subscription: Option<CommandSubscription>,
    /// 还在跑的异步发送：插件是动态库，卸载之后线程还在跑库里的代码就是 UB——
    /// 所以 `stop()` 里要把它们 join 掉。
    sending: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl Plugin for StartPlugin {
    fn init(host: HostApi) -> Result<Self, String> {
        Ok(Self {
            host,
            subscription: None,
            sending: Arc::new(Mutex::new(Vec::new())),
        })
    }

    fn start(&mut self, _config_json: &str) -> Result<(), String> {
        let host = self.host;
        let sending = Arc::clone(&self.sending);
        let subscription = on_command(host, move |command: &Command| {
            if !command.is("start") {
                return;
            }
            let params = SendMessageParams {
                text: Some(Text::markdown(WELCOME)),
                reply_to_message_id: Some(command.message_id),
                ..SendMessageParams::default()
            };
            // 命令回调跑在接收循环那个线程上，所以用 **then**：等待版会把自己卡死。
            // 结局（成功 / 失败 / 超时）由闭包接住，这里立刻返回。
            let handle =
                Simplify::new(host).send_message_then(command.chat_id, &params, move |result| {
                    if let Err(error) = result {
                        host.log(LogLevel::Error, &format!("start 插件回复失败: {error}"));
                    }
                });
            if let Ok(mut sending) = sending.lock() {
                sending.push(handle);
            }
        });
        let subscription = subscription.ok_or_else(|| "订阅 updateNewMessage 失败".to_owned())?;
        self.subscription = Some(subscription);
        Ok(())
    }

    fn stop(&mut self) {
        // 守卫 drop 就退订（宿主卸载时也会整批回滚，这里只是早一点松手）。
        self.subscription = None;
        // 等异步发送收工：不能让它们在动态库卸载之后还在跑。
        let mut sending = self
            .sending
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        for handle in sending.drain(..) {
            let _ = handle.join();
        }
    }
}

define_plugin!(StartPlugin, "fuyubot.start");
