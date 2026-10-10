//! Cordis 内核：生命周期、插件依赖解析与 Context 传播。
//!
//! 只做这三件事，不依赖 `fuyubot-tdlib` 或 `fuyubot-event`，也不认识 Telegram。
//!
//! ```
//! use fuyubot_core::{Context, Effect, KernelError, Plugin, PluginManager};
//!
//! struct Db;
//! impl Plugin for Db {
//!     fn name(&self) -> &str { "db" }
//!     fn apply(&self, ctx: &Context) -> Result<Effect, KernelError> {
//!         ctx.provide(42u64) // 卸载时这个服务会被自动注销
//!     }
//! }
//!
//! let ctx = Context::new();
//! let mut manager = PluginManager::new(ctx.clone());
//! manager.load(Db).unwrap();
//! assert_eq!(*ctx.get::<u64>().unwrap(), 42);
//!
//! manager.unload("db").unwrap();
//! assert!(ctx.get::<u64>().is_err());
//! ```

mod context;
mod effect;
mod error;
mod manager;
mod plugin;

pub use context::Context;
pub use effect::Effect;
pub use error::{BoxError, KernelError, Result};
pub use manager::PluginManager;
pub use plugin::Plugin;

/// 内核生命周期阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Created,
    Started,
    Stopped,
}

/// 内核：持有 [`Context`] 与 [`PluginManager`]，负责阶段转换。
pub struct Kernel {
    ctx: Context,
    plugins: PluginManager,
    phase: Phase,
}

impl Kernel {
    pub fn new() -> Self {
        let ctx = Context::new();
        let plugins = PluginManager::new(ctx.clone());
        Self {
            ctx,
            plugins,
            phase: Phase::Created,
        }
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// 插件与内核共享的那张服务表。
    pub fn context(&self) -> &Context {
        &self.ctx
    }

    /// 加载 / 卸载插件走这里。
    pub fn plugins(&mut self) -> &mut PluginManager {
        &mut self.plugins
    }

    /// `Created` → `Started`。
    pub async fn start(&mut self) -> Result<(), KernelError> {
        if self.phase != Phase::Created {
            return Err(KernelError::BadPhase {
                current: self.phase,
                expected: Phase::Created,
            });
        }
        self.phase = Phase::Started;
        Ok(())
    }

    /// 卸载全部插件（倒序）→ `Stopped`。
    pub async fn stop(&mut self) -> Result<(), KernelError> {
        self.plugins.unload_all()?;
        self.phase = Phase::Stopped;
        Ok(())
    }
}

impl Default for Kernel {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for Kernel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Kernel")
            .field("phase", &self.phase)
            .field("plugins", &self.plugins.loaded())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kernel_starts_once_and_stops_idempotently() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();

        runtime.block_on(async {
            let mut kernel = Kernel::new();
            assert_eq!(kernel.phase(), Phase::Created);

            kernel.start().await.unwrap();
            assert_eq!(kernel.phase(), Phase::Started);
            assert!(matches!(
                kernel.start().await,
                Err(KernelError::BadPhase { .. })
            ));

            kernel.stop().await.unwrap();
            assert_eq!(kernel.phase(), Phase::Stopped);
            // 再 stop 一次不该报错（插件已经空了）。
            kernel.stop().await.unwrap();
        });
    }
}
