# fuyubot

**基于 TDLib 的 Telegram bot / user 框架**：Rust 内核 + C ABI 插件 + 多语言 SDK。

代码分成三个仓库：

| 仓库 | 放什么 |
| --- | --- |
| **本仓**（[Fuyu-Bot](https://github.com/CatMoeCircle/Fuyu-Bot)） | 运行时本体（`fuyubot` CLI）、内置插件、插件 ABI（`fuyubot-abi`）、TDLib 绑定 |
| [FuyuBot-SDK](https://github.com/CatMoeCircle/FuyuBot-SDK) | 插件作者 SDK（`fuyubot-plugin-api`、`fuyubot-sdk-script`）与 `fuyubot-sdk` CLI |
| [FuyuBot-Docs](https://github.com/CatMoeCircle/FuyuBot-Docs) | 文档站（Fumadocs） |

- **插件是动态库**（`.dll` / `.so` / `.dylib`）。Rust / C / C++ / Python / JS / TypeScript 都能写，
  一个 cdylib 就是一个插件。
- **TDLib 运行时动态加载**，不静态链接。`td_api.tl` 在**编译期**被转成元数据烤进二进制，
  所以运行时不读文件、不解析。条数取决于构建时用的那份 `td_api.tl`：现在 Release 发布的与本机
  `.tdlib-cache/1.8.68` 那份都是 **3325 条（函数 1065 / 构造器 2257 / 未归类 3）**
  ——**别把条数当常量**，1.8.66 那份是 3178，以前还有一份 3214（见 [gaps.md 缺口 30](docs/gaps.md)）。
- 两个 CLI，职责严格分离：**`fuyubot`** 跑框架（`init` / `run` / `status` / `stop` / `config` / `plugin`），
  **`fuyubot-sdk`** 写插件（脚手架 / 生成绑定 / 静态检查，在 SDK 仓库）。**两个都不编译、不打包插件**——
  打包归插件作者自己的语言工具链。
- **离线也能验插件**：`fuyubot run --replay` 把一批更新的 JSON 灌进整条管线，**不需要 Telegram 账号**。
- 内核是 Cordis 式的：`Context` 只暴露 `&self`，注册返回 `Effect`，**drop 即回滚**。

> ### 现状（先看这个）
>
> | | 状态 |
> |---|---|
> | 内核 / 插件 ABI / 加载器 / 更新管线 / 4 大机制 | ✅ 有测试，全绿 |
> | 真 TDLib（真 `tdjson` + 真 `td_api.tl`）：`getMe` 往返、同步 `execute`、插件调 TDLib 收响应 | ✅ 验过（`-- --ignored`） |
> | Rust / C / C++ / Python / JS / TS 插件脚手架 + 生成绑定 + 静态检查 | ✅ |
> | 离线回放（`--replay`） | ✅ 本文档里所有命令都真跑过 |
> | 运行时 CLI 的实例三件套：单实例锁 / `run --background` / `status` / `stop` | ✅ 真跑过（见 ⑤） |
> | **真实登录 + `fuyubot run` 接 TDLib 收真更新** | ⚠️ **接上了但没拿真账号端到端验过**：`run` 会问凭据、驱动授权状态机、`getMe` 打印账户，然后才开分发；真库上的登录 + session 复用是在 `fuyubot-tdlib` 的 `#[ignore]` 自检里验的 |
> | 打包/部署（把 tdjson 跟二进制一起分发） | ✅ 构建期 `fuyubot-tdlib/build.rs` 把 tdjson（+ 兄弟 DLL）拷到 exe 同级目录，loader 优先级 2 认「可执行文件所在目录」；`cargo install` 装出来的二进制是唯一例外（见 [gaps.md 缺口 3](docs/gaps.md)） |
>
> 所以现在能完整跑通的路径是**离线回放**与**实例三件套**；`fuyubot run`（不带 `--replay`）
> 会接上 TDLib 走登录，但**真实收发的最后一环还没有真账号证据**。

---

## 普通用户：下载即用（不用编译）

到 [Releases](https://github.com/CatMoeCircle/Fuyu-Bot/releases) 下对应平台的 zip，解压后 `tdjson` 就在
可执行文件旁边、内置插件在 `plugins/`，直接跑：

```powershell
Expand-Archive -Path fuyubot-windows-x86_64.zip -DestinationPath .
cd fuyubot-windows-x86_64
.\fuyubot.exe init --dir demo --name mybot --api-id 12345
.\fuyubot.exe run --config demo\fuyubot.toml
```

| 系统 | 资产 |
| --- | --- |
| Windows 10/11（x64） | `fuyubot-windows-x86_64.zip` |
| Windows 11（ARM64） | `fuyubot-windows-aarch64.zip` |
| Linux（x86_64） | `fuyubot-linux-x86_64.zip` |
| Linux（ARM64） | `fuyubot-linux-aarch64.zip` |
| macOS（Apple Silicon） | `fuyubot-macos-aarch64.zip` |

插件开发 CLI 在 [FuyuBot-SDK](https://github.com/CatMoeCircle/FuyuBot-SDK/releases) 的
`fuyubot-sdk-<系统>-<架构>.zip` 里。完整的使用文档（下载 / 配置 / 运行 / 回放 / 排错）在
[FuyuBot-Docs](https://github.com/CatMoeCircle/FuyuBot-Docs)；本仓的发布包由
[`.github/workflows/release-bot.yml`](.github/workflows/release-bot.yml) 产出。
下面是从源码构建的**开发者**路线。

## 5 分钟跑起来（不需要账号）

前置：Rust **1.99+**（`edition 2024`）；Windows 上还需要 MSVC 工具链（C/C++ 插件与 Rust 插件都用它链接）。

```bash
git clone https://github.com/CatMoeCircle/Fuyu-Bot
cd Fuyu-Bot
cargo build --workspace
```

> **构建需要一份 `td_api.tl`**（`fuyubot-tdlib-sys` 的必需输入，见 `AGENTS.md` 约束 25）。
> 优先级：`TDLIB_TD_API_TL` → 缓存 `<缓存根>/<版本>/td_api.tl` → 从 Releases 下载。
> 缓存根按顺序找：`FUYUBOT_TDLIB_VENDORED_CACHE` → `$CARGO_HOME/fuyubot-tdlib-vendored` →
> **项目本地 `<工作区>/.tdlib-cache`**（`$HOME` 只读的容器 / 受限沙箱里只有它写得进去）。
> 仓库里的 `.tdlib-cache/` 就是这一档，**不入库**。仓里的 `.cargo/config.toml` 已经把它指好了
> （`relative = true`，相对仓库根），所以在仓库里敲 `cargo build` 不用手写这条：
>
> ```powershell
> # 要临时换缓存位置时手写（**必须绝对路径**：两个 build.rs 的 CWD 是各自的 crate 根）
> $env:FUYUBOT_TDLIB_VENDORED_CACHE = "$PWD\.tdlib-cache"
> ```
>
> 没有缓存时 `build.rs` 会自己从 Releases 下载，**下完一次就进缓存**（Release `TDLib-v1.8.68`
> 已发布）。一次构建只会看到一条「已转换 …（N 条）」的提示；只有第一次下载才会多说几句。
> ⚠️ 缓存根**全都**写不进去时才退到 `$OUT_DIR`——那个目录每次重跑构建脚本都会被清空，
> 于是下次构建**会重新下载**（构建脚本会明确警告这一点）。要彻底避免就设
> `FUYUBOT_TDLIB_VENDORED_CACHE` 指一个可写目录。
> 要完全离线构建就用 `TDLIB_TD_API_TL` 显式给一份文件。

### ① 起一个项目

```bash
cargo run -q -p fuyubot-cli -- init --dir demo --name mybot --api-id 12345
```

```
已生成 demo\fuyubot.toml
插件目录：demo\plugins
下一步：fuyubot-sdk init demo --lang rust（插件脚手架由开发 CLI 生成）
```

生成的 `fuyubot.toml` 长这样（`api_id` 不是秘密，可以放；**`api_hash` 是凭据，不许写进配置文件**）：

```toml
[bot]
name = "mybot"
api_id = 12345

[plugins]
dir = "plugins"

[tdlib]
path = ""          # 留空 = 按加载优先级自动找 tdjson
```

### ② 生成一个插件（或者直接跳到 ③ 用自带的示例）

插件脚手架由 **`fuyubot-sdk`** 生成——它和 SDK crate 在
[FuyuBot-SDK](https://github.com/CatMoeCircle/FuyuBot-SDK)。默认写 crates.io 依赖
（`fuyubot-plugin-api = "0.1.0"`）；对着 SDK 源码开发时设 `FUYUBOT_SDK_PATH` 指向 SDK 仓库的
`crates/`，生成器会改写成 `path = …`。

```bash
fuyubot-sdk init mybot --lang rust --dir demo
```

```
已生成 rust 插件脚手架：demo\plugins\mybot
下一步：用你自己的工具链编译它（cargo build --release / gcc -shared / …）
```

### ③ 用它自己的工具链编译，再装进插件目录

**`fuyubot` 不编译插件**（过渡期的 `build-plugin` 已按新规格拆掉）：编是插件作者自己的事，
`fuyubot plugin install` 只负责把编好的东西**复制**进插件目录。

```bash
cargo build --release --manifest-path demo/plugins/mybot/Cargo.toml   # 生成的插件是独立 crate
cargo run -q -p fuyubot-cli -- plugin install demo/plugins/mybot --plugin-dir demo/plugins
```

```
已安装 demo\plugins\mybot.dll
```

> 传**工程目录**时，工程根目录里的 `.py` / `.js` / `.ts` / `.pyi` 会一起拷过去
> （脚本插件要求脚本与 cdylib 同目录）；传单个 `.dll` 时看不到脚本在哪，所以脚本插件请传目录。
> 各语言怎么编见 [`docs/plugin-guide.md`](docs/plugin-guide.md)；五个示例工程在
> [FuyuBot-SDK 的 `examples/`](https://github.com/CatMoeCircle/FuyuBot-SDK/tree/main/examples)。

### ④ 离线回放，看插件干活

```bash
cargo run -q -p fuyubot-cli -- run --replay examples/replay/new_message.json --plugin-dir demo/plugins
```

```
--replay：本次不接 TDLib（回放是离线的，请求只留档）
已加载插件 mybot (demo\plugins\mybot.dll)
fuyubot（fuyubot 0.1.0）已启动：插件 1 个，工具 0 个
mybot: 收到 updateNewMessage
回放 updateNewMessage → UpdateOutcome { update_type: "updateNewMessage", listeners: 1, warnings: [] }
mybot: 收到 updateNewMessage
回放 updateNewMessage → UpdateOutcome { update_type: "updateNewMessage", listeners: 1, warnings: [] }
mybot: 收到 updateNewMessage
回放 updateNewMessage → UpdateOutcome { update_type: "updateNewMessage", listeners: 1, warnings: [] }
插件打出来的日志：
  Info mybot: init
  Info mybot: start()
插件发出去的请求：
```

回放是**离线**的，所以插件发出去的请求只**留档**（SDK 仓库里的 echo / counter 示例会列出
`#请求号 sendMessage {…}`），不会真的发到 Telegram。退出码 0 表示回放成功。

自带的工具：`cargo run -q -p fuyubot-cli -- plugin list --plugin-dir demo/plugins`

### ⑤ 跑起来、后台跑、看状态、停掉（要 TDLib；真实更新还要账号）

```powershell
# 前台：会问 api_id / api_hash（或从 TG_API_ID / TG_API_HASH 读），然后驱动登录
.\target\debug\fuyubot.exe run --config demo\fuyubot.toml

# 后台：派生一个没有终端的自己，日志进 demo\logs\fuyubot.log，命令立刻返回
.\target\debug\fuyubot.exe run --config demo\fuyubot.toml --no-login --background
```

```
已在后台运行：pid 15252，日志 demo\logs\fuyubot.log
`fuyubot status` 看状态，`fuyubot stop` 停掉它
```

```powershell
.\target\debug\fuyubot.exe status --config demo\fuyubot.toml
```

```
demo：运行中
  pid        15252
  已运行     0 秒
  版本       0.1.0
  名字       mybot
  插件       0 个
  日志       demo\logs\fuyubot.log
```

```powershell
.\target\debug\fuyubot.exe stop --config demo\fuyubot.toml
.\target\debug\fuyubot.exe status --config demo\fuyubot.toml
```

```
已停止 mybot（pid 15252，优雅退出：插件 stop 与内核都已收摊）
demo：没有运行中的实例
（`lock` 文件还在，但没人在持有它——不用手工清理，下次 `fuyubot run` 自己接管）
  上次：mybot pid 15252，启动于 1 秒前（版本 0.1.0，插件 0 个）
  日志       demo\logs\fuyubot.log
```

- **一个目录一个实例**：第二次 `run`（前台或后台都算）会被拒，报错里带上在跑的那个 PID。
- **`stop` 是优雅的**：它写一个停止请求（`.fuyubot/stop`），实例自己停接收循环 → 卸载插件
  （跑 `plugin_stop`）→ 放锁；8 秒不理会才退回硬杀。状态放在 `.fuyubot/` 下，
  陈旧的锁**不用手工清理**。
- **后台跑不能交互**：没有 session 又没给 `TG_BOT_TOKEN` 时它会直接失败，并把日志尾巴打出来——
  先用前台 `run` 登一次（session 落在 `data/`）。
- 上面这段的 `--no-login` 是「只起框架、不接登录流程」，方便在没有账号的机器上验证实例三件套。

---

## 写一个插件

用 **`fuyubot-sdk`**（在 [FuyuBot-SDK](https://github.com/CatMoeCircle/FuyuBot-SDK)）：

```bash
fuyubot-sdk init <名字> --lang <rust|c|cpp|python|js> --dir <项目目录>   # 落在 <项目目录>/plugins/<名字>
fuyubot-sdk codegen --lang <…> --dir <工程目录>                          # 刷新 C 头 / .pyi / .d.ts
fuyubot-sdk check <产物路径>                                             # 静态检查（不加载、不运行）
fuyubot-sdk td-methods --filter sendMessage                              # 查 TDLib 方法
fuyubot-sdk td-method sendMessage                                        # 看签名与参数
```

`check` 是**静态**读导出符号（`object` 解析 PE / ELF / Mach-O，**绝不 dlopen**——那会在宿主里跑插件的
初始化代码）。非零退出码 = 宿主会拒绝加载：

```
文件：target\debug\fuyubot_plugin_demo.dll
格式：Pe / X86_64 / Dynamic
导出符号：
  [ok] plugin_init          必需
  [ok] plugin_start
  [ok] plugin_stop
```

**脚本插件（Python / JS / TS）有一条硬约定**：`.py` / `.js` / `.ts` 必须和那个 cdylib **待在同一个目录**
（宿主按「库旁边找脚本」的规则找它）。`plugin install` 会连同伴文件一起拷，但它**只在两个地方找脚本**：
你传进去的**工程目录**、以及**产物所在目录**——所以传 `.dll` 就只扫 dll 旁边，传工程目录就只扫工程目录。

```bash
# 产物在工程自己的 target/ 下：直接给工程目录
fuyubot plugin install <插件工程目录> --plugin-dir <插件目录>
```

> 示例工程在 [SDK 仓库的 `examples/`](https://github.com/CatMoeCircle/FuyuBot-SDK/tree/main/examples)；
> 它们不是本仓库的 workspace 成员，所以产物落在 SDK 仓库自己的 `target/` 下。

TS 靠 **Node 自带的类型剥离**跑（`node x.ts`，需要 Node ≥ 23.6），所以不需要 `tsc`，
但只能用能剥掉的语法（注解 / `interface` / `type` / `as`；**不能用** `enum` / `namespace` / 构造器参数属性）。

各语言的完整模板与 ABI 细节见 [`docs/plugin-guide.md`](docs/plugin-guide.md) 与
[`docs/abi-contract.md`](docs/abi-contract.md)；可跑的示例在 [`examples/`](examples/README.md)。

---

## 接真 TDLib 跑（当前进度）

`fuyubot run`（不带 `--replay` / `--no-tdlib`）会接 TDLib 并起接收循环，**登录也在里面**：
自动 init → 问/读凭据 → attach → 加载插件 → 发 `setTdlibParameters` → 登录 → `getMe` → 才开分发
（`--no-login` 跳过登录那几步）。下面这段是**旧版**的输出，只作历史参考：

```
$ TG_API_ID=12345 TG_API_HASH=… cargo run -q -p fuyubot-cli -- run --plugin-dir demo/plugins
TDLib 1.8.66 命中「TDLIB_TDJSON_PATH 目录」：…\tdjson.dll
已加载插件 fuyubot.example.counter (…\counter.dll)
环境变量里有 api_id=12345 + TG_API_HASH，但真实登录流程还没接（见 gaps 缺口 4）；现在只在接入 TDLib 后收发更新
fuyubot（fuyubot 0.1.0）已启动：插件 1 个，工具 0 个
开始接收 TDLib 更新（Ctrl-C 退出）
```

两点要知道：**它就一直跑着**（停它用 `fuyubot stop`，或进程里持有 `ReceiveLoop` 停止把手；
Ctrl-C 仍是硬杀，见 [gaps.md 缺口 8](docs/gaps.md)），而且**没登录就收不到任何更新**——
所以要立刻验插件，用 `--replay`。

凭据**只允许两个来源**（`AGENTS.md` 约束 41，配置里写了也会被忽略）：

```bash
export TG_API_ID=…        # 或写进 fuyubot.toml 的 api_id（它不是秘密）
export TG_API_HASH=…      # 只走环境变量，等 C1 做完还会支持交互式输入
```

tdjson 的四级查找顺序（`fuyubot-tdlib::loader`，不要重排）：

1. `TDLIB_TDJSON_PATH`（指目录或文件）
2. vendored（`FUYUBOT_TDLIB_VENDORED_CACHE` / `FUYUBOT_TDLIB_VENDORED_DIR`）→ **可执行文件所在目录**
   （构建期 `build.rs` 拷过去的 tdjson + 兄弟 DLL；`LibrarySource::ExeDir`）
3. Linux 带版本号的系统库（`libtdjson.so.1.8.68` …）
4. 裸库名（`tdjson.dll` / `libtdjson.so`，走 `PATH`）

加载动态库时**先查库自己所在的目录**（Windows 上是 `LOAD_WITH_ALTERED_SEARCH_PATH`）——
否则 tdjson 的兄弟依赖（libssl/zlib 之类）找不到，报的却是 `ERROR_MOD_NOT_FOUND`（126），很有误导性。

---

## 构建与测试（开发者）

仓里的 [`.cargo/config.toml`](.cargo/config.toml) 已经把开发缓存指好了（`FUYUBOT_TDLIB_VENDORED_CACHE`
= 仓内 `.tdlib-cache`，相对仓库根），所以下面这些命令**直接敲就行**，不用先 export：

```powershell
cargo build --workspace --offline     # 0 退出、0 warning
cargo test  --workspace --offline     # 180 passed / 0 failed / 9 ignored
cargo tree  --workspace --offline     # 必须无环
cargo fmt --all --check               # 0 diff（整仓 rustfmt-clean）
```

**一键命令**（同样来自 `.cargo/config.toml`，都真跑过）：

```powershell
cargo bot                             # = fuyubot run：一键启动当前目录的 bot
cargo bot --no-login                  # 参数照常往后接（--config / --plugin-dir / --no-login…）
cargo package-bot                     # 本地打包：target\release\fuyubot.exe
                                      # （tdjson.dll + 兄弟 DLL 由 build.rs 放到它同级）
```

**SDK 不在这里**：`fuyubot-sdk` 由 [FuyuBot-SDK](https://github.com/CatMoeCircle/FuyuBot-SDK) 仓库
`cargo build --release -p fuyubot-sdk-cli` 编出来（它不需要 tdjson）。
本地打包出的是运行时本体（exe + 同级的 TDLib 库），产物目录 `target/release/` 直接拷走就能用——
**`cargo install` 是唯一例外**（安装目录不受构建脚本管，tdjson 得自己放，见 [gaps.md 缺口 3](docs/gaps.md)）。

要临时换缓存位置就照旧设环境变量（**手写必须绝对路径**，相对路径会被解析到各自的 crate 根）：

```powershell
$env:FUYUBOT_TDLIB_VENDORED_CACHE = "F:\Rust\FuyuBot\.tdlib-cache"
```

git bash 里同样可以（MSYS 会把 `/f/...` 转成 `F:/...`）：

```bash
FUYUBOT_TDLIB_VENDORED_CACHE=/f/Rust/FuyuBot/.tdlib-cache cargo test --workspace --offline
```

**默认自检不碰任何外部文件**：真 tdjson、真 `td_api.tl`、真 Releases 下载都藏在 `#[ignore]` 后面。
「全绿」只证明代码自洽，**与真 TDLib 对不对得上只有那几条 `--ignored` 能回答**：

```bash
export TDLIB_TDJSON_PATH=".tdlib-cache/1.8.68/windows-x86_64"
cargo test -p fuyubot-tdlib --test integration --offline -- --ignored

# 真 bot 登录 + session 复用（要真凭据；先删 session 目录，否则验的是「复用」不是「首次登录」）
rm -rf target/tdlib-login-test
TG_API_ID=… TG_API_HASH=… TG_BOT_TOKEN=… cargo test -p fuyubot-tdlib --test integration --offline -- --ignored real_bot_login --nocapture
```

改了 `ffi.rs` 的符号名/arity、`loader::open_library`（**或任何加载动态库的地方**）、
`tl_schema`/`tl_codegen`，或碰任何登录/授权相关代码之后，**必须**跑一次 `-- --ignored`。

---

## 调试手册

**① 先离线复现，别连 Telegram。** `--replay` 把更新灌进管线，插件日志会原样回显
（`插件打出来的日志：` 那一段），插件发出的请求也会列出来。慢插件用 `--replay-wait <毫秒>` 等它。

**② `--plugin-dir` 指向「只放你要的插件」的目录，别指 `target/debug`。**
那里面混着所有示例插件的产物，其中 `counter.dll` 是**脚本插件**、脚本不在旁边，于是 Node 报
`Cannot find module '...\counter.ts'` 并且**整个回放以非零码中止**。正确做法是
`plugin install` 到一个目录，或把 dll + 脚本一起拷进一个干净目录。

**③ `unsupported` / 找不到 tdjson。** `fuyubot-tdlib` **编译期不依赖 tdjson 存在**，
所以构建成功不代表运行找得到。按上面四级顺序排查；嫌麻烦就设 `TDLIB_TDJSON_PATH`。

**④ Windows 上 `LNK1104: cannot open file '...\Temp\lnk{GUID}.tmp'`。**
这是 **MSVC 链接器写不了 `%TEMP%`**（受限环境 / 沙箱里常见），跟你的代码无关。
把临时目录指到可写的位置即可：

```powershell
$env:TEMP = "$PWD\target\linktmp"; $env:TMP = $env:TEMP; New-Item -ItemType Directory -Force $env:TEMP
```

**⑤ 同一坑的另一面：测试不许依赖 `%TEMP%`。** 仓库约定临时目录一律放
`target/test-tmp/<crate>/<名字>`——MSYS 会把 `TEMP` 改成 `/tmp`（在 Windows 上解析成当前盘根，
建目录被拒），受限环境里真实 `%TEMP%` 也可能写不了。**换个 shell 就红的测试几乎总是这个原因。**

**⑥ PowerShell 读 UTF-8 文件会变乱码**（本机 cp936）。看文件内容用编辑器/`read`，
别用 `Get-Content`；中文注释被 GBK 解码还会**吞换行**导致行号错位。

---

## 环境变量

| 变量 | 谁读 | 作用 |
|---|---|---|
| `FUYUBOT_TDLIB_VENDORED_CACHE` | `build.rs`（**构建期**） | 缓存根（**要绝对路径**）：`td_api.tl` 与 vendored 动态库都放这儿。不设就按 `$CARGO_HOME` → 项目本地 `.tdlib-cache` 找。运行时的动态库位置是编译期烤进去的，**不是**运行期读这个变量 |
| `TDLIB_TDJSON_PATH` | 加载器 | tdjson 的目录或文件（优先级 1） |
| `TDLIB_TD_API_TL` | `build.rs` | 显式指定 `td_api.tl`（优先级最高） |
| `FUYUBOT_TDLIB_RELEASES_REPO` | `build.rs` | 覆盖 `[package.metadata.fuyubot] releases-repo` |
| `TG_API_ID` / `TG_API_HASH` | `fuyubot run` / 集成测试 | **凭据只走环境变量**（`TG_BOT_TOKEN` 也认，`run` 与测试都用） |
| `FUYUBOT_SDK_PATH` | `fuyubot-sdk`（在 SDK 仓库） | **可选**：脚手架里 `path = …` 指向 SDK 源码目录；不设就写 crates.io 版本号 |

> 插件的编译是**作者自己的工具链**的事（`cargo build`、`cl /LD`、`gcc -shared`……），
> 两个 CLI 都不读 `CC` 之类的编译器开关——`build-plugin` 已按新规格拆掉。

---

## 仓库结构

本仓只放**运行时**。插件 SDK 在 [FuyuBot-SDK](https://github.com/CatMoeCircle/FuyuBot-SDK)，
文档站在 [FuyuBot-Docs](https://github.com/CatMoeCircle/FuyuBot-Docs)。

```
crates/
  fuyubot-core         Cordis 内核（Context / Effect / Plugin / PluginManager）。不认识 Telegram
  fuyubot-abi          C ABI：HostApiVTable、PluginHandle、define_plugin!、cbindgen 生成的头（发 crates.io）
  fuyubot-tdlib-sys    TDLib 绑定（-sys）：td_api.tl 编译期生成的类型 + 元数据 + ffi 声明（发 crates.io）
  fuyubot-tdlib        TDLib 运行时：loader / client / authorize（状态机），并重导出绑定
  fuyubot-event        事件总线          fuyubot-service    命名服务容器
  fuyubot-tool         工具注册表        fuyubot-middleware 六阶段洋葱管线
  fuyubot-loader       宿主层：VTable 桥 + 更新管线 + 插件加载
  fuyubot-simplify     发消息 / 编辑消息 / Markdown 转 entities / 命令解析等便捷封装
  fuyubot-cli          `fuyubot` 运行时 CLI
  fuyubot-plugin-demo  最小 Rust 插件（集成测试真的 dlopen 它）
  fuyubot-plugin-start 内置插件：/start   fuyubot-plugin-help  /help   fuyubot-plugin-manage  /plugins
examples/replay/       离线回放用的更新 JSON
docs/                  架构、ABI 契约、插件指南、进度台账、交接文档
```

## 文档地图

| 文件 | 什么时候看 |
|---|---|
| [FuyuBot-Docs](https://github.com/CatMoeCircle/FuyuBot-Docs) | **给使用者看的文档站**（Fumadocs）：使用文档 + 插件开发文档 |
| [FuyuBot-SDK](https://github.com/CatMoeCircle/FuyuBot-SDK) | SDK crate、`fuyubot-sdk` CLI、五种语言的示例插件 |
| [`docs/plugin-guide.md`](docs/plugin-guide.md) | 写插件（各语言模板、构建、调试） |
| [`docs/abi-contract.md`](docs/abi-contract.md) | 插件 ABI 的正式契约（VTable、字符串归属、版本协商） |
| [`docs/architecture.md`](docs/architecture.md) | 内核与更新管线的设计（结构 / 数据流 / ABI 契约 / 两种插件形态） |
| [`docs/tdlib-simplify.md`](docs/tdlib-simplify.md) | 便捷封装的用法 |
| [`docs/HANDOFF.md`](docs/HANDOFF.md) | **入口**：当前状态 / 构建契约 / 每个 crate 有什么 |
| [`docs/gaps.md`](docs/gaps.md) | 未验证的部分与已知缺口（编号是引用锚） |
| [`docs/pitfalls.md`](docs/pitfalls.md) | 踩过的坑（编号是引用锚，别再踩回去） |
| [`docs/decisions.md`](docs/decisions.md) | 被拍板的架构变更 + 核心原则 |
| [`docs/tech-debt.md`](docs/tech-debt.md) | 技术债清单 |
| [`docs/ROADMAP.md`](docs/ROADMAP.md) | 进度台账（新规格与验收补跑） |
| [`AGENTS.md`](AGENTS.md) | 仓库硬约束（改代码前必读） |

## 还缺什么

按优先级排的完整列表在 [`docs/gaps.md`](docs/gaps.md)（编号即引用锚），这里只提三条最影响使用的：

1. **真实账号的最后一环**：`run` 已经会问凭据、驱动授权状态机、`getMe` 打印账户后再开分发，
   真库上的登录 + session 复用也在 `#[ignore]` 自检里验过；但**没有拿真账号端到端跑过
   「收真更新 → 插件回一条」**。手机号登录那条路同样没验过。
2. **`cargo install` 出来的二进制旁边没有 tdjson**：构建期的拷贝管不到安装目录
   （其余部署形态已经解决：构建产物同级目录就有 tdjson，见 [gaps.md 缺口 3](docs/gaps.md)）。
3. **Ctrl-C 不是优雅退出**：`fuyubot stop` 是优雅的（插件 `plugin_stop` 会跑），
   但终端里按 Ctrl-C 还是硬杀——要捕获它得开 tokio 的 `signal` feature，
   会带进 `mio` + `signal-hook-registry` + `errno`（见 [`docs/decisions.md`](docs/decisions.md) 变更 3-c）。

另外仓库目前**没有 `LICENSE` 文件**（未声明授权）。

## 参与开发

改之前先读 [`AGENTS.md`](AGENTS.md)：那里是硬约束（依赖方向、加载优先级、凭据不许落盘、
`Context` 只暴露 `&self` …）。提交前请过一遍它的检查清单，其中三条是硬门槛：

```bash
cargo build --workspace --offline   # 0 warning
cargo test  --workspace --offline   # 全绿
cargo fmt --all --check             # 0 diff
```
