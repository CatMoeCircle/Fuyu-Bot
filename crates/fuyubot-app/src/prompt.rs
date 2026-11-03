//! 把 [`AppHost`] 包成 `fuyubot_tdlib::Prompter`。
//!
//! 登录状态机只认 `Prompter`（`ask` / `choose_login_method` / `notify` / `show_qr`），
//! 而各宿主的实现千差万别（stdin / 弹窗 / Java 回调）。这一层是两者之间**唯一**的桥：
//! 提问文本 → [`AskKind`]、回答 → `String`、二维码 → `qr`。
//!
//! ⚠️ 提问文本是 `fuyubot-tdlib` 里的**人话**字符串（例如「6 位验证码」）。
//! [`kind_of`] 靠前缀认它：认不出来就报 [`AskKind::Other`]，宿主把 prompt 原样显示——
//! 文本以后改了也不会坏，只是输入框的提示语没那么贴心。

use crate::host::{AppHost, AskKind};

use fuyubot_tdlib::{LoginError, LoginMethod, Prompter};

/// 提问文本 → 种类。前缀是在 `fuyubot-tdlib` 的 `authorize::ask_and_send` 里写死的。
pub fn kind_of(question: &str) -> AskKind {
    if question.starts_with("Bot Token") {
        AskKind::BotToken
    } else if question.starts_with("手机号") {
        AskKind::Phone
    } else if question.contains("验证码") {
        AskKind::Code
    } else if question.starts_with("两步验证密码") {
        AskKind::Password
    } else {
        AskKind::Other
    }
}

/// 宿主给的 [`AppHost`] + 宿主预先提供的 bot token → `Prompter`。
pub struct HostPrompter<'a> {
    host: &'a dyn AppHost,
    /// 宿主**已经给过**的 bot token：这一类问题不弹 UI，直接用（与 CLI 读 `TG_BOT_TOKEN` 同一条路）。
    bot_token: Option<&'a str>,
}

impl<'a> HostPrompter<'a> {
    pub fn new(host: &'a dyn AppHost, bot_token: Option<&'a str>) -> Self {
        Self { host, bot_token }
    }
}

impl Prompter for HostPrompter<'_> {
    fn ask(&mut self, question: &str) -> Result<String, LoginError> {
        let kind = kind_of(question);
        if kind == AskKind::BotToken
            && let Some(token) = self.bot_token
        {
            return Ok(token.to_owned());
        }

        match self.host.ask(kind, question) {
            Some(answer) if !answer.trim().is_empty() => Ok(answer),
            // 超时 / 取消 / 宿主不支持交互：都算「这一步没拿到输入」。
            _ => Err(LoginError::NoInput {
                question: question.to_owned(),
            }),
        }
    }

    fn choose_login_method(&mut self) -> Result<LoginMethod, LoginError> {
        let question = "登录方式（回复 phone 用手机号，qr 用二维码）";
        match self.host.ask(AskKind::LoginMethod, question) {
            Some(answer) if answer.trim().eq_ignore_ascii_case("qr") => Ok(LoginMethod::QrCode),
            Some(answer) if !answer.trim().is_empty() => Ok(LoginMethod::PhoneNumber),
            _ => Err(LoginError::NoInput {
                question: question.to_owned(),
            }),
        }
    }

    fn notify(&mut self, message: &str) {
        // 不打扰宿主 UI：登录期的状态变化本来就密集，进日志就够。
        tracing::info!(target: "fuyubot::app", "登录：{message}");
    }

    fn show_qr(&mut self, link: &str) {
        self.host.qr(link);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct RecordingHost {
        asked: Mutex<Vec<(AskKind, String)>>,
        answer: Option<String>,
    }

    impl AppHost for RecordingHost {
        fn state(&self, _state: crate::host::AppState, _detail: &str) {}
        fn ask(&self, kind: AskKind, prompt: &str) -> Option<String> {
            self.asked.lock().unwrap().push((kind, prompt.to_owned()));
            self.answer.clone()
        }
        fn qr(&self, _link: &str) {}
    }

    #[test]
    fn kinds_follow_the_prompts_in_tdlib() {
        assert_eq!(
            kind_of("Bot Token（@BotFather 给的那串）"),
            AskKind::BotToken
        );
        assert_eq!(
            kind_of("手机号（带国家码，例如 +8613800138000）"),
            AskKind::Phone
        );
        assert_eq!(kind_of("6 位验证码"), AskKind::Code);
        assert_eq!(
            kind_of("验证码（按短信/来电里给的原样输入）"),
            AskKind::Code
        );
        assert_eq!(kind_of("两步验证密码（提示：生日）"), AskKind::Password);
        assert_eq!(kind_of("将来的新问题"), AskKind::Other);
    }

    /// 给过 bot token 就不该去问宿主（宿主那边会记下「被问了」，这里断言没记录）。
    #[test]
    fn bot_token_is_answered_without_asking_the_host() {
        let host = RecordingHost::default();
        let mut prompter = HostPrompter::new(&host, Some("1:AAA"));
        assert_eq!(
            prompter.ask("Bot Token（@BotFather 给的那串）").unwrap(),
            "1:AAA"
        );
        assert!(host.asked.lock().unwrap().is_empty());
    }

    /// 手机号这类问题不会用 bot token 冒充：照常问宿主。
    #[test]
    fn other_questions_still_go_to_the_host() {
        let host = RecordingHost {
            answer: Some("+8613800138000".to_owned()),
            ..Default::default()
        };
        let mut prompter = HostPrompter::new(&host, Some("1:AAA"));
        assert_eq!(
            prompter
                .ask("手机号（带国家码，例如 +8613800138000）")
                .unwrap(),
            "+8613800138000"
        );
        assert_eq!(host.asked.lock().unwrap().len(), 1);
    }

    /// 宿主说「没有」时必须是 `NoInput`，不能把空串交给状态机。
    #[test]
    fn no_answer_becomes_no_input() {
        let host = RecordingHost::default();
        let mut prompter = HostPrompter::new(&host, None);
        assert!(matches!(
            prompter.ask("6 位验证码"),
            Err(LoginError::NoInput { .. })
        ));
    }

    /// 选登录方式：`qr` 走扫码，其它一律按手机号。
    #[test]
    fn login_method_maps_the_answer() {
        let host = RecordingHost {
            answer: Some("qr".to_owned()),
            ..Default::default()
        };
        let mut prompter = HostPrompter::new(&host, None);
        assert_eq!(prompter.choose_login_method().unwrap(), LoginMethod::QrCode);

        let host = RecordingHost {
            answer: Some("phone".to_owned()),
            ..Default::default()
        };
        let mut prompter = HostPrompter::new(&host, None);
        assert_eq!(
            prompter.choose_login_method().unwrap(),
            LoginMethod::PhoneNumber
        );
    }
}
