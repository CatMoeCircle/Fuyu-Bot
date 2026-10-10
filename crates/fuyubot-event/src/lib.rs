//! 事件系统：`event_type` → 订阅表，带字段过滤的广播。
//!
//! 只依赖 `fuyubot-core`（拿 `Context` 与 `Effect`），不认识 Telegram，也不认识工具与中间件。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use fuyubot_core::{Context, Effect};
use serde_json::Value;

/// 事件负载：就是 JSON，跨插件跨语言都能传。
pub type Payload = Value;

/// 订阅回调。返回 `Err` 不 panic，由 [`EventBus::emit`] 决定怎么处理。
pub type Callback = Arc<dyn Fn(&Payload) -> fuyubot_core::Result<()> + Send + Sync>;

/// 订阅标识。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SubscriptionId(u64);

impl std::fmt::Display for SubscriptionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "#{}", self.0)
    }
}

/// 字段过滤：**所有**条件都相等才算匹配。
///
/// 路径用点分写法，`"a.b"` 匹配 `{"a": {"b": ...}}`；每一段都要是合法的 JSON 指针段，
/// 所以键里含 `.` 或 `/` 的字段没法用过滤表达（真遇到就自己建一个专门的 event_type）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Filter {
    fields: Vec<(String, Value)>,
}

impl Filter {
    pub fn new() -> Self {
        Self::default()
    }

    /// 加一条「字段等于某值」的条件。
    pub fn field(mut self, path: impl Into<String>, expected: impl Into<Value>) -> Self {
        self.fields.push((path.into(), expected.into()));
        self
    }

    /// 没有条件 = 全部匹配。
    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    pub fn matches(&self, payload: &Payload) -> bool {
        self.fields.iter().all(|(path, expected)| {
            let pointer = format!("/{}", path.replace('.', "/"));
            payload.pointer(&pointer) == Some(expected)
        })
    }
}

/// 一条订阅。
struct Subscription {
    id: SubscriptionId,
    /// 归属：0 = 全局（宿主与所有插件都收），其它值 = 只有该插件收。
    scope: u64,
    filter: Option<Filter>,
    callback: Callback,
}

#[derive(Default)]
struct Inner {
    next_id: u64,
    by_type: HashMap<String, Vec<Subscription>>,
}

/// 事件总线。`Clone` 共享同一张订阅表。
#[derive(Clone, Default)]
pub struct EventBus {
    inner: Arc<Mutex<Inner>>,
}

impl EventBus {
    pub fn new() -> Self {
        Self::default()
    }

    /// 订阅。返回的句柄 drop 掉（或交给 [`off`](Self::off)）即退订。
    ///
    /// `filter` 为 `None` 表示这个 `event_type` 全收。订阅是全局的（`scope = 0`）。
    pub fn on(
        &self,
        event_type: impl Into<String>,
        filter: Option<Filter>,
        callback: impl Fn(&Payload) -> fuyubot_core::Result<()> + Send + Sync + 'static,
    ) -> SubscriptionHandle {
        self.on_scoped(0, event_type, filter, callback)
    }

    /// 带归属的订阅：只有 `emit_scoped(同一个 scope, ..)` 或全局 `emit` 才会调到它。
    ///
    /// 用途是「只给某一个插件看的事件」——比如 TDLib 的请求响应：
    /// 它可能带着聊天内容，不该广播给所有插件。
    pub fn on_scoped(
        &self,
        scope: u64,
        event_type: impl Into<String>,
        filter: Option<Filter>,
        callback: impl Fn(&Payload) -> fuyubot_core::Result<()> + Send + Sync + 'static,
    ) -> SubscriptionHandle {
        let event_type = event_type.into();
        let id;
        {
            let mut inner = self.lock();
            id = SubscriptionId(inner.next_id);
            inner.next_id += 1;
            inner
                .by_type
                .entry(event_type.clone())
                .or_default()
                .push(Subscription {
                    id,
                    scope,
                    filter,
                    callback: Arc::new(callback),
                });
        }

        let bus = Arc::clone(&self.inner);
        let key = event_type.clone();
        let effect = Effect::new(move || {
            let mut inner = bus.lock().unwrap_or_else(|e| e.into_inner());
            let now_empty = match inner.by_type.get_mut(&key) {
                Some(subscriptions) => {
                    subscriptions.retain(|s| s.id != id);
                    subscriptions.is_empty()
                }
                None => false,
            };
            if now_empty {
                inner.by_type.remove(&key);
            }
        });

        SubscriptionHandle {
            id,
            event_type,
            effect,
        }
    }

