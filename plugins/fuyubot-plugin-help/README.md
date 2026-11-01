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
- `list` 方法返回合并后的命令 JSON 数组（`[{"name":…,"description":…}, …]`），
  即 `/help` 实际渲染的内容。
- 静态兜底 `BUILTIN_COMMANDS` 只兜住三个内置命令（`/start`、`/help`、`/plugins`），
  其余命令由各插件自行为自己注册；渲染时静态与动态合并、**按名字去重**。
- 产物随可执行文件一起打包，`fuyubot init` / `run` 会把它复制进项目的 `plugins/`。

## 相关

- [插件开发](https://catmoecircle.github.io/FuyuBot-docs/docs/plugin/)
- [仓库](https://github.com/CatMoeCircle/Fuyu-Bot)
