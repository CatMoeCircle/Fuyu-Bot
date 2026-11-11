# fuyubot

**基于 TDLib 的 Telegram bot / user 框架**：Rust 内核 + C ABI 插件。一个宿主进程负责内核与
Telegram 连接，每个插件都是独立编译的动态库，按需加载、按依赖排序、可单独启用停用。

[![](https://img.shields.io/badge/powered_by-dsh-4D6BFE?style=flat-square&logo=deepseek&logoColor=white)](https://github.com/deepseek-ai/deepseek-harness)

## 特性

- **插件就是动态库**：`.dll` / `.so` / `.dylib`，Rust、C、C++、Go、Python、JS / TS 都能写。
  插件只认少数几个导出符号加一张宿主编排表（`HostApiVTable`），跨语言共享同一份契约。
- **一个进程，两种账号**：bot（Bot Token）与 user（手机号 / 验证码 / 扫码）走同一套授权状态机，
  登录态由 TDLib 自己落盘，下次启动直接复用。
- **下载即用**：Release 包里可执行文件、TDLib（`tdjson`）与内置插件都是齐的，不需要装 Rust，
  也不用另外配置 TDLib。
- **离线验证插件**：`--replay` 把一批更新灌进真实管线，不接 TDLib、不用账号、不联网。
- **内核与机制分离**：`Context` 按名字寻址的服务表，加上事件 / 服务 / 工具 / 中间件四个机制 crate，
  组合全部发生在宿主层。
- **运行期可控**：实例锁、优雅停止、平台面板（启停实例、插件开关、日志过滤与级别、配置编辑）。
- **TDLib 运行时加载**：五级定位（环境变量 → 随包分发 → exe 同级 → 系统库 → 裸库名），
  换 TDLib 不用重编。
- **全链路 tracing**：命令结果、宿主诊断、插件日志、TDLib 更新是同一份 `时间 来源 级别 内容`
  格式，各带颜色。

## 下载

到 [Releases](https://github.com/CatMoeCircle/Fuyu-Bot/releases) 下对应平台的 zip：

| 系统 | 资产 |
| --- | --- |
| Windows 10/11（x64） | `fuyubot-<版本>-windows-x86_64.zip` |
| Windows 11（ARM64） | `fuyubot-<版本>-windows-aarch64.zip` |
| Linux（x86_64） | `fuyubot-<版本>-linux-x86_64.zip` |
| Linux（ARM64） | `fuyubot-<版本>-linux-aarch64.zip` |
| macOS（Apple Silicon） | `fuyubot-<版本>-macos-aarch64.zip` |
| Android（arm64-v8a，真机） | `fuyubot-<版本>-arm64-v8a.apk` |
| Android（x86_64，模拟器） | `fuyubot-<版本>-x86_64.apk` |

`<版本>` 就是 Release 的版本号（如 `v0.1.0`），资产名里带它，方便一眼看出下的是哪个版本。

**Android 的 APK 是开箱即用的图形客户端**，界面在
[FuyuBot-for-Android](https://github.com/CatMoeCircle/FuyuBot-for-Android) 仓库，
内核（`libfuyubotjni.so` + TDLib）由本仓构建后随包打进 `assets/`，第一次启动自动释放到应用私有目录。
JNI 内核包**不再单独作为 Release 资产发布**——它只在 CI 里作为中间产物喂给 APK job；
要自己集成内核，用 `crates/fuyubot-android` 自己编。

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
cd demo
..\fuyubot.exe        # 开终端操作面板，按 s 登录并启动
```

桌面 / 本机**推荐用 TUI**：不带子命令（或 `run --tui`）就是操作面板，登录、启停、日志、
配置、插件和商店都在里面。`init --api-hash <HASH>` 也可以把 `api_hash` 一起明文写进配置；
不写就等面板首次按 `s` 时问。

```
2026-10-31 07:05:13 cli INFO  已安装 3 个内置插件到 demo\plugins
2026-10-31 07:05:13 cli INFO  已生成 demo\fuyubot.toml
2026-10-31 07:05:13 cli INFO  插件目录：demo\plugins
```

`init` 建出项目骨架：`fuyubot.toml`、`plugins/`，并把三个内置插件复制进去。

`api_id` 到 <https://my.telegram.org> 申请，它**不是**秘密，写进配置没问题。
`api_hash` 每次冷启动都要发给 TDLib：可以用 `init --api-hash` 或环境变量 `TG_API_HASH`，
也可以等 `run` / 面板首次问过之后**明文写回** `fuyubot.toml` 的 `[telegram]`。

```powershell
$env:TG_API_HASH = "…"
.\fuyubot.exe run --config demo\fuyubot.toml
```

第一次跑会走登录：bot 用 Bot Token（`TG_BOT_TOKEN` 或提示输入），user 用手机号 + 验证码（或扫码）。
登录态由 TDLib 自己落进 `data/`，下次启动直接进。要停就 `fuyubot stop`，它会把插件和内核都收好再退出。

## 常用命令

| 命令 | 作用 |
| --- | --- |
| `fuyubot`（不带参数） | **直接开终端操作面板**，等价于 `fuyubot run --tui`（双击 exe 就是这条） |
| `fuyubot init --dir <目录> --name <名字> --api-id <id> [--api-hash <hash>]` | 建项目：生成 `fuyubot.toml`、`plugins/`，并复制内置插件 |
| `fuyubot run --config <目录>/fuyubot.toml` | 前台跑（含登录） |
| `fuyubot run … --background` | 后台跑，日志进 `<项目>/logs/fuyubot.log`（每次启动先清空旧日志） |
| `fuyubot run --tui`（短写 `-t`） | 终端操作面板：顶部版本栏 + 导航/工作区 + 底部资源占用 |
| `fuyubot status --config …` / `fuyubot stop --config …` | 看状态 / 优雅停止 |
| `fuyubot config --config …` | 打印**生效**配置（默认值 + 实际读到的文件） |
| `fuyubot run --replay <json> --plugin-dir <目录>` | 离线回放：不接 TDLib、不用账号 |
| `fuyubot plugin list --plugin-dir <目录>` | 列出插件目录里能加载的插件 |
| `fuyubot plugin install <工程目录或产物> --plugin-dir <目录>` | 把编好的插件复制进插件目录 |

一个目录只能跑一个实例：第二次 `run` 会被拒，报错里带正在跑的那个 PID。
`--no-login` 只接 TDLib、不走登录；`--no-tdlib` 则完全不接 TDLib。

### 终端面板

`run --tui`（或直接 `fuyubot`）打开的是**操作面板**而不是前台 bot：面板不加载插件，
平时也不接 TDLib，只读 `.fuyubot/status`、配置、插件目录与日志；`s` / `L` 的登录探测是唯一例外，
跑完立刻关掉。面板里可以：

- **启停实例**：`s` 先自动检查登录（缺凭据先弹两步输入，缺登录弹授权框），成功后派生
  `run --background`；`x` 走与 `fuyubot stop` 相同的优雅停止。`L` 只登录、不启动。
  退出面板（`q`）不会停掉已经起来的实例。
- **导航**：概览 → 日志 → 配置 → 插件 → 商店 → 帮助；数字 `1`–`6` 直达。
- **登录**：`s` / `L` 都会按授权状态弹 bot token / 手机号 / 验证码 / 两步验证密码输入框。
- **开关插件**：插件页 `Space` 启用 / 禁用选中的 dylib——禁用即改名（`foo.dll` → `foo.dll.disable`），
  加载器只认扩展名为 `dll` 的文件；再按一次改回来，重启实例生效。
- **看日志**：首屏读文件尾部，往回翻自动分段回读更早的日志（滑动窗口），翻在中间时窗口冻结，
  `End` 回到最新；`F` 按级别与来源（插件 / 宿主 / 管道 / 更新 / TDLib）过滤，`c` 清空当前日志。
- **调插件日志级别**：`l` 写 `.fuyubot/log-level`，运行中的实例下一圈生效，重启回到配置值。
- **改配置**：配置页选中一行，`Enter` / `e` 编辑，文本 / 整数 / 枚举先校验，写回保留注释与 CRLF，
  跨行值（多行数组 / 内联表）也能改（整段折叠，值里的注释搬到新值下面），重启生效；
  `api_hash`、凭据与 `[plugin.*]` 段只读。
- **窄终端自适应**：Termux 这类 40 列终端会自动收缩布局，完整按键见
  [文档站](https://catmoecircle.github.io/FuyuBot-docs/docs/usage/run)。

## 配置

`fuyubot init` 生成的 `fuyubot.toml`（所有键都有默认值，缺文件时 `run` 会自动生成一份）：

```toml
[bot]
name = "mybot"

[telegram]
account_type = "bot"   # bot：Bot Token；user：手机号 / 扫码。写错会报错，不会静默当 bot
api_id = 12345         # 不是秘密
data_dir = "data"      # TDLib 的 database 与 files 都放这儿
api_hash = ""          # 首次 run / 面板问过后明文写回；也可用 init --api-hash

[roles]
owners = []            # 主人 id；为空时 user 账户默认自己是主人
admins = []            # 全局管理员 id

[cache]
enabled = true
capacity = 100         # 每种 TDLib 对象类型的上限

[command]
# prefix = ["/", "."]  # 命令头数组；不写按账号默认（bot "/"、user "."），写了整套替换
whitelist = []         # 允许触发命令的用户 id；自己发的始终触发
whitelist_only = true  # user 账户白名单模式；⚠️ 不建议关闭（false = 任何人都能触发）

[plugins]
dir = "plugins"

[tdlib]
path = ""              # 留空 = 自动找 tdjson（环境变量 → 自带 → exe 同级 → 系统库 → 裸库名）
log_verbosity = 1      # 默认只留警告与错误；排查问题再调大
```

**凭据规则**：`api_id` / `api_hash` 会写进 `fuyubot.toml`（`api_hash` 明文；也可以只用
`TG_API_ID` / `TG_API_HASH`，环境变量优先）。Bot token、手机号、验证码、两步验证密码仍然
只来自环境变量或交互输入，不落盘。

**命令头**：`[command].prefix` 是**数组**，可以配多个（每项单个字符，不能是字母 / 数字 /
空白）。没配时按账号类型取默认值：bot 认 `/`（`/start`、`/help`…），user 认 `.`
（`.help`）。配了就是**整套替换**——bot 不会额外保留 `/`，要用就写进数组；
user 账户下 `/xxx` 始终不交给插件（自己发的也不认），即使写进数组。放行的自定义命令头
会归一化成 `/xxx`，插件不用改。**user 账户默认白名单模式**：只有账号自己发的消息，
或在 `[command].whitelist` 里的用户 id 才会触发，别人发的命令一律不触发；
`[command].whitelist_only = false` 可以关掉这个模式，但**⚠️ 不建议**——关掉后任何人都能
触发插件。`whitelist` 非空时 bot 也只响应表里的用户（bot 不受 `whitelist_only` 影响）。

插件用 `Command::reply` / `Command::reply_then` 回命令（入参与 `send_message` 一样），
宿主决定回复方式：bot 账户、或 user 账户里**白名单用户**触发的命令发一条新消息并
`reply_to` 原命令；**user 账户自己触发的命令编辑原命令消息**（不再刷屏）。返回的是
TDLib 的最终 `Message`。被拦下的命令仍然出现在 `update` 通用通道里，只是不会进入
`updateNewMessage` 命令通道。

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
`set_config(key, value)` 只能写**自己**那段（宿主按调用者身份判定）。
Rust / C / Python / JS 四种写法见
[配置文件](https://catmoecircle.github.io/FuyuBot-docs/docs/usage/config)。

配置分三层（后者只填前者的缺失键）：插件包里的 `config.default.toml` <
`plugin-config/<插件名>.toml`（插件 `set_config` 的落盘） < 本文件的 `[plugin.*]`（**你写的，最高**）。
插件写配置**不会**改动本文件——注释和 `api_hash` 都安全。

## 插件

内置三个：`/start`、`/help`、`/plugins`。自己的插件放进 `plugins/`：Rust / C / C++ / Go 编成动态库；
Python / JS / TS 写脚本目录，由 [`fuyubot-script`](https://github.com/CatMoeCircle/FuyuBot-Script)
托管（从它的 Release 下 `fuyubot-script-<OS>-<平台>.zip`，解压到 `plugins/`）：

```bash
pip install fuyubot    # Python 脚本薄 SDK
bun add fuyubot        # JS / TS 脚本薄 SDK
```

Rust 插件直接依赖 [`fuyubot-abi`](https://crates.io/crates/fuyubot-abi) 写 `define_plugin!`；
C / C++ 用 `fuyubot_abi::PLUGIN_HEADER`。脚手架、示例与各语言模板见
[文档站的「插件开发」](https://catmoecircle.github.io/FuyuBot-docs/docs/plugin/)。

插件用你自己的工具链编（`cargo build`、`cl /LD`、`gcc -shared`…），`plugin install` 负责把编好的
产物复制进插件目录：

```bash
fuyubot plugin install <插件工程目录> --plugin-dir <插件目录>
```

插件分发遵循 **bundle** 格式：一个插件一个目录，根上的 `plugin.json` 声明 `id` / `version` /
`kind` / `entry` / `depends`，加载器不加载动态库就能先读到依赖，再按依赖顺序决定加载次序。

## 文档

| 去哪 | 看什么 |
| --- | --- |
| [FuyuBot-Docs](https://github.com/CatMoeCircle/FuyuBot-Docs) | **使用文档站**：安装、配置、运行、回放、排错 |
| [FuyuBot-Script](https://github.com/CatMoeCircle/FuyuBot-Script) | 脚本插件宿主（Python / JS / TS）+ 两个薄 SDK |
| [FuyuBot-for-Android](https://github.com/CatMoeCircle/FuyuBot-for-Android) | Android 图形客户端（fuyuTD）：Kotlin + Compose 壳，内核随 APK 内置 |
| [架构与数据流](https://catmoecircle.github.io/FuyuBot-docs/docs/dev/architecture/) | 内核、更新管线、绑定层与数据流 |
| [调用 TDLib](https://catmoecircle.github.io/FuyuBot-docs/docs/plugin/tdlib/) | `td_invoke`、响应通道、`fuyubot-simplify` 全量对照 |

## 从源码构建

Rust 1.99+（Edition 2024）；Windows 上还要 MSVC 工具链。构建期会自动准备 `td_api.tl` 与 TDLib 动态库，
第一次会联网下载一次，之后走缓存。

```bash
cargo build --workspace          # 产物在 target/debug/
cargo build --release -p fuyubot-cli
```

开发文档：[结构、构建与测试、发布与排错](https://catmoecircle.github.io/FuyuBot-docs/docs/dev/)。
