# fuyubot 架构与数据流

结论先说：**一个宿主进程 + N 个动态库插件（cdylib）**。插件不认识 Rust，只认识
`HostApiVTable`；宿主是唯一实现这张表的 crate（`fuyubot-loader`）。
脚本插件（Python / JS）不改变这个结构——它们的 cdylib 里只多了一个子进程。

## 1. crate 依赖方向

```
                          fuyubot-core                （Cordis 内核，不认识 Telegram）
                        ┌──────┬──────┬──────┬──────┐
                        │      │      │      │      │
                   event  service  tool  middleware │     四个机制 crate 只依赖 core，彼此不依赖
                        └──────┴──────┴──────┴──────┘
                                    ▲
                                    │
fuyubot-abi ─────────────────────► fuyubot-loader ◄───── fuyubot-tdlib
（不依赖 core / 机制 crate）        （唯一宿主层）              （ffi / loader / client / schema）
                                    ▲
                                    │
                              fuyubot-cli（二进制 fuyubot）

插件侧（不依赖宿主）：
fuyubot-plugin-api ──► fuyubot-abi                 Rust 插件只用这一个 crate
fuyubot-sdk-script ──► fuyubot-plugin-api          Python / JS 插件的 cdylib 用它
fuyubot-simplify   ──► fuyubot-abi                 TDLib 便捷封装（可选糖）
```

| 边 | 允许吗 | 依据 |
|---|---|---|
| `fuyubot-core` → `fuyubot-tdlib` | 禁止 | AGENTS 约束 2 |
| `fuyubot-abi` → `fuyubot-core` | 禁止 | 同上 |
| `event`/`service`/`tool`/`middleware` 互相依赖 | 禁止 | AGENTS 约束 12 |
| `fuyubot-loader` → 四个机制 crate + `tdlib` + `abi` + `core` | 允许（只有它） | AGENTS 约束 17 |

组合发生在两处，且只有这两处：`HostRuntime::new`（把四个机制 `install` 进 `Context`）
与 `HostRuntime` 里的 14 个 `unsafe extern "C"` 桥。四个机制 crate 绝不彼此调用。

## 2. 启动顺序：先 attach TDLib，再 load 插件

`fuyubot-cli` 的 `run_async` 里顺序是定死的：

```
Kernel::new() → kernel.start()
  → PluginLoader::new(kernel.context().clone())     // 装四个机制 + 生成 HostApiVTable
  → pipeline.attach_tdlib(client)                   // 把 TdClient 挂进 HostRuntime，起 td_receive 循环
  → loader.load_with_config(dll, {"script": ...})   // 逐个 dlopen 插件
```

为什么不能反：`attach_tdlib` 之前，`HostRuntime::td_send` 走的是「没接 TDLib」分支——
请求只留档、返回一个合成请求号（起点 `SYNTHETIC_REQUEST_BASE = 1 << 48`）。
插件在 `plugin_start` 里就发请求（模板与示例都这样），顺序反了它的请求永远进不了真库。

`--replay` 与 `--no-tdlib` 故意不 attach：回放是离线验证，接了反而需要账号、
且接收循环停不下来（见 §6）。

## 3. 主链路：VTable → 更新管线 → 双通道路由 → 响应按归属回投

