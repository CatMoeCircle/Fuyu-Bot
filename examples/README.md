# examples：示例插件与回放数据

本目录只放运行时仓库自己的示例；**各语言插件示例（Rust / C / Go / Python / TS）统一在
[文档站「插件开发」](https://catmoecircle.github.io/FuyuBot-docs/docs/plugin/)**。

| 路径 | 内容 |
|---|---|
| `fuyubot-plugin-demo/` | 最小 Rust 插件示例（`define_plugin!` 的直接抄写模板），同时是 `fuyubot-loader` 端到端测试的夹具 |
| `replay/` | 回放用的更新 JSON，见下表 |

## 回放数据

| 文件 | 内容 | 打给谁 |
|---|---|---|
| `replay/new_message.json` | 3 条 `updateNewMessage`：`/ping`、一句中文、一条 `is_outgoing` | echo 只回前 2 条、跳过第 3 条；c-ping 只命中第 1 条 |
| `replay/weather.json` | 1 条 `/weather 上海` | 文档站的 Python 示例（**要联网**访问 wttr.in） |
| `replay/counter.json` | 3 条 `updateNewMessage`，分属两个聊天（A、A、B） | 文档站的 TS 示例 |
| `replay/builtin_commands.json` | `/start`、`/help`、`/plugins` | 三个内置插件（随 `fuyubot init` 装进项目）；`/plugins` 只给主人，配合 `replay/fuyubot.toml` 的 `[roles].owners = [42]` |

## 跑回放

```powershell
.\fuyubot.exe run --config examples\replay\fuyubot.toml `
  --replay examples\replay\builtin_commands.json --plugin-dir <项目>\plugins
```

内置插件的离线自检（不需要账号）：

```powershell
.\target\debug\fuyubot.exe init --dir <项目> --name demo --api-id 1
.\target\debug\fuyubot.exe run --config examples\replay\fuyubot.toml `
  --replay examples\replay\builtin_commands.json --plugin-dir <项目>\plugins
```

`/plugins` 登记为 `role = "owner"`：`replay/fuyubot.toml` 把 `42` 写成主人，
回放里那条 `/plugins` 带了 `sender_id = 42`。不要这个配置时它会被角色闸门拦下
（这是预期行为，不是回放坏了）。

插件示例怎么编、怎么装、怎么回放，见
[文档站「插件开发」](https://catmoecircle.github.io/FuyuBot-docs/docs/plugin/)。
