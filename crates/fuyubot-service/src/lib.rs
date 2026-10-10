//! 命名服务容器：`名字 → Arc<dyn Any + Send + Sync>`。
//!
//! 为什么不用内核 [`Context`] 的类型表：跨语言插件报不出 Rust 的 `TypeId`，
//! 只能报名字。两者并存——`Context` 给宿主内部用，这里是给插件与跨语言调用用的。
//!
//! 只依赖 `fuyubot-core`。

use std::any::{Any, type_name};
use std::collections::HashMap;
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

use fuyubot_core::{Context, Effect, KernelError};

/// 容器错误。
#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error("服务 {0} 已经注册过了")]
    AlreadyProvided(String),
    #[error("服务名不能为空")]
    EmptyName,
    #[error(
        "Context 里还没有 ServiceContainer：先 `let _guard = fuyubot_service::install(&ctx)?;` 并持有它"
    )]
    NotInstalled,
    #[error(transparent)]
    Kernel(#[from] KernelError),
}

/// 表里一格。
struct Entry {
    value: Arc<dyn Any + Send + Sync>,
    /// 注册时那个 Rust 类型名。只用于诊断：`get::<T>` 返回 `None` 时，
    /// 用它区分“没注册”和“注册了但类型不对”。
    type_name: &'static str,
}

/// 按名字注册的服务容器。`Clone` 共享同一张表。
#[derive(Clone, Default)]
pub struct ServiceContainer {
    services: Arc<RwLock<HashMap<String, Entry>>>,
}

impl ServiceContainer {
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册服务，返回的句柄 drop 即注销。
    ///
    /// 同名重复注册直接报错（不做静默覆盖：覆盖会让先注册者的句柄日后误删新服务）。
    pub fn provide<T: Any + Send + Sync>(
        &self,
        name: impl Into<String>,
        service: T,
    ) -> Result<ServiceHandle, ServiceError> {
        let name = name.into();
        if name.is_empty() {
            return Err(ServiceError::EmptyName);
        }

        {
            let mut services = self.write();
            if services.contains_key(&name) {
                return Err(ServiceError::AlreadyProvided(name));
            }
            services.insert(
                name.clone(),
                Entry {
                    value: Arc::new(service),
                    type_name: type_name::<T>(),
                },
            );
        }

        let table = Arc::clone(&self.services);
        let key = name.clone();
        Ok(ServiceHandle {
            name,
            effect: Effect::new(move || {
                table
                    .write()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&key);
            }),
        })
    }

    /// 类型安全查找。名字不存在、或存的类型不是 `T`，都返回 `None`
    /// （要区分两者就用 [`contains`](Self::contains) 与 [`type_of`](Self::type_of)）。
    pub fn get<T: Any + Send + Sync>(&self, name: &str) -> Option<Arc<T>> {
        self.read()
            .get(name)
            .and_then(|entry| Arc::clone(&entry.value).downcast::<T>().ok())
    }

    pub fn contains(&self, name: &str) -> bool {
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

    pub fn len(&self) -> usize {
        self.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.read().is_empty()
    }

    /// 中毒的锁也照用：一个服务的 panic 不该让整个容器卡死。
    fn read(&self) -> RwLockReadGuard<'_, HashMap<String, Entry>> {
        self.services.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self) -> RwLockWriteGuard<'_, HashMap<String, Entry>> {
        self.services.write().unwrap_or_else(|e| e.into_inner())
    }
}

impl std::fmt::Debug for ServiceContainer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServiceContainer")
            .field("services", &self.names())
            .finish()
    }
}

/// 服务句柄。drop 即注销。
#[must_use = "服务句柄一被丢弃就立刻注销刚注册的服务；要保留就把它存起来"]
pub struct ServiceHandle {
    name: String,
    effect: Effect,
}

impl ServiceHandle {
    pub fn name(&self) -> &str {
        &self.name
    }

    /// 显式注销。
    pub fn remove(self) {
        drop(self.effect);
    }
}

impl std::fmt::Debug for ServiceHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServiceHandle")
            .field("name", &self.name)
            .finish()
    }
}

/// 把容器装进内核 [`Context`]。
///
/// 返回的 `Effect` **必须一直持有**：它一 drop，容器就从 Context 里消失了。
pub fn install(ctx: &Context) -> Result<Effect, ServiceError> {
    Ok(ctx.provide(ServiceContainer::new())?)
}

