# examples：五个最小插件

结论先说：**这些示例从没在真实 Telegram 环境里验证过**——仓库里没有 api_id/api_hash。
`fuyubot run --replay` 是当前**唯一**可复现的验证方式：它不接 TDLib、不用账号，
把一批更新 JSON 灌进管线后退出。（`run` 本身已经接上 TDLib 与登录状态机，
但没有真账号端到端跑过「收真更新 → 插件回一条」，见 `docs/gaps.md` 缺口 4。）

| 例子 | 语言 | crate / 产物 | 做什么 |
|---|---|---|---|
| `rust-echo` | Rust | `fuyubot-example-echo` → `fuyubot_example_echo.dll` | 订阅 `updateNewMessage`，把正文**原样**回过去（`sendMessage`）。跳过 `is_outgoing`（否则自激）、正文空着不回 |
| `c-ping` | C | 裸 `ping.c` → `c_ping.dll`（**不是** workspace 成员） | 订阅 `updateNewMessage`，正文等于 `/ping` 就回 `pong`。C 侧最短闭环：`plugin_init` 拿 VTable → `plugin_start` 订阅 → 回调里判正文 → `td_invoke_simplified` |
| `go-ping` | Go（cgo / c-shared） | `main.go` + ABI 薄壳 `plugin.c` → `ping.dll`（**不是** workspace 成员） | 和 `c-ping` 同一个行为，逻辑写在 Go 里（`encoding/json` + `td_invoke_simplified`）。见 [`go-ping/README.md`](go-ping/README.md)；Go 的库**不会被卸载**（loader 识别后故意留着） |
| `python-weather` | Python + Rust 胶水 | `fuyubot-example-weather`，`[lib] name = "weather"` → `weather.dll` + `weather.py` | 逻辑全在 `weather.py`：`/weather <城市>` 去 wttr.in 查天气再回过去。Rust 那 9 行只是 ABI 胶水 |
| `ts-counter` | TypeScript + Rust 胶水 | `fuyubot-example-counter`，`[lib] name = "counter"` → `counter.dll` + `counter.ts` | 逻辑全在 `counter.ts`：数**每个聊天**收到多少条，回一条「本聊天第 N 条消息」。顺带证明 `fuyubot-sdk codegen` 落的 `.d.ts` 能给 TS 用，而且**不需要 `tsc`**（Node ≥ 23.6 自带类型剥离）。见 [`ts-counter/README.md`](ts-counter/README.md) |

两个脚本插件的 `[lib] name`（`weather` / `counter`）**不能省**：宿主是**按动态库文件名**
去找同名脚本的（`scaffold::find_script`，认 `.py` / `.js` / `.ts`），
默认名 `fuyubot_example_weather.dll` 就找不到 `weather.py`。

`examples/replay/` 里是回放用的更新样例：

| 文件 | 内容 | 打给谁 |
|---|---|---|
| `new_message.json` | 3 条 `updateNewMessage`：`/ping`、一句中文、一条 `is_outgoing` | `c-ping` 只命中第 1 条；`rust-echo` 命中前 2 条并跳过第 3 条 |
| `weather.json` | 1 条 `/weather 上海` | `python-weather`（**要联网**访问 wttr.in） |
| `counter.json` | 3 条 `updateNewMessage`，分属两个聊天（A、A、B） | `ts-counter` → 三条 `sendMessage`：`第 1 条` / `第 2 条` / `第 1 条`（**B 的计数独立**） |

## 编译与跑通（Windows，实际用过的命令）

> **本 CLI 不编译插件**（`fuyubot build-plugin` 已按新规格拆掉，见 `docs/decisions.md` 变更 4/5）：
> 编译是作者自己工具链的事，`fuyubot plugin install` 只**复制**已经编好的产物。
> 关键一点：`plugin install <工程目录>` 只看**工程自己的** `target/{release,debug}`，
> 而 workspace 成员的产物默认落在**仓库根**的 `target/` 下——所以候选 A 用 `--target-dir`
> 把产物落到工程自己那里，脚本同伴（`.py`/`.js`/`.ts`）也会跟着被拷。

```powershell
# 0. 构建 fuyubot 二进制要用仓里的离线缓存（td_api.tl 是构建必需输入）。
#    必须写**绝对路径**：build.rs 的 CWD 是 crate 根，写相对路径会被解析到
#    crates/fuyubot-tdlib/ 下面，然后下载 404。
$env:FUYUBOT_TDLIB_VENDORED_CACHE = "F:\Rust\FuyuBot\.tdlib-cache"
cargo build -p fuyubot-cli --offline

# 1. 在仓库根建一个项目（生成 fuyubot.toml + plugins/）
.\target\debug\fuyubot.exe init --dir . --name demo

# 2. 用作者工具链编，再装进 plugins/
#    A. 有脚本或想让产物落在工程里：加 --target-dir，然后指着工程目录装
cargo build -p fuyubot-example-echo    --target-dir examples\rust-echo\target --offline
cargo build -p fuyubot-example-weather --target-dir examples\python-weather\target --offline
cargo build -p fuyubot-example-counter --target-dir examples\ts-counter\target --offline
.\target\debug\fuyubot.exe plugin install examples\rust-echo
.\target\debug\fuyubot.exe plugin install examples\python-weather      # 连 weather.py / fuyubot_plugin.py 一起拷
.\target\debug\fuyubot.exe plugin install examples\ts-counter          # 连 counter.ts / fuyubot_plugin.js 一起拷

#    B. c-ping 不是 workspace 成员，走 C 编译器那条路：
#       头文件是产物（没签进仓库），先用 SDK CLI 刷一份，再用你自己的 cl/gcc 编
cargo run -q -p fuyubot-sdk-cli --offline -- codegen --lang c --dir examples\c-ping
$vcvars = & "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe" `
    -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 `
    -property installationPath
$vcvars = Join-Path $vcvars "VC\Auxiliary\Build\vcvars64.bat"
New-Item -ItemType Directory -Force examples\c-ping\target\debug | Out-Null
cmd /c "call `"$vcvars`" >nul && cl /nologo /utf-8 /LD /Iexamples\c-ping examples\c-ping\ping.c /Fe:examples\c-ping\target\debug\c_ping.dll"
.\target\debug\fuyubot.exe plugin install examples\c-ping

