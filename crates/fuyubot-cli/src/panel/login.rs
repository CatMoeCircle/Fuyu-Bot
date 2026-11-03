//! TUI 的登录弹窗：后台线程跑 `fuyubot-app` 的登录流程，提问经通道抛到界面上。
//!
//! 为什么不在面板进程里直接开 TDLib：TDLib 的数据库同一时刻只能被一个进程打开（约束 47），
//! 所以登录**只能在没起实例的时候做**；跑起来之后的「被下线」还是走 `run` 自己的重登。
//!
//! 交互是怎么接上的：`fuyubot-app` 只认 [`AppHost`]（`state` / `ask` / `qr`）。
//! 这里实现一份「往界面塞事件、等回答」的宿主——
//!   `ask` → 发 [`LoginEvent::Ask`] 到界面 → 界面弹输入框 → Enter 回填 → `ask` 返回。

use std::sync::Arc;
use std::sync::mpsc::{Sender, channel};
use std::time::Duration;

use fuyubot_app::{App, AppHost, AppOptions, AppState, AskKind};

/// 单次提问最多等这么久（登录总预算 600s，这里留出余量）。
const ASK_TIMEOUT: Duration = Duration::from_secs(300);

/// 后台登录线程 → 界面的事件。
pub enum LoginEvent {
    /// 需要用户输入一行。界面弹框，把答案 `send` 回 `reply`（`None` = 取消）。
    Ask {
        kind: AskKind,
        prompt: String,
        reply: Sender<Option<String>>,
    },
    /// 状态变化（也用来显示二维码链接）。
    State(AppState, String),
    /// 流程结束。
    Done(Result<(), String>),
}

/// 界面上那个输入框（一次只显示一个问题）。
pub struct LoginModal {
    pub kind: AskKind,
    pub prompt: String,
    pub input: String,
    reply: Option<Sender<Option<String>>>,
}

impl LoginModal {
    pub fn new(kind: AskKind, prompt: String, reply: Sender<Option<String>>) -> Self {
        Self {
            kind,
            prompt,
            input: String::new(),
            reply: Some(reply),
        }
    }

    pub fn push(&mut self, character: char) {
        if !character.is_control() {
            self.input.push(character);
        }
    }

    pub fn backspace(&mut self) {
        self.input.pop();
    }

    /// 提交：把去掉首尾空白的答案送回去。返回送出的内容（给调用方打提示用）。
    pub fn submit(&mut self) -> Option<String> {
        let reply = self.reply.take()?;
        let answer = self.input.trim().to_owned();
        let _ = reply.send(Some(answer.clone()));
        Some(answer)
    }

    /// 取消：送 `None`（`fuyubot-app` 会把它当「这一步没拿到输入」）。
    pub fn cancel(&mut self) {
        if let Some(reply) = self.reply.take() {
            let _ = reply.send(None);
        }
    }

    /// 密码 / 验证码不该原样显示——这里只用来决定输入框要不要打码。
    pub fn masked(&self) -> bool {
        matches!(self.kind, AskKind::Password)
    }
}

/// 后台线程用的宿主：把问题塞进通道，等界面回填。
pub struct TuiHost {
    tx: Sender<LoginEvent>,
}

impl TuiHost {
    pub fn new(tx: Sender<LoginEvent>) -> Arc<Self> {
        Arc::new(Self { tx })
    }
}

impl AppHost for TuiHost {
    fn state(&self, state: AppState, detail: &str) {
        let _ = self.tx.send(LoginEvent::State(state, detail.to_owned()));
    }

    fn ask(&self, kind: AskKind, prompt: &str) -> Option<String> {
        let (reply, wait) = channel();
        if self
            .tx
            .send(LoginEvent::Ask {
                kind,
                prompt: prompt.to_owned(),
                reply,
            })
            .is_err()
        {
            // 界面已经走了：别让登录线程吊在这儿。
            return None;
        }
        wait.recv_timeout(ASK_TIMEOUT).ok().flatten()
    }

    fn qr(&self, link: &str) {
        let _ = self.tx.send(LoginEvent::State(
            AppState::Starting,
            format!("扫码登录：{link}"),
        ));
    }
}

/// 起一个线程跑「只登录」：`App::boot` → `login` → `shutdown`（不装插件、不开分发）。
///
/// 失败 / 成功都通过 [`LoginEvent::Done`] 回到界面；线程自己收尾，不 join。
pub fn spawn_login(options: AppOptions, tx: Sender<LoginEvent>) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("fuyubot-tui-login".to_owned())
        .spawn(move || {
            let outcome = (|| -> Result<(), String> {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|error| format!("建不了 tokio 运行时：{error}"))?;
                runtime.block_on(async {
                    let mut app = App::boot(options)
                        .await
                        .map_err(|error| error.to_string())?;
                    app.login().map_err(|error| error.to_string())?;
                    app.shutdown(None, Duration::from_secs(3))
                        .await
                        .map_err(|error| error.to_string())?;
                    Ok(())
                })
            })();
            let _ = tx.send(LoginEvent::Done(outcome));
        })
        .expect("起不了登录线程")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::Receiver;

    fn modal() -> (LoginModal, Receiver<Option<String>>) {
        let (reply, wait) = channel();
        (
            LoginModal::new(AskKind::Code, "6 位验证码".to_owned(), reply),
            wait,
        )
    }

    #[test]
    fn typing_and_backspace_edit_the_buffer() {
        let (mut modal, _wait) = modal();
        for ch in "12345".chars() {
            modal.push(ch);
        }
        modal.backspace();
        modal.push('6');
        assert_eq!(modal.input, "12346");
    }

    /// 控制字符（回车 / 退格）不该进缓冲区——它们是按键，不是文本。
    #[test]
    fn control_characters_are_ignored() {
        let (mut modal, _wait) = modal();
        modal.push('\n');
        modal.push('\r');
        modal.push('\t');
        assert!(modal.input.is_empty());
    }

    #[test]
    fn submit_sends_the_trimmed_answer_once() {
        let (mut modal, wait) = modal();
        modal.push(' ');
        modal.push('4');
        modal.push('2');
        modal.push(' ');
        assert_eq!(modal.submit().as_deref(), Some("42"));
        assert_eq!(wait.try_recv().unwrap().as_deref(), Some("42"));
        // 第二次提交没有通道可送了：不能伪造一个答案。
        assert!(modal.submit().is_none());
    }

    #[test]
    fn cancel_sends_none() {
        let (mut modal, wait) = modal();
        modal.cancel();
        assert_eq!(wait.try_recv().unwrap(), None);
    }

    /// 密码要打码，验证码不打（用户要能看见自己输了什么）。
    #[test]
    fn only_passwords_are_masked() {
        let (modal, _wait) = modal();
        assert!(!modal.masked());
        let (reply, _wait) = channel();
        let password = LoginModal::new(AskKind::Password, "两步验证密码".to_owned(), reply);
        assert!(password.masked());
    }
}
