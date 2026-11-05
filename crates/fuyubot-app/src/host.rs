//! 宿主交互面：状态、提问、二维码。
//!
//! 编排层（[`crate::App`]）**不认识 TUI / 控制台 / JNI**：它只会调这三个方法。
//! 谁实现它们，谁就是这一侧的宿主：
//!
//! | 宿主 | `state` | `ask` | `qr` |
//! | --- | --- | --- | --- |
//! | `fuyubot-cli` 的前台 `run` | 打日志 | 读 stdin | 打链接 |
//! | `fuyubot-cli` 的 TUI | 状态栏 / 提示条 | 弹输入框 | 弹二维码页 |
//! | `fuyubot-android` | `Callback.onState` | `Callback.onAsk` + 等 `nativeAnswer` | `Callback.onQr` |

use std::sync::Arc;

/// 运行时对外报的状态。数值与 Android 的 `onState(int)` 一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum AppState {
    /// 起了，正在登录 / 装插件。
    Starting = 0,
    /// 登录完成、开始分发更新。
    Ready = 1,
    /// 收到停止请求，正在收摊。
    Stopping = 2,
    /// 收摊完成（或从未真正开始）。
    Stopped = 3,
    /// 出事退出，细节在 `detail` 里。
    Failed = 4,
}

impl AppState {
    pub fn as_i32(self) -> i32 {
        self as i32
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Ready => "ready",
            Self::Stopping => "stopping",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
        }
    }
}

/// 登录提问的种类。数值与 Android 的 `onAsk(int kind, …)` 一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum AskKind {
    BotToken = 0,
    Phone = 1,
    Code = 2,
    Password = 3,
    /// 只在 user 账号登录时问：回答 `"phone"` 或 `"qr"`。
    LoginMethod = 4,
    /// 认不出来的提问：把 `prompt` 原样显示，收一行文本。
    Other = 5,
}

impl AskKind {
    pub fn as_i32(self) -> i32 {
        self as i32
    }
}

/// 宿主必须实现的三个出口。
///
/// **可能从任何线程被调**（登录跑在调用方给的那个线程上），实现要自己保证线程安全；
/// `ask` 允许阻塞（等人输入），但别在里面做长时间的网络 / IO——它挡着登录状态机。
pub trait AppHost: Send + Sync {
    /// 状态变化。实现方只该**记录 / 展示**，别在里面阻塞。
    fn state(&self, state: AppState, detail: &str);

    /// 问一句、等一行回答。返回 `None` = 用户取消 / 这一步不支持交互。
    fn ask(&self, kind: AskKind, prompt: &str) -> Option<String>;

    /// 展示二维码 / 登录链接（`authStateWaitOtherDeviceConfirmation`），**不需要回答**。
    fn qr(&self, link: &str);

    /// 插件装载结果，一次性上报（发生在登录**之前**）。
    ///
    /// `report` 是 JSON，形状就是 [`crate::LiveView::plugins_json`] 那一份：
    /// `{"count":…, "scanned":…, "active":…, "pending":…, "activating":…,
    /// "failed":…, "unknown":…, "skipped":…, "plugins":[…], "skipped_plugins":[…]}`。
    ///
    /// **默认什么都不做**：`fuyubot::app` 的日志里已经有一条汇总（「插件：扫到 … 个，
    /// 交给内核 … 个，跳过 … 个」）。Android 把它转成 `Callback.onPlugins(String)`，
    /// 让 APK 在登录窗口弹出**之前**就拿到列表与失败原因；CLI 用不上。
    ///
    /// 这是**可选**回调：实现方忽略它不影响启动；Java 类没有 `onPlugins` 方法时
    /// 也只记一条日志（见 `fuyubot-android`）。
    fn plugin_report(&self, _report: &str) {}
}

/// 什么都不做的宿主：只把状态打进日志。
///
/// 给「测试 / 纯 bot token 场景」用——那种情况登录一次都不会问。
#[derive(Debug, Default, Clone, Copy)]
pub struct SilentHost;

impl AppHost for SilentHost {
    fn state(&self, state: AppState, detail: &str) {
        tracing::info!(target: "fuyubot::app", "{}：{detail}", state.name());
    }

    fn ask(&self, kind: AskKind, prompt: &str) -> Option<String> {
        tracing::warn!(
            target: "fuyubot::app",
            "需要输入（kind={}）但宿主不提供交互：{prompt}",
            kind.as_i32()
        );
        None
    }

    fn qr(&self, link: &str) {
        tracing::info!(target: "fuyubot::app", "二维码登录链接：{link}");
    }
}

/// 把 `Arc<dyn AppHost>` 也当宿主收下（调用方通常已经持有 Arc）。
impl AppHost for Arc<dyn AppHost> {
    fn state(&self, state: AppState, detail: &str) {
        (**self).state(state, detail);
    }

    fn ask(&self, kind: AskKind, prompt: &str) -> Option<String> {
        (**self).ask(kind, prompt)
    }

    fn qr(&self, link: &str) {
        (**self).qr(link);
    }

    fn plugin_report(&self, report: &str) {
        (**self).plugin_report(report);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_numbers_match_the_jni_contract() {
        assert_eq!(AppState::Starting.as_i32(), 0);
        assert_eq!(AppState::Ready.as_i32(), 1);
        assert_eq!(AppState::Stopping.as_i32(), 2);
        assert_eq!(AppState::Stopped.as_i32(), 3);
        assert_eq!(AppState::Failed.as_i32(), 4);
    }

    #[test]
    fn ask_kinds_match_the_jni_contract() {
        assert_eq!(AskKind::BotToken.as_i32(), 0);
        assert_eq!(AskKind::Phone.as_i32(), 1);
        assert_eq!(AskKind::Code.as_i32(), 2);
        assert_eq!(AskKind::Password.as_i32(), 3);
        assert_eq!(AskKind::LoginMethod.as_i32(), 4);
        assert_eq!(AskKind::Other.as_i32(), 5);
    }

    /// 没有交互的宿主必须**立刻**说「没有」，别让登录卡在那儿等。
    #[test]
    fn silent_host_never_answers() {
        assert!(SilentHost.ask(AskKind::Code, "6 位验证码").is_none());
    }
}
