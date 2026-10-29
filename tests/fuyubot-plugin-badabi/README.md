# fuyubot-plugin-badabi

手写 C ABI 导出的插件样例（不用 `define_plugin!`）：故意返回不兼容的 ABI 版本，用来验证加载器会拒绝它。

[![](https://img.shields.io/badge/powered_by-dsh-4D6BFE?style=flat-square&logo=deepseek&logoColor=white)](https://github.com/deepseek-ai/deepseek-harness)

## 用途

- 证明 ABI 版本协商是**双向**的：插件声明的 `fuyubot_abi_version` 对不上，宿主在 `load` 阶段就返回 `AbiMismatch`，`HostApiVTable` 一律不碰。
- 手写导出（裸 `#[unsafe(no_mangle)] extern "C"`）也是一条合法的插件实现路径——宏只是便利，不是门槛。

仅供 `fuyubot-loader` 的拒绝路径测试使用，别拿它当模板抄。

## 相关

- [插件开发](https://catmoecircle.github.io/FuyuBot-docs/docs/plugin/)
- [仓库](https://github.com/CatMoeCircle/Fuyu-Bot)
