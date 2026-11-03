//! 一次 TDLib 会话：建客户端 → 登录 → `getMe` → 关会话。
//!
//! 为什么把这四步从宿主侧拿回来：它们是**纯 TDLib 的事**（只用本 crate 的类型），
//! 而「什么时候接管线、什么时候装插件、什么时候收摊」是宿主的事。
//! 拆开之后 CLI 与 Android 的 JNI 走同一份登录流程，各自只提供自己的 [`Prompter`]。
//!
//! 顺序上有一件事必须由调用方保证（本模块**不**替它做）：**插件先装好，再调 [`Session::login`]**。
//! 登录窗口里 TDLib 已经在产出更新，插件晚一步就漏掉了；`login` 把这些东西经 `bypass`
//! 交回给调用方（一般是 `UpdatePipeline::dispatch`），不吞。
//!
//! ```text
//! Session::create()  →  pipeline.attach_tdlib(client)  →  装插件  →  Session::login(...)
//!                                                                      ↓
//!                                                          spawn_dispatch(client)
//! ```
//!
//! ⚠️ `login` 与 [`get_me`] 内部自己调 `td_receive`：**期间不能有接收循环**在跑，
//! 否则两条 `td_receive` 会互相抢更新。

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::authorize::{
    AccountType, LoginError, LoginOutcome, Prompter, TdlibParameters, log_in_with,
};
use crate::client::{Routed, TdClient, TdlibError};

/// 登录总预算（交互式登录：等验证码 / 扫码可能要好几分钟）。
pub const LOGIN_BUDGET: Duration = Duration::from_secs(600);

/// `getMe` 的等待上限。拿不到不影响运行。
pub const GET_ME_BUDGET: Duration = Duration::from_secs(15);

/// 会话这一路可能出的错。
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// 建客户端失败（库找不到 / 符号缺失 / 句柄是空的）。
    #[error("{0}")]
    Client(#[from] TdlibError),
    /// 登录失败（超时 / 用户取消 / TDLib 报错）。
    #[error("{0}")]
    Login(#[from] LoginError),
    /// TDLib 把会话关掉了（`authorizationStateClosed`）。
    #[error("TDLib 把会话关掉了（authorizationStateClosed）")]
    Closed,
}

/// 一个活着的 TDLib 客户端 + 它登录后的账户信息。
pub struct Session {
    client: Arc<TdClient>,
    me: Option<Value>,
}

// `TdClient` 不带 `Debug`（里面是原生句柄）：只报「登录了没有」，别把句柄打进日志。
impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("logged_in", &self.me.is_some())
            .finish_non_exhaustive()
    }
}

impl Session {
    /// 建客户端（**还没发 `setTdlibParameters`**：那要等宿主把插件装好）。
    ///
    /// `log_verbosity`：`None` = 用 TDLib 默认（[`DEFAULT_LOG_VERBOSITY`](crate::DEFAULT_LOG_VERBOSITY)，
    /// 已经在 `TdClient::new` 里设过）；`Some(n)` 覆盖它。负数会由 `TdClient` 钳到 0。
    pub fn create(log_verbosity: Option<i32>) -> Result<Self, TdlibError> {
        let client = TdClient::new()?;
        if let Some(level) = log_verbosity {
            client.set_log_verbosity(level);
        }
        Ok(Self {
            client: Arc::new(client),
            me: None,
        })
    }

    /// 给 `attach_tdlib` / `spawn_dispatch` 用的句柄。
    pub fn client(&self) -> &Arc<TdClient> {
        &self.client
    }

    /// 登录后 `getMe` 的结果；没登录 / 没拿到就是 `None`。
    pub fn me(&self) -> Option<&Value> {
        self.me.as_ref()
    }

    /// 走完授权状态机，成功就是 `authorizationStateReady`，顺便问一次 `getMe`。
    ///
    /// **阻塞**：交互式登录等的是人，不是网络。调用方负责在合适的线程上跑它
    /// （CLI 在 tokio worker 上、JNI 在自己的 worker 线程上）。
    pub fn login(
        &mut self,
        parameters: &TdlibParameters,
        account: AccountType,
        budget: Duration,
        prompter: &mut dyn Prompter,
        bypass: &mut dyn FnMut(Routed),
    ) -> Result<(), SessionError> {
        match log_in_with(&self.client, parameters, account, prompter, budget, bypass)? {
            LoginOutcome::Ready => {}
            LoginOutcome::Closed => return Err(SessionError::Closed),
        }
        self.me = get_me(&self.client, GET_ME_BUDGET, bypass);
        Ok(())
    }

    /// 关会话并等 TDLib 确认（数据库刷盘）。
    ///
    /// 返回 `false` = 没在预算内确认关闭，调用方按「直接销毁」处理。
    /// 调它之前**必须**先停掉接收循环、把插件卸掉：`close` 之后句柄就不该再被任何人用。
    pub fn close(self, budget: Duration) -> bool {
        self.client.close_and_wait(budget)
    }
}

/// 问一次 `getMe`。失败（超时 / 没登录）返回 `None`——调用方只当少了账户信息。
///
/// 期间收到别的东西一律交给 `bypass`，不吞：那可能是插件 `start` 期请求的响应。
fn get_me(client: &TdClient, budget: Duration, bypass: &mut dyn FnMut(Routed)) -> Option<Value> {
    let id = client.send(json!({ "@type": "getMe" })).ok()?;
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        match client.receive(Duration::from_millis(500)) {
            Some(Routed::Response(response)) if response.id == id => return Some(response.payload),
            Some(other) => bypass(other),
            None => continue,
        }
    }
    None
}
