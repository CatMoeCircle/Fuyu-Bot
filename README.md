# fuyubot

**基于 TDLib 的 Telegram bot / user 框架**：Rust 内核 + C ABI 插件 + 多语言 SDK。

- **插件是动态库**（`.dll` / `.so` / `.dylib`）。Rust / C / C++ / Python / JS / TypeScript 都能写，
  一个 cdylib 就是一个插件。
- **TDLib 运行时动态加载**，不静态链接。`td_api.tl` 在**编译期**被转成 3214 条元数据烤进二进制
  （函数 1022 / 构造器 2189 / 未归类 3），所以运行时不读文件、不解析。
- 两个 CLI，职责严格分离：**`fuyubot`** 跑框架，**`fuyubot-sdk`** 写插件（脚手架 / 生成绑定 / 静态检查）。
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
> | **真实登录 + `fuyubot run` 接 TDLib 跑起来** | ❌ **还没接**：授权状态机 `fuyubot-tdlib::authorize` 已就绪，CLI 还没串起来 |
> | 打包/部署（把 tdjson 跟二进制一起分发） | ❌ 见 [还缺什么](#还缺什么) |
>
> 所以现在能完整跑通的路径是**离线回放**；`fuyubot run`（不带 `--replay`）会接上 TDLib 起接收循环，
> 但因为没登录，收不到任何更新。

---

## 5 分钟跑起来（不需要账号）

前置：Rust **1.99+**（`edition 2024`）；Windows 上还需要 MSVC 工具链（C/C++ 插件与 Rust 插件都用它链接）。

```bash
git clone https://github.com/xiaoqvan/Fuyu-Bot
cd Fuyu-Bot
cargo build --workspace
```

> **构建需要一份 `td_api.tl`**（`fuyubot-tdlib` 的必需输入，见 `AGENTS.md` 约束 25）。
> 优先级：`TDLIB_TD_API_TL` → 缓存 `<cache>/<版本>/td_api.tl` → 从 Releases 下载。
> 仓库里的 `.tdlib-cache/` 是本机开发缓存（**不入库**）：
>
> ```powershell
> # 本机已有缓存时，指一下就不用联网（必须写绝对路径）
> $env:FUYUBOT_TDLIB_VENDORED_CACHE = "$PWD\.tdlib-cache"
> ```
>
> 还没有缓存、也还没有 Releases 资产时，用 `TDLIB_TD_API_TL` 显式给一份文件。

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
path = ""          # 留空 = 按四级优先级自动找 tdjson
```

### ② 生成一个插件（或者直接跳到 ③ 用自带的示例）

```bash
export FUYUBOT_SDK_PATH="$PWD/crates"     # 插件用 path 依赖 SDK（那两个 crate 还没发到 crates.io）
cargo run -q -p fuyubot-sdk-cli -- init mybot --lang rust --dir demo
```

```
已生成 rust 插件脚手架：demo\plugins\mybot
下一步：用你自己的工具链编译它（cargo build --release / gcc -shared / …）
```

### ③ 编译并装进插件目录

```bash
cargo run -q -p fuyubot-cli -- build-plugin --dir demo/plugins/mybot
```

```
   Compiling mybot v0.1.0 (demo\plugins\mybot)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 10.69s
编好了：demo\plugins\mybot\target\debug\mybot.dll
已装进 demo\plugins：
  demo\plugins\mybot.dll
```

> `build-plugin` 是**过渡期**命令（打包将来归作者自己的工具链）。它要**祖先目录里有 `fuyubot.toml`**
> 才会把产物装进插件目录；找不到就只编译并告诉你「跳过安装」。

### ④ 离线回放，看插件干活

```bash
cargo run -q -p fuyubot-cli -- run --replay examples/replay/new_message.json --plugin-dir demo/plugins
```

```
--replay：本次不接 TDLib（回放是离线的，请求只留档）
已加载插件 fuyubot.demo (demo\plugins\mybot.dll)
fuyubot（fuyubot 0.1.0）已启动：插件 1 个，工具 1 个
回放 updateNewMessage → UpdateOutcome { update_type: "updateNewMessage", listeners: 1, warnings: [] }
回放 updateNewMessage → UpdateOutcome { update_type: "updateNewMessage", listeners: 1, warnings: [] }
回放 updateNewMessage → UpdateOutcome { update_type: "updateNewMessage", listeners: 1, warnings: [] }
插件打出来的日志：
  Info fuyubot.demo: init
  Info fuyubot.demo: start()
  Info fuyubot.demo: updateNewMessage {…"text":"/ping"…}
插件发出去的请求：
  #281474976710656 getMe {"@type":"getMe"}
```

回放是**离线**的，所以插件发出去的请求只**留档**（上面那行 `#281474976710656` 是合成请求号），
不会真的发到 Telegram。退出码 0 表示回放成功。

自带的工具：`cargo run -q -p fuyubot-cli -- plugin list --plugin-dir demo/plugins`

---

## 写一个插件

```bash
cargo run -q -p fuyubot-sdk-cli -- init <名字> --lang <rust|c|cpp|python|js> --dir <项目目录>   # 落在 <项目目录>/plugins/<名字>
cargo run -q -p fuyubot-sdk-cli -- codegen --lang <…> --dir <工程目录>                          # 刷新 C 头 / .pyi / .d.ts
cargo run -q -p fuyubot-sdk-cli -- check <产物路径>                                             # 静态检查（不加载、不运行）
cargo run -q -p fuyubot-sdk-cli -- td-methods --filter sendMessage                              # 查 TDLib 方法
cargo run -q -p fuyubot-sdk-cli -- td-method sendMessage                                        # 看签名与参数
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
# 普通情况（产物就在工程自己的 target/ 下）：直接给工程目录
cargo run -q -p fuyubot-cli -- plugin install <插件工程目录> --plugin-dir <插件目录>

# ⚠️ examples/ 下的示例是 **workspace 成员**，产物落在**仓库根**的 target/，不在工程自己的 target/ 里
#    （直接 `plugin install examples/ts-counter` 会报「里面没找到 .dll」）。
#    这种情况把 dll 和脚本拷进同一个目录最简单：
cargo build -p fuyubot-example-counter
mkdir demo-ts && cp target/debug/counter.dll examples/ts-counter/counter.ts examples/ts-counter/fuyubot_plugin.js demo-ts/
cargo run -q -p fuyubot-cli -- run --replay examples/replay/counter.json --plugin-dir demo-ts
```

TS 靠 **Node 自带的类型剥离**跑（`node x.ts`，需要 Node ≥ 23.6），所以不需要 `tsc`，
但只能用能剥掉的语法（注解 / `interface` / `type` / `as`；**不能用** `enum` / `namespace` / 构造器参数属性）。

各语言的完整模板与 ABI 细节见 [`docs/plugin-guide.md`](docs/plugin-guide.md) 与
[`docs/abi-contract.md`](docs/abi-contract.md)；可跑的示例在 [`examples/`](examples/README.md)。

---

## 接真 TDLib 跑（当前进度）

`fuyubot run`（不带 `--replay` / `--no-tdlib`）会接 TDLib 并起接收循环。**但登录还没接**：

```
$ TG_API_ID=12345 TG_API_HASH=… cargo run -q -p fuyubot-cli -- run --plugin-dir demo/plugins
TDLib 1.8.66 命中「TDLIB_TDJSON_PATH 目录」：…\tdjson.dll
已加载插件 fuyubot.example.counter (…\counter.dll)
环境变量里有 api_id=12345 + TG_API_HASH，但真实登录流程还没接（见 HANDOFF 缺口 4）；现在只在接入 TDLib 后收发更新
fuyubot（fuyubot 0.1.0）已启动：插件 1 个，工具 0 个
开始接收 TDLib 更新（Ctrl-C 退出）
```

两点要知道：**它就一直跑着**（接收循环没有停止位，只能 Ctrl-C，见缺口 8），而且**没登录就收不到任何更新**——
所以要立刻验插件，用 `--replay`。

凭据**只允许两个来源**（`AGENTS.md` 约束 41，配置里写了也会被忽略）：

```bash
export TG_API_ID=…        # 或写进 fuyubot.toml 的 api_id（它不是秘密）
export TG_API_HASH=…      # 只走环境变量，等 C1 做完还会支持交互式输入
```

tdjson 的四级查找顺序（`fuyubot-tdlib::loader`，不要重排）：

1. `TDLIB_TDJSON_PATH`（指目录或文件）
2. vendored（`FUYUBOT_TDLIB_VENDORED_CACHE` / `FUYUBOT_TDLIB_VENDORED_DIR`）
3. Linux 带版本号的系统库（`libtdjson.so.1.8.66` …）
4. 裸库名（`tdjson.dll` / `libtdjson.so`，走 `PATH`）

加载动态库时**先查库自己所在的目录**（Windows 上是 `LOAD_WITH_ALTERED_SEARCH_PATH`）——
否则 tdjson 的兄弟依赖（libssl/zlib 之类）找不到，报的却是 `ERROR_MOD_NOT_FOUND`（126），很有误导性。

---

## 构建与测试（开发者）

```powershell
$env:FUYUBOT_TDLIB_VENDORED_CACHE = "F:\Rust\FuyuBot\.tdlib-cache"   # 绝对路径！

cargo build --workspace --offline     # 0 退出、0 warning
cargo test  --workspace --offline     # 176 passed / 0 failed / 10 ignored
cargo tree  --workspace --offline     # 必须无环
cargo fmt --all --check               # 0 diff（整仓 rustfmt-clean）
```

git bash 里同样可以（MSYS 会把 `/f/...` 转成 `F:/...`）：

```bash
FUYUBOT_TDLIB_VENDORED_CACHE=/f/Rust/FuyuBot/.tdlib-cache cargo test --workspace --offline
```

**默认自检不碰任何外部文件**：真 tdjson、真 `td_api.tl`、真 Releases 下载都藏在 `#[ignore]` 后面。
「全绿」只证明代码自洽，**与真 TDLib 对不对得上只有那几条 `--ignored` 能回答**：

```bash
export TDLIB_TDJSON_PATH=".tdlib-cache/1.8.66/windows-x86_64"
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
| `FUYUBOT_TDLIB_VENDORED_CACHE` | `fuyubot-tdlib`（构建期 + 运行时） | vendored 缓存根目录（**要绝对路径**） |
| `TDLIB_TDJSON_PATH` | 加载器 | tdjson 的目录或文件（优先级 1） |
| `TDLIB_TD_API_TL` | `build.rs` | 显式指定 `td_api.tl`（优先级最高） |
| `FUYUBOT_TDLIB_RELEASES_REPO` | `build.rs` | 覆盖 `[package.metadata.fuyubot] releases-repo` |
| `TG_API_ID` / `TG_API_HASH` | CLI / 集成测试 | **凭据只走环境变量**（`TG_BOT_TOKEN` 仅测试用） |
| `FUYUBOT_SDK_PATH` | `fuyubot-sdk` | 脚手架里 `path = …` 指向 SDK 的目录 |
| `FUYUBOT_OFFLINE` / `CARGO_NET_OFFLINE` | `build-plugin` | 非空且非 `0`/`false` 时给 cargo 加 `--offline` |
| `CC` | `build-plugin` | 覆盖 C/C++ 插件用的编译器 |

---

## 仓库结构

```
crates/
  fuyubot-core         Cordis 内核（Context / Effect / Plugin / PluginManager）。不认识 Telegram
  fuyubot-abi          C ABI：HostApiVTable、PluginHandle、define_plugin!、cbindgen 生成的头
  fuyubot-plugin-api   插件作者唯一要依赖的 crate（重导出 ABI + 安全包装）
  fuyubot-sdk-script   Python / JS / TS SDK（cdylib 起解释器 + 行 JSON 协议 + script_plugin!）
  fuyubot-tdlib        TDLib：ffi / loader / client / authorize（状态机）/ tl_schema / tl_codegen
  fuyubot-event        事件总线          fuyubot-service    命名服务容器
  fuyubot-tool         工具注册表        fuyubot-middleware 六阶段洋葱管线
  fuyubot-loader       宿主层：VTable 桥 + 更新管线 + 插件加载
  fuyubot-simplify     发消息 / 内联键盘 / 下载进度等便捷封装
  fuyubot-cli          `fuyubot` CLI      fuyubot-sdk-cli  `fuyubot-sdk` CLI
  fuyubot-plugin-demo  最小 Rust 插件（集成测试真的 dlopen 它）
examples/              rust-echo / python-weather / ts-counter / c-ping + 回放用的 JSON
docs/                  架构、ABI 契约、插件指南、进度台账、交接文档
```

## 文档地图

| 文件 | 什么时候看 |
|---|---|
| [`docs/plugin-guide.md`](docs/plugin-guide.md) | 写插件（各语言模板、构建、调试） |
| [`docs/abi-contract.md`](docs/abi-contract.md) | 插件 ABI 的正式契约（VTable、字符串归属、版本协商） |
| [`docs/architecture.md`](docs/architecture.md) | 内核与更新管线的设计 |
| [`docs/tdlib-simplify.md`](docs/tdlib-simplify.md) | 便捷封装的用法 |
| [`docs/HANDOFF.md`](docs/HANDOFF.md) | **当前状态 / 踩过的坑 / 未验证的缺口 / 下一步** |
| [`docs/ROADMAP.md`](docs/ROADMAP.md) | 进度台账（新规格与验收补跑） |
| [`AGENTS.md`](AGENTS.md) | 仓库硬约束（改代码前必读） |

## 还缺什么

按优先级排的完整列表在 [`docs/HANDOFF.md`](docs/HANDOFF.md) 的「未验证的缺口」，这里只提三条最影响使用的：

1. **登录**（缺口 4）：`authorize` 状态机写好了（13 个真状态、`request_for` 有真 schema 校验），
   但没接到 `fuyubot run` 上。这是「真能跑起来」的最后一环。
2. **打包/部署**（缺口 3）：`FUYUBOT_TDLIB_VENDORED_DIR` 指向**构建机**的 `target/.../out/vendored`，
   所以 `cargo install` 出来的二进制旁边没有 tdjson，优先级 2 会静默失效。
3. **进程退出**（缺口 8）：接了 TDLib 的路径**停不下来**（接收循环没有停止位），只能 Ctrl-C；
   侧效应是 `--replay` 故意不接 TDLib。

另外仓库目前**没有 `LICENSE` 文件**（未声明授权）。

## 参与开发

改之前先读 [`AGENTS.md`](AGENTS.md)：那里是硬约束（依赖方向、加载优先级、凭据不许落盘、
`Context` 只暴露 `&self` …）。提交前请过一遍它的检查清单，其中三条是硬门槛：

```bash
cargo build --workspace --offline   # 0 warning
cargo test  --workspace --offline   # 全绿
cargo fmt --all --check             # 0 diff
```
