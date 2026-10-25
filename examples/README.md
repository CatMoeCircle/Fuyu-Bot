# examples：回放用的更新 JSON

这个目录只剩**回放数据**。五种语言的示例插件工程搬到了
[FuyuBot-SDK 的 `examples/`](https://github.com/CatMoeCircle/FuyuBot-SDK/tree/main/examples)
（它们依赖 SDK crate，跟运行时仓库分开）。

| 文件 | 内容 | 打给谁 |
|---|---|---|
| `replay/new_message.json` | 3 条 `updateNewMessage`：`/ping`、一句中文、一条 `is_outgoing` | echo 只回前 2 条、跳过第 3 条；c-ping 只命中第 1 条 |
| `replay/weather.json` | 1 条 `/weather 上海` | python-weather（**要联网**访问 wttr.in） |
| `replay/counter.json` | 3 条 `updateNewMessage`，分属两个聊天（A、A、B） | ts-counter → 三条 `sendMessage`，B 的计数独立 |
| `replay/builtin_commands.json` | `/start`、`/help`、`/plugins` | 三个内置插件（随 `fuyubot init` 装进项目） |

## 跑回放

```powershell
.\fuyubot.exe run --replay examples\replay\builtin_commands.json --plugin-dir <项目>\plugins
```

内置插件的离线自检（不需要账号）：

```powershell
.\target\debug\fuyubot.exe init --dir <项目> --name demo --api-id 1
.\target\debug\fuyubot.exe run --replay examples\replay\builtin_commands.json --plugin-dir <项目>\plugins
```

插件示例怎么编、怎么装、怎么回放，见
[FuyuBot-SDK 的 examples/README.md](https://github.com/CatMoeCircle/FuyuBot-SDK/blob/main/examples/README.md)。
