# fuyubot-plugin-help

内置插件（cdylib）：`/help` 从**宿主**的命令能力表渲染可用命令。

[![](https://img.shields.io/badge/powered_by-dsh-4D6BFE?style=flat-square&logo=deepseek&logoColor=white)](https://github.com/deepseek-ai/deepseek-harness)

- 命令表的主人是宿主，不是这个插件。任何插件都能在 `start()` 里经
  `fuyubot.host.commands` 的 `register` 方法登记，不必改 help 插件半行代码：
  ```rust
  host.call_service(
      "fuyubot.host.commands",
      "register",
      r#"{"name":"mycmd","description":"我的命令","role":"admin","scopes":["private"]}"#,
  )?;
  ```
  服务由宿主在启动时预置，**不再需要** `soft_inject` 等 help 先就位。
  旧名字 `fuyubot.help.commands` 仍指向同一张表（保留一个版本）。
- **归属由宿主判定**：`register` 会回问 `HostApi::current_plugin()`（宿主按调用者栈取，
  与 `set_config` 的越权判定同一份映射），成功返回
  `{"ok":true,"name":"mycmd","plugin":"fuyubot.start"}`。同名命令由别的插件注册会被拒绝。
- 元数据字段：
  - `role`：`user`（默认）/ `chat_admin`（本会话管理员）/ `admin`（全局管理员）/
    `owner`（主人）；
  - `scopes`：`private` / `group` / `channel`，默认 `private + group`
    （`admin` / `owner` 默认只有 `private`，要群里用必须显式写）；
  - `hidden`：不进 `/help`，但仍按能力表执行。
- `/help` 只读宿主的 `list`，并在**回复线程**里按「全局角色 × 会话类型」过滤：
  群管理员要问 `getChatMember`，不能在接收循环上等 TDLib。群管理员只在本群看到
  `chat_admin` 命令；全局 `admin` / `owner` 命令默认只出现在私聊。
- 正文**按归属插件分组**：一个插件一段，组标题优先用宿主报告里的展示名，缺了退回短名：
  ```
  *可用命令*

  *帮助（help）*
  /help — 这条帮助

  *插件管理（manage）*
  /plugins — 列出已安装和已禁用的插件
  ```
- 插件卸载 / 加载失败时，宿主按 owner 把它登记的命令整批摘掉；脚本插件按子作用域摘。
- 产物随可执行文件一起打包，`fuyubot init` / `run` 会把它复制进项目的 `plugins/`。

## 相关

- [插件开发](https://catmoecircle.github.io/FuyuBot-docs/docs/plugin/)
- [仓库](https://github.com/CatMoeCircle/Fuyu-Bot)
