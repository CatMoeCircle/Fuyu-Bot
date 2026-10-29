# fuyubot-cli

命令行入口（二进制 `fuyubot`）：建项目、启动内核、加载插件目录里的动态库，然后跑 TDLib 接收循环。

[![](https://img.shields.io/badge/powered_by-dsh-4D6BFE?style=flat-square&logo=deepseek&logoColor=white)](https://github.com/deepseek-ai/deepseek-harness)

## 子命令

| 命令 | 作用 |
| --- | --- |
| `init` | 建项目：生成 `fuyubot.toml`、`plugins/`，复制内置插件 |
| `run` | 前台 / 后台运行（含登录）；`--replay` 离线回放不接 TDLib |
| `login` / `logout` | 只登录不跑框架 / 清本地 TDLib 会话目录 |
| `status` / `stop` | 看实例在不在跑 / 优雅停止（`.fuyubot/` 三个状态文件） |
| `config` | 打印**生效**配置（默认值 + 实际读到的文件） |
| `plugin list` / `plugin install` | 列出 / 复制插件产物——本 CLI **不编译插件** |

## 约定

- 一个目录只能跑一个实例：`.fuyubot/lock` 独占，第二次 `run` 会被拒并报出正在跑的 PID。
- 凭据（bot token / 手机号 / 两步验证密码）只走环境变量或交互输入，**不落盘**；`api_id` / `api_hash` 例外，交互问过会写回 `fuyubot.toml`。
- `scaffold` 的模板与 FuyuBot-SDK 只有一份来源（`include_str!`），不在这里复制第二份。
- `clap` 只允许出现在本 crate 与 SDK CLI。

## 相关

- [使用文档](https://catmoecircle.github.io/FuyuBot-docs/docs/usage/config)
- [仓库](https://github.com/CatMoeCircle/Fuyu-Bot)