    /// 广播：按订阅顺序调用**所有**匹配的订阅（不论归属），返回被调用的个数。
    ///
    /// 某个回调返回 `Err` 会**中断**本次广播并把错误抛出去（错误信息里带上 event_type）。
    ///
    /// 回调是在锁**外面**跑的：回调里再 `on`/`emit`/`off` 不会自锁。
    pub fn emit(&self, event_type: &str, payload: &Payload) -> fuyubot_core::Result<usize> {
        self.dispatch(None, event_type, payload)
    }

    /// 定向广播：**全局订阅 + `scope` 这个归属的订阅**。
    ///
    /// 归属的含义是「额外定向」，不是「排除广播」——所以 `emit` 照旧发给所有人。
    /// 这正是我们要的性质：更新对所有插件透明，而响应只多送给发起请求的那个。
    ///
    /// ⚠️ `scope = 0` **不等同于** [`emit`](Self::emit)：全局订阅的归属本来就是 0
    /// （[`on`](Self::on) 就是 `on_scoped(0, ..)`），所以传 0 只会打到全局订阅者，
    /// 带归属的订阅者收不到。**要广播就用 `emit`。**
    pub fn emit_scoped(
        &self,
        scope: u64,
        event_type: &str,
        payload: &Payload,
    ) -> fuyubot_core::Result<usize> {
        self.dispatch(Some(scope), event_type, payload)
    }

    fn dispatch(
        &self,
        scope: Option<u64>,
        event_type: &str,
        payload: &Payload,
    ) -> fuyubot_core::Result<usize> {
        let callbacks: Vec<Callback> = {
            let inner = self.lock();
            inner
                .by_type
                .get(event_type)
                .map(|subscriptions| {
                    subscriptions
                        .iter()
                        .filter(|s| scope.is_none_or(|scope| s.scope == 0 || s.scope == scope))
                        .filter(|s| {
                            s.filter
                                .as_ref()
                                .is_none_or(|filter| filter.matches(payload))
                        })
                        .map(|s| Arc::clone(&s.callback))
                        .collect()
                })
                .unwrap_or_default()
        };

        for callback in &callbacks {
            callback(payload).map_err(|e| -> fuyubot_core::BoxError {
                format!("事件 {event_type} 的回调失败: {e}").into()
            })?;
        }
        Ok(callbacks.len())
    }

    /// 退订。等价于 `drop(handle)`，留着是为了让调用点意图更明确。
    pub fn off(&self, handle: SubscriptionHandle) {
        handle.off();
    }

    /// 某个 event_type 上还挂着几个订阅。
    pub fn subscriber_count(&self, event_type: &str) -> usize {
        self.lock()
            .by_type
            .get(event_type)
            .map_or(0, |subscriptions| subscriptions.len())
    }

    /// 中毒的锁也照用：一个回调的 panic 不该让整条总线卡死。
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl std::fmt::Debug for EventBus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.lock();
        let subscriptions: usize = inner.by_type.values().map(Vec::len).sum();
        f.debug_struct("EventBus")
            .field("event_types", &inner.by_type.len())
            .field("subscriptions", &subscriptions)
            .finish()
    }
}

/// 订阅句柄。drop 即退订。
#[must_use = "订阅句柄一被丢弃就立刻退订；要保留监听就把它存起来"]
pub struct SubscriptionHandle {
    id: SubscriptionId,
    event_type: String,
    effect: Effect,
}

impl SubscriptionHandle {
    pub fn id(&self) -> SubscriptionId {
        self.id
    }

    pub fn event_type(&self) -> &str {
        &self.event_type
    }

    /// 显式退订。
    pub fn off(self) {
        drop(self.effect);
    }
}

impl std::fmt::Debug for SubscriptionHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubscriptionHandle")
            .field("id", &self.id)
            .field("event_type", &self.event_type)
            .finish()
    }
}

