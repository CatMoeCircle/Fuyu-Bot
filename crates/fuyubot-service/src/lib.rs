//! 命名服务：**已经下沉到内核**，这里只剩兼容层。
//!
//! 过去这里自己维护一张「名字 → 服务」的表（`ServiceContainer`），与内核
//! [`Context`] 的「类型 → 服务」表并存。Cordis 化之后 `Context` 本身就是
//! 按名字寻址的 Container，两套合成一套（`docs/gaps.md` 缺口 5）：
//! 插件报得出名字、`inject` 判得了依赖、宿主也只有一张表要管。
//!
//! 还留在这里的是 [`ServiceHandle`]（带名字的注册守卫，`fuyubot-loader`
//! 的回滚链仍在用）与 [`install`]（现在返回一个空守卫）。
//!
//! 只依赖 `fuyubot-core`。

use std::any::Any;
use std::fmt;
use std::sync::Arc;

use fuyubot_core::{Context, Effect, KernelError};

/// 服务错误。
#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error("服务 {0} 已经注册过了")]
    AlreadyProvided(String),
    #[error("服务名不能为空")]
    EmptyName,
    #[error(transparent)]
    Kernel(#[from] KernelError),
}

/// 服务注册句柄。drop 即注销。
///
/// 比裸的 [`Effect`] 多带了名字：`fuyubot-loader` 回滚时要用它认出注的是什么。
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

impl fmt::Debug for ServiceHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServiceHandle")
            .field("name", &self.name)
            .finish()
    }
}

/// 兼容旧调用点。
///
/// 命名服务现在是 [`Context`] 自带的，不需要再往里装一个容器，所以它只返回
/// 一个**空守卫**。留着这个函数是因为「先 install 再 provide」的调用点很多，
/// 让它们保持原样比一次改干净更划算——它们是 `&Context` 上的no-op。
pub fn install(_ctx: &Context) -> Result<Effect, ServiceError> {
    Ok(Effect::none())
}

/// `Context` 上的命名服务入口。
///
/// 方法名**不能**叫 `provide_named` / `resolve`：那是内核 `Context` 现在的固有方法，
/// 而固有方法优先于 trait 方法（约束 14），重名会让这里的永远调不到。
pub trait ServiceContextExt {
    /// 按名字注册服务，返回带名字的守卫。
    fn provide_service<T: Any + Send + Sync>(
        &self,
        name: impl Into<String>,
        service: T,
    ) -> Result<ServiceHandle, ServiceError>;

    /// 按名字取服务。名字不在、或注册的是别的类型，都返回 `None`。
    fn get_service<T: Any + Send + Sync>(&self, name: &str) -> Option<Arc<T>>;

    /// 名字在不在（不看类型）。
    fn has_service(&self, name: &str) -> bool;

    /// 已注册的名字，已排序（输出稳定）。
    fn service_names(&self) -> Vec<String>;
}

impl ServiceContextExt for Context {
    fn provide_service<T: Any + Send + Sync>(
        &self,
        name: impl Into<String>,
        service: T,
    ) -> Result<ServiceHandle, ServiceError> {
        let name = name.into();
        if name.is_empty() {
            return Err(ServiceError::EmptyName);
        }
        // 内核报的是自己的错误码，这里翻回本 crate 历史上那几个变体：
        // 调用方可能已经在 match `AlreadyProvided` 了。
        let effect = self
            .provide_named(&name, service)
            .map_err(|error| match error {
                KernelError::ServiceNameExists(name) => ServiceError::AlreadyProvided(name),
                KernelError::EmptyServiceName => ServiceError::EmptyName,
                other => ServiceError::Kernel(other),
            })?;
        Ok(ServiceHandle { name, effect })
    }

    fn get_service<T: Any + Send + Sync>(&self, name: &str) -> Option<Arc<T>> {
        self.resolve::<T>(name).ok()
    }

    fn has_service(&self, name: &str) -> bool {
        self.has(name)
    }

    fn service_names(&self) -> Vec<String> {
        self.names()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provide_then_get_service() {
        let ctx = Context::new();
        let _guard = install(&ctx).unwrap();

        let handle = ctx.provide_service("db", 1u8).unwrap();
        assert_eq!(handle.name(), "db");
        assert_eq!(*ctx.get_service::<u8>("db").unwrap(), 1);
        assert!(ctx.has_service("db"));

        drop(handle);
        assert!(ctx.get_service::<u8>("db").is_none());
        assert!(!ctx.has_service("db"));
    }

    #[test]
    fn duplicate_and_empty_names_are_errors() {
        let ctx = Context::new();
        let _handle = ctx.provide_service("db", 1u8).unwrap();

        assert!(matches!(
            ctx.provide_service("db", 2u8),
            Err(ServiceError::AlreadyProvided(_))
        ));
        assert!(matches!(
            ctx.provide_service("", 3u8),
            Err(ServiceError::EmptyName)
        ));
    }

    #[test]
    fn type_mismatch_is_none_and_names_are_sorted() {
        let ctx = Context::new();
        let beta = ctx.provide_service("beta", 1u8).unwrap();
        let _alpha = ctx.provide_service("alpha", 2u8).unwrap();

        // 名字在但类型不对：这里只报 None。要区分就用 Context::resolve。
        assert!(ctx.get_service::<String>("beta").is_none());
        assert_eq!(ctx.service_names(), ["alpha", "beta"]);

        beta.remove();
        assert_eq!(ctx.service_names(), ["alpha"]);
    }

    /// 命名服务和类型化服务是同一张表：
    /// 这是「两套并存」被消掉的直接证据。
    #[test]
    fn typed_and_named_registrations_share_one_table() {
        struct Marker;
        let ctx = Context::new();
        let _typed = ctx.provide(Marker);
        let _named = ctx.provide_service("adb", 1u8).unwrap();

        // 类型注册的那条，按 type_name 这个「名字」也找得到。
        assert!(ctx.has_service(&std::any::type_name::<Marker>()));
        assert_eq!(ctx.len(), 2, "两条注册落在同一张表里");
    }
}
