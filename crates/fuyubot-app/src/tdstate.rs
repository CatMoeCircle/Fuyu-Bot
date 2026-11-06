//! TDLib 的两个运行期状态：**授权**（登录走到哪一步）与**连接**（到 Telegram 的链路）。
//!
//! 两者都只有 `update*` 这一条来源——TDLib 没有 `getConnectionState` 之类的方法可问，
//! 而「现在登录到哪一步」在登录窗口里被状态机自己消费掉了（见 `log_in_with`）。
//! 所以 `App::boot` 里各订阅一次、只记最新一条；登录期的授权状态走
//! [`Prompter::auth_state`](fuyubot_tdlib::Prompter::auth_state) 那条钩子补上。
//!
//! 这里留的是最新的 [`AuthState`]；不保整条原始更新，也**不落盘、不外传**——
//! 授权负载里的手机号 / 密码提示只活在状态机与这个快照里，对外 JSON 只有状态名 + 三个布尔
//! （APK 拿到状态名 + `onAsk` 的 prompt 就够显示了）。

use std::sync::{Arc, Mutex};

use fuyubot_event::SubscriptionHandle;
use fuyubot_loader::HostRuntime;
use fuyubot_simplify::{CONNECTION_UPDATE, ConnectionState};
use fuyubot_tdlib::AuthState;
use serde_json::{Value, json};

/// `updateAuthorizationState` 的通道名（连接那条是 [`CONNECTION_UPDATE`]）。
pub const AUTH_UPDATE: &str = "updateAuthorizationState";

/// 最新状态的可共享快照。`Clone` 只复制一个 `Arc`，可以随用随取。
#[derive(Clone, Default)]
pub struct TdState {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    auth: Mutex<Option<AuthState>>,
    connection: Mutex<Option<ConnectionState>>,
}

/// 两条订阅的守卫：**drop 就退订**（约束 11 同一条道理）。
#[must_use = "不保存这个守卫，订阅会被立刻退掉"]
pub struct TdStateWatch {
    _auth: SubscriptionHandle,
    _connection: SubscriptionHandle,
}

impl TdState {
    pub fn new() -> Self {
        Self::default()
    }

    /// 订阅授权 / 连接两条更新（`boot` 里调一次）。
    ///
    /// **必须早于登录**：连接状态只在**变化**时来一条，等登录完再订阅就漏掉
    /// `connectionStateReady`。
    pub fn watch(&self, runtime: &HostRuntime) -> TdStateWatch {
        let auth = self.clone();
        let auth_handle = runtime
            .events()
            .on(AUTH_UPDATE, None, move |payload: &Value| {
                auth.learn_auth(payload);
                Ok(())
            });
        let connection = self.clone();
        let connection_handle =
            runtime
                .events()
                .on(CONNECTION_UPDATE, None, move |payload: &Value| {
                    connection.learn_connection(payload);
                    Ok(())
                });
        TdStateWatch {
            _auth: auth_handle,
            _connection: connection_handle,
        }
    }

