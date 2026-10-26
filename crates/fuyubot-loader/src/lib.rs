//! 宿主运行时与插件加载器。
//!
//! - [`runtime`]（内部）：把 [`fuyubot_abi::HostApiVTable`] 的每个函数桥到阶段 2/3 的机制上。
//!   四个机制 crate 彼此不依赖，把它们组合起来的地方只能有一个，就是这里。
//! - [`UpdatePipeline`]：`td_receive` 的原始 JSON → 六个中间件阶段 → 插件监听器。
//! - [`PluginLoader`]：dlopen 一个 cdylib，走 `plugin_init` / `plugin_start` / `plugin_stop`。
//!
//! 这里是本仓库唯一的「宿主层」：机制 crate 之间不许互相依赖，组合与桥接都发生在本 crate。

mod loader;
mod pipeline;
mod runtime;

/// 插件作者写 cdylib 时直接对着 ABI crate 编程。
pub use fuyubot_abi as abi;

pub use loader::{DLL_EXTENSION, LoadError, LoadedPlugin, PluginLoader, plugin_files};
pub use pipeline::{PipelineError, UpdateOutcome, UpdatePipeline};
pub use runtime::{
    DEFAULT_LOG_VERBOSITY, HostRuntime, LogLevel, RESPONSE_EVENT, SentRequest, UPDATE_EVENT,
};
