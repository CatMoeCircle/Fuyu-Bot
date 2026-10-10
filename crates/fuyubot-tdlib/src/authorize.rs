//! 授权：把 TDLib 的 `updateAuthorizationState` 状态机跑到底（含二维码扫码登录）。
//!
//! 这段逻辑原来只活在 `tests/integration.rs` 里（验收夹具），现在提成产品代码——
//! `fuyubot login` 与 `fuyubot run` 都走它。
//!
//! **两条底线**：
//! 1. TDLib 的方法名、参数名、**嵌套字段路径**全部对着真 `td_api.tl` 核过，不猜（AGENTS 42/43）。
//!    已经抓到两处按直觉一定会写错的：验证码长度在 **`code_info.type.length`**（不是
//!    `code_info.length`，而且「短信里的词/短语」「flash call」这几种类型**根本没有**这个字段）；
//!    `phoneNumberAuthenticationSettings` 有 **7 个字段**，少了真库会抱怨。
//! 2. 拼请求的部分做成**纯函数**（[`request_for`]），所以能离线拿烤进二进制的真 schema 校验，
//!    写错名字立刻红——`every_auth_request_passes_the_real_schema` 就是那道闸。
//!
//! 循环本身（[`log_in`]）需要真客户端，只有 `-- --ignored` 那条集成测试能真跑。
//!
//! # 与运行期的分工（用户拍板）
//!
//! 「登录」与「运行」是**两个判断**：登录阶段用**自己**的客户端把状态机跑完，
//! 到 `Ready` 之后**重启一个客户端**进运行期（插件在运行期客户端创建**之前**全部加载好，
//! 免得漏掉更新）。运行期还要盯着 `updateAuthorizationState`：
//! 一旦从 `Ready` 掉出去（被下线/封禁），就得报错并退回登录——[`AuthState::needs_re_login`]
//! 就是给那个判断用的。

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use thiserror::Error;

use crate::client::{Routed, TdClient, TdlibError};

/// 账号类型。**配置里记它**（用户拍板：bot 与 user 必须分开）。
///
/// 它决定登录走哪条路（bot → Bot Token；user → 手机号 / 扫码），也决定后续发送路由。
/// 注意凭据本身（token / phone / password）**不落配置**（AGENTS 约束 41），只落类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AccountType {
    #[default]
    Bot,
    User,
}

impl AccountType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bot => "bot",
            Self::User => "user",
        }
    }

    /// 从配置里读。**大小写不敏感**，认不出就是 `None`（让调用方报错，别默认成 bot）。
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "bot" => Some(Self::Bot),
            "user" => Some(Self::User),
            _ => None,
        }
    }
}

/// `setTdlibParameters` 的参数。
///
/// 字段与真 `td_api.tl` 一一对应（共 15 个）。两个容易写错的：
/// `database_encryption_key` 是 `bytes`（真库接受**空字符串**），`api_id` 是 **int32**。
#[derive(Debug, Clone)]
pub struct TdlibParameters {
    /// `database_directory` 与 `files_directory` **都指这个目录**（用户拍板：只建 `data/`，
    /// 不再拆 `data/db` 与 `data/files`）。TDLib 允许两者同目录，代价是下载的文件与
    /// 数据库文件混在一起。
    pub data_dir: PathBuf,
    pub api_id: i32,
    pub api_hash: String,
    pub use_test_dc: bool,
    pub system_language_code: String,
    pub device_model: String,
    pub system_version: String,
    pub application_version: String,
}

impl TdlibParameters {
    /// 除了目录与 api 凭据，其余给合理默认。
    pub fn new(data_dir: impl Into<PathBuf>, api_id: i32, api_hash: impl Into<String>) -> Self {
        Self {
            data_dir: data_dir.into(),
            api_id,
            api_hash: api_hash.into(),
            use_test_dc: false,
            system_language_code: "en".to_owned(),
            device_model: "fuyubot".to_owned(),
            system_version: std::env::consts::OS.to_owned(),
            application_version: env!("CARGO_PKG_VERSION").to_owned(),
        }
    }

