# fuyubot-plugin-manage

内置插件（cdylib）：`/plugins` 列出宿主进程里**真在跑**的插件，外加已禁用的。

[![](https://img.shields.io/badge/powered_by-dsh-4D6BFE?style=flat-square&logo=deepseek&logoColor=white)](https://github.com/deepseek-ai/deepseek-harness)

- 每行是 `名称（id）— 介绍`：展示名 / 介绍来自插件自报的 `plugin_manifest`（`display_name` / `description`），脚本插件等没有 manifest 的再从 `plugins/scripts/<id>/plugin.json` 补；老插件没有展示名就只写 id。
- 运行状态问宿主预置的 `fuyubot.host.plugins` service（进程里的真实四态：active / pending / failed / activating），读不到才退回目录清单。
- 已禁用的单独列一段：`plugins/.disabled/<id>/`、`<id>.disable/` 与 `<stem>.dll.disable`。禁用项**不 dlopen**，名字能读清单就读。
- 插件自己算不出「插件目录」在哪——它跑在宿主进程里，`current_exe()` 拿到的是 `fuyubot.exe`。所以目录由宿主在 `plugin_start` 的配置里给：`{"plugins_dir": "<绝对路径>"}`，没给就退回 `plugins/`。
- 回复前缀等可走 `[plugin.fuyubot.manage]` 配置段。

## 相关

- [插件开发](https://catmoecircle.github.io/FuyuBot-docs/docs/plugin/)
- [配置文件](https://catmoecircle.github.io/FuyuBot-docs/docs/usage/config)
- [仓库](https://github.com/CatMoeCircle/Fuyu-Bot)
