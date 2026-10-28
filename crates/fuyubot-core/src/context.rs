//! [`Context`]：插件之间共享服务的**命名**表（Cordis 式 Container）。
//!
//! 表里按**名字**存，不按 Rust 类型：跨语言插件报不出 `TypeId`，只能报名字。
//! 类型化的 [`provide`](Context::provide) / [`get`](Context::get) 退化成**语法糖**——
//! 隐式拿 [`type_name::<T>()`](std::any::type_name) 当名字。
//! 于是「给 Rust 用的类型表」和「给插件用的名字表」是**同一张表**，不再有两套。
//!
//! 服务一旦注册/注销，会通知 [`Context::on_service_change`] 登记的钩子。
//! 这是 Cordis 式 `inject` 依赖跟踪的触发源：被依赖的能力何时出现、何时消失，
//! 得有个地方知道。

use std::any::{Any, type_name};
use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::effect::Effect;
use crate::error::KernelError;

/// 服务表里一格的值。
type Service = Arc<dyn Any + Send + Sync>;
/// 服务表的底层存储。按名字，不按 `TypeId`。
type ServiceMap = HashMap<String, Entry>;

/// 服务表里一格。
struct Entry {
    value: Service,
    /// 注册时那个 Rust 类型名。只用于诊断：`resolve::<T>` 类型不对时，
    /// 用它区分「名字不存在」和「名字在但类型不对」。
    type_name: &'static str,
}

/// 服务变更的种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceChange {
    Provided,
    Removed,
}

/// 服务变更钩子。做成 `Arc` 是为了通知前能把快照拷出来——
/// 钩子里完全可能再去注册/注销服务，持着读锁回调会死锁。
type ChangeHook = Arc<dyn Fn(&str, ServiceChange) + Send + Sync>;

/// 跨插件共享的上下文。
///
/// 内部是 `Arc<RwLock<..>>`，所以 `Clone` 只复制句柄、不复制状态：
/// 分发给插件的那份和内核持有的那份看到的是同一张表。
#[derive(Clone, Default)]
pub struct Context {
    services: Arc<RwLock<ServiceMap>>,
    hooks: Arc<RwLock<Vec<ChangeHook>>>,
}

impl Context {
    pub fn new() -> Self {
        Self::default()
    }

    /// 一个 Rust 类型默认的服務名。
    fn implicit_name<T: Any>() -> String {
        type_name::<T>().to_owned()
    }

    /// 注册服务，名字隐式取类型名，返回的回滚守卫会在 drop 时把它注销。
    ///
    /// 同一个类型重复注册直接报错（不做静默覆盖：覆盖会让先注册者的守卫
    /// 在稍后误删后注册者的服务）。
    pub fn provide<T: Any + Send + Sync>(&self, value: T) -> Result<Effect, KernelError> {
        let name = Self::implicit_name::<T>();
        self.provide_named(&name, value)
            .map_err(|error| match error {
                KernelError::ServiceNameExists(_) => KernelError::ServiceExists(type_name::<T>()),
                other => other,
            })
    }