    /// 拼 `setTdlibParameters`（15 个字段一个不少）。
    ///
    /// 三个 `use_*_database` 都是 true：插件靠更新驱动，没有消息库 TDLib 的能力会残。
    /// `use_secret_chats` 是 false——还没有加密聊天相关的功能。
    pub fn to_request(&self) -> Value {
        let dir = self.data_dir.display().to_string();
        json!({
            "@type": "setTdlibParameters",
            "use_test_dc": self.use_test_dc,
            "database_directory": dir.clone(),
            "files_directory": dir,
            "database_encryption_key": "",
            "use_file_database": true,
            "use_chat_info_database": true,
            "use_message_database": true,
            "use_secret_chats": false,
            "api_id": self.api_id,
            "api_hash": self.api_hash,
            "system_language_code": self.system_language_code,
            "device_model": self.device_model,
            "system_version": self.system_version,
            "application_version": self.application_version,
        })
    }
}

/// 真 `td_api.tl` 里的授权状态。
///
/// 那 13 个名字**一个不多一个不少**；认不出来的进 [`AuthState::Unknown`]——**不猜**
/// （和 AGENTS 24 判 `td_api.tl` 分区时的态度一样）。`every_real_authorization_state_is_known`
/// 会拿真 schema 反过来钉住这个覆盖：TDLib 以后加了状态，那条测试立刻红。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthState {
    WaitTdlibParameters,
    WaitPhoneNumber,
    /// 二维码/链接登录：等用户在手机上确认。`link` 形如 `tg://login?token=…`。
    WaitOtherDeviceConfirmation {
        link: String,
    },
    /// 等验证码。`length` 来自 **`code_info.type.length`**，**可能是 `None`**——
    /// 「短信里的词/短语」「flash call」那几种类型没有长度字段。
    WaitCode {
        phone_number: String,
        length: Option<u64>,
        code_type: String,
    },
    WaitPassword {
        hint: String,
        has_recovery_email: bool,
    },
    WaitRegistration,
    WaitEmailAddress,
    WaitEmailCode,
    WaitPremiumPurchase {
        product: String,
    },
    Ready,
    LoggingOut,
    Closing,
    Closed,
    Unknown(String),
}

/// 取一个字符串字段；没有就给空串（这些字段只用来显示，不该因为它缺就让流程崩）。
fn text(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

impl AuthState {
    /// 从 `@type` 名字认。**纯函数**，是覆盖率的落点。
    pub fn from_type_name(name: &str) -> Self {
        match name {
            "authorizationStateWaitTdlibParameters" => Self::WaitTdlibParameters,
            "authorizationStateWaitPhoneNumber" => Self::WaitPhoneNumber,
            "authorizationStateWaitOtherDeviceConfirmation" => Self::WaitOtherDeviceConfirmation {
                link: String::new(),
            },
            "authorizationStateWaitCode" => Self::WaitCode {
                phone_number: String::new(),
                length: None,
                code_type: String::new(),
            },
            "authorizationStateWaitPassword" => Self::WaitPassword {
                hint: String::new(),
                has_recovery_email: false,
            },
            "authorizationStateWaitRegistration" => Self::WaitRegistration,
            "authorizationStateWaitEmailAddress" => Self::WaitEmailAddress,
            "authorizationStateWaitEmailCode" => Self::WaitEmailCode,
            "authorizationStateWaitPremiumPurchase" => Self::WaitPremiumPurchase {
                product: String::new(),
            },
            "authorizationStateReady" => Self::Ready,
            "authorizationStateLoggingOut" => Self::LoggingOut,
            "authorizationStateClosing" => Self::Closing,
            "authorizationStateClosed" => Self::Closed,
            other => Self::Unknown(other.to_owned()),
        }
    }

    /// 从 `updateAuthorizationState` 的负载里认，并把附加字段填上。
    ///
    /// **字段路径都对着真 `td_api.tl` 核过**（AGENTS 42）：`link`、
    /// `code_info.{phone_number,type}`、`type.length`、`password_hint`、
    /// `has_recovery_email_address`、`store_product_id`。
    pub fn from_update(payload: &Value) -> Self {
        let state = payload.get("authorization_state").unwrap_or(&Value::Null);
        let name = state
            .get("@type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let mut parsed = Self::from_type_name(name);
        match &mut parsed {
            Self::WaitOtherDeviceConfirmation { link } => *link = text(state, "link"),
            Self::WaitCode {
                phone_number,
                length,
                code_type,
            } => {
                let info = state.get("code_info").unwrap_or(&Value::Null);
                *phone_number = text(info, "phone_number");
                // 长度在 type 里，不在 code_info 顶层；有些类型压根没有这个字段。
                let kind = info.get("type").unwrap_or(&Value::Null);
                *code_type = text(kind, "@type");
                *length = kind.get("length").and_then(Value::as_u64);
            }
            Self::WaitPassword {
                hint,
                has_recovery_email,
            } => {
                *hint = text(state, "password_hint");
                *has_recovery_email = state
                    .get("has_recovery_email_address")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
            }
            Self::WaitPremiumPurchase { product } => *product = text(state, "store_product_id"),
            _ => {}
        }
        parsed
    }

    /// 用来打日志/报错的名字。
    pub fn name(&self) -> &str {
        match self {
            Self::WaitTdlibParameters => "authorizationStateWaitTdlibParameters",
            Self::WaitPhoneNumber => "authorizationStateWaitPhoneNumber",
            Self::WaitOtherDeviceConfirmation { .. } => {
                "authorizationStateWaitOtherDeviceConfirmation"
            }
            Self::WaitCode { .. } => "authorizationStateWaitCode",
            Self::WaitPassword { .. } => "authorizationStateWaitPassword",
            Self::WaitRegistration => "authorizationStateWaitRegistration",
            Self::WaitEmailAddress => "authorizationStateWaitEmailAddress",
            Self::WaitEmailCode => "authorizationStateWaitEmailCode",
            Self::WaitPremiumPurchase { .. } => "authorizationStateWaitPremiumPurchase",
            Self::Ready => "authorizationStateReady",
            Self::LoggingOut => "authorizationStateLoggingOut",
            Self::Closing => "authorizationStateClosing",
            Self::Closed => "authorizationStateClosed",
            Self::Unknown(name) => name,
        }
    }

    /// 运行期用：已经登好、可以收更新了。
    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready)
    }

    /// 运行期用：**从 `Ready` 掉出来了**（被下线、被封、会话失效），要退回登录。
    ///
    /// 只把「要用户再出一次凭据」的状态算进来；`LoggingOut`/`Closing`/`Closed` 是
    /// 我们自己要退出的路上会看到的状态，不算「被踢」。
    pub fn needs_re_login(&self) -> bool {
        matches!(
            self,
            Self::WaitPhoneNumber
                | Self::WaitCode { .. }
                | Self::WaitPassword { .. }
                | Self::WaitOtherDeviceConfirmation { .. }
        )
    }
}