```
插件（任意语言）
  │  host.on / invoke / emit / use_middleware / ...
  ▼
HostApiVTable           17 个字段：version / size / host_ctx + 14 个函数指针（全是 Option<...>）
  │                      每个函数的第一个参数都是 host: *mut c_void
  ▼
HostRuntime             把每个桥翻译成「事件总线 / 服务容器 / 工具表 / 中间件管线 / TdClient」
  │
  ├── td_invoke / td_invoke_simplified ──► HostRuntime::td_send
  │                                         ├ 接了就 TdClient::send（注入 @extra）
  │                                         └ 没接就留档 + 合成请求号
  │                                         然后 record_request(id, owner, ...)
  ▼
TdClient::spawn_receive_loop ──► Routed
  │
  ├─ Routed::Update   ──► UpdatePipeline::process_value
  │     a. Stage::PreUpdate   中间件
  │     b. 解析 @type
  │     c. Stage::PreRoute    中间件
  │     d. Stage::InPlugin    中间件
  │     e. HostRuntime::publish_update：双通道广播
  │          emit("update", payload)              ← 兜底通道，收全部更新
  │          emit(@type,   payload)               ← 具体通道，如 updateNewMessage
  │     f. Stage::PostResponse 中间件
  │
  └─ Routed::Response ──► UpdatePipeline::deliver_response
        request_id_of(payload)：先看数值型 request_id，再回退解析字符串 @extra
        request_owners.remove(id) → owner
        给原 JSON 额外挂一个数值型 "request_id"
        emit_scoped(owner, "response", payload)   ← 只多送给发起者，全局订阅者照样收得到
```

三条容易搞错的语义（都在 AGENTS 约束 19/20/21）：

- **归属是「额外定向」，不是「排除广播」**。`emit` 发给所有订阅者；
  `emit_scoped(owner, …)` 是「全局 + 该归属」。响应因此对所有插件透明。
- **中间件一条更新跑一遍，不是每个监听器跑一遍**。`InPlugin` 的产出就是所有监听器看到的版本。
- **过滤器里的 `event_type` 键只用来选通道**，不参与负载匹配。等响应写
  `{"event_type":"response","request_id":N}`。

### 六阶段里谁被谁驱动

| 阶段 | 数值 | 谁跑它 |
|---|---|---|
| `Stage::PreUpdate` | 0 | `UpdatePipeline::process_value` |
| `Stage::PreRoute` | 1 | `UpdatePipeline::process_value` |
| `Stage::InPlugin` | 2 | `UpdatePipeline::process_value` |
| `Stage::PostResponse` | 3 | `UpdatePipeline::process_value` |
| `Stage::PreToolCall` | 4 | **目前没有生产调用方**（只有 `fuyubot-middleware` 自己的测试） |
| `Stage::PostToolCall` | 5 | 同上 |

六阶段的数值在 `fuyubot-abi::Stage`，与 `fuyubot-middleware::Stage` 一一对应，
由 `runtime.rs` 的 `stage_numbers_match_the_middleware_crate` 测试钉住。

### 注册作用域与回滚

`HostRuntime::plugin_scope()` 给这次加载分配一个 `owner`，之后所有跨 ABI 的注册
（订阅 / 工具 / 服务 / 中间件）都记在这个 owner 名下（`RegistrationKind` + `Registration`）。
`scope.commit()` 保留；提前 drop（init 失败、start 报错、ABI 不匹配）或 `LoadedPlugin`
被 drop 时，`rollback_owner(owner)` 一次性把这批注册撤掉。宿主自己（owner 0）不受影响。

`LoadedPlugin` 的字段声明顺序就是卸载顺序：先 `plugin_stop`，再 `rollback_owner`，
最后 `_lib: Library` 才 drop——到这一步动态库才真正卸载。

## 4. 两种插件形态

### 动态库插件（Rust / C / C++）

```
fuyubot 进程
├─ fuyubot-cli / HostRuntime / UpdatePipeline
└─ dlopen 进来的 cdylib ── 同一地址空间，同一批线程
     导出 plugin_init / plugin_start / plugin_stop / plugin_free_string
     回调：unsafe extern "C-unwind" fn(user_data, payload_json) -> i32
```

数据不走序列化管道：回调直接收到宿主的临时 NUL 结尾字符串。
代价是插件与宿主共享地址空间（崩了一起崩），收益是零拷贝、零协议。

### 脚本插件（Python / JS）

