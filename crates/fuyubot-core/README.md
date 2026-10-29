# fuyubot-core

Cordis 内核：管理插件生命周期、依赖解析，并提供跨模块共享的 `Context`。

[![](https://img.shields.io/badge/powered_by-dsh-4D6BFE?style=flat-square&logo=deepseek&logoColor=white)](https://github.com/deepseek-ai/deepseek-harness)

## 里面有什么

- **`Context`** —— 按**名字**寻址的服务表，带作用域链：`Context::new()` 永远造根，子作用域显式 `child()`；查找沿父链走、注册只写自己那层，`isolate` 可把某个名字从查找链里摘掉。整个接口只暴露 `&self`。
- **`Effect`** —— 每次注册返回的守卫，`#[must_use]`：存住才留效，drop / rollback 立刻撤销。
- **`Plugin` / `PluginManager`** —— 插件 Fiber 的生命周期与依赖跟踪：硬/软依赖的激活顺序、`apply` 期间的防递归推进、失败回滚。

本 crate 不认识 Telegram，也不加载任何动态库；四个机制 crate（`fuyubot-event` / `fuyubot-service` / `fuyubot-tool` / `fuyubot-middleware`）都只依赖它。

## 相关

- [架构与数据流](https://catmoecircle.github.io/FuyuBot-docs/docs/dev/architecture/)
- [仓库](https://github.com/CatMoeCircle/Fuyu-Bot)