/// `Context` 上的命名服务入口。
///
/// 方法名故意**不叫** `provide`/`get`：内核 [`Context`] 已经有同名的类型化方法，
/// 且固有方法优先于 trait 方法——真叫 `provide` 的话 trait 方法根本调不到。
pub trait ServiceContextExt {
    /// 取出装好的容器。
    fn services(&self) -> Result<Arc<ServiceContainer>, ServiceError>;

    /// 按名字注册服务。
    fn provide_named<T: Any + Send + Sync>(
        &self,
        name: impl Into<String>,
        service: T,
    ) -> Result<ServiceHandle, ServiceError>;

    /// 按名字取服务。
    fn resolve<T: Any + Send + Sync>(&self, name: &str) -> Option<Arc<T>>;
}

impl ServiceContextExt for Context {
    fn services(&self) -> Result<Arc<ServiceContainer>, ServiceError> {
        self.get::<ServiceContainer>()
            .map_err(|_| ServiceError::NotInstalled)
    }

    fn provide_named<T: Any + Send + Sync>(
        &self,
        name: impl Into<String>,
        service: T,
    ) -> Result<ServiceHandle, ServiceError> {
        self.services()?.provide(name, service)
    }

    fn resolve<T: Any + Send + Sync>(&self, name: &str) -> Option<Arc<T>> {
        self.services().ok()?.get::<T>(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 验收 2：provide 一个 String，get 到；drop ServiceHandle 后，get 返回 None。
    #[test]
    fn provide_get_then_drop_makes_it_disappear() {
        let container = ServiceContainer::new();
        let handle = container
            .provide("greeting", String::from("hello"))
            .unwrap();

        assert_eq!(*container.get::<String>("greeting").unwrap(), "hello");
        assert!(container.contains("greeting"));

        drop(handle);
        assert!(container.get::<String>("greeting").is_none());
        assert!(!container.contains("greeting"));
    }

    #[test]
    fn duplicate_name_and_empty_name_are_errors() {
        let container = ServiceContainer::new();
        let _handle = container.provide("db", 1u8).unwrap();

        assert!(matches!(
            container.provide("db", 2u8),
            Err(ServiceError::AlreadyProvided(_))
        ));
        assert!(matches!(
            container.provide("", 3u8),
            Err(ServiceError::EmptyName)
        ));
    }

    #[test]
    fn type_mismatch_is_distinguishable_from_missing() {
        let container = ServiceContainer::new();
        let _handle = container.provide("port", 8080u32).unwrap();

        // 名字在，但类型不对。
        assert!(container.get::<String>("port").is_none());
        assert!(container.contains("port"));
        assert_eq!(container.type_of("port"), Some("u32"));

        // 名字不在。
        assert!(container.get::<u32>("nope").is_none());
        assert_eq!(container.type_of("nope"), None);
    }

    #[test]
    fn explicit_remove_and_names_are_sorted() {
        let container = ServiceContainer::new();
        let beta = container.provide("beta", 1u8).unwrap();
        let _alpha = container.provide("alpha", 2u8).unwrap();
        assert_eq!(container.names(), ["alpha", "beta"]);
        assert_eq!(container.len(), 2);

        beta.remove();
        assert_eq!(container.names(), ["alpha"]);
    }

    #[test]
    fn clones_share_the_same_table() {
        let container = ServiceContainer::new();
        let fork = container.clone();
        let _handle = fork.provide("shared", 7u16).unwrap();
        assert_eq!(*container.get::<u16>("shared").unwrap(), 7);
    }

    #[test]
    fn context_integration() {
        let ctx = Context::new();
        let _guard = install(&ctx).unwrap();

        let handle = ctx.provide_named("greeting", String::from("hi")).unwrap();
        assert_eq!(*ctx.resolve::<String>("greeting").unwrap(), "hi");
        drop(handle);
        assert!(ctx.resolve::<String>("greeting").is_none());

        // 没装容器时给出的错误要能指路。
        let bare = Context::new();
        let error = bare.services().unwrap_err();
        assert!(error.to_string().contains("fuyubot_service::install"));
        assert!(bare.resolve::<String>("greeting").is_none());
    }
}
