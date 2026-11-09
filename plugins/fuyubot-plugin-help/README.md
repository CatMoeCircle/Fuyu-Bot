# fuyubot-plugin-help

内置插件（cdylib）：`/help` 列出可用命令。

[![](https://img.shields.io/badge/powered_by-dsh-4D6BFE?style=flat-square&logo=deepseek&logoColor=white)](https://github.com/deepseek-ai/deepseek-harness)

- 命令表是**动态**的：任何插件都能在 `start()` 里经进程级共享服务
  `fuyubot.help.commands` 的 `register` 方法，把自己的命令登记进 `/help`，
  不必改这个插件半行代码：
  ```rust
  host.call_service(
      "fuyubot.help.commands",
      "register",
      r#"{"name":"mycmd","description":"我的命令"}"#,
  )?;
  ```
  想让宿主在跑你的 `start()` 前先把该服务准备好，在插件里声明软依赖即可：
  ```rust
  fn soft_inject() -> Vec<String> {
      vec!["fuyubot.help.commands".to_owned()]
  }
  ```
- **归属由宿主判定**：`register` 会回问 `HostApi::current_plugin()`（宿主按本线程
  作用域栈顶取调用者，与 `set_config` 的越权判定同一份映射），成功返回
  `{"ok":true,"plugin":"fuyubot.start"}`；问不到身份时 `plugin` 是 `null`。
  插件没法把自己冒充成别人。
- `list` 方法返回合并后的命令 JSON 数组
  （`[{"name":…,"description":…,"plugin":"…"}, …]`），**扁平**——这是给程序读的协议；
  给 `list` 加分组会破坏调用方，所以分组只发生在 `/help` 的正文里。
- `/help` 正文**按归属插件分组**：一个插件一段，组标题优先用宿主报告里的展示名
  （`plugin_manifest` / `plugin.json` 的 `display_name`），缺了退回短名：
  ```
  *可用命令*

  *帮助（help）*
  /help — 这条帮助

  *插件管理（manage）*
  /plugins — 列出已安装和已禁用的插件
  ```
- 静态兜底 `BUILTIN_COMMANDS` 只兜住三个内置命令（`/start`、`/help`、`/plugins`），
  其余命令由各插件自行为自己注册；渲染时**动态优先**、静态只补缺（基线没有来源，
  所以补位的那条收在最后的「其它」段，不去编造归属）。
- 产物随可执行文件一起打包，`fuyubot init` / `run` 会把它复制进项目的 `plugins/`。

## 相关

- [插件开发](https://catmoecircle.github.io/FuyuBot-docs/docs/plugin/)
- [仓库](https://github.com/CatMoeCircle/Fuyu-Bot)
