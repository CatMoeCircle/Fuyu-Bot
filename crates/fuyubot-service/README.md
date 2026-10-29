# fuyubot-service

命名服务的**兼容层**：真正的那张表在 `fuyubot-core::Context` 里（按名字寻址）。

[![](https://img.shields.io/badge/powered_by-dsh-4D6BFE?style=flat-square&logo=deepseek&logoColor=white)](https://github.com/deepseek-ai/deepseek-harness)

## 现状

这里只剩两样东西：

- **`ServiceHandle`** —— 带名字的注册守卫，`fuyubot-loader` 的回滚链要用它；
- **`install(&ctx)`** —— 现在是 no-op，让老调用点不用改。

新代码请直接用 `Context::provide_named` / `Context::resolve`（`Context` 的固有方法）。
本 crate 的扩展 trait 叫 `provide_service` / `get_service`——与固有方法重名是禁止的，
固有方法永远优先；服务缺失时报错会带「该怎么 install」。

## 相关

- [架构与数据流](https://catmoecircle.github.io/FuyuBot-docs/docs/dev/architecture/)
- [仓库](https://github.com/CatMoeCircle/Fuyu-Bot)
