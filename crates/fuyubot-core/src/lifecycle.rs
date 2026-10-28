//! 插件生命周期事件：给内核之外的观察者用（CLI 日志、宿主事件总线、测试）。
//!
//! [`Context`](crate::Context) 早就有 `on_service_change` 了——那观察的是**服务表**；
//! 这里观察的是**插件本身**的状态迁移（缺口 12）：登记、激活、因依赖消失退回、失败、卸载。
//!
//! 刻意不引 serde：核心 crate 只留纯 Rust 的类型。[`LifecycleEvent::kind`] 给跨进程 /
//! 跨语言桥用的字符串，负载怎么拼由调用方决定（loader 那边有 serde_json）。
//!
//! 事件是在**锁外面**发的：监听器里再查内核、卸载插件都不会自锁。

use std::sync::{Arc, Mutex};

use crate::effect::Effect;

/// 一次插件状态迁移。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LifecycleEvent {
    /// 已登记，开始等依赖（Pending）。**不等于已激活。**
    Loaded { name: String },
    /// `apply` 成功，进入 Active。
    Activated { name: String },
    /// 从 Active 退回 Pending（依赖消失、或主动卸载前的撤注册）。
    Deactivated { name: String },
    /// `apply` 返回了错误（也包括「依赖的提供者失败 → 级联跳过」）。
    Failed { name: String, reason: String },
    /// 从管理器的表里摘掉了，不再参与依赖求值。
    Unloaded { name: String },
}

impl LifecycleEvent {
    /// 跨进程 / 跨语言桥用的通道名（与 `fuyubot-event` 的命名习惯一致）。
    pub fn kind(&self) -> &'static str {
        match self {
            LifecycleEvent::Loaded { .. } => "plugin_loaded",
            LifecycleEvent::Activated { .. } => "plugin_activated",
            LifecycleEvent::Deactivated { .. } => "plugin_deactivated",
            LifecycleEvent::Failed { .. } => "plugin_failed",
            LifecycleEvent::Unloaded { .. } => "plugin_unloaded",
        }
    }

    /// 事件说的是哪个插件。
    pub fn name(&self) -> &str {
        match self {
            LifecycleEvent::Loaded { name }
            | LifecycleEvent::Activated { name }
            | LifecycleEvent::Deactivated { name }
            | LifecycleEvent::Failed { name, .. }
            | LifecycleEvent::Unloaded { name } => name,
        }
    }

    /// 失败原因；其它事件返回 `None`。
    pub fn reason(&self) -> Option<&str> {
        match self {
            LifecycleEvent::Failed { reason, .. } => Some(reason),
            _ => None,
        }
    }
}

/// 监听器表。放在 `Arc` 里，回滚守卫（[`Effect`]）与管理器共享同一份。
type Listener = Arc<dyn Fn(&LifecycleEvent) + Send + Sync>;

#[derive(Default)]
pub(crate) struct LifecycleHooks {
    listeners: Mutex<Vec<Option<Listener>>>,
}

impl LifecycleHooks {
    /// 登记一个监听器，返回它的槽位（回滚时按槽位置空，下标不漂移）。
    pub(crate) fn add(&self, listener: impl Fn(&LifecycleEvent) + Send + Sync + 'static) -> usize {
        let mut listeners = self.lock();
        let slot = listeners.len();
        listeners.push(Some(Arc::new(listener)));
        slot
    }

    pub(crate) fn remove(&self, slot: usize) {
        if let Some(slot) = self.lock().get_mut(slot) {
            *slot = None;
        }
    }

    /// 广播。监听器在锁**外面**被调用：它可能反过来查内核。
    ///
    /// 每个监听器单独 `catch_unwind`：观察者的 panic 不该反过来打断内核的状态机，
    /// 也不该让后面的监听器收不到这条事件（和工具表监听器一个道理）。
    pub(crate) fn emit(&self, event: &LifecycleEvent) {
        let snapshot: Vec<Listener> = self.lock().iter().flatten().cloned().collect();
        for listener in snapshot {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| listener(event)));
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Option<Listener>>> {
        // 中毒的锁照用：一个监听器 panic 不该让后面的生命周期事件全部消失。
        self.listeners.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// `PluginManager::on_lifecycle` 的便利包装（让调用方不用管槽位）。
pub(crate) fn subscribe(
    hooks: &Arc<LifecycleHooks>,
    listener: impl Fn(&LifecycleEvent) + Send + Sync + 'static,
) -> Effect {
    let slot = hooks.add(listener);
    let hooks = Arc::clone(hooks);
    Effect::new(move || hooks.remove(slot))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn a_listener_can_be_removed_and_events_carry_kind_and_name() {
        let hooks = Arc::new(LifecycleHooks::default());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let hook = subscribe(&hooks, move |event| {
            sink.lock().unwrap().push(event.kind().to_owned());
        });

        hooks.emit(&LifecycleEvent::Loaded {
            name: "demo".to_owned(),
        });
        assert_eq!(*seen.lock().unwrap(), ["plugin_loaded"]);
        assert_eq!(
            LifecycleEvent::Failed {
                name: "demo".to_owned(),
                reason: "boom".to_owned()
            }
            .reason(),
            Some("boom")
        );

        drop(hook);
        hooks.emit(&LifecycleEvent::Unloaded {
            name: "demo".to_owned(),
        });
        assert_eq!(*seen.lock().unwrap(), ["plugin_loaded"], "摘掉后不再收");
    }

    /// 一个监听器 panic 只丢它自己那条：后面的监听器照常收到，`emit` 也不炸。
    #[test]
    fn a_panicking_listener_does_not_break_the_others() {
        let hooks = Arc::new(LifecycleHooks::default());
        let hits = Arc::new(AtomicUsize::new(0));

        let _bad = subscribe(&hooks, |_| panic!("监听器故意 panic"));
        let sink = Arc::clone(&hits);
        let _good = subscribe(&hooks, move |_| {
            sink.fetch_add(1, Ordering::SeqCst);
        });

        hooks.emit(&LifecycleEvent::Loaded {
            name: "x".to_owned(),
        });
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "panic 只丢掉那一个监听器，不能连累后面的"
        );
    }
}