/// 用户在 `WaitPhoneNumber` 时选的登录方式。
///
/// **只有 user 账号才需要选**：bot 账号直接要 Bot Token，没有别的路。
/// 注意扫码登录只能登**用户账号**，bot 扫不了。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginMethod {
    PhoneNumber,
    QrCode,
}

/// 面对某个授权状态，要问用户什么。**纯函数**，所以离线可测。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Question {
    /// 不用问（参数我们直接给，或者状态本身在等 TDLib）也不用发请求。
    Nothing,
    /// 让用户选登录方式（只有 user 账号会走到这条）。
    LoginMethod,
    BotToken,
    PhoneNumber,
    /// 把二维码/链接给他，等他在手机上确认——**不需要输入**。
    ShowQr {
        link: String,
    },
    Code {
        length: Option<u64>,
    },
    Password {
        hint: String,
    },
    /// 本版不处理。带上是哪个状态与为什么，报错时要能指路。
    Unsupported {
        state: String,
        reason: String,
    },
}

/// 某个状态下该问什么。
pub fn question_for(
    state: &AuthState,
    account: AccountType,
    chosen: Option<LoginMethod>,
) -> Question {
    match state {
        AuthState::WaitPhoneNumber => match account {
            // bot 只能填 token，不用选
            AccountType::Bot => Question::BotToken,
            // user：还没选过就先选；选了手机号就问号码；选了扫码就**不问**（直接发请求）
            AccountType::User => match chosen {
                None => Question::LoginMethod,
                Some(LoginMethod::PhoneNumber) => Question::PhoneNumber,
                Some(LoginMethod::QrCode) => Question::Nothing,
            },
        },
        AuthState::WaitOtherDeviceConfirmation { link } => Question::ShowQr { link: link.clone() },
        AuthState::WaitCode { length, .. } => Question::Code { length: *length },
        AuthState::WaitPassword { hint, .. } => Question::Password { hint: hint.clone() },
        AuthState::WaitRegistration => Question::Unsupported {
            state: state.name().to_owned(),
            reason: "这个会话要注册新账号（填名字），本版不处理；请先在官方客户端注册".to_owned(),
        },
        AuthState::WaitEmailAddress | AuthState::WaitEmailCode => Question::Unsupported {
            state: state.name().to_owned(),
            reason: "走的是邮箱登录/验证那条线，本版只处理 Bot Token、手机号、二维码".to_owned(),
        },
        AuthState::WaitPremiumPurchase { .. } => Question::Unsupported {
            state: state.name().to_owned(),
            reason: "TDLib 要求先买 premium，本版不处理".to_owned(),
        },
        AuthState::Unknown(name) => Question::Unsupported {
            state: name.clone(),
            reason: "本版不认识的授权状态（可能 TDLib 版本比我们新）".to_owned(),
        },
        // 这几种要么不用问（参数我们给），要么在等 TDLib 自己往前走。
        AuthState::WaitTdlibParameters
        | AuthState::Ready
        | AuthState::LoggingOut
        | AuthState::Closing
        | AuthState::Closed => Question::Nothing,
    }
}