    /// 按名字注册服务。跨语言插件注册进来的都走这条路。
    pub fn provide_named<T: Any + Send + Sync>(
        &self,
        name: &str,
        value: T,
    ) -> Result<Effect, KernelError> {
        if name.is_empty() {
            return Err(KernelError::EmptyServiceName);
        }

        {
            let mut services = self.write();
            if services.contains_key(name) {
                return Err(KernelError::ServiceNameExists(name.to_owned()));
            }
            services.insert(
                name.to_owned(),
                Entry {
                    value: Arc::new(value),
                    type_name: type_name::<T>(),
                },
            );
        }

        notify(&self.hooks, name, ServiceChange::Provided);

        let table = Arc::clone(&self.services);
        let hooks = Arc::clone(&self.hooks);
        let key = name.to_owned();
        Ok(Effect::new(move || {
            table
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&key);
            notify(&hooks, &key, ServiceChange::Removed);
        }))
    }

    /// 按类型取服务。拿到的是共享句柄，服务本身不会被复制。
    pub fn get<T: Any + Send + Sync>(&self) -> Result<Arc<T>, KernelError> {
        self.lookup::<T>(&Self::implicit_name::<T>())
            .ok_or_else(|| KernelError::ServiceMissing(type_name::<T>()))
    }

    /// 按名字取服务。名字不存在报 [`KernelError::ServiceNameMissing`]，
    /// 名字在但注册的是别的类型报 [`KernelError::ServiceTypeMismatch`]。
    pub fn resolve<T: Any + Send + Sync>(&self, name: &str) -> Result<Arc<T>, KernelError> {
        let services = self.read();
        let Some(entry) = services.get(name) else {
            return Err(KernelError::ServiceNameMissing(name.to_owned()));
        };
        let found = entry.type_name;
        Arc::clone(&entry.value)
            .downcast::<T>()
            .map_err(|_| KernelError::ServiceTypeMismatch {
                name: name.to_owned(),
                expected: type_name::<T>(),
                found,
            })
    }

    /// 名字在不在，**不看类型**。inject 判定依赖是否满足只需要这个。
    pub fn has(&self, name: &str) -> bool {
        self.read().contains_key(name)
    }

    /// 某个名字上注册的实际类型名。
    pub fn type_of(&self, name: &str) -> Option<&'static str> {
        self.read().get(name).map(|entry| entry.type_name)
    }

    /// 已注册的名字，已排序（输出稳定）。
    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.read().keys().cloned().collect();
        names.sort();
        names
    }

    pub fn contains<T: Any + Send + Sync>(&self) -> bool {
        self.has(&Self::implicit_name::<T>())
    }

    pub fn len(&self) -> usize {
        self.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.read().is_empty()
    }

    /// 登记服务变更钩子。依赖跟踪（某个能力出现/消失）靠它驱动。
    ///
    /// 钩子是**追加**的，没有反注册——和 [`Context`] 同生命周期的东西才该用它。
    pub fn on_service_change(&self, hook: impl Fn(&str, ServiceChange) + Send + Sync + 'static) {
        self.hooks
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .push(Arc::new(hook));
    }

    /// 按名字取，**不区分名字不在和类型不对**——统一的下游。
    fn lookup<T: Any + Send + Sync>(&self, name: &str) -> Option<Arc<T>> {
        self.read()
            .get(name)
            .and_then(|entry| Arc::clone(&entry.value).downcast::<T>().ok())
    }

    /// 中毒的锁也要能用：一个插件的 panic 不该让整个内核卡死。
    fn read(&self) -> RwLockReadGuard<'_, ServiceMap> {
        self.services.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self) -> RwLockWriteGuard<'_, ServiceMap> {
        self.services.write().unwrap_or_else(|e| e.into_inner())
    }
}

/// 通知所有钩子。**先把快照拷出来再回调**，钩子里改服务表才不会死锁。
fn notify(hooks: &Arc<RwLock<Vec<ChangeHook>>>, name: &str, change: ServiceChange) {
    let Ok(guard) = hooks.read() else {
        return;
    };
    let snapshot: Vec<ChangeHook> = guard.clone();
    drop(guard);
    for hook in snapshot {
        hook(name, change);
    }
}