    /// 当前授权状态；一次都没收到过 = `None`。
    pub fn auth(&self) -> Option<AuthState> {
        self.inner
            .auth
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    /// 当前连接状态；一次都没收到过 = `None`（没收到过按「没连上」算）。
    pub fn connection(&self) -> Option<ConnectionState> {
        *self
            .inner
            .connection
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    /// 记一条授权状态：登录期的 `Prompter::auth_state` 直接调这里。
    pub fn set_auth(&self, state: &AuthState) {
        *self
            .inner
            .auth
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(state.clone());
    }

    /// 授权状态 JSON（APK 的 `nativeAuthState()` 就是它）。
    ///
    /// ```json
    /// {"known":true,"state":"authorizationStateReady","ready":true,"needs_re_login":false}
    /// ```
    ///
    /// 一次都没收到过时是同一份形状的 `known:false` / `state:null`——客户端不用为
    /// 「还没起来」写第二套解析（与 [`crate::empty_plugins_json`] 同一条纪律）。
    pub fn auth_json(&self) -> String {
        render_auth(self.auth())
    }

    /// 连接状态 JSON（APK 的 `nativeConnectionState()` 就是它）。
    ///
    /// ```json
    /// {"known":true,"state":"connectionStateReady","ready":true}
    /// ```
    pub fn connection_json(&self) -> String {
        render_connection(self.connection())
    }

    /// 事件总线上的授权更新：只认带 `authorization_state.@type` 的负载。
    ///
    /// 空名字（缺字段 / 不是这条更新）一律**不覆盖**——宁可保留上一条，
    /// 也别把界面写成「未知状态」。
    fn learn_auth(&self, payload: &Value) {
        let name = payload
            .get("authorization_state")
            .and_then(|state| state.get("@type"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        if name.is_empty() {
            return;
        }
        self.set_auth(&AuthState::from_update(payload));
    }

    /// 事件总线上的连接更新：状态在 **`state`** 里（不是顶层）。
    ///
    /// 认不出的构造器直接忽略（TDLib 以后加了新状态时，保留上一条好过乱猜）。
    fn learn_connection(&self, payload: &Value) {
        let Some(state) = payload.get("state").and_then(ConnectionState::from_json) else {
            return;
        };
        *self
            .inner
            .connection
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(state);
    }
}

/// 运行时还没起来时的授权状态：合法 JSON，形状与正常返回一致（`known:false`）。
pub fn empty_auth_state_json() -> String {
    render_auth(None)
}

/// 运行时还没起来时的连接状态：合法 JSON，形状与正常返回一致（`known:false`）。
pub fn empty_connection_state_json() -> String {
    render_connection(None)
}

fn render_auth(state: Option<AuthState>) -> String {
    match state {
        Some(state) => json!({
            "known": true,
            "state": state.name(),
            "ready": state.is_ready(),
            "needs_re_login": state.needs_re_login(),
        }),
        None => json!({
            "known": false,
            "state": Value::Null,
            "ready": false,
            "needs_re_login": false,
        }),
    }
    .to_string()
}

fn render_connection(state: Option<ConnectionState>) -> String {
    match state {
        Some(state) => json!({
            "known": true,
            "state": state.type_name(),
            "ready": state.is_ready(),
        }),
        None => json!({
            "known": false,
            "state": Value::Null,
            "ready": false,
        }),
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuyubot_core::Context;

    fn runtime() -> Arc<HostRuntime> {
        HostRuntime::new(Context::new()).expect("运行时应该建得起来")
    }

    fn auth_update(name: &str) -> Value {
        json!({
            "@type": AUTH_UPDATE,
            "authorization_state": { "@type": name },
        })
    }

    fn connection_update(name: &str) -> Value {
        json!({
            "@type": CONNECTION_UPDATE,
            "state": { "@type": name },
        })
    }

    /// 没收到过任何更新：两个 JSON 都必须是合法、同形状的 `known:false`，
    /// 而不是 `null` 或者「少几个字段」。
    #[test]
    fn the_unknown_state_is_well_formed() {
        let state = TdState::new();
        assert!(state.auth().is_none());
        assert!(state.connection().is_none());

        let auth: Value = serde_json::from_str(&state.auth_json()).unwrap();
        assert_eq!(
            auth,
            json!({
                "known": false,
                "state": Value::Null,
                "ready": false,
                "needs_re_login": false,
            })
        );
        assert_eq!(state.auth_json(), empty_auth_state_json());

        let connection: Value = serde_json::from_str(&state.connection_json()).unwrap();
        assert_eq!(
            connection,
            json!({"known": false, "state": Value::Null, "ready": false})
        );
        assert_eq!(state.connection_json(), empty_connection_state_json());
    }

    /// 订阅两条更新：只留最新一条，登录窗口里的中间态也要能看见。
    #[test]
    fn watch_records_the_latest_auth_and_connection_states() {
        let runtime = runtime();
        let state = TdState::new();
        let _watch = state.watch(&runtime);

        runtime
            .publish_update(&auth_update("authorizationStateWaitCode"))
            .unwrap();
        assert_eq!(
            state.auth().map(|auth| auth.name().to_owned()),
            Some("authorizationStateWaitCode".to_owned())
        );
        let auth: Value = serde_json::from_str(&state.auth_json()).unwrap();
        assert_eq!(auth["known"], json!(true));
        assert_eq!(auth["state"], json!("authorizationStateWaitCode"));
        assert_eq!(auth["ready"], json!(false));
        assert_eq!(auth["needs_re_login"], json!(true));

        // 连接状态在 `state` 里，不在顶层
        runtime
            .publish_update(&connection_update("connectionStateConnecting"))
            .unwrap();
        assert_eq!(state.connection(), Some(ConnectionState::Connecting));
        assert_eq!(
            serde_json::from_str::<Value>(&state.connection_json()).unwrap()["ready"],
            json!(false)
        );

        runtime
            .publish_update(&connection_update("connectionStateReady"))
            .unwrap();
        let connection: Value = serde_json::from_str(&state.connection_json()).unwrap();
        assert_eq!(connection["state"], json!("connectionStateReady"));
        assert_eq!(connection["ready"], json!(true));

        runtime
            .publish_update(&auth_update("authorizationStateReady"))
            .unwrap();
        assert_eq!(state.auth(), Some(AuthState::Ready));
    }

    /// 登录期的钩子直接写状态（事件总线看不到被 `log_in_with` 消费掉的那条）。
    #[test]
    fn the_login_hook_sets_the_auth_state_directly() {
        let state = TdState::new();
        state.set_auth(&AuthState::WaitPassword {
            hint: "生日".to_owned(),
            has_recovery_email: false,
        });
        let auth: Value = serde_json::from_str(&state.auth_json()).unwrap();
        assert_eq!(auth["state"], json!("authorizationStateWaitPassword"));
        assert_eq!(auth["needs_re_login"], json!(true));

        state.set_auth(&AuthState::Ready);
        assert_eq!(
            serde_json::from_str::<Value>(&state.auth_json()).unwrap()["ready"],
            json!(true)
        );
    }

    /// 缺字段 / 认不出的负载不能把已知状态抹成空：保留上一条。
    #[test]
    fn malformed_updates_do_not_clobber_the_known_state() {
        let runtime = runtime();
        let state = TdState::new();
        let _watch = state.watch(&runtime);

        runtime
            .publish_update(&connection_update("connectionStateReady"))
            .unwrap();
        state.set_auth(&AuthState::Ready);

        runtime
            .publish_update(&json!({"@type": AUTH_UPDATE}))
            .unwrap();
        runtime
            .publish_update(&json!({"@type": CONNECTION_UPDATE, "state": {"@type": "nope"}}))
            .unwrap();
        assert_eq!(state.auth(), Some(AuthState::Ready));
        assert_eq!(state.connection(), Some(ConnectionState::Ready));
    }

    /// 守卫 drop = 退订：之后的更新不再有人接（也不该崩）。
    #[test]
    fn dropping_the_watch_stops_recording() {
        let runtime = runtime();
        let state = TdState::new();
        let watch = state.watch(&runtime);
        drop(watch);
        runtime
            .publish_update(&connection_update("connectionStateReady"))
            .unwrap();
        assert!(state.connection().is_none());
    }
}
