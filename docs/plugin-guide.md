# 插件开发指南

结论先说：**一个插件 = 一个 cdylib**（`.dll` / `.so` / `.dylib`），四个导出符号
`plugin_init` / `plugin_start` / `plugin_stop` / `plugin_free_string` 就是全部契约。
Rust / C / C++ 直接写它；Python / JS 用 `fuyubot-sdk-script` 生成的胶水 cdylib
去启动解释器。契约细节见 [`abi-contract.md`](abi-contract.md)。

## 0. 通用流程

```bash
fuyubot init --name mybot --api-id 0 --api-hash ""   # 生成 fuyubot.toml + plugins/（运行时 CLI）
fuyubot-sdk init demo --lang rust                    # 生成 plugins/demo/（开发 CLI）
cd plugins/demo && cargo build --release             # 用**你自己的**工具链编译
cd ../.. && fuyubot plugin install plugins/demo      # 把产物装进插件目录
fuyubot run                                          # 加载插件
```

> **两个 CLI 分工**（`docs/ROADMAP.md` 变更 4/5）：`fuyubot` 管运行，`fuyubot-sdk` 管开发；
> **两个都不打包**——编译是插件作者用自己的语言工具链完成的事
> （Rust `cargo build`、C/C++ `gcc -shared`、Python `maturin`、JS `napi build`）。

- `fuyubot init` 生成的 `fuyubot.toml` 三个键都有默认值，最简可以只有 `[bot]`；
  重复 `init` 会被挡住（不覆盖你改过的配置）。`plugins.dir` 默认 `"plugins"`。
- 插件名只允许字母、数字和 `.` `_` `-`（它会被拼进源码和 TOML）；目录名里的 `.` / `-`
  换成 `_` 当 crate 名（`fuyubot.weather` → `fuyubot_weather`）。
- `fuyubot plugin list` 会**真的加载一次**再看名字（插件名只有 dlopen 之后才知道）；
  `fuyubot plugin install <文件或工程目录>` 手动装一个（会连旁边的 `.py`/`.js` 一起拷）。

### `fuyubot build-plugin`（**过渡期命令**）

新规格里它不该存在（构建/打包不归两个 CLI）；它现在还在 `fuyubot` 里，因为 `examples/`
的流程靠它。上表的流程用「作者工具链 + `plugin install`」就不需要它：

| 行为 | 说明 |
|---|---|
| 找工程 | `--dir` 没给就取当前目录；当前目录不像插件工程时，去插件目录下找唯一的一个 |
| 选编译路径 | 有 `Cargo.toml` → `cargo build --message-format=json`；否则找 `.c`/`.cpp` → 调 C 编译器。`--lang` 可强制 |
| 读回产物 | cargo 那条用 JSON 消息拿**真正的 cdylib 路径**，不猜 `target/<profile>/` |
| C/C++ 刷新头文件 | 每次编译前把 cbindgen 生成的 `fuyubot_plugin.h` 重新落到工程目录（头文件是产物，不该签进仓库） |
| 装进插件目录 | 默认**顺手安装**（`--no-install` 关掉）：往上找到 `fuyubot.toml`，把 cdylib + 同目录的 `.py`/`.js`/`.pyi`/`.d.ts` 一起拷进 `plugins.dir` |
| profile / 联网 | `--release` 走 release；cargo 那条会**自动**加 `--offline`（判据是 `FUYUBOT_OFFLINE` / `CARGO_NET_OFFLINE` 非空且不为 `0`/`false`），不是命令行开关 |
| C/C++ 编译器 | 查找顺序 `CC` → `cc` / `gcc` / `clang` → MSVC（经 `vswhere` 找 `vcvars64.bat`）。MSVC 那条会加 `/utf-8`，不加会把 UTF-8 源码里的中文注释拆坏成看不懂的语法错误 |

### 环境变量

| 变量 | 作用 |
|---|---|
| `FUYUBOT_SDK_PATH` | 指向**装着 SDK crate 的那个目录**（仓库里就是 `crates/`）。生成的插件用 `path = "..."` 依赖 SDK，因为这两个 crate 还没发到 crates.io。一个变量管 `fuyubot-plugin-api` 与 `fuyubot-sdk-script` 两个。没设就按仓库布局（`CARGO_MANIFEST_DIR` 的上一级）找 |
| `FUYUBOT_OFFLINE` | 非空、且不是 `0`/`false` 时，`build-plugin` 给 cargo 加 `--offline` |

### 脚本插件的一条硬约定

