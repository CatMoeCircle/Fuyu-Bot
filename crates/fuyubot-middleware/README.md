# fuyubot-middleware

按阶段挂载的中间件管道，洋葱模型执行：`next.run()` 之前是进入段，之后是退出段。

[![](https://img.shields.io/badge/powered_by-dsh-4D6BFE?style=flat-square&logo=deepseek&logoColor=white)](https://github.com/deepseek-ai/deepseek-harness)

## 里面有什么

- **`Stage` 六个挂载点** —— `PreUpdate`（收到更新、还没解析）→ `PreRoute` → `InPlugin` → `PostResponse` → `PreToolCall` → `PostToolCall`，顺序即事件流经顺序。
- **`Pipeline`** —— `use_middleware(stage, …)` 挂载一层，返回的 `MiddlewareHandle` drop 即摘除；`run(stage, ctx)` 按注册顺序进入、逆序退出。
- **中断就是不调 `next.run()`** —— 中间件拿到 `Next`，不往下走即终止本次阶段。
- 层列表按挂载表版本号**缓存快照**，中间件里再挂载 / 摘除不会自锁；单层 panic 不会卡死整条管道。

`install(&ctx)` 返回 `Effect`（必须持有），扩展 trait 是 `MiddlewareContextExt`。

## 相关

- [架构与数据流](https://catmoecircle.github.io/FuyuBot-docs/docs/dev/architecture/)
- [仓库](https://github.com/CatMoeCircle/Fuyu-Bot)
