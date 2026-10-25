# 插件开发指南

结论先说：**一个插件 = 一个 cdylib**（`.dll` / `.so` / `.dylib`），四个导出符号
`plugin_init` / `plugin_start` / `plugin_stop` / `plugin_free_string` 就是全部契约。
Rust / C / C++ 直接写它；Python / JS 用 `fuyubot-sdk-script`（在 **FuyuBot-SDK** 仓库）
生成的胶水 cdylib 去启动解释器。契约细节见 [`abi-contract.md`](abi-contract.md)。

## 0. 通用流程

```bash
fuyubot init --name mybot --api-id 0                # 生成 fuyubot.toml + plugins/（运行时 CLI）
fuyubot-sdk init demo --lang rust                    # 生成 plugins/demo/（开发 CLI）
cd plugins/demo && cargo build --release             # 用**你自己的**工具链编译
cd ../.. && fuyubot plugin install plugins/demo      # 把产物（连同脚本）装进插件目录
fuyubot run                                          # 加载插件
```

> **两个 CLI 分工**（[`decisions.md`](decisions.md) 变更 4/5）：`fuyubot` 管运行，`fuyubot-sdk` 管开发；
> **两个都不打包**——编译是插件作者用自己的语言工具链完成的事
> （Rust `cargo build`、C/C++ `gcc -shared` / `cl /LD`、Python `maturin`、JS `napi build`）。
> 运行时 CLI 里**没有** `build-plugin`（2026 拆掉）：它只按 dll 同目录找脚本，不替你编译。

- `fuyubot init` 生成的 `fuyubot.toml` 三个键都有默认值，最简可以只有 `[bot]`；
  重复 `init` 会被挡住（不覆盖你改过的配置）。`plugins.dir` 默认 `"plugins"`。
- 插件名只允许字母、数字和 `.` `_` `-`（它会被拼进源码和 TOML）；目录名里的 `.` / `-`
  换成 `_` 当 crate 名（`fuyubot.weather` → `fuyubot_weather`）。
- `fuyubot plugin list` 会**真的加载一次**再看名字（插件名只有 dlopen 之后才知道）；
  `fuyubot plugin install <文件或工程目录>` 手动装一个（**传工程目录**时会把旁边的
  `.py`/`.js`/`.ts`/`.pyi` 一起拷；传单个 dll 时看不到脚本在哪，脚本插件请传目录）。

### 产物该放哪：workspace 成员要 `--target-dir`

`plugin install <工程目录>` 只看 `<工程>/target/{release,debug}` 与工程目录本身。
如果插件是**当前 workspace 的成员**（比如 SDK 仓库的 `examples/*`），`cargo build -p <crate>`
默认把产物放在**仓库根**的 `target/`，于是它找不到。两种写法都行（在 SDK 仓库根敲）：

```powershell
# A. 让产物落在工程自己的 target/ 下（脚本同伴也会被一起拷）
cargo build -p fuyubot-example-weather --target-dir examples\python-weather\target
fuyubot plugin install examples\python-weather

# B. 直接指着产物装（适合单个原生插件；脚本插件用 A，否则脚本不会被拷）
cargo build -p fuyubot-example-echo
fuyubot plugin install target\debug\fuyubot_example_echo.dll
```

### 环境变量

| 变量 | 作用 |
|---|---|
| `FUYUBOT_SDK_PATH` | **可选**：指向装着 SDK crate 的目录（FuyuBot-SDK 仓库的 `crates/`）。生成的插件用 `path = "..."` 依赖 SDK；不设就写 crates.io 版本号（默认）。一个变量管 `fuyubot-plugin-api` 与 `fuyubot-sdk-script` 两个 |

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
> 完整可跑的例子见 [FuyuBot-SDK 的 `examples/ts-counter`](https://github.com/xiaoqvan/FuyuBot-SDK/tree/main/examples/ts-counter)。

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
- 回调没有捕获环境，状态经 `user_data` 传（示例见 [FuyuBot-SDK 的 `examples/rust-echo`](https://github.com/xiaoqvan/FuyuBot-SDK/tree/main/examples/rust-echo)）。

## 2. C

`fuyubot-sdk init ping --lang c` 会写出 `ping.c` 与 `fuyubot_plugin.h`。
四个导出符号的骨架（完整模板见 SDK 仓库的 `crates/fuyubot-sdk-cli/templates/c/plugin.c`，逻辑是 `/ping` → 回 `pong`）：

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
- 编译（**用你自己的 C 工具链**，运行时 CLI 不编译插件）：
  `fuyubot-sdk codegen --lang c --dir <工程目录>` 先把头文件刷新进去，然后
  `cl /nologo /utf-8 /LD /I <工程目录> <工程目录>/ping.c /Fe:<工程目录>/target/debug/ping.dll`
  （MSVC 要先 `call vcvars64.bat`；gcc/clang 则是 `cc -shared -fPIC -I <工程目录> … -o …`）。
  编完 `fuyubot plugin install <工程目录>`。
  ⚠️ MSVC 的 `/utf-8` **必须给**：源码与生成的头文件都是 UTF-8，不加会按系统代码页读，
  中文注释被拆坏成「注释提前结束、后面全是语法错误」这种看不出原因的样子。
- `plugin_start` / `plugin_stop` 可省略（只登记元数据的插件可以不写）；
  `plugin_init` 与 `plugin_free_string` 必须有。
- 模板里那段 JSON 取值是**朴素查找**，不处理转义、不处理同名嵌套键；认真写就引一个 JSON 库。

## 3. C++

同 C，**头文件同一份**，唯一差别是四个导出符号必须放在 `extern "C"` 里——
C++ 会改函数名，宿主就 dlsym 不到了。其余差异只是用 `std::string` 拼 JSON。
完整模板见 `templates/cpp/plugin.cpp`。编译同 C，也是两步：
`fuyubot-sdk codegen --lang cpp --dir <工程目录>` → 你自己的 C++ 工具链（`cl /LD` / `g++ -shared`）。

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
# 编 + 装（作者工具链；示例工程在 FuyuBot-SDK 仓库，产物落在那边仓库根的 target/）
cargo build -p fuyubot-example-echo --target-dir <SDK仓库>/examples/rust-echo/target
fuyubot plugin install <SDK仓库>/examples/rust-echo
fuyubot-sdk codegen --lang c --dir <SDK仓库>/examples/c-ping   # C 插件先刷头文件，再自己调 cl/gcc
fuyubot plugin install <SDK仓库>/examples/c-ping
fuyubot run --replay examples/replay/new_message.json
fuyubot run --replay examples/replay/weather.json    # python-weather 要联网访问 wttr.in
```

输出里能看到三样东西：每条回放的 `UpdateOutcome`（`update_type` / `listeners` / `warnings`）、
插件打出来的日志、插件发出去的请求（`#id method params`）。

`--replay-wait <MS>` 是「回放后等插件的静默窗口」，不填时按插件形态自动选：有脚本插件 2000ms，
纯原生插件 150ms。脚本在另一个进程里、反应要经管道回来，还可能捎带网络请求，窗口太短会把
最终答复漏掉（上限 60 秒）。两个样例文件的内容与预期结果见
[FuyuBot-SDK 的 examples/README](https://github.com/xiaoqvan/FuyuBot-SDK/blob/main/examples/README.md)。
