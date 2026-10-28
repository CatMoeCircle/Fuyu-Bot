//! 连接状态：`updateConnectionState` 的记录与查询。
//!
//! TDLib 的连接状态**只以更新的形式出现**（`updateConnectionState`），没有
//! `getConnectionState` 这样的方法可问——想「现在连上了吗」要么等下一条更新，
//! 要么自己订阅并把最新那条存下来。本模块给的是后者：
//!
//! ```ignore
//! let connection = simplify.track_connection_state().expect("订阅失败");
//! // 之后随时问
//! if connection.is_ready() { /* … */ }
//! connection.wait_ready(5_000);
//! ```
//!
//! 守卫**存进插件状态**——`drop` 就是退订（和 [`CommandSubscription`](crate::CommandSubscription)
//! 一个语义）。订阅之前那段时间里 `update()` 是 `Null`，`is_ready()` 是 `false`：
//! 「还没收到过」和「收到了、但不是 Ready」都按**没连上**处理。
//!
//! ⚠️ 这是**连接**状态（网络到 Telegram 的链路），不是**授权**状态
//! （`updateAuthorizationState`，登录流程那一套）——两者独立：
//! `connectionStateReady` 也可能还没登录。

use std::ffi::{CStr, c_char, c_void};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use fuyubot_abi::{FILTER_EVENT_TYPE_KEY, HostApi, SubscriptionHandle};
use serde_json::{Value, json};

use crate::Simplify;

/// `updateConnectionState` 这条更新的 `@type`（也是宿主的事件通道名）。
pub const CONNECTION_UPDATE: &str = "updateConnectionState";

/// TDLib 的 `ConnectionState`：五种，按「离可用还有多远」排。
///
/// `Ready` 是唯一「可以发请求」的状态；`Updating` 是「连上了、正在同步」——
/// 这时候发请求多半会被 TDLib 排队，不算错，但别拿它当「在线」的依据。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConnectionState {
    /// 等网络（设备离线）。
    WaitingForNetwork,
    /// 正在连代理。
    ConnectingToProxy,
    /// 正在连 Telegram。
    Connecting,
    /// 连上了，正在下载/同步数据。
    Updating,
    /// 就绪：可以正常发请求了。
    Ready,
}

impl ConnectionState {
    /// TDLib 的构造器名（`state.@type`）。
    pub fn type_name(self) -> &'static str {
        match self {
            Self::WaitingForNetwork => "connectionStateWaitingForNetwork",
            Self::ConnectingToProxy => "connectionStateConnectingToProxy",
            Self::Connecting => "connectionStateConnecting",
            Self::Updating => "connectionStateUpdating",
            Self::Ready => "connectionStateReady",
        }
    }

    /// 从构造器名反查；认不出（TDLib 以后又加了一种）返回 `None`。
    pub fn from_type_name(name: &str) -> Option<Self> {
        [
            Self::WaitingForNetwork,
            Self::ConnectingToProxy,
            Self::Connecting,
            Self::Updating,
            Self::Ready,
        ]
        .into_iter()
        .find(|state| state.type_name() == name)
    }

    /// 从一份 `ConnectionState` JSON（`{"@type": "connectionStateReady"}`）解析。
    ///
    /// 认不出返回 `None`——**不要**默认成 `Ready`：未知状态按「不可用」才安全。
    pub fn from_json(value: &Value) -> Option<Self> {
        value
            .get("@type")
            .and_then(Value::as_str)
            .and_then(Self::from_type_name)
    }

    pub fn is_ready(self) -> bool {
        matches!(self, Self::Ready)
    }
}

/// 订阅并**记住**最新一条 `updateConnectionState`。
///
/// 回调只做一件事：把整条更新存进共享槽位。查询是同步的读，不发请求、不等更新。
pub struct ConnectionTracker {
    host: HostApi,
    handle: SubscriptionHandle,
    /// 回调写、查询读的同一份最新更新。**装箱只为地址稳定**：
    /// 递给 ABI 的 `user_data` 指的就是这里内层那个 `Mutex`——结构体本身会被移动
    /// （按值返回），只有堆上那一份的地址是可以交出去的。
    latest: Box<Mutex<Value>>,
}

