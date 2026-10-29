# fuyubot-plugin-start

内置插件（cdylib）：`/start` 回一句欢迎语（Markdown 交给 TDLib 解析）。

[![](https://img.shields.io/badge/powered_by-dsh-4D6BFE?style=flat-square&logo=deepseek&logoColor=white)](https://github.com/deepseek-ai/deepseek-harness)

- 写法就是「插件作者从这儿抄」的最小样本：`fuyubot_simplify::on_command` 订阅 `updateNewMessage`，命中 `/start` 就把回复指向 `reply_to` 发回同一个会话。
- 欢迎语用 TDLib 方言的 Markdown（粗体是 `*x*`，不是 `**x**`）。
- 产物随可执行文件一起打包，`fuyubot init` / `run` 会把它复制进项目的 `plugins/`。
- 异步发送的线程句柄在 `stop()` 里 join——卸载后线程还跑库里的代码是 UB。

## 相关

- [插件开发](https://catmoecircle.github.io/FuyuBot-docs/docs/plugin/)
- [仓库](https://github.com/CatMoeCircle/Fuyu-Bot)
