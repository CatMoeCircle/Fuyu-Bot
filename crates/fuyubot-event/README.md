# fuyubot-event

事件总线：按 `event_type` 订阅、按字段过滤，`emit` 时顺序调用匹配的回调。

[![](https://img.shields.io/badge/powered_by-dsh-4D6BFE?style=flat-square&logo=deepseek&logoColor=white)](https://github.com/deepseek-ai/deepseek-harness)

## 里面有什么

- **`EventBus`** —— `on` / `on_scoped` 订阅，`emit` / `emit_scoped` 派发；回调按注册顺序同步调用。
- **`Filter`** —— 按负载字段匹配（`field(path, expected)`），`event_type` 只负责选通道、不参与负载匹配。
- **scope 是「额外定向」** —— `emit` 发所有人，`emit_scoped` 发「全局 + 该归属」；`SubscriptionHandle` 可 `off()` 退订。
- **`install(&ctx)`** —— 返回 `Effect`，必须持有；扩展 trait 是 `EventContextExt`。

更新走**双通道**：具体 `@type` 与兜底 `"update"`，两条都发，订阅哪条都行。

## 相关

- [架构与数据流](https://catmoecircle.github.io/FuyuBot-docs/docs/dev/architecture/)
- [仓库](https://github.com/CatMoeCircle/Fuyu-Bot)