/// 把总线装进内核 [`Context`]。
///
/// 返回的 `Effect` **必须一直持有**：它一 drop，总线就从 Context 里消失了。
pub fn install(ctx: &Context) -> fuyubot_core::Result<Effect> {
    Ok(ctx.provide(EventBus::new())?)
}

/// `Context` 上的事件入口。
pub trait EventContextExt {
    /// 取出装好的总线。
    fn events(&self) -> fuyubot_core::Result<Arc<EventBus>>;

    /// 订阅事件。
    fn on(
        &self,
        event_type: impl Into<String>,
        filter: Option<Filter>,
        callback: impl Fn(&Payload) -> fuyubot_core::Result<()> + Send + Sync + 'static,
    ) -> fuyubot_core::Result<SubscriptionHandle>;

    /// 广播事件。
    fn emit(&self, event_type: &str, payload: &Payload) -> fuyubot_core::Result<usize>;
}

impl EventContextExt for Context {
    fn events(&self) -> fuyubot_core::Result<Arc<EventBus>> {
        let bus = self
            .get::<EventBus>()
            .map_err(|e| -> fuyubot_core::BoxError {
                format!("{e}（先 `let _guard = fuyubot_event::install(&ctx)?;` 并持有它）").into()
            })?;
        Ok(bus)
    }

    fn on(
        &self,
        event_type: impl Into<String>,
        filter: Option<Filter>,
        callback: impl Fn(&Payload) -> fuyubot_core::Result<()> + Send + Sync + 'static,
    ) -> fuyubot_core::Result<SubscriptionHandle> {
        Ok(self.events()?.on(event_type, filter, callback))
    }