```
fuyubot 进程                                   子进程（job：一个插件 = 一个解释器）
├─ dlopen 的 cdylib（fuyubot-sdk-script）
│    ├─ plugin_start → Bridge::start
│    │    ├─ resolve_script(config_json)   配置里的 "script" 优先，否则用宏里写的名字
│    │    ├─ spawn(python|python3|py | node|bun, script, config_json)
│    │    │     stdin/stdout = 管道，stderr = inherit（脚本报错直接进宿主控制台）
│    │    ├─ pump 线程读脚本 stdout，一行一个 JSON
│    │    └─ 等 {"op":"ready"}（STARTUP_TIMEOUT 10 秒）↑ 不等就会丢第一批事件
│    └─ 订阅回调 forward() → 往子进程 stdin 写一行 JSON
└─ 宿主 ◄──▶ 桥 ◄── 一行一个 JSON ──▶ 解释器
```

| | 动态库插件 | 脚本插件 |
|---|---|---|
| 进程 | 与宿主同进程 | 一个插件一个子进程 |
| 边界 | C ABI 函数调用 | stdin/stdout，一行一个 JSON |
| 语言 | Rust / C / C++ | Python（`python`/`python3`/`py`）、JS（`node`/`bun`） |
| 脚本定位 | 不适用 | **脚本必须与 cdylib 同名同目录**（`scaffold::find_script`；cargo 的 `lib` 前缀会被去掉） |
| 启动时序 | 同步 | `plugin_start` 阻塞等脚本报 `ready` |
| 崩溃影响 | 拖垮宿主 | 子进程死，宿主记一条错 |
| 已知天花板 | panic 靠 `C-unwind` + `catch_unwind` 接住 | 管道是同步写的：脚本一直不读 stdin 会拖住宿主那条线程 |

脚本侧协议见 `fuyubot-sdk-script` 的 crate 文档；两门语言共用同一份 Rust 桥，
所以「Python SDK 与 JS SDK 对 ABI 的理解」不可能漂移。

## 5. 关键类型对照

| 类型 / 函数 | 位置 | 作用 |
|---|---|---|
| `HostApiVTable` | `fuyubot-abi` | 宿主交给插件的函数表（17 字段） |
| `PluginHandle` | `fuyubot-abi` | `plugin_init` 的返回值；`abi_version == 0` 表示失败 |
| `HostApi` | `fuyubot-abi` | 插件侧门面：`from_raw` 解包一次，之后没有 `Option` |
| `HostExt` | `fuyubot-plugin-api` | 日志简写、`subscribe_json`、`wait_for_update_json`、`invoke_value` |
| `HostRuntime` | `fuyubot-loader` | 14 个桥的落点、请求留档、响应回投、`rollback_owner` |
| `UpdatePipeline` | `fuyubot-loader` | 六阶段（实跑 4 个）+ 双通道路由 + `attach_tdlib`/`dispatch` |
| `PluginLoader` / `LoadedPlugin` | `fuyubot-loader` | dlopen、`plugin_init`/`plugin_start`/`plugin_stop`、drop 即卸载 |
| `TdClient` / `Routed` | `fuyubot-tdlib` | TDLib 客户端；`route()` 把接收到的 JSON 分成 `Update` / `Response` |
| `Simplify` | `fuyubot-simplify` | `send_message` / `wait_for_update` / `resolve_entities` / `download_file` |
| `Bridge` | `fuyubot-sdk-script` | 脚本插件的桥：spawn 解释器 + pump + 订阅转发 |

## 6. 没验证的部分

- **真实 Telegram 登录流程没接**：没有 api_id/api_hash，`setTdlibParameters` 之后的授权、
  真 `updateNewMessage` 都没跑过；`fuyubot-cli` 的 `login_note` 会明确提醒这一点。
- **收真更新没验过**：管线只用 `examples/replay/*.json` 与手造 JSON 走过。
- **接收循环停不下来**：`spawn_receive_loop` 没有退出条件，`attach_tdlib` 也不返回句柄，
  所以接了 TDLib 只能靠 Ctrl-C 结束，`plugin_stop` / `Kernel::stop` 走不到。
- **工具处理器跨不了 ABI**：`register_tool` 只带元数据，插件注册的工具一律 `handler: None`。