impl fmt::Debug for Context {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Context")
            .field("services", &self.names())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq)]
    struct Config(&'static str);

    #[test]
    fn provide_then_get_returns_a_shared_handle() {
        let ctx = Context::new();
        assert!(ctx.is_empty());
        let _guard = ctx.provide(Config("prod")).unwrap();

        let one = ctx.get::<Config>().unwrap();
        let two = ctx.get::<Config>().unwrap();
        assert_eq!(*one, Config("prod"));
        assert!(Arc::ptr_eq(&one, &two), "拿到的应该是同一个 Arc");
        assert_eq!(ctx.len(), 1);
    }

    #[test]
    fn missing_service_and_duplicate_provide_are_errors() {
        let ctx = Context::new();
        assert!(matches!(
            ctx.get::<Config>(),
            Err(KernelError::ServiceMissing(_))
        ));

        let _guard = ctx.provide(Config("prod")).unwrap();
        assert!(matches!(
            ctx.provide(Config("dev")),
            Err(KernelError::ServiceExists(_))
        ));
        assert_eq!(*ctx.get::<Config>().unwrap(), Config("prod"));
    }

    #[test]
    fn dropping_the_guard_unregisters_the_service() {
        let ctx = Context::new();
        let guard = ctx.provide(7u32).unwrap();
        assert!(ctx.contains::<u32>());

        drop(guard);
        assert!(!ctx.contains::<u32>());
        assert!(matches!(
            ctx.get::<u32>(),
            Err(KernelError::ServiceMissing(_))
        ));
    }

    #[test]
    fn clones_share_the_same_table() {
        let ctx = Context::new();
        let fork = ctx.clone();
        let _guard = fork.provide(1u8).unwrap();
        assert_eq!(*ctx.get::<u8>().unwrap(), 1);
    }

    #[test]
    fn named_registration_lands_in_the_same_table() {
        let ctx = Context::new();
        let _guard = ctx.provide_named("greeting", String::from("hi")).unwrap();

        assert!(ctx.has("greeting"));
        assert_eq!(*ctx.resolve::<String>("greeting").unwrap(), "hi");
        assert_eq!(ctx.type_of("greeting"), Some("alloc::string::String"));
        assert_eq!(ctx.names(), ["greeting"]);
        assert_eq!(ctx.len(), 1);
    }

    #[test]
    fn typed_registration_is_visible_by_its_type_name() {
        struct Marker;
        let ctx = Context::new();
        let _guard = ctx.provide(Marker).unwrap();

        // 隐式名字就是 type_name：按名字查也应该找得到。
        assert!(ctx.has(&std::any::type_name::<Marker>()));
    }

    #[test]
    fn resolving_reports_missing_and_type_mismatch_differently() {
        let ctx = Context::new();
        let _guard = ctx.provide_named("port", 8080u32).unwrap();

        // 名字在，类型不对。
        assert!(matches!(
            ctx.resolve::<String>("port"),
            Err(KernelError::ServiceTypeMismatch { .. })
        ));
        // 名字不在。
        assert!(matches!(
            ctx.resolve::<u32>("nope"),
            Err(KernelError::ServiceNameMissing(_))
        ));
        assert_eq!(ctx.type_of("nope"), None);
    }

    #[test]
    fn empty_name_is_rejected() {
        let ctx = Context::new();
        assert!(matches!(
            ctx.provide_named("", 1u8),
            Err(KernelError::EmptyServiceName)
        ));
    }

    #[test]
    fn change_hooks_fire_on_provide_and_on_removal() {
        let ctx = Context::new();
        let log = Arc::new(RwLock::new(Vec::new()));

        let sink = Arc::clone(&log);
        ctx.on_service_change(move |name, change| {
            let kind = matches!(change, ServiceChange::Provided);
            sink.write().unwrap().push(format!("{name}:{kind}"));
        });

        let guard = ctx.provide_named("db", 1u8).unwrap();
        drop(guard);

        assert_eq!(*log.read().unwrap(), ["db:true", "db:false"]);
    }

    /// 钩子里再去注册服务不能死锁——这正是先把快照拷出来的理由。
    #[test]
    fn a_hook_may_touch_the_service_table_without_deadlocking() {
        let ctx = Context::new();
        // 钩子注册出来的服务得把 `Effect` 存住：`Effect` 是 `#[must_use]`，
        // `let _ =` 会立刻把它回滚掉（约束 11），那就测不到东西了。
        let held = Arc::new(std::sync::Mutex::new(Vec::new()));

        let reentrant = ctx.clone();
        let sink = Arc::clone(&held);
        ctx.on_service_change(move |name, change| {
            if matches!(change, ServiceChange::Provided) && name == "a" {
                if let Ok(effect) = reentrant.provide_named("b", 2u8) {
                    sink.lock().unwrap_or_else(|e| e.into_inner()).push(effect);
                }
            }
        });

        let _guard_a = ctx.provide_named("a", 1u8);
        assert!(ctx.has("b"), "钩子里的注册该成功，且不该卡住");
    }
}
