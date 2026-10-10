# examples：四个最小插件

结论先说：**这四个示例从没在真实 Telegram 环境里验证过**——仓库里没有 api_id/api_hash，
真实登录流程也没接（见 `docs/HANDOFF.md` 缺口 4）。`fuyubot run --replay` 是当前**唯一**
可复现的验证方式：它不接 TDLib、不用账号，把一批更新 JSON 灌进管线后退出。

| 例子 | 语言 | crate / 产物 | 做什么 |
|---|---|---|---|
| `rust-echo` | Rust | `fuyubot-example-echo` → `fuyubot_example_echo.dll` | 订阅 `updateNewMessage`，把正文**原样**回过去（`sendMessage`）。跳过 `is_outgoing`（否则自激）、正文空着不回 |
| `c-ping` | C | 裸 `ping.c` → `c_ping.dll`（**不是** workspace 成员） | 订阅 `updateNewMessage`，正文等于 `/ping` 就回 `pong`。C 侧最短闭环：`plugin_init` 拿 VTable → `plugin_start` 订阅 → 回调里判正文 → `td_invoke_simplified` |
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

```powershell
# 0. 构建 fuyubot 二进制要用仓里的离线缓存（td_api.tl 是构建必需输入）。
#    必须写**绝对路径**：build.rs 的 CWD 是 crate 根，写相对路径会被解析到
#    crates/fuyubot-tdlib/ 下面，然后下载 404。
$env:FUYUBOT_TDLIB_VENDORED_CACHE = "F:\Rust\FuyuBot\.tdlib-cache"
cargo build -p fuyubot-cli --offline

# 1. 在仓库根建一个项目（生成 fuyubot.toml + plugins/）
.\target\debug\fuyubot.exe init --dir . --name demo

# 2. 编插件并装进 plugins/（build-plugin 默认顺手安装，不用再 plugin install）
.\target\debug\fuyubot.exe build-plugin --dir examples\rust-echo
.\target\debug\fuyubot.exe build-plugin --dir examples\c-ping --lang c
.\target\debug\fuyubot.exe build-plugin --dir examples\python-weather
.\target\debug\fuyubot.exe build-plugin --dir examples\ts-counter

# 3. 离线回放
.\target\debug\fuyubot.exe run --replay examples\replay\new_message.json
.\target\debug\fuyubot.exe run --replay examples\replay\weather.json
.\target\debug\fuyubot.exe run --replay examples\replay\counter.json
```

> `ts-counter` 也可以完全不走 `build-plugin`：`cargo build -p fuyubot-example-counter`
> 之后把 `counter.dll` + `counter.ts` + `fuyubot_plugin.js` 放进同一个目录，
> 再用 `run --plugin-dir <那个目录>` 跑（见 [`ts-counter/README.md`](ts-counter/README.md)）。

- `build-plugin` 会往上找 `fuyubot.toml` 决定装到哪个插件目录；没找到就只编译、跳过安装。
- `rust-echo` / `python-weather` / `ts-counter` 是 workspace 成员，产物落在**仓库根**的 `target/debug/`；
  `build-plugin` 靠 `cargo --message-format=json` 读回真实路径，所以不用管这一点。
- `c-ping` 不是 workspace 成员，走 C 编译器那条路：需要 `cc`/`gcc`/`clang`（或设 `CC`），
  或者装 Visual Studio 的 C++ 生成工具（`build-plugin` 用 `vswhere` 找 `vcvars64.bat`）。
  `build-plugin` 每次会先把 `fuyubot_plugin.h` 刷新到
  `examples/c-ping/` 再编译——**那个头文件是产物，没签进仓库**（`ping.c` 要编译它，
  所以第一次得先跑一次 `build-plugin`）。
- 只装一个插件也行：`fuyubot plugin list` 看当前插件目录里有什么，
  `fuyubot plugin install <文件或工程目录>` 手动装（**工程目录**那条会把 `.py`/`.js`/`.ts`/`.pyi`
  一起拷；传单个 `.dll` 时看不到脚本在哪，所以脚本插件请传目录）。
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

- **没在真实 Telegram 里验过**：没有 api_id/api_hash，`fuyubot run` 接上 TDLib 之后也走不到
  真实登录与真实更新。`rust-echo` / `c-ping` / `weather.py` 发送的参数形状都是按 TDLib 文档写的。
- **`--replay` 覆盖不到的东西**：真库的响应、真实更新字段是否和样例一样、
  `sendMessage` 的参数 TDLib 认不认。回放只能证明「插件收到更新后做出了正确判断、发出了正确请求」。
- **`python-weather` 的回放也要联网**：它调用 wttr.in。想完全离线就只跑 `new_message.json`。
- 想让示例跑到真环境里，需要：`fuyubot.toml` 填真实 api_id/api_hash + 接上登录流程
  （**目前还没实现**，`fuyubot run` 只会提醒配置里有凭据）以及一份可用的 `tdjson`。
