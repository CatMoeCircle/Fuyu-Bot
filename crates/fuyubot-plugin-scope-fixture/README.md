# fuyubot-plugin-scope-fixture

测试夹具：在 `plugin_start` 里注册一个私有服务、立刻取回来，再确认根上的公共服务沿父链仍然可见。只服务 `fuyubot-loader` 的服务作用域集成测试。

[![](https://img.shields.io/badge/powered_by-dsh-4D6BFE?style=flat-square&logo=deepseek&logoColor=white)](https://github.com/deepseek-ai/deepseek-harness)

- 验证 dylib 插件的 `Context` 子作用域：插件的 `provide_service` 落**自己那层**，`get_service` 从自己那层沿父链查到根上的公共服务。
- `publish = false`，不发 crates.io，别在别处依赖它。

## 相关

- [仓库](https://github.com/CatMoeCircle/Fuyu-Bot)