/// 用「状态 + 用户的选择 + 用户的回答」拼出要发的请求。**纯函数**。
///
/// 返回 `Ok(None)` = 这个状态不需要发请求（[`AuthState::WaitOtherDeviceConfirmation`]
/// 是在等用户扫码，不是发什么）。
pub fn request_for(
    parameters: &TdlibParameters,
    state: &AuthState,
    account: AccountType,
    chosen: Option<LoginMethod>,
    answer: &str,
) -> Result<Option<Value>, LoginError> {
    Ok(match state {
        AuthState::WaitTdlibParameters => Some(parameters.to_request()),
        AuthState::WaitPhoneNumber => match account {
            AccountType::Bot => Some(json!({
                "@type": "checkAuthenticationBotToken",
                "token": answer,
            })),
            AccountType::User => match chosen {
                // 扫码：让 TDLib 给一个 link，下一秒就会来 WaitOtherDeviceConfirmation。
                // `other_user_ids` 是 vector<int53>，我们不让别的用户共享这次会话，给空数组。
                Some(LoginMethod::QrCode) => Some(json!({
                    "@type": "requestQrCodeAuthentication",
                    "other_user_ids": [],
                })),
                // 手机号。settings 的 7 个字段要写全（class 类型没有值就给 null）——
                // 只写 `{"@type": ...}` 真库会抱怨缺字段。
                _ => Some(json!({
                    "@type": "setAuthenticationPhoneNumber",
                    "phone_number": answer,
                    "settings": {
                        "@type": "phoneNumberAuthenticationSettings",
                        "allow_flash_call": false,
                        "allow_missed_call": false,
                        "is_current_phone_number": false,
                        "has_unknown_phone_number": false,
                        "allow_sms_retriever_api": false,
                        "firebase_authentication_settings": null,
                        "authentication_tokens": [],
                    },
                })),
            },
        },
        AuthState::WaitCode { .. } => Some(json!({
            "@type": "checkAuthenticationCode",
            "code": answer,
        })),
        AuthState::WaitPassword { .. } => Some(json!({
            "@type": "checkAuthenticationPassword",
            "password": answer,
        })),
        // 其余状态要么不用发（等 TDLib / 等用户扫码），要么在 question_for 里就被挡掉了。
        AuthState::WaitOtherDeviceConfirmation { .. }
        | AuthState::Ready
        | AuthState::LoggingOut
        | AuthState::Closing
        | AuthState::Closed => None,
        other => {
            return Err(match question_for(other, account, chosen) {
                Question::Unsupported { state, reason } => {
                    LoginError::UnsupportedState { state, reason }
                }
                _ => LoginError::UnsupportedState {
                    state: other.name().to_owned(),
                    reason: "这个状态没有对应的请求".to_owned(),
                },
            });
        }
    })
}

/// 收输入与展示的出口。
///
/// CLI 用 stdin 实现，测试用脚本化回答实现——两边都不用改状态机。
pub trait Prompter {
    /// 纯文本提问。答案会被原样交给 TDLib。
    fn ask(&mut self, question: &str) -> Result<String, LoginError>;

    /// 让用户挑登录方式（只有 user 账号会走到）。
    fn choose_login_method(&mut self) -> Result<LoginMethod, LoginError>;

    /// 不阻塞的提示（状态变化、TDLib 报错等）。
    fn notify(&mut self, message: &str);