**脚本必须与 cdylib 同名、同目录**：`weather.dll` ↔ `weather.py`，`demo.dll` ↔ `demo.js`，
`counter.dll` ↔ `counter.ts`。`find_script` 依次找 `.py` / `.js` / `.ts`，按**动态库文件名**
去找（cargo 在类 Unix 上加的 `lib` 前缀会被去掉），把**绝对路径**写进插件的 config JSON
（`{"script": "..."}`）——cdylib 自己不知道自己在哪个目录。找不到就按宏里写的名字、
以当前目录为基准找。

> **TypeScript 不需要 `tsc`**：宿主起的是 `node counter.ts`，靠 Node 自带的**类型剥离**跑
> （Node ≥ 23.6 默认开；22.6+ 要 `--experimental-strip-types`）。所以 TS 里只能用能剥掉的语法
> （注解 / `interface` / `type` / `as`），**不能用** `enum` / `namespace` / 构造器参数属性。
> Node 太老就自己 `tsc` 编成 `.js`——**编译与打包是插件作者自己的工具链**。
> 完整可跑的例子见 [`../examples/ts-counter`](../examples/ts-counter)。

## 1. Rust

最小可用插件（就是 `fuyubot-sdk init --lang rust` 生成的 `templates/rust/src/lib.rs`，
`__NAME__` 已替换成插件名）：

```rust
use fuyubot_plugin_api::{HostApi, HostExt, Plugin, Subscription, define_plugin};

struct MyPlugin {
    host: HostApi,
    /// 订阅句柄**必须活着**：它 drop 的那一刻就退订了。
    _updates: Option<Subscription>,
}

impl Plugin for MyPlugin {
    fn init(host: HostApi) -> Result<Self, String> {
        host.info("demo: init");
        Ok(Self { host, _updates: None })
    }

    fn start(&mut self, config_json: &str) -> Result<(), String> {
        self.host.info(&format!("demo: start({config_json})"));
        // 订阅兜底通道；要精确就换成 "updateNewMessage" 之类具体的 @type。
        self._updates = self
            .host
            .subscribe_json("update", None, on_update, std::ptr::null_mut());
        Ok(())
    }
}

extern "C-unwind" fn on_update(_user: *mut std::ffi::c_void, payload: *const std::ffi::c_char) -> i32 {
    let update = unsafe { fuyubot_plugin_api::parse_payload(payload) };
    let update_type = update.get("@type").and_then(|v| v.as_str()).unwrap_or("?");
    println!("demo: 收到 {update_type}");
    0
}

define_plugin!(MyPlugin, "demo");
```

- `define_plugin!` **一个 crate 只能展开一次**（四个符号名是 ABI 定死的）。
- 回调签名必须 `extern "C-unwind"`：`extern "C"` 边界上逃出去的 panic 直接 abort。
- 回调没有捕获环境，状态经 `user_data` 传（示例见 [`../examples/rust-echo`](../examples/rust-echo)）。

## 2. C

`fuyubot-sdk init ping --lang c` 会写出 `ping.c` 与 `fuyubot_plugin.h`。
四个导出符号的骨架（完整模板见 `crates/fuyubot-sdk-cli/templates/c/plugin.c`，逻辑是 `/ping` → 回 `pong`）：

```c
#include "fuyubot_plugin.h"

static const HostApiVTable *g_host = NULL;
static int g_state = 0;              /* PluginHandle::context 必须非空 */

static int32_t on_update(void *user_data, const char *payload) {
    /* 判正文 == "/ping"，然后： */
    g_host->td_invoke_simplified(g_host->host_ctx, "sendMessage", params);
    g_host->log(g_host->host_ctx, 3, "ping 插件：回了 pong");
    return 0;
}

FUYUBOT_EXPORT PluginHandle plugin_init(const HostApiVTable *host) {
    PluginHandle failed = {0, 0, NULL, NULL};
    if (host == NULL || host->version != ABI_VERSION) return failed;
    g_host = host;
    PluginHandle handle = { ABI_VERSION, (uint32_t)sizeof(PluginHandle), &g_state, "ping" };
    return handle;
}

FUYUBOT_EXPORT int32_t plugin_start(PluginHandle handle, const char *config_json) {
    SubscriptionHandle s = g_host->on(g_host->host_ctx, "updateNewMessage", NULL, on_update, NULL);
    return s.id == 0 ? -1 : 0;
}

FUYUBOT_EXPORT void plugin_stop(PluginHandle handle) { g_host = NULL; }
FUYUBOT_EXPORT void plugin_free_string(char *ptr) { (void)ptr; }
```

