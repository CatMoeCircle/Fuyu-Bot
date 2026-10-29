# fuyubot-plugin-demo

最小 Rust 插件示例：实现 `fuyubot_abi::Plugin`，用 `define_plugin!` 导出 C ABI，是插件作者的抄写模板。

[![](https://img.shields.io/badge/powered_by-dsh-4D6BFE?style=flat-square&logo=deepseek&logoColor=white)](https://github.com/deepseek-ai/deepseek-harness)

## 它演示什么

1. `init` / `start` / `stop` 各经 VTable 打一条日志（`log_lines()` 可断言）；
2. 注册一个工具声明（`demo.echo`）；
3. 订阅 `updateNewMessage`，把收到的整条更新打日志；
4. 发一条 `getMe` 并订阅它自己的响应，收到后打日志——走一遍「VTable → 更新管线 → 响应」的完整链路。

回调没有捕获环境，状态靠 `user_data` 传（本例是 `Box::into_raw` / `stop` 时收回）。
产物 `fuyubot_plugin_demo.{dll,so,dylib}` 会被 `fuyubot-loader` 的集成测试真的 dlopen 起来，所以这份代码不是摆设。

## 相关

- [插件开发](https://catmoecircle.github.io/FuyuBot-docs/docs/plugin/)
- [SDK 五种语言示例](https://github.com/CatMoeCircle/FuyuBot-SDK/tree/main/examples)
- [仓库](https://github.com/CatMoeCircle/Fuyu-Bot)