    /// 展示二维码/链接。
    ///
    /// **默认只把链接打出来**：真正的终端二维码要引一个 QR 编码依赖（那件事定了再补，
    /// 见 `ROADMAP.md`）。补的时候只覆盖这个方法，状态机一行都不用动。
    fn show_qr(&mut self, link: &str) {
        let message = format!("用手机 Telegram 的「设置 → 设备 → 扫描二维码」扫这个链接：{link}");
        self.notify(&message);
    }
}

/// 登录阶段跑完的结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginOutcome {
    /// 到 `Ready` 了，可以进运行期。
    Ready,
    /// TDLib 把会话关掉了（`Closed`）。
    Closed,
}

#[derive(Debug, Error)]
pub enum LoginError {
    #[error("TDLib 调用失败：{0}")]
    Tdlib(#[from] TdlibError),

    #[error("用户没给出「{question}」这一步需要的输入")]
    NoInput { question: String },

    /// 本版不处理的授权状态。带上是哪个状态、为什么——报错要能指路。
    #[error("这个授权状态本版不处理：{state}（{reason}）")]
    UnsupportedState { state: String, reason: String },

    #[error("等了 {seconds} 秒还没登录完，停在 {last_state}")]
    Timeout { seconds: u64, last_state: String },
}

/// 问一个问题、把请求发出去。
///
/// `awaiting` 记住「刚把哪一问的答案发出去了」：请求失败时（验证码错、密码错）
/// TDLib **不会再报一次授权状态**，所以只能靠它带着报错重问。
fn ask_and_send(
    client: &TdClient,
    parameters: &TdlibParameters,
    state: &AuthState,
    account: AccountType,
    chosen: &mut Option<LoginMethod>,
    prompter: &mut dyn Prompter,
    awaiting: &mut Option<AuthState>,
) -> Result<(), LoginError> {
    let answer = loop {
        break match question_for(state, account, *chosen) {
            // 先选登录方式，选完重新算这一问
            Question::LoginMethod => {
                *chosen = Some(prompter.choose_login_method()?);
                continue;
            }
            // 等用户扫码：不提问、不发请求，等 TDLib 把状态推过来
            Question::ShowQr { link } => {
                prompter.show_qr(&link);
                *awaiting = None;
                return Ok(());
            }
            Question::Nothing => String::new(),
            Question::BotToken => prompter.ask("Bot Token（@BotFather 给的那串）")?,
            Question::PhoneNumber => prompter.ask("手机号（带国家码，例如 +8613800138000）")?,
            Question::Code { length } => {
                let question = match length {
                    Some(length) => format!("{length} 位验证码"),
                    // 没有长度字段就算了——别编一个默认位数
                    None => "验证码（按短信/来电里给的原样输入）".to_owned(),
                };
                prompter.ask(&question)?
            }
            Question::Password { hint } => {
                prompter.ask(&format!("两步验证密码（提示：{hint}）"))?
            }
            Question::Unsupported { state, reason } => {
                return Err(LoginError::UnsupportedState { state, reason });
            }
        };
    };

    if answer.trim().is_empty() {
        return Err(LoginError::NoInput {
            question: state.name().to_owned(),
        });
    }

    if let Some(request) = request_for(parameters, state, account, *chosen, answer.trim())? {
        client.send(request)?;
        *awaiting = Some(state.clone());
    }
    Ok(())
}

/// 驱动授权状态机，直到 `Ready` / `Closed` / 超时。
///
/// 调用方给一个**已经建好、还没发过参数**的客户端——第一条 `setTdlibParameters`
/// 由本函数在 `WaitTdlibParameters` 时发出（那时才知道要发）。
pub fn log_in(
    client: &TdClient,
    parameters: &TdlibParameters,
    account: AccountType,
    prompter: &mut dyn Prompter,
    budget: Duration,
) -> Result<LoginOutcome, LoginError> {
    let deadline = Instant::now() + budget;
    let mut chosen: Option<LoginMethod> = None;
    let mut awaiting: Option<AuthState> = None;
    let mut last_state = AuthState::WaitTdlibParameters;

    while Instant::now() < deadline {
        let Some(routed) = client.receive(Duration::from_millis(500)) else {
            continue;
        };
        match routed {
            Routed::Response(response) => {
                let Some(message) = response.error_message() else {
                    continue;
                };
                let message = message.to_owned();
                prompter.notify(&format!("TDLib 说这一步没成：{message}"));
                // 刚才那一问失败 → 带着报错重问（状态不会再来一次）
                if let Some(state) = awaiting.take() {
                    ask_and_send(
                        client,
                        parameters,
                        &state,
                        account,
                        &mut chosen,
                        prompter,
                        &mut awaiting,
                    )?;
                }
            }
            Routed::Update(update) => {
                if update.kind != "updateAuthorizationState" {
                    continue;
                }
                let state = AuthState::from_update(&update.payload);
                prompter.notify(&format!("授权状态：{}", state.name()));
                match state {
                    AuthState::Ready => return Ok(LoginOutcome::Ready),
                    AuthState::Closed => return Ok(LoginOutcome::Closed),
                    // 注销路上会看到的中间态：等它走到 Closed
                    AuthState::LoggingOut | AuthState::Closing => {
                        last_state = state;
                    }
                    other => {
                        last_state = other.clone();
                        ask_and_send(
                            client,
                            parameters,
                            &other,
                            account,
                            &mut chosen,
                            prompter,
                            &mut awaiting,
                        )?;
                    }
                }
            }
        }
    }

    Err(LoginError::Timeout {
        seconds: budget.as_secs(),
        last_state: last_state.name().to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TdApiSchema;

    fn parameters() -> TdlibParameters {
        TdlibParameters::new("data", 12345, "hash")
    }

    /// 真 schema 反过来钉住我们的覆盖：TDLib 以后加了授权状态，这条立刻红。
    ///
    /// 这是 AGENTS 42/43 的正向用法——**先问真库有哪些状态**，再决定要不要处理，
    /// 而不是凭印象列几个。
    #[test]
    fn every_real_authorization_state_is_known() {
        let schema = TdApiSchema::compiled();
        let real: Vec<&str> = schema
            .entries()
            .iter()
            .map(|entry| entry.name.as_str())
            .filter(|name| name.starts_with("authorizationState"))
            .collect();
        assert!(
            real.len() >= 13,
            "真 td_api.tl 里的授权状态少于 13 个，这条测试的前提变了：{real:?}"
        );
        for name in real {
            let state = AuthState::from_type_name(name);
            assert!(
                !matches!(state, AuthState::Unknown(_)),
                "{name} 没被识别——TDLib 加了新状态？它必须在这里有个明确归属"
            );
        }
    }

    /// 我们拼的**每一条**授权请求都要过真 schema（离线跑，写错名字/参数名立刻红）。
    ///
    /// 只挡拼写——`check_request` 不查必填，所以字段写不全它不会报（那要靠真库）。
    #[test]
    fn every_auth_request_passes_the_real_schema() {
        let schema = TdApiSchema::compiled();
        let parameters = parameters();
        let cases: Vec<(AuthState, AccountType, Option<LoginMethod>, &str)> = vec![
            (AuthState::WaitTdlibParameters, AccountType::Bot, None, ""),
            (
                AuthState::WaitPhoneNumber,
                AccountType::Bot,
                None,
                "123:ABC",
            ),
            (
                AuthState::WaitPhoneNumber,
                AccountType::User,
                Some(LoginMethod::PhoneNumber),
                "+8613800138000",
            ),
            (
                AuthState::WaitPhoneNumber,
                AccountType::User,
                Some(LoginMethod::QrCode),
                "",
            ),
            (
                AuthState::WaitCode {
                    phone_number: "+8613800138000".to_owned(),
                    length: Some(5),
                    code_type: "authenticationCodeTypeSms".to_owned(),
                },
                AccountType::User,
                Some(LoginMethod::PhoneNumber),
                "12345",
            ),
            (
                AuthState::WaitPassword {
                    hint: "生日".to_owned(),
                    has_recovery_email: false,
                },
                AccountType::User,
                None,
                "hunter2",
            ),
        ];

        for (state, account, chosen, answer) in cases {
            let request = request_for(&parameters, &state, account, chosen, answer)
                .unwrap_or_else(|e| panic!("{state:?} 不该是「不处理」：{e}"))
                .unwrap_or_else(|| panic!("{state:?} 应该要发个请求"));
            let name = request["@type"].as_str().expect("@type 必须是字符串");
            schema
                .check_request(name, &request)
                .unwrap_or_else(|e| panic!("{name} 过不了真 schema：{e}"));
        }
    }

    /// 验证码长度在 `code_info.type.length`，不在顶层；而且**有些类型没有它**。
    #[test]
    fn code_length_comes_from_the_code_type_and_may_be_absent() {
        let sms = json!({
            "authorization_state": {
                "@type": "authorizationStateWaitCode",
                "code_info": {
                    "phone_number": "+8613800138000",
                    "type": {"@type": "authenticationCodeTypeSms", "length": 5},
                },
            }
        });
        assert_eq!(
            AuthState::from_update(&sms),
            AuthState::WaitCode {
                phone_number: "+8613800138000".to_owned(),
                length: Some(5),
                code_type: "authenticationCodeTypeSms".to_owned(),
            }
        );

        // 「短信里的词」那几种类型没有 length——不许猜一个默认位数
        let word = json!({
            "authorization_state": {
                "@type": "authorizationStateWaitCode",
                "code_info": {
                    "phone_number": "+8613800138000",
                    "type": {"@type": "authenticationCodeTypeSmsWord", "first_letter": "a"},
                },
            }
        });
        match AuthState::from_update(&word) {
            AuthState::WaitCode { length, .. } => assert_eq!(length, None),
            other => panic!("认成 {other:?} 了"),
        }
    }

    /// bot 与 user 必须走不同的路——这是「把 account_type 加回来」的核心理由。
    #[test]
    fn bot_and_user_take_different_paths() {
        use AccountType::{Bot, User};

        assert_eq!(
            question_for(&AuthState::WaitPhoneNumber, Bot, None),
            Question::BotToken
        );
        assert_eq!(
            question_for(&AuthState::WaitPhoneNumber, User, None),
            Question::LoginMethod
        );
        assert_eq!(
            question_for(
                &AuthState::WaitPhoneNumber,
                User,
                Some(LoginMethod::PhoneNumber)
            ),
            Question::PhoneNumber
        );
        // 选扫码就**不再问**，直接把请求发出去
        assert_eq!(
            question_for(&AuthState::WaitPhoneNumber, User, Some(LoginMethod::QrCode)),
            Question::Nothing
        );

        // bot 账号永远拿不到扫码这条路：请求里必须还是 checkAuthenticationBotToken
        let bot = request_for(&parameters(), &AuthState::WaitPhoneNumber, Bot, None, "tok")
            .unwrap()
            .unwrap();
        assert_eq!(bot["@type"], "checkAuthenticationBotToken");

        let qr = request_for(
            &parameters(),
            &AuthState::WaitPhoneNumber,
            User,
            Some(LoginMethod::QrCode),
            "",
        )
        .unwrap()
        .unwrap();
        assert_eq!(qr["@type"], "requestQrCodeAuthentication");
        assert_eq!(qr["other_user_ids"], json!([]));
    }

    /// 二维码那一步是把 link 交给 prompter，**不提问也不发请求**。
    #[test]
    fn qr_step_shows_the_link_instead_of_asking() {
        let state = AuthState::WaitOtherDeviceConfirmation {
            link: "tg://login?token=abc".to_owned(),
        };
        assert_eq!(
            question_for(&state, AccountType::User, None),
            Question::ShowQr {
                link: "tg://login?token=abc".to_owned()
            }
        );
        assert_eq!(
            request_for(&parameters(), &state, AccountType::User, None, "").unwrap(),
            None
        );
    }

    /// 运行期判断：`Ready` 之外那几个「要再出凭据」的状态必须能认出来。
    #[test]
    fn only_credential_states_count_as_needing_re_login() {
        assert!(AuthState::Ready.is_ready());
        assert!(
            AuthState::WaitCode {
                phone_number: String::new(),
                length: None,
                code_type: String::new(),
            }
            .needs_re_login()
        );
        assert!(AuthState::WaitPhoneNumber.needs_re_login());
        // 自己注销的路上不算「被踢」
        assert!(!AuthState::LoggingOut.needs_re_login());
        assert!(!AuthState::Closing.needs_re_login());
        assert!(!AuthState::Closed.needs_re_login());
        assert!(!AuthState::Ready.needs_re_login());
    }

    /// 不认识的状态必须报「不处理」并带上名字，而不是静默卡住。
    #[test]
    fn unknown_states_fail_loudly() {
        let state = AuthState::from_type_name("authorizationStateSomethingNew");
        match question_for(&state, AccountType::Bot, None) {
            Question::Unsupported { state, reason } => {
                assert_eq!(state, "authorizationStateSomethingNew");
                assert!(!reason.is_empty());
            }
            other => panic!("认成 {other:?} 了"),
        }
    }
}
