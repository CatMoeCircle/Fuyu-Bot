# fuyubot

**基于 TDLib 的 Telegram bot / user 框架**：Rust 内核 + C ABI 插件。

- **插件就是动态库**（`.dll` / `.so` / `.dylib`），Rust / C / C++ / Python / JS / TypeScript 都能写。
- **下载即用**：Release 包里可执行文件、TDLib（`tdjson`）和内置插件都是齐的，不用装 Rust。
- **bot 与 user 两种账号**都支持；登录一次，session 由 TDLib 存进 `data/`，之后不用再登。
- **离线也能验插件**：`--replay` 把一批更新灌进管线，不需要 Telegram 账号、不联网。

## 下载

到 [Releases](https://github.com/CatMoeCircle/Fuyu-Bot/releases) 下对应平台的 zip：

| 系统 | 资产 |
| --- | --- |
| Windows 10/11（x64） | `fuyubot-windows-x86_64.zip` |
| Windows 11（ARM64） | `fuyubot-windows-aarch64.zip` |
| Linux（x86_64） | `fuyubot-linux-x86_64.zip` |
| Linux（ARM64） | `fuyubot-linux-aarch64.zip` |
| macOS（Apple Silicon） | `fuyubot-macos-aarch64.zip` |

解压后目录长这样——`tdjson` 必须和可执行文件待在一起，别单独删掉：

```
fuyubot-windows-x86_64/
  fuyubot.exe
  tdjson.dll
  plugins/          # 内置插件：/start、/help、/plugins
  README.txt
```

## 快速开始

```powershell
.\fuyubot.exe init --dir demo --name mybot --api-id 12345
```

```
已安装 3 个内置插件到 demo\plugins
已生成 demo\fuyubot.toml
插件目录：demo\plugins
```

`api_id` 到 <https://my.telegram.org> 申请，它**不是**秘密，写进配置没问题。
`api_hash` 是凭据，**不写配置文件**——用环境变量 `TG_API_HASH`，或者跑起来后按提示输入。

```powershell
$env:TG_API_HASH = "…"
.\fuyubot.exe run --config demo\fuyubot.toml
```

第一次跑会走登录：bot 用 Bot Token（`TG_BOT_TOKEN` 或提示输入），user 用手机号 + 验证码（或扫码）。
登录态由 TDLib 自己落进 `data/`，下次启动直接进。要停就 `fuyubot stop`，它会把插件和内核都收好再退出。

## 常用命令

| 命令 | 作用 |
| --- | --- |
| `fuyubot init --dir <目录> --name <名字> --api-id <id>` | 建项目：生成 `fuyubot.toml`、`plugins/`，并复制内置插件 |
| `fuyubot run --config <目录>/fuyubot.toml` | 前台跑（含登录） |
| `fuyubot run … --background` | 后台跑，日志进 `<项目>/logs/fuyubot.log` |
| `fuyubot status --config …` / `fuyubot stop --config …` | 看状态 / 优雅停止 |
| `fuyubot config --config …` | 打印**生效**配置（默认值 + 实际读到的文件） |
| `fuyubot run --replay <json> --plugin-dir <目录>` | 离线回放：不接 TDLib、不用账号 |
| `fuyubot plugin list --plugin-dir <目录>` | 列出插件目录里能加载的插件 |
| `fuyubot plugin install <工程目录或产物> --plugin-dir <目录>` | 把编好的插件复制进插件目录 |

一个目录只能跑一个实例：第二次 `run` 会被拒，报错里带正在跑的那个 PID。
`--no-login` 只接 TDLib、不走登录，适合「先让它起来」的场景；`--no-tdlib` 则完全不接 TDLib。

## 配置

`fuyubot init` 生成的 `fuyubot.toml`（所有键都有默认值，缺文件时 `run` 会自动生成一份）：

```toml
[bot]
name = "mybot"

[telegram]
account_type = "bot"   # bot：Bot Token；user：手机号 / 扫码。写错会报错，不会静默当 bot
api_id = 12345         # 不是秘密
data_dir = "data"      # TDLib 的 database 与 files 都放这儿

[plugins]
dir = "plugins"

[tdlib]
path = ""              # 留空 = 自动找 tdjson（环境变量 → 自带 → exe 同级 → 系统库 → 裸库名）
log_verbosity = 1      # 默认只留警告与错误；排查问题再调大
```

**凭据规则**：`api_hash`、bot token、手机号、验证码**只**来自环境变量
（`TG_API_ID` / `TG_API_HASH` / `TG_BOT_TOKEN`）或交互式输入，配置文件里写了也会被忽略。

## 插件

内置三个：`/start`、`/help`、`/plugins`。自己的插件放进 `plugins/` 即可，一个动态库就是一个插件；
用 Python / JS / TS 写的话，脚本要和那个动态库**同目录**。

写插件用 **[`fuyubot-sdk`](https://github.com/CatMoeCircle/FuyuBot-SDK)**（在 SDK 仓库）：

```bash
fuyubot-sdk init <名字> --lang <rust|c|cpp|python|js> --dir <项目目录>
fuyubot-sdk codegen --lang <…> --dir <工程目录>   # 刷新 C 头 / .pyi / .d.ts
fuyubot-sdk check <产物路径>                      # 静态检查（不加载、不运行）
```

编是插件作者自己的工具链的事（`cargo build`、`cl /LD`、`gcc -shared`…），
`fuyubot` **不编译插件**，`plugin install` 只负责把编好的东西复制进插件目录：

```bash
fuyubot plugin install <插件工程目录> --plugin-dir <插件目录>
```

各语言的模板、ABI 与调试方法见[文档站的「插件开发」](https://catmoecircle.github.io/FuyuBot-docs/docs/plugin/)，示例工程在
[SDK 仓库的 `examples/`](https://github.com/CatMoeCircle/FuyuBot-SDK/tree/main/examples)。

## 文档

| 去哪 | 看什么 |
| --- | --- |
| [FuyuBot-Docs](https://github.com/CatMoeCircle/FuyuBot-Docs) | **使用文档站**：安装、配置、运行、回放、排错 |
| [FuyuBot-SDK](https://github.com/CatMoeCircle/FuyuBot-SDK) | 插件 SDK、`fuyubot-sdk` CLI、五种语言示例 |
| [架构与数据流](https://catmoecircle.github.io/FuyuBot-docs/docs/dev/architecture/) | 内核、更新管线、绑定层与数据流 |
| [调用 TDLib](https://catmoecircle.github.io/FuyuBot-docs/docs/plugin/tdlib/) | `td_invoke`、响应通道、`fuyubot-simplify` 全量对照 |

本仓只放运行时；插件 SDK 在 FuyuBot-SDK，文档站在 FuyuBot-Docs。

## 从源码构建

Rust 1.99+（Edition 2024）；Windows 上还要 MSVC 工具链。构建期会自动准备 `td_api.tl` 与 TDLib 动态库，
第一次会联网下载一次，之后走缓存。

```bash
cargo build --workspace          # 产物在 target/debug/
cargo build --release -p fuyubot-cli
```

改代码前先看[开发文档](https://catmoecircle.github.io/FuyuBot-docs/docs/dev/)：结构、构建与测试、发布与排错。
