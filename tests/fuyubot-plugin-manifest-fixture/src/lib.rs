//! 测试夹具：一个**只声明依赖、别的什么都不做**的插件。
//!
//! 它存在的唯一理由是证明两件事，而且必须是独立 crate 才证得出来：
//!
//! 1. `plugin_manifest` 里的 `version` 取的是**插件 crate** 的 `CARGO_PKG_VERSION`
//!    ——这里是 `2.3.4`，故意与 workspace 的版本不同。同一个 workspace 里的
//!    demo 插件版本号与 `fuyubot-abi` 一样，永远证明不了 `env!` 的求值点；
//! 2. 宿主在 `probe` 阶段（`plugin_start` 之前）就能读到非空的
//!    `inject` / `soft_inject`。
//!
//! ⚠️ 别把它改成「顺便做点事」的插件：`fuyubot-loader/tests/plugins.rs` 里的
//! 断言直接依赖这两个列表的**确切值**与这个版本号。

use fuyubot_abi::{HostApi, Plugin, define_plugin};

/// 空插件本身。
struct ManifestFixture;

impl Plugin for ManifestFixture {
    fn init(_host: HostApi) -> Result<Self, String> {
        Ok(ManifestFixture)
    }

    fn start(&mut self, _config_json: &str) -> Result<(), String> {
        Ok(())
    }

    /// 硬依赖：宿主找不到 `demo.dep` 就不该激活本插件。
    fn inject() -> Vec<String> {
        vec!["demo.dep".to_owned()]
    }

    /// 软依赖：缺了照常加载，有了优先排前面。
    fn soft_inject() -> Vec<String> {
        vec!["demo.soft".to_owned()]
    }
}

define_plugin!(ManifestFixture, "fuyubot.manifest-fixture");
