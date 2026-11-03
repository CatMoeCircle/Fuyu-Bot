# fuyubot

**基于 TDLib 的 Telegram bot / user 框架**：Rust 内核 + C ABI 插件。

[![](https://img.shields.io/badge/powered_by-dsh-4D6BFE?style=flat-square&logo=deepseek&logoColor=white)](https://github.com/deepseek-ai/deepseek-harness)

- **插件就是动态库**（`.dll` / `.so` / `.dylib`），Rust / C / C++ / Python / JS / TypeScript 都能写。
- **下载即用**：Release 包里可执行文件、TDLib（`tdjson`）和内置插件都是齐的，不用装 Rust。
- **bot 与 user 两种账号**都支持；登录一次，session 由 TDLib 存进 `data/`，之后不用再登。
- **离线也能验插件**：`--replay` 把一批更新灌进管线，不需要 Telegram 账号、不联网。

## 下载

到 [Releases](https://github.com/CatMoeCircle/Fuyu-Bot/releases) 下对应平台的 zip：

| 系统 | 资产 |
| --- | --- |
| Windows 10/11（x64） | `fuyubot-<版本>-windows-x86_64.zip` |
| Windows 11（ARM64） | `fuyubot-<版本>-windows-aarch64.zip` |
| Linux（x86_64） | `fuyubot-<版本>-linux-x86_64.zip` |
| Linux（ARM64） | `fuyubot-<版本>-linux-aarch64.zip` |
| macOS（Apple Silicon） | `fuyubot-<版本>-macos-aarch64.zip` |

`<版本>` 就是 Release 的版本号（如 `v0.1.0`），资产名里带它，方便一眼看出下的是哪个版本。

**解压出来就是本体**，zip 里没有额外的一层目录——建议先建一个空目录再往里解压。
`tdjson` 必须和可执行文件待在一起，别单独删掉：

```
<你解压到的目录>/
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
2026-10-31 07:05:13 cli INFO  已安装 3 个内置插件到 demo\plugins
2026-10-31 07:05:13 cli INFO  已生成 demo\fuyubot.toml
2026-10-31 07:05:13 cli INFO  插件目录：demo\plugins
2026-10-31 07:05:13 cli INFO  下一步：装 fuyubot-script 插件，或按文档站示例自己写插件（plugins/ 或 plugins/scripts/）
```

命令行输出**全部**走 `tracing`：命令结果、运行期诊断、致命错误都是同一份
`时间 来源 级别 内容` 格式（插件是 `[plugins - 名字]`、TDLib 更新是 `TDLib Update`，各带颜色；日期灰、时间亮蓝）。

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
| `fuyubot run … --background` | 后台跑，日志进 `<项目>/logs/fuyubot.log`（每次启动先清空旧日志） |
| `fuyubot run --tui`（短写 `-t`） | 终端操作面板：顶部版本栏 + 导航/工作区 + 底部资源占用，`s` 启动 / `x` 停止 / 插件页 `Space` 开关插件 |
| `fuyubot status --config …` / `fuyubot stop --config …` | 看状态 / 优雅停止 |
| `fuyubot config --config …` | 打印**生效**配置（默认值 + 实际读到的文件） |
| `fuyubot run --replay <json> --plugin-dir <目录>` | 离线回放：不接 TDLib、不用账号 |
| `fuyubot plugin list --plugin-dir <目录>` | 列出插件目录里能加载的插件 |
| `fuyubot plugin install <工程目录或产物> --plugin-dir <目录>` | 把编好的插件复制进插件目录 |

一个目录只能跑一个实例：第二次 `run` 会被拒，报错里带正在跑的那个 PID。
`--no-login` 只接 TDLib、不走登录，适合「先让它起来」的场景；`--no-tdlib` 则完全不接 TDLib。

`run --tui` 打开的是**操作面板**而不是前台 bot：面板自己不接 TDLib、不加载插件，只读
`.fuyubot/status`、配置、插件目录与日志；按 `s` 会派生 `run --background`，按 `x` 走
与 `fuyubot stop` 相同的优雅停止。退出面板（`q`）**不会**停掉已经起来的实例。
`Tab` 在**导航 / 工作区**之间切换焦点（当前窗口边框高亮），`Enter` 进入工作区、`Esc` 返回导航。
焦点在导航时 `↑↓` / `1–5` 换页；焦点在工作区时 `↑↓` / `PgUp` / `PgDn` 滚动日志或选择配置项。
`r` 刷新，`?` 看全部按键。
插件页（工作区焦点）按 `Space` 启用 / 禁用选中的 dylib：**禁用就是改名**（`foo.dll` →
`foo.dll.disable`），加载器只认扩展名正好是 `dll` 的文件，所以下次 `run` 不会再加载它；
再按一次改回来。改完重启实例才生效（运行中的实例已经把库映射进进程了）。
日志页（工作区焦点）按 `l` 调整运行中实例的插件日志级别（写 `.fuyubot/log-level`，实例下一圈生效；
只对本次运行有效，重启回到 `[plugins].log_level`）；按 `F` 按**级别**和**来源**（插件 / 宿主 / 管道 /
更新 / TDLib / 其他）过滤日志，`f` 开关跟随末尾。TDLib Update 行只显示 `@type`；
日志字段不按固定列补空格，长行按显示宽度折行（续行缩进两格）。
日志页**不显示行数**（窗口里装多少行随滚动变）：首屏只读文件尾部（最多 800 行），
往回翻到头会自动**分段回读**更早的日志——一次 256KB，窗口最多留 2 万行 / 8MB，
超过就从另一头丢（滑动窗口），内存不会随翻页增长；底栏的 `↑ 更早` 说明还能往回翻。
翻在中间时窗口**冻结**（实例继续写也不往窗口里塞），按 `End`（或 `f` 打开跟随）
重读尾部、回到最新。
日志页按 `Enter` 进入选择模式，`↑↓` 选行，`y` / `Enter` 复制选中行（TDLib Update 复制**完整 JSON**）。
`c` 清空当前日志文件（要确认）；后台实例每次新启动也会先清空旧日志。
窄终端（如 Termux 40 列）会自动收缩：宽度 < 60 列时导航移到工作区上方，变成一行
横向标签，用 `←` / `→` 或 `h` / `l`（导航焦点）切换；更窄时标签退化成单字。
底栏在窄屏只留 `CPU / 内存 / 级别`，不显示运行时长、插件数量和 PID；配置项在窄屏
改成「完整键 + 值」两行显示，帮助页可以 `↑↓` / `PgUp` / `PgDn` 滚动；日志窄屏只留
`HH:MM:SS`、插件名最后一段和级别，消息照常折行。复制优先用
`termux-clipboard-set`（装 `termux-api` 后可用），否则退回 OSC 52。
配置页（工作区焦点）用 `↑↓` 选中一行，`Enter` / `e` 编辑单个配置项：文本 / 整数 / 枚举先校验，
写回时保留注释与 CRLF；运行中改完**重启生效**。`api_hash`、凭据和 `[plugin.*]` 段只读。
界面顶部是版本栏（`[fuyubot - v…]`，右侧**始终**显示实例状态：运行中 / 上次已停止 / 上次异常退出 /
未运行；正常停止会带上停止时间，崩溃或硬杀则标成异常退出），底部两行分别是**当前可用操作**与
**CPU · 内存 · 运行时长 · 插件数 · 日志级别 · PID**（取的是运行中实例的进程）。

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

### 插件自己的配置段

每个插件可以在 `[plugin.<插件名>]` 下放自己的配置（键值任意、可嵌套）。插件名是点分的，
所以 `[plugin.fuyubot.manage]` 就是插件 `fuyubot.manage` 那段：

```toml
[plugin.fuyubot.manage]
reply_prefix = "已安装插件："

[plugin.my.weather]
city = "上海"
```

插件经 ABI 读写它：`get_config(section, key)` 能读**任意**段（自己 + 别人），
`set_config(key, value)` 只能写**自己**那段（宿主按调用者身份判定，写完回写本文件）。
Rust / C / Python / JS 四种写法见
[配置文件](https://catmoecircle.github.io/FuyuBot-docs/docs/usage/config)。

⚠️ `set_config` 回写是**全量重写**，本文件里手写的注释会丢。

## 插件

内置三个：`/start`、`/help`、`/plugins`。自己的插件放进 `plugins/`：Rust / C / C++ / Go 编成动态库；
Python / JS / TS 写脚本目录，由 [`fuyubot-script`](https://github.com/CatMoeCircle/FuyuBot-Script)
托管（从它的 Release 下 `fuyubot-script-<OS>-<平台>.zip`，解压到 `plugins/`）：

```bash
pip install fuyubot    # Python 脚本薄 SDK
bun add fuyubot        # JS / TS 脚本薄 SDK
```

Rust 插件直接依赖 [`fuyubot-abi`](https://crates.io/crates/fuyubot-abi) 写 `define_plugin!`；
C / C++ 用 `fuyubot_abi::PLUGIN_HEADER`。**没有插件开发 CLI**：脚手架、示例与各语言模板都在
[文档站的「插件开发」](https://catmoecircle.github.io/FuyuBot-docs/docs/plugin/)。

编是插件作者自己的工具链的事（`cargo build`、`cl /LD`、`gcc -shared`…），
`fuyubot` **不编译插件**，`plugin install` 只负责把编好的东西复制进插件目录：

```bash
fuyubot plugin install <插件工程目录> --plugin-dir <插件目录>
```

## 文档

| 去哪 | 看什么 |
| --- | --- |
| [FuyuBot-Docs](https://github.com/CatMoeCircle/FuyuBot-Docs) | **使用文档站**：安装、配置、运行、回放、排错 |
| [FuyuBot-Script](https://github.com/CatMoeCircle/FuyuBot-Script) | 脚本插件宿主（Python / JS / TS）+ 两个薄 SDK |
| [架构与数据流](https://catmoecircle.github.io/FuyuBot-docs/docs/dev/architecture/) | 内核、更新管线、绑定层与数据流 |
| [调用 TDLib](https://catmoecircle.github.io/FuyuBot-docs/docs/plugin/tdlib/) | `td_invoke`、响应通道、`fuyubot-simplify` 全量对照 |

本仓只放运行时；脚本插件宿主在 FuyuBot-Script，文档站在 FuyuBot-Docs。

## 从源码构建

Rust 1.99+（Edition 2024）；Windows 上还要 MSVC 工具链。构建期会自动准备 `td_api.tl` 与 TDLib 动态库，
第一次会联网下载一次，之后走缓存。

```bash
cargo build --workspace          # 产物在 target/debug/
cargo build --release -p fuyubot-cli
```

改代码前先看[开发文档](https://catmoecircle.github.io/FuyuBot-docs/docs/dev/)：结构、构建与测试、发布与排错。
