# fuyubot-plugin-manage

内置插件（cdylib）：`/plugins` 列出插件目录里装了哪些插件（按文件名，含 `.dll` / `.so` / `.dylib`）。

[![](https://img.shields.io/badge/powered_by-dsh-4D6BFE?style=flat-square&logo=deepseek&logoColor=white)](https://github.com/deepseek-ai/deepseek-harness)

- 插件自己算不出「插件目录」在哪——它跑在宿主进程里，`current_exe()` 拿到的是 `fuyubot.exe`。所以目录由宿主在 `plugin_start` 的配置里给：`{"plugins_dir": "<绝对路径>"}`，没给就退回 `plugins/`。
- 列的是**插件目录里的文件**（装了什么），不是「宿主进程里加载了哪些」——后者 ABI 查询不到。
- 回复前缀等可走 `[plugin.fuyubot.manage]` 配置段。

## 相关

- [插件开发](https://catmoecircle.github.io/FuyuBot-docs/docs/plugin/)
- [配置文件](https://catmoecircle.github.io/FuyuBot-docs/docs/usage/config)
- [仓库](https://github.com/CatMoeCircle/Fuyu-Bot)