    fn emit(&self, event_type: &str, payload: &Payload) -> fuyubot_core::Result<usize> {
        self.events()?.emit(event_type, payload)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 验收 1：注册两个监听器 → emit 一次两个都被调用 → off 一个后只剩一个。
    #[test]
    fn two_listeners_then_off_leaves_one() {
        let bus = EventBus::new();
        let first = Arc::new(AtomicUsize::new(0));
        let second = Arc::new(AtomicUsize::new(0));

        let handle_a = bus.on("notice", None, {
            let counter = Arc::clone(&first);
            move |_| {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        });
        let _handle_b = bus.on("notice", None, {
            let counter = Arc::clone(&second);
            move |_| {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        });

        let called = bus.emit("notice", &json!({ "text": "hi" })).unwrap();
        assert_eq!(called, 2);
        assert_eq!(first.load(Ordering::SeqCst), 1);
        assert_eq!(second.load(Ordering::SeqCst), 1);

        bus.off(handle_a);
        assert_eq!(bus.subscriber_count("notice"), 1);

        let called = bus.emit("notice", &json!({ "text": "again" })).unwrap();
        assert_eq!(called, 1);
        assert_eq!(first.load(Ordering::SeqCst), 1, "退订过的监听不该再被调用");
        assert_eq!(second.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn dropping_the_handle_unsubscribes() {
        let bus = EventBus::new();
        let seen = Arc::new(AtomicUsize::new(0));
        let handle = bus.on("notice", None, {
            let counter = Arc::clone(&seen);
            move |_| {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        });
        drop(handle);
        assert_eq!(bus.emit("notice", &json!({})).unwrap(), 0);
        assert_eq!(seen.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn filter_matches_only_the_matching_payloads() {
        let bus = EventBus::new();
        let hits = Arc::new(AtomicUsize::new(0));
        let _handle = bus.on(
            "update",
            Some(Filter::new().field("message.chat.id", 42i64)),
            {
                let counter = Arc::clone(&hits);
                move |_| {
                    counter.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }
            },
        );

        bus.emit("update", &json!({ "message": { "chat": { "id": 42 } } }))
            .unwrap();
        bus.emit("update", &json!({ "message": { "chat": { "id": 7 } } }))
            .unwrap();
        bus.emit("update", &json!({})).unwrap();

        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn callbacks_may_subscribe_and_emit_without_deadlocking() {
        let bus = EventBus::new();
        let outer = bus.clone();
        let reentry = Arc::new(AtomicUsize::new(0));
        let _first = bus.on("a", None, move |_| {
            // 回调里再改订阅表：锁已经放开了，不该自锁。
            let _nested = outer.on("b", None, |_| Ok(()));
            outer.emit("b", &json!({}))?;
            Ok(())
        });
        let _second = bus.on("a", None, {
            let counter = Arc::clone(&reentry);
            move |_| {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        });

        assert_eq!(bus.emit("a", &json!({})).unwrap(), 2);
        assert_eq!(reentry.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn callback_errors_propagate_with_the_event_type() {
        let bus = EventBus::new();
        let _handle = bus.on("boom", None, |_| Err("听众炸了".into()));

        let error = bus.emit("boom", &json!({})).unwrap_err();
        let text = error.to_string();
        assert!(text.contains("boom"), "{text}");
        assert!(text.contains("听众炸了"), "{text}");
    }

    #[test]
    fn scoped_subscriptions_get_directed_events_on_top_of_broadcasts() {
        let bus = EventBus::new();
        let a = Arc::new(AtomicUsize::new(0));
        let b = Arc::new(AtomicUsize::new(0));
        let global = Arc::new(AtomicUsize::new(0));

        let counter = |counter: &Arc<AtomicUsize>| {
            let counter = Arc::clone(counter);
            move |_: &Payload| {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        };
        let _scoped_a = bus.on_scoped(1, "response", None, counter(&a));
        let _scoped_b = bus.on_scoped(2, "response", None, counter(&b));
        let _global = bus.on("response", None, counter(&global));

        // 全局广播发给所有人（不论归属）——更新就是这么对所有插件透明的
        assert_eq!(bus.emit("response", &json!({})).unwrap(), 3);
        assert_eq!(
            (
                a.load(Ordering::SeqCst),
                b.load(Ordering::SeqCst),
                global.load(Ordering::SeqCst)
            ),
            (1, 1, 1)
        );

        // 定向到 1：1 和全局收到，2 收不到
        assert_eq!(bus.emit_scoped(1, "response", &json!({})).unwrap(), 2);
        assert_eq!(
            (
                a.load(Ordering::SeqCst),
                b.load(Ordering::SeqCst),
                global.load(Ordering::SeqCst)
            ),
            (2, 1, 2)
        );

        // 定向到 3：没有归属 3 的订阅，只有全局收到
        assert_eq!(bus.emit_scoped(3, "response", &json!({})).unwrap(), 1);
        assert_eq!(global.load(Ordering::SeqCst), 3);
    }

    /// `scope = 0` 是「全局订阅」自己的归属，**不是**「广播」。
    /// 曾经这里的文档写成「等同于 `emit`」，那会让调用方以为传 0 能广播出去。
    #[test]
    fn emit_scoped_zero_hits_only_global_subscribers() {
        let bus = EventBus::new();
        let scoped = Arc::new(AtomicUsize::new(0));
        let global = Arc::new(AtomicUsize::new(0));

        let counter = |counter: &Arc<AtomicUsize>| {
            let counter = Arc::clone(counter);
            move |_: &Payload| {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        };
        let _scoped = bus.on_scoped(1, "response", None, counter(&scoped));
        let _global = bus.on("response", None, counter(&global));

        assert_eq!(bus.emit_scoped(0, "response", &json!({})).unwrap(), 1);
        assert_eq!(
            scoped.load(Ordering::SeqCst),
            0,
            "归属 1 的订阅不该收到 scope=0 的定向事件"
        );
        assert_eq!(global.load(Ordering::SeqCst), 1);

        // 对照：`emit` 才是真的广播。
        assert_eq!(bus.emit("response", &json!({})).unwrap(), 2);
        assert_eq!(scoped.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn context_integration() {
        let ctx = Context::new();
        let _guard = install(&ctx).unwrap();
        let seen = Arc::new(AtomicUsize::new(0));
        let handle = ctx
            .on("hello", None, {
                let counter = Arc::clone(&seen);
                move |_| {
                    counter.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }
            })
            .unwrap();

        assert_eq!(ctx.emit("hello", &json!({})).unwrap(), 1);
        assert_eq!(seen.load(Ordering::SeqCst), 1);
        drop(handle);
        assert_eq!(ctx.emit("hello", &json!({})).unwrap(), 0);

        // 没装总线时给出的错误要能指路。
        let bare = Context::new();
        let text = bare.events().unwrap_err().to_string();
        assert!(text.contains("fuyubot_event::install"), "{text}");
    }
}
