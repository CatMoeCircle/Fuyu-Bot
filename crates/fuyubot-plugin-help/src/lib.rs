//! 内置插件：`/help` 列出可用命令。
//!
//! 命令表是**静态**的：`fuyubot-abi` 里没有「别的插件登记了自己的命令」这种能力
//! （跨插件的命令注册要走服务容器或一条新桥，还没做）。装了自己的命令就照 `COMMANDS`
//! 加一行——它也是这个插件里唯一需要改的地方。

use fuyubot_abi::{HostApi, LogLevel, Plugin, define_plugin};
use fuyubot_simplify::{
    Command, CommandSubscription, SendMessageParams, Simplify, Text, on_command,
};

/// `命令` + 一行说明。顺序就是显示顺序。
const COMMANDS: &[(&str, &str)] = &[
    ("start", "开始使用"),
    ("help", "这条帮助"),
    ("plugins", "看看装了哪些插件"),
];

struct HelpPlugin {
    host: HostApi,
    subscription: Option<CommandSubscription>,
}

impl Plugin for HelpPlugin {
    fn init(host: HostApi) -> Result<Self, String> {
        Ok(Self {
            host,
            subscription: None,
        })
    }

    fn start(&mut self, _config_json: &str) -> Result<(), String> {
        let host = self.host;
        let subscription = on_command(host, move |command: &Command| {
            if !command.is("help") {
                return;
            }
            let params = SendMessageParams {
                text: Some(Text::markdown(help_text())),
                reply_to_message_id: Some(command.message_id),
                ..SendMessageParams::default()
            };
            if let Err(error) = Simplify::new(host).send_message(command.chat_id, &params) {
                host.log(LogLevel::Error, &format!("help 插件回复失败: {error}"));
            }
        });
        let subscription = subscription.ok_or_else(|| "订阅 updateNewMessage 失败".to_owned())?;
        self.subscription = Some(subscription);
        Ok(())
    }

    fn stop(&mut self) {
        self.subscription = None;
    }
}

/// 帮助文本（Markdown：命令用 ``` `…` ``` 包起来，粗体是 `*x*`）。
fn help_text() -> String {
    let mut text = String::from("*可用命令*\n");
    for (name, description) in COMMANDS {
        text.push_str(&format!("/{name} — {description}\n"));
    }
    text
}

define_plugin!(HelpPlugin, "fuyubot.help");