impl ConnectionTracker {
    /// 最新那条 `updateConnectionState` 的原始负载；一条都没收到过时是 `Null`。
    pub fn update(&self) -> Value {
        self.latest
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    /// 更新里的 `state` 字段（`ConnectionState` 对象）；没有就是 `Null`。
    pub fn state(&self) -> Value {
        self.update().get("state").cloned().unwrap_or(Value::Null)
    }

    /// 解析过的连接状态；没收到过 / 认不出时是 `None`。
    pub fn connection(&self) -> Option<ConnectionState> {
        ConnectionState::from_json(&self.state())
    }

    /// 收到过至少一条 `updateConnectionState`。
    pub fn has_update(&self) -> bool {
        !self.update().is_null()
    }

    /// 当前是不是 `connectionStateReady`。
    ///
    /// 「还没收到过」也算没连上——宁可保守。
    pub fn is_ready(&self) -> bool {
        self.connection().is_some_and(ConnectionState::is_ready)
    }

    /// 等到 `Ready`（已经是就直接 `true`）。**阻塞**当前线程，最多 `timeout_ms`。
    ///
    /// 循环里每次都用**剩余**预算去等：等到一条但不是 Ready（比如 `Updating`）就继续等，
    /// 直到预算用完。返回前会再看一眼——最后一次等待的回调可能刚好把状态写成 Ready。
    pub fn wait_ready(&self, timeout_ms: u64) -> bool {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            if self.is_ready() {
                return true;
            }
            let handle = self
                .host
                .wait_for_update(&filter_json(), crate::remaining_ms(deadline));
            if crate::payload_of(handle).is_none() {
                // 超时 / 被取消：返回前再确认一次（回调可能刚写完）
                return self.is_ready();
            }
        }
    }
}

impl Drop for ConnectionTracker {
    fn drop(&mut self) {
        if self.handle.is_valid() {
            self.host.off(self.handle);
        }
    }
}

/// 订阅 `updateConnectionState`，把每条更新记进返回的守卫里。
///
/// 宿主拒绝订阅（句柄无效）时返回 `None`。**把守卫存进插件状态**——drop 就是退订。
pub fn track_connection_state(host: HostApi) -> Option<ConnectionTracker> {
    let mut latest: Box<Mutex<Value>> = Box::new(Mutex::new(Value::Null));
    // 递给 ABI 的是**堆上那个 Mutex 的地址**：外层 Box 活着它就稳定。
    let user_data = (&raw mut *latest).cast::<c_void>();
    let handle = host.on(CONNECTION_UPDATE, None, connection_trampoline, user_data);
    handle.is_valid().then_some(ConnectionTracker {
        host,
        handle,
        latest,
    })
}

/// 等「下一条」`updateConnectionState` 的过滤器（走 `@type` 那条通道）。
pub(crate) fn filter_json() -> String {
    json!({ FILTER_EVENT_TYPE_KEY: CONNECTION_UPDATE }).to_string()
}

/// 连接状态订阅的 C 回调：把整条更新存进共享槽位。
extern "C-unwind" fn connection_trampoline(user_data: *mut c_void, payload: *const c_char) -> i32 {
    if user_data.is_null() || payload.is_null() {
        return -1;
    }
    // SAFETY: `user_data` 是 `track_connection_state` 里那个 Box，订阅活多久它就活多久。
    let latest = unsafe { &*(user_data as *const Mutex<Value>) };
    // SAFETY: 宿主保证 `payload` 是 NUL 结尾的临时字符串。
    let text = unsafe { CStr::from_ptr(payload) }.to_string_lossy();
    let Ok(update) = serde_json::from_str::<Value>(&text) else {
        return 0;
    };
    *latest.lock().unwrap_or_else(|error| error.into_inner()) = update;
    0
}

impl Simplify {
    /// 开始记录 `updateConnectionState`（守卫 drop 即退订）。
    ///
    /// 订阅之前推过的更新**补不回来**——TDLib 的状态更新不重放，所以要在插件
    /// `start` 里尽早调；晚订阅只会漏掉开头的 `Connecting` / `Updating`。
    pub fn track_connection_state(&self) -> Option<ConnectionTracker> {
        track_connection_state(self.host())
    }

    /// 等**下一条** `updateConnectionState`；超时返回 `None`。
    ///
    /// 一次性查询用它；要反复问「现在连上了吗」请用 [`track_connection_state`](Self::track_connection_state)。
    /// ⚠️ **阻塞**当前线程。
    pub fn next_connection_state(&self, timeout_ms: u64) -> Option<Value> {
        crate::payload_of(self.host().wait_for_update(&filter_json(), timeout_ms))
    }

