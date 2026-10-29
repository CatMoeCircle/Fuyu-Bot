# fuyubot-plugin-manifest-fixture

测试夹具：一个声明了硬/软依赖、别的什么都不做的空插件，只服务 `fuyubot-loader` 的 `plugin_manifest` 端到端测试。

[![](https://img.shields.io/badge/powered_by-dsh-4D6BFE?style=flat-square&logo=deepseek&logoColor=white)](https://github.com/deepseek-ai/deepseek-harness)

- 它的版本号 `2.3.4` 故意与 workspace 不同，用来证明 manifest 里的 `version` 取自插件 crate 的 `CARGO_PKG_VERSION` 而不是 `fuyubot-abi` 的。
- `inject` / `soft_inject` 声明走 manifest，不读 `[plugin.<名>]` 配置——覆盖而非合并、逐侧判定。
- `publish = false`，不发 crates.io，别在别处依赖它。

## 相关

- [仓库](https://github.com/CatMoeCircle/Fuyu-Bot)
