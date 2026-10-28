//! 内核错误与跨 crate 统一的回调错误类型。

use crate::Phase;

/// 回调统一的错误类型。
///
/// 事件、服务、工具、中间件四个 crate 只能依赖 `fuyubot-core`、彼此不能依赖，
/// 所以回调签名里的错误类型必须由 core 定。这是本仓库唯一允许 `Box<dyn Error>`
/// 的地方（二进制 `main` 除外）：窄化到某个具体错误就没法跨机制统一了。
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// 回调统一返回它：`fn(&Payload) -> Result<()>`。
///
/// 任何实现了 `Error + Send + Sync + 'static` 的错误都能 `?` 进来。
pub type Result<T, E = BoxError> = std::result::Result<T, E>;

/// 内核与插件管理器的统一错误类型。
///
/// 公开 API 只要可能失败就走它，调用方不必再叠一层 `Box<dyn Error>`。
#[derive(Debug, thiserror::Error)]
pub enum KernelError {
    #[error("服务 {0} 已经注册过了")]
    ServiceExists(&'static str),
    #[error("服务 {0} 没有注册")]
    ServiceMissing(&'static str),

    // —— 命名服务（Cordis 式 Container）：名字不但承担查找，也承担 inject 依赖判定 ——
    #[error("命名服务 {0} 已经注册过了")]
    ServiceNameExists(String),
    #[error("命名服务 {0} 没有注册")]
    ServiceNameMissing(String),
    #[error("命名服务 {name} 注册的是 {found}，不是 {expected}")]
    ServiceTypeMismatch {
        name: String,
        expected: &'static str,
        found: &'static str,
    },
    #[error("服务名不能为空")]
    EmptyServiceName,

    #[error("插件 {0} 已经加载过了")]
    PluginAlreadyLoaded(String),
    #[error("插件 {0} 没有加载")]
    PluginNotLoaded(String),
    /// `apply` 失败。**和 `Pending` 不是一回事**：Pending 是「等着」，
    /// 这是「试过了、起不来」。宿主层（比如 dylib 适配器）把外部错误翻成它。
    #[error("插件 {plugin} 启动失败: {reason}")]
    PluginApplyFailed { plugin: String, reason: String },
    #[error("插件集合里有重名的 {0}")]
    DuplicatePlugin(String),
    #[error("插件 {plugin} 依赖 {dependency}，但 {dependency} 既没加载也不在本次加载集合里")]
    MissingDependency { plugin: String, dependency: String },
    #[error("依赖成环：{cycle}")]
    DependencyCycle { cycle: String },
    #[error("插件 {plugin} 还被 {dependents} 依赖着，先卸载它们")]
    HasDependents { plugin: String, dependents: String },

    #[error("阶段非法：当前 {current:?}，期望 {expected:?}")]
    BadPhase { current: Phase, expected: Phase },
}
