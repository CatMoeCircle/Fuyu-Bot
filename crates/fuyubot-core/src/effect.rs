//! [`Effect`]：一次注册的回滚守卫。

use std::fmt;

/// 回滚闭包。`FnOnce` + `Send`：只跑一次，且要能跨线程随守卫一起被丢弃。
type Rollback = Box<dyn FnOnce() + Send>;

/// 注册的守卫。**Drop 就是回滚**。
///
/// [`Context::provide`](crate::Context::provide) 这类注册操作都返回它：
/// 拿到就说明注册生效了，丢掉就自动撤销。所以要显式处理——存进管理器、[`merge`](Effect::merge)
/// 起来，或者 [`disarm`](Effect::disarm) 掉表示“这次注册不打算撤销”。
#[must_use = "Effect 一被丢弃就会立刻回滚刚做的注册；要保留就把它存起来，要合并就 merge"]
pub struct Effect {
    rollback: Option<Rollback>,
}

impl Effect {
    /// 什么都不做的守卫。
    pub fn none() -> Self {
        Self { rollback: None }
    }

    /// 用回滚闭包构造。
    pub fn new(rollback: impl FnOnce() + Send + 'static) -> Self {
        Self {
            rollback: Some(Box::new(rollback)),
        }
    }

    /// 有没有实际要跑的回滚。
    pub fn is_empty(&self) -> bool {
        self.rollback.is_none()
    }

    /// 合并两个守卫，drop 时**先跑 `other` 再跑 `self`**（后注册的先撤销，和撤销栈一致）。
    ///
    /// 链式写法：`a.merge(b).merge(c)` 的撤销顺序是 `c → b → a`。
    pub fn merge(mut self, other: Self) -> Self {
        let mut other = other;
        let mut other_rollback = other.rollback.take();
        let mut self_rollback = self.rollback.take();

        if other_rollback.is_none() && self_rollback.is_none() {
            return Self::none();
        }
        if other_rollback.is_none() {
            self.rollback = self_rollback;
            return self;
        }
        if self_rollback.is_none() {
            self.rollback = other_rollback;
            return self;
        }

        self.rollback = Some(Box::new(move || {
            if let Some(run) = other_rollback.take() {
                run();
            }
            if let Some(run) = self_rollback.take() {
                run();
            }
        }));
        self
    }

    /// 主动放弃回滚：注册留下的痕迹当作永久的。
    pub fn disarm(mut self) {
        self.rollback = None;
    }

    /// 主动触发回滚，等价于 drop 但能立刻确定跑完了。
    pub fn rollback(mut self) {
        self.run();
    }

    fn run(&mut self) {
        if let Some(run) = self.rollback.take() {
            run();
        }
    }
}

impl Default for Effect {
    fn default() -> Self {
        Self::none()
    }
}

impl fmt::Debug for Effect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Effect")
            .field("armed", &self.rollback.is_some())
            .finish()
    }
}

impl Drop for Effect {
    fn drop(&mut self) {
        self.run();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn log() -> (Arc<Mutex<Vec<&'static str>>>, Arc<Mutex<Vec<&'static str>>>) {
        let shared = Arc::new(Mutex::new(Vec::new()));
        (Arc::clone(&shared), shared)
    }

    #[test]
    fn drop_runs_the_rollback() {
        let (log, seen) = log();
        let effect = Effect::new(move || log.lock().unwrap().push("rolled back"));
        assert!(!effect.is_empty());
        assert!(seen.lock().unwrap().is_empty(), "drop 之前不该回滚");

        drop(effect);
        assert_eq!(*seen.lock().unwrap(), ["rolled back"]);
    }

    #[test]
    fn merge_rolls_back_in_reverse_registration_order() {
        let (log, seen) = log();
        let first = Effect::new({
            let log = Arc::clone(&log);
            move || log.lock().unwrap().push("first")
        });
        let second = Effect::new({
            let log = Arc::clone(&log);
            move || log.lock().unwrap().push("second")
        });
        let third = Effect::new({
            let log = Arc::clone(&log);
            move || log.lock().unwrap().push("third")
        });

        let merged = first.merge(second).merge(third);
        assert!(!merged.is_empty());
        drop(merged);
        assert_eq!(*seen.lock().unwrap(), ["third", "second", "first"]);
    }

    #[test]
    fn empty_effects_stay_empty_and_disarm_skips_rollback() {
        let (log, seen) = log();
        let merged = Effect::none().merge(Effect::none());
        assert!(merged.is_empty());
        drop(merged);

        Effect::new(move || log.lock().unwrap().push("never")).disarm();
        assert!(seen.lock().unwrap().is_empty());
    }
}
