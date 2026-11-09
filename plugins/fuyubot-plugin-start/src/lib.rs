//! 内置插件：`/start` 回一句欢迎语。
//!
//! 写法就是「插件作者从这儿抄」的最小样板：`fuyubot_simplify::on_command` 订阅
//! `updateNewMessage`，命中**不带参数的** `/start` 就交给 `Command::reply_then`——由宿主决定
//! 发新消息（bot 账户 / 白名单用户触发）还是编辑原命令消息（user 账户自己触发）。
//! 带参数的 `/start`（如深链 `/start payload`）是别的用途，这里不回。
//! 点名给别人的（`/start@别的Bot`）**根本到不了这里**——`on_command` 已经按
//! 「裸命令所有人都响应、带 `@` 的只有被点名的响应」过滤过了。
//! 产物 `fuyubot_plugin_start.{dll,so,dylib}` 跟可执行文件一起打包，
//! `fuyubot init` / `run` 会把它复制进项目的 `plugins/`。

use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use fuyubot_abi::{HostApi, LogLevel, Plugin, define_plugin};
use fuyubot_simplify::{Command, CommandSubscription, SendMessageParams, Text, on_command};

/// 欢迎语。Markdown 的语法以 TDLib 为准（粗体是 `*x*`，不是 `**x**`）。
const WELCOME: &str = "👋 你好！我是 *FuyuBot*。\n\n发 /help 看看我能做什么。";

struct StartPlugin {
    host: HostApi,
    subscription: Option<CommandSubscription>,
    /// 还在跑的异步回复：插件是动态库，卸载之后线程还在跑库里的代码就是 UB——
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
            // 只回「裸」/start：带参数时 `args` 非空（如深链 `/start payload`），
            // 那是别处的载荷，不该被欢迎语顶掉。
            if !command.is("start") || !command.args.is_empty() {
                return;
            }
            let params = SendMessageParams {
                text: Some(Text::markdown(WELCOME)),
                ..SendMessageParams::default()
            };
            // 命令回调跑在接收循环线程上：用 then（reply 会阻塞等 TDLib）。
            let handle = command.reply_then(host, &params, move |result| {
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

        // 经共享服务把自己的命令登记进 /help（见 fuyubot.help.commands 的 `register` 方法）。
        // 失败也不影响 /start 本身——/help 里有静态兜底，只是少一条动态登记。
        match host.call_service(
            "fuyubot.help.commands",
            "register",
            r#"{"name":"start","description":"开始使用"}"#,
        ) {
            Ok(reply) => host.log(
                LogLevel::Info,
                &format!("start 插件已向 /help 注册命令：{reply}"),
            ),
            Err(error) => host.log(
                LogLevel::Warn,
                &format!("start 插件注册命令到 /help 失败：{error}"),
            ),
        }

        Ok(())
    }

    fn soft_inject() -> Vec<String> {
        // 把自己的命令登记进 /help：等 help 插件的命令注册服务就位后再激活，
        // 这样 start() 里 `call_service` 注册命令必定成功。
        vec!["fuyubot.help.commands".to_owned()]
    }

    /// `/plugins` 列表里显示的名字。
    fn display_name() -> &'static str {
        "启动问候"
    }

    /// `/plugins` 列表里那一行介绍。
    fn description() -> &'static str {
        "回复 /start 的欢迎语"
    }

    fn stop(&mut self) {
        // 守卫 drop 就退订（宿主卸载时也会整批回滚，这里只是早一点松手）。
        self.subscription = None;
        // 等异步回复收工：不能让它们在动态库卸载之后还在跑。
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