- `#define ABI_VERSION` 来自 `fuyubot_plugin.h`（cbindgen 从 Rust 定义生成，不要手改）。
- 编译：`fuyubot build-plugin --lang c`（会先把头文件刷新到目录里）。
- `plugin_start` / `plugin_stop` 可省略（只登记元数据的插件可以不写）；
  `plugin_init` 与 `plugin_free_string` 必须有。
- 模板里那段 JSON 取值是**朴素查找**，不处理转义、不处理同名嵌套键；认真写就引一个 JSON 库。

## 3. C++

同 C，**头文件同一份**，唯一差别是四个导出符号必须放在 `extern "C"` 里——
C++ 会改函数名，宿主就 dlsym 不到了。其余差异只是用 `std::string` 拼 JSON。
完整模板见 `templates/cpp/plugin.cpp`。编译：`fuyubot build-plugin --lang cpp`。

## 4. Python

逻辑写在 `.py` 里，`src/lib.rs` 只有一行胶水：

```rust
fuyubot_sdk_script::script_plugin!("demo", python, "demo.py");
```

生成的 `demo.py`（模板 `templates/python/plugin.py`）：

```python
import fuyubot_plugin as host

@host.on("updateNewMessage")
def on_message(payload):
    host.log("demo 收到一条更新")

@host.on("updateNewMessage", filter={"message.content.text.text": "/hello"})
def on_hello(payload):
    host.log("有人说 /hello")

host.run()
```

- `fuyubot_plugin.py` 与 `fuyubot_plugin.pyi`（桥与类型声明）由 `fuyubot-sdk init` 一起放进目录，
  与 `fuyubot-sdk-script` 里那份**逐字相同**。
- 可用的函数：`log` / `on` / `off` / `invoke` / `emit` / `register_tool` / `config` / `run`。
  `invoke(method, params, on_response)` 的 `on_response` 按 `request_id` 配对，脚本不用自己记请求号。
- **插件调 `emit` 不是全局广播**：宿主桥成 `emit_scoped(自己, ..)`，只送给**自己 + 全局订阅者**
  （见 [`abi-contract.md`](abi-contract.md) 的 `emit` 一行）。要影响别的插件，让它订阅全局事件。
- **`host.run()` 必须是最后一行**：宿主等脚本报 `{"op":"ready"}` 才算加载完成，
  早退会被判成加载失败（超时 10 秒）。
- 解释器依次试 `python` / `python3` / `py`；桥会把 stdio 钉成 UTF-8（Windows 默认代码页会把中文拆坏）。
- 调试：`python demo.py` 能跑（桥自带自检：`python fuyubot_plugin.py`），只是没有宿主可说话。

## 5. JS / TS

```rust
fuyubot_sdk_script::script_plugin!("demo", js, "demo.js");
```

生成的 `demo.js`（模板 `templates/js/plugin.js`）：

```js
const host = require("./fuyubot_plugin.js");

host.on("updateNewMessage", (payload) => {
  host.log("demo 收到一条更新");
});

host.on("updateNewMessage", (payload) => {
  host.log("有人说 /hello");
}, { "message.content.text.text": "/hello" });

host.run();
```

- 可用的函数：`log` / `on` / `off` / `invoke` / `emit` / `registerTool` / `config` / `run`。
  类型声明在 `fuyubot_plugin.d.ts`。
- 解释器依次试 `node` / `bun`；`host.run()` 同样必须是最后一行。自检：`node fuyubot_plugin.js`。

## 6. `--replay`：不接账号也能验插件

`fuyubot run --replay <文件>` 把一批更新 JSON 灌进管线后退出：**不接 TDLib、不用账号**，
请求只留档。文件可以是单个 JSON 对象或对象数组。

```bash
fuyubot init --dir . --name demo                     # 建项目（生成 fuyubot.toml + plugins/）
fuyubot build-plugin --dir examples/rust-echo        # 编译 + 装进 plugins/
fuyubot build-plugin --dir examples/c-ping --lang c
fuyubot build-plugin --dir examples/python-weather
fuyubot run --replay examples/replay/new_message.json
fuyubot run --replay examples/replay/weather.json    # python-weather 要联网访问 wttr.in
```

输出里能看到三样东西：每条回放的 `UpdateOutcome`（`update_type` / `listeners` / `warnings`）、
插件打出来的日志、插件发出去的请求（`#id method params`）。

`--replay-wait <MS>` 是「回放后等插件的静默窗口」，不填时按插件形态自动选：有脚本插件 2000ms，
纯原生插件 150ms。脚本在另一个进程里、反应要经管道回来，还可能捎带网络请求，窗口太短会把
最终答复漏掉（上限 60 秒）。两个样例文件的内容与预期结果见
[`../examples/README.md`](../examples/README.md)。
