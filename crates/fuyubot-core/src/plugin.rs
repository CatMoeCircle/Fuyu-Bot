//! [`Plugin`] trait 与 `define_plugin!` 宏。

use crate::context::Context;
use crate::effect::Effect;
use crate::error::KernelError;

/// 一个内核插件。
///
/// `apply` 收 `&self`、`&Context`：插件自己的可变状态要靠内部可变性，
/// 注册出去的东西靠返回的 [`Effect`] 撤销。
pub trait Plugin: Send + Sync {
    /// 唯一名字。依赖解析和 [`unload`](crate::PluginManager::unload) 都认它。
    fn name(&self) -> &str;

    /// 声明依赖的插件名。默认不依赖任何插件。
    fn dependencies(&self) -> Vec<String> {
        Vec::new()
    }

    /// 声明要哪些**能力**（Cordis 的 `inject`）。这些能力没到位，
    /// [`apply`] 就不会被调用——插件停在 Pending 等待，而不是报错。
    ///
    /// 一个名字有两种满足方式：**服务表里有它**（某个插件 `<provide>` 的能力，
    /// 跨语言插件也一样查得到），或者**有个同名插件正活着**（依赖某个具体插件
    /// 已经起来了）。所以「按插件依赖」和「按能力依赖」是同一套机制。
    ///
    /// 默认转发到 [`dependencies`](Self::dependencies)，以便旧的按插件名声明继续有效。
    fn inject(&self) -> Vec<String> {
        self.dependencies()
    }

    /// **软依赖**：缺失时照常加载，但存在时排在它前面。
    ///
    /// 与 [`inject`](Self::inject) 的区别是**阻塞性**：
    /// - `inject` 是硬依赖：缺了就不激活（Pending 等待）；
    /// - `soft_inject` 是排序提示：缺了不影响激活，但两者都就绪时**先激活被依赖的那个**。
    ///
    /// 用途：「A 希望在 B 之后初始化（比如要读 B 注册的配置），但没有 B 也行」。
    ///
    /// ⚠️ 不参与环检测——软依赖成环只是排序上的死循环，不影响正确性，
    /// 检测它会把合法的「互相软依赖」误报成错误。
    fn soft_inject(&self) -> Vec<String> {
        Vec::new()
    }

    /// 注册服务、启动自己的东西，返回回滚守卫。
    ///
    /// 中途失败时请直接返回 `Err`：已经注册的部分由你持有的 `Effect` 负责回滚。
    fn apply(&self, ctx: &Context) -> Result<Effect, KernelError>;

    /// 收到卸载时的清理钩子。服务注销不在这里做——那是 `apply` 返回的 `Effect` 的活。
    fn dispose(&self, _ctx: &Context) {}
}

/// 用最少样板实现一个 [`Plugin`]。
///
/// 只适用于无状态的插件；需要字段的插件请手写 `impl`。
///
/// ```
/// use fuyubot_core::{define_plugin, Context, Effect, KernelError};
///
/// define_plugin! {
///     /// 打个招呼。
///     pub struct Greeter,
///     name: "greeter",
///     deps: ["config"],
///     apply(ctx) {
///         let _ = ctx;
///         Ok(Effect::none())
///     },
///     dispose(_ctx) {
///         // 清理
///     }
/// }
///
/// let plugin = Greeter;
/// assert_eq!(fuyubot_core::Plugin::name(&plugin), "greeter");
/// assert_eq!(fuyubot_core::Plugin::dependencies(&plugin), ["config"]);
/// ```
#[macro_export]
macro_rules! define_plugin {
    (
        $(#[$meta:meta])*
        $vis:vis struct $ty:ident,
        name: $name:expr
        $(, deps: [$($dep:expr),* $(,)?])?
        ,
        apply($ctx:ident) $apply:block
        $(, dispose($dctx:ident) $dispose:block)?
        $(,)?
    ) => {
        $(#[$meta])*
        $vis struct $ty;

        impl $crate::Plugin for $ty {
            fn name(&self) -> &str {
                $name
            }

            fn dependencies(&self) -> ::std::vec::Vec<::std::string::String> {
                // 两层重复：外层对应可选的 `deps: [...]`，内层对应其中每一项。
                ::std::vec![$( $( ::std::string::String::from($dep) ),* )?]
            }

            fn apply(
                &self,
                $ctx: &$crate::Context,
            ) -> ::std::result::Result<$crate::Effect, $crate::KernelError> {
                $apply
            }

            $(
                fn dispose(&self, $dctx: &$crate::Context) $dispose
            )?
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    define_plugin! {
        struct Bare,
        name: "bare",
        apply(_ctx) {
            Ok(Effect::none())
        }
    }

    define_plugin! {
        /// 有依赖也有 dispose 的插件。
        struct WithDeps,
        name: "with-deps",
        deps: ["bare", "other"],
        apply(_ctx) {
            Ok(Effect::none())
        },
        dispose(_ctx) {
            // 什么都不做，只验证宏能展开这段
        }
    }

    #[test]
    fn macro_generates_a_usable_plugin() {
        assert_eq!(Bare.name(), "bare");
        assert!(Bare.dependencies().is_empty());

        let ctx = Context::new();
        assert!(Bare.apply(&ctx).unwrap().is_empty());

        assert_eq!(WithDeps.name(), "with-deps");
        assert_eq!(WithDeps.dependencies(), ["bare", "other"]);
        WithDeps.dispose(&ctx);
    }
}