#    C. go-ping 用 cgo 编（需要 C 工具链：Windows 上是 mingw-w64，Linux/macOS 用 cc）
cargo run -q -p fuyubot-sdk-cli --offline -- codegen --lang c --dir examples\go-ping
$env:CGO_ENABLED = "1"; $env:CC = "gcc"
Push-Location examples\go-ping; go build -buildmode=c-shared -o ping.dll .; Pop-Location
.\target\debug\fuyubot.exe plugin install examples\go-ping\ping.dll

# 3. 离线回放
.\target\debug\fuyubot.exe run --replay examples\replay\new_message.json
.\target\debug\fuyubot.exe run --replay examples\replay\weather.json
.\target\debug\fuyubot.exe run --replay examples\replay\counter.json
```

真跑出来的样子（`plugin install` 与回放各摘一段）：

```text
PS> .\target\debug\fuyubot.exe plugin install examples\python-weather
已安装 F:\…\plugins\weather.dll
已安装 F:\…\plugins\fuyubot_plugin.py
已安装 F:\…\plugins\fuyubot_plugin.pyi
已安装 F:\…\plugins\weather.py

PS> .\target\debug\fuyubot.exe plugin install examples\c-ping
已安装 F:\…\plugins\c_ping.dll

PS> .\target\debug\fuyubot.exe run --replay examples\replay\new_message.json --plugin-dir <插件目录>
已加载插件 fuyubot.example.ping (<插件目录>\c_ping.dll)
已加载插件 fuyubot.example.echo (<插件目录>\fuyubot_example_echo.dll)
  #281474976710656 sendMessage {"@type":"sendMessage","chat_id":-1001234567890,"text":"pong"}
  #281474976710657 sendMessage {"@type":"sendMessage","chat_id":-1001234567890,"text":"/ping"}
  #281474976710658 sendMessage {"@type":"sendMessage","chat_id":-1001234567890,"text":"你好，echo 插件"}
```

- **workspace 成员的产物默认在仓库根 `target/`**，所以 `plugin install examples\<工程>` 会报
  「没找到 .dll」并提示改用作者工具链——那句提示不是让你去装什么东西，是让你加 `--target-dir`
  或直接指着产物装（见上面 A/B 两条）。
- **`plugin install <单个 .dll>` 不会拷脚本**：它只在 dll 同目录找同伴。脚本插件
  （`python-weather` / `ts-counter`）请传**工程目录**（A 那条）。
- `c-ping` 的 C 头文件 `examples/c-ping/fuyubot_plugin.h` 是**产物**（`codegen` 落的，没签进仓库），
  所以第一次得先跑一次 `codegen`；MSVC 那条 `/utf-8` 不能省（源码与头文件都是 UTF-8，
  不加会把中文注释拆坏成看不懂的语法错误）。
- 一次装了多个也不要紧：回放时每个订阅者都会被打到，输出里能分别看到日志与请求。

## 回放输出长什么样

`--replay` 会打印三样东西：

1. 每条更新的 `UpdateOutcome`：`update_type` / `listeners`（实际被调到的监听器个数）/ `warnings`；
2. **插件打出来的日志**（`HostRuntime::log_lines`，上限 256 行）；
3. **插件发出去的请求**（`#请求号 方法名 参数`）。回放不接 TDLib，所以请求只留档不发——
   这正是「离线能看到插件干不干对」的原因。

`rust-echo` 对 `new_message.json` 的合理结果：第 1、2 条各产生一条 `sendMessage`，第 3 条没有。
`c-ping` 只对第 1 条产生一条 `{"chat_id":-1001234567890,"text":"pong"}` 的 `sendMessage`。

脚本插件（Python）在另一个进程里跑：`--replay` 默认等 2000ms 的静默窗口
（纯原生插件 150ms），可以用 `--replay-wait <MS>` 覆盖。`weather.json` 那条还要等
wttr.in 的网络往返，网络不通时插件会把错误原样回过去（这是它在 `.py` 里写死的行为）。

## 如实说明

- **没在真实 Telegram 里验过**：没有 api_id/api_hash，所以走不到真实登录与真实更新。
  `rust-echo` / `c-ping` / `weather.py` 发送的参数形状都是按 TDLib 文档写的。
- **`--replay` 覆盖不到的东西**：真库的响应、真实更新字段是否和样例一样、
  `sendMessage` 的参数 TDLib 认不认。回放只能证明「插件收到更新后做出了正确判断、发出了正确请求」。
- **`python-weather` 的回放也要联网**：它调用 wttr.in。想完全离线就只跑 `new_message.json`。
- 想让示例跑到真环境里，需要：`fuyubot.toml` 里填真实 api_id、环境变量给 `TG_API_HASH` /
  `TG_BOT_TOKEN`（凭据**不落盘**，见 `AGENTS.md` 约束 41）、一份可用的 `tdjson`，
  然后 `fuyubot run`（它会驱动登录）。**这一步还没有真账号证据。**
