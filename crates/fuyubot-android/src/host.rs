//! `AppHost` 的 JNI 实现：状态 / 提问 / 二维码全部转到 Java 回调。
//!
//! 编排（[`fuyubot_app::App`]）只认这三个出口，所以这一层薄到只剩转发；
//! 真正的 Java 侧交互在 [`crate::callback`]（GlobalRef、等待位、线程 attach）。

use fuyubot_app::{AppHost, AppState, AskKind};

use crate::callback;

/// 把 `fuyubot-app` 的状态 / 提问 / 二维码接到 JNI 回调上。
///
/// 无状态：所有跨调用状态都在 `callback` 里（它管 JavaVM、GlobalRef 与等待位）。
#[derive(Debug, Default, Clone, Copy)]
pub struct JniHost;

impl AppHost for JniHost {
    fn state(&self, state: AppState, detail: &str) {
        // 收摊阶段要留档：`nativeStop` 超时（返回 3）时，`last_error()` 会把它写出来，
        // App 弹窗能看到「卡在停接收循环 / 刷盘 / 卸插件」中的哪一步。
        if state == AppState::Stopping {
            crate::state::set_stop_phase(detail);
        }
        // `AppState` 的数值就是 `onState(int, …)` 的契约，直接转。
        callback::state(state.as_i32(), detail);
    }

    fn ask(&self, kind: AskKind, prompt: &str) -> Option<String> {
        // 阻塞等 `nativeAnswer`；没有注册回调（纯 C 宿主）时立刻返回 `None`。
        callback::ask(kind.as_i32(), prompt)
    }

    fn qr(&self, link: &str) {
        callback::qr(link);
    }
}
