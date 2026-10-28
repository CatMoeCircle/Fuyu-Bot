# fuyubot-service

命名服务的**兼容层**：真正的那张表在 `fuyubot-core::Context` 里（按名字寻址）。

这里只剩 `ServiceHandle`（带名字的注册守卫，`fuyubot-loader` 的回滚链要用它）
与 `install(&ctx)`（现在是 no-op，让老调用点不用改）。
新代码请直接用 `Context::provide_named` / `Context::resolve`。
