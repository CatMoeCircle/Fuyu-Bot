//! [`Context`]：插件之间共享服务的类型表。

use std::any::{Any, TypeId, type_name};
use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::effect::Effect;
use crate::error::KernelError;

/// 服务表里一格的值。
type Service = Arc<dyn Any + Send + Sync>;
/// 服务表的底层存储。
type ServiceMap = HashMap<TypeId, Service>;

/// 跨插件共享的上下文。
///
/// 内部是 `Arc<RwLock<..>>`，所以 `Clone` 只复制句柄、不复制状态：
/// 分发给插件的那份和内核持有的那份看到的是同一张表。
#[derive(Clone, Default)]
pub struct Context {
    services: Arc<RwLock<ServiceMap>>,
}

impl Context {
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册服务，返回的回滚守卫会在 drop 时把它注销。
    ///
    /// 同一个类型重复注册直接报错（不做静默覆盖：覆盖会让先注册者的守卫
    /// 在稍后误删后注册者的服务）。
    pub fn provide<T: Any + Send + Sync>(&self, value: T) -> Result<Effect, KernelError> {
        let id = TypeId::of::<T>();
        {
            let mut services = self.write();
            if services.contains_key(&id) {
                return Err(KernelError::ServiceExists(type_name::<T>()));
            }
            services.insert(id, Arc::new(value));
        }

        let table = Arc::clone(&self.services);
        Ok(Effect::new(move || {
            table.write().unwrap_or_else(|e| e.into_inner()).remove(&id);
        }))
    }

    /// 按类型取服务。拿到的是共享句柄，服务本身不会被复制。
    pub fn get<T: Any + Send + Sync>(&self) -> Result<Arc<T>, KernelError> {
        self.read()
            .get(&TypeId::of::<T>())
            .cloned()
            .and_then(|service| service.downcast::<T>().ok())
            .ok_or_else(|| KernelError::ServiceMissing(type_name::<T>()))
    }

    pub fn contains<T: Any + Send + Sync>(&self) -> bool {
        self.read().contains_key(&TypeId::of::<T>())
    }

    pub fn len(&self) -> usize {
        self.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.read().is_empty()
    }

    /// 中毒的锁也要能用：一个插件的 panic 不该让整个内核卡死。
    fn read(&self) -> RwLockReadGuard<'_, ServiceMap> {
        self.services.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self) -> RwLockWriteGuard<'_, ServiceMap> {
        self.services.write().unwrap_or_else(|e| e.into_inner())
    }
}

impl fmt::Debug for Context {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Context")
            .field("services", &self.len())
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
}
