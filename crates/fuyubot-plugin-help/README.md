# fuyubot-plugin-help

内置插件（cdylib）：`/help` 列出可用命令。

[![](https://img.shields.io/badge/powered_by-dsh-4D6BFE?style=flat-square&logo=deepseek&logoColor=white)](https://github.com/deepseek-ai/deepseek-harness)

- 命令表是**静态**的：`fuyubot-abi` 没有「别的插件登记了自己的命令」这种能力，所以装了自己的命令就照 `COMMANDS` 常量加一行——它也是这个插件里唯一需要改的地方。
- 顺序即显示顺序，一行一条「命令 + 一行说明」。
- 产物随可执行文件一起打包，`fuyubot init` / `run` 会把它复制进项目的 `plugins/`。

## 相关

- [插件开发](https://catmoecircle.github.io/FuyuBot-docs/docs/plugin/)
- [仓库](https://github.com/CatMoeCircle/Fuyu-Bot)