    /// 等到连接变成 `Ready`（不订阅，纯等待），最多 `timeout_ms`。
    ///
    /// 已有一条订阅时优先用 [`ConnectionTracker::wait_ready`]——它连「在等待之前
    /// 就已经 Ready」这种情况也能立刻答。
    /// ⚠️ **阻塞**当前线程。
    pub fn wait_until_connected(&self, timeout_ms: u64) -> bool {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            let handle = self
                .host()
                .wait_for_update(&filter_json(), crate::remaining_ms(deadline));
            let Some(update) = crate::payload_of(handle) else {
                return false;
            };
            if ConnectionState::from_json(update.get("state").unwrap_or(&Value::Null))
                .is_some_and(ConnectionState::is_ready)
            {
                return true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuyubot_core::Context;
    use fuyubot_loader::HostRuntime;
    use std::sync::Arc;

    fn host(rt: &Arc<HostRuntime>) -> HostApi {
        // SAFETY: rt 比 host 活得久。
        unsafe { HostApi::from_raw(rt.vtable_ptr()) }.unwrap()
    }

    fn connected() -> Value {
        json!({
            "@type": CONNECTION_UPDATE,
            "state": { "@type": "connectionStateReady" },
        })
    }

    #[test]
    fn connection_state_maps_the_five_tdlib_constructors() {
        let cases = [
            (
                "connectionStateWaitingForNetwork",
                ConnectionState::WaitingForNetwork,
            ),
            (
                "connectionStateConnectingToProxy",
                ConnectionState::ConnectingToProxy,
            ),
            ("connectionStateConnecting", ConnectionState::Connecting),
            ("connectionStateUpdating", ConnectionState::Updating),
            ("connectionStateReady", ConnectionState::Ready),
        ];
        for (name, expected) in cases {
            assert_eq!(ConnectionState::from_type_name(name), Some(expected));
            assert_eq!(expected.type_name(), name);
        }
        assert_eq!(
            ConnectionState::from_json(&json!({ "@type": "connectionStateUpdating" })),
            Some(ConnectionState::Updating)
        );
        // 认不出的一律 None，绝不默认成 Ready
        assert_eq!(ConnectionState::from_type_name("connectionStateNope"), None);
        assert_eq!(ConnectionState::from_json(&Value::Null), None);
        assert!(!ConnectionState::Updating.is_ready());
        assert!(ConnectionState::Ready.is_ready());
    }

    #[test]
    fn tracker_records_the_latest_connection_update() {
        let rt = HostRuntime::new(Context::new()).unwrap();
        let api = host(&rt);
        let tracker = track_connection_state(api).expect("订阅应该成功");

        // 订阅之前什么都没有：「没收到过」按没连上算
        assert!(!tracker.has_update());
        assert!(!tracker.is_ready());
        assert_eq!(tracker.update(), Value::Null);

        // 掉线 → 就绪：只记最后一条
        rt.publish_update(&json!({
            "@type": CONNECTION_UPDATE,
            "state": { "@type": "connectionStateConnecting" },
        }))
        .unwrap();
        assert!(tracker.has_update());
        assert!(!tracker.is_ready());
        assert_eq!(tracker.connection(), Some(ConnectionState::Connecting));

        rt.publish_update(&connected()).unwrap();
        assert!(tracker.is_ready());
        assert_eq!(tracker.state()["@type"], "connectionStateReady");
        // 整条更新都在（不只是 state）
        assert_eq!(tracker.update()["@type"], CONNECTION_UPDATE);

        // 等到 Ready：已经是了，立刻返回
        assert!(tracker.wait_ready(0));
    }

    #[test]
    fn tracker_stops_recording_after_drop() {
        let rt = HostRuntime::new(Context::new()).unwrap();
        let api = host(&rt);
        let tracker = track_connection_state(api).expect("订阅应该成功");
        drop(tracker);

        // 退订之后再推：不该有人接（也不该崩）
        rt.publish_update(&connected()).unwrap();
        // 重新订阅拿不到历史——TDLib 的状态更新不重放
        let again = track_connection_state(api).expect("订阅应该成功");
        assert!(!again.has_update());
    }

    #[test]
    fn wait_until_connected_needs_a_real_ready_update() {
        let rt = HostRuntime::new(Context::new()).unwrap();
        let api = host(&rt);
        let simplify = Simplify::new(api);

        // 没人推更新 → 超时（0 ms 立刻返回）
        assert!(!simplify.wait_until_connected(0));
        // 一条非 Ready 的更新也算不上连上：0 ms 预算下第二次等待同样立刻超时
        rt.publish_update(&json!({
            "@type": CONNECTION_UPDATE,
            "state": { "@type": "connectionStateUpdating" },
        }))
        .unwrap();
        assert!(simplify.next_connection_state(0).is_none());
    }
}
