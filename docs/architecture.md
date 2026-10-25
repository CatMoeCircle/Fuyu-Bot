# fuyubot 架构与数据流

结论先说：**一个宿主进程 + N 个动态库插件（cdylib）**。插件不认识 Rust，只认识 `HostApiVTable`；
宿主是唯一实现这张表的 crate（`fuyubot-loader`）。脚本插件（Python / JS / TS）不改变这个结构
——它们的 cdylib 里只多了一个子进程。本文只讲**结构与数据流**：坑在 [`pitfalls.md`](pitfalls.md)，
没验的在 [`gaps.md`](gaps.md)，被拍板的决定在 [`decisions.md`](decisions.md)，硬约束见 [`../AGENTS.md`](../AGENTS.md)。

## 1. crate 依赖方向

```
                          fuyubot-core                （Cordis 内核，不认识 Telegram）
                        ┌──────┬──────┬──────┬──────┐
                        │      │      │      │      │
                   event  service  tool  middleware │     四个机制 crate 只依赖 core，彼此不依赖
                        └──────┴──────┴──────┴──────┘
                                    ▲
fuyubot-abi ─────────────────────► fuyubot-loader ◄───── fuyubot-tdlib ──► fuyubot-tdlib-sys
（不依赖 core / 机制 crate）        （唯一宿主层）              （加载/循环/授权）（类型/元数据/FFI）
                                    ▲
                              fuyubot-cli（二进制 fuyubot）

插件侧（**在 FuyuBot-SDK 仓库**，通过 crates.io 依赖本仓的 `fuyubot-abi`）：
fuyubot-plugin-api ──► fuyubot-abi                 Rust 插件只用这一个 crate
fuyubot-sdk-script ──► fuyubot-plugin-api          Python / JS / TS 插件的 cdylib 用它
fuyubot-simplify   ──► fuyubot-tdlib-sys           TDLib 便捷封装（可选糖，重导 td_api）
```

| 边 | 允许吗 | 依据 |
|---|---|---|
| `fuyubot-core` → `fuyubot-tdlib` | 禁止 | AGENTS 约束 2 |
| `fuyubot-abi` → `fuyubot-core` | 禁止 | 同上 |
| `event`/`service`/`tool`/`middleware` 互相依赖 | 禁止 | AGENTS 约束 12 |
| `fuyubot-tdlib-sys` → 任何 fuyu crate | 禁止（它是最底层） | AGENTS 约束 2 |
| `fuyubot-loader` → 四个机制 crate + `tdlib` + `abi` + `core` | 允许（只有它） | AGENTS 约束 17 |

组合只发生在两处：`HostRuntime::new`（把四个机制 `install` 进 `Context`）与 `HostRuntime` 里的
15 个 `unsafe extern "C"` 桥。四个机制 crate 绝不彼此调用。

## 2. 启动顺序：先 attach TDLib，再 load 插件

```
Kernel::new() → kernel.start()
  → PluginLoader::new(kernel.context().clone())     // 装四个机制 + 生成 HostApiVTable
  → pipeline.attach_tdlib(client)                   // 把 TdClient 挂进 HostRuntime（不起循环）
  → loader.load_with_config(dll, {"script": ...})   // 逐个 dlopen 插件
  → pipeline.spawn_dispatch()                       // 拿到 ReceiveLoop，才开始分发
```

为什么不能反：`attach_tdlib` 之前 `HostRuntime::td_send` 走「没接 TDLib」分支——请求只留档、
返回合成请求号（起点 `SYNTHETIC_REQUEST_BASE = 1 << 48`），而插件在 `plugin_start` 里就发请求
（模板与示例都这样）。分发排在插件加载之后，否则插件漏订阅（AGENTS 约束 31）。
`--replay` 与 `--no-tdlib` 故意不 attach：回放是**离线验证**，接了反而需要账号（AGENTS 约束 38）。

## 3. 主链路：VTable → 更新管线 → 双通道路由 → 响应按归属回投

```
插件（任意语言）── host.on / invoke / emit / use_middleware / ...
  ▼  HostApiVTable：18 个字段 = version / size / host_ctx + 15 个函数指针（全是 Option<...>，
     每个函数的第一个参数都是 host: *mut c_void）
  ▼  HostRuntime：把每个桥翻译成「事件总线 / 服务容器 / 工具表 / 中间件管线 / TdClient」
  ├── td_invoke / td_invoke_simplified ──► HostRuntime::td_send
  │     接了就 TdClient::send（注入 @extra）／没接就留档 + 合成请求号 → record_request(id, owner, …)
  ├── td_execute ──► HostRuntime::td_execute ──► TdClient::execute（**同步**，不等接收循环）
  │     只用于不碰服务器的方法（`parseTextEntities` 之类）；没接 TDLib 就返回 NULL
  ▼
TdClient::spawn_receive_loop ──► Routed
  ├─ Update   ──► UpdatePipeline::process_value（六阶段 + 双通道）
  │     PreUpdate → 解析 @type → PreRoute → InPlugin → publish_update → PostResponse
  │     publish_update：emit("update", …)（兜底，收全部）+ emit(@type, …)（具体，如 updateNewMessage）
  └─ Response ──► UpdatePipeline::deliver_response
        request_id_of()：先看数值型 request_id，再回退解析字符串 @extra → request_owners.remove(id)
        给原 JSON 额外挂数值型 "request_id" → emit_scoped(owner, "response", payload)
```

`process`（吃 `&str`）只是 `process_value` 的前半段（解析 JSON 再交给它）；接收循环那条路
本来就有 `Value`，所以走 `process_value`。三条容易搞错的语义（AGENTS 约束 19/20/21）：

- **归属是「额外定向」，不是「排除广播」**：`emit` 发给所有订阅者，`emit_scoped(owner, …)` 是
  「全局 + 该归属」。响应因此对所有插件透明。
- **中间件一条更新跑一遍，不是每个监听器跑一遍**：`InPlugin` 的产出就是所有监听器看到的版本
  （按监听器各跑一遍的话，第 N 个会看到被改过 N 次的 payload）。
- **过滤器里的 `event_type` 键只用来选通道**，不参与负载匹配——等响应写
  `{"event_type":"response","request_id":N}`。

| 阶段 | 数值 | 谁跑它 |
|---|---|---|
| `Stage::PreUpdate` / `PreRoute` / `InPlugin` / `PostResponse` | 0 / 1 / 2 / 3 | `UpdatePipeline::process_value` |
| `Stage::PreToolCall` / `PostToolCall` | 4 / 5 | **目前没有生产调用方**（只有 `fuyubot-middleware` 自己的测试） |

六阶段的数值在 `fuyubot-abi::Stage`，与 `fuyubot-middleware::Stage` 一一对应，由 `runtime.rs` 的
`stage_numbers_match_the_middleware_crate` 测试钉住。

## 4. ABI 契约与注册作用域

三条必须知道的设计（改了会 UB，见 AGENTS 约束 15/16）：

- **每个 VTable 函数第一个参数是 `host: *mut c_void`**：Rust 的裸函数指针没有捕获环境，VTable 里放不了闭包；不用显式上下文参数就只能靠进程级全局，那样一个进程只能有一个宿主、测试也没法并行。`HostApi` 门面会自动带上这个指针，插件作者看不见它。
- **函数指针字段全是 `Option<...>`**：C 侧传 NULL 合法，非 `Option` 字段收到 NULL 就是 UB。`HostApi::from_raw` 把它们全部解包校验一遍，之后调用路径上没有 `Option`、也没有 panic 分支。
- **`size` 字段（紧跟 `version`）** 让插件判断宿主的表够不够大：老宿主 + 新插件时先比 `size` 再读字段。加上 `version`/`size`/`host_ctx`，VTable 一共 17 个字段，布局由 `const _: () = { ... }` 里的 `offset_of!` / `size_of` 断言钉死。

**注册作用域**是回滚的实现方式：`HostRuntime::plugin_scope()` 给这次加载分配一个 owner id，之后所有
跨 ABI 的注册（订阅 / 工具 / 服务 / 中间件）都记在这个 owner 名下（`RegistrationKind` + `Registration`）。
`scope.commit()` 保留；提前 drop（init 失败、start 报错、ABI 不匹配）或 `LoadedPlugin` 被 drop 时，
`rollback_owner(owner)` 一次性撤掉这批注册；宿主自己（owner 0）不受影响。
`LoadedPlugin` 的字段声明顺序就是卸载顺序：先 `plugin_stop`，再 `rollback_owner`，最后 `_lib: Library`
才 drop——到这一步动态库才真正卸载。

**唯一的例外是 Go 的 c-shared 库**：它的运行时有线程与信号处理器，卸载后进程会 access violation
（`0xC0000005`，而且崩在退出那一刻）。`PluginLoader` 用文件里的 `\xff Go buildinf:` 魔数认出 Go 产物，
对这类库 `plugin_stop` 与注册回滚照跑，但**故意 `mem::forget` 掉那个 `Library`**，让它映射到进程退出。
代价是「同一进程内反复加载同一个 Go 插件」不可靠（热重载路径），收益是 Go 插件能正常用（见 pitfalls 坑 90）。

## 5. 两种插件形态

```
动态库插件（Rust / C / C++）——同一地址空间、同一批线程
  导出 plugin_init / plugin_start / plugin_stop / plugin_free_string；回调 unsafe extern "C-unwind" fn(user_data, payload_json) -> i32
  数据不走序列化管道：回调直接收到宿主的临时 NUL 结尾字符串（只在下一次宿主调用前有效）
  代价是共享地址空间（崩了一起崩），收益是零拷贝、零协议

脚本插件（Python / JS / TS）——一个插件 = 一个解释器子进程
  plugin_start → Bridge::start：resolve_script(config_json)（配置里的 "script" 优先，否则用宏里写的名字）
    → spawn(python|python3|py | node|bun, script, config_json)：stdin/stdout = 管道，stderr = inherit
    → pump 线程读脚本 stdout（一行一个 JSON）→ 等 {"op":"ready"}（STARTUP_TIMEOUT 10 秒，不等就丢第一批事件）
  订阅回调 forward() 往子进程 stdin 写一行 JSON；宿主 ◄──▶ 桥 ◄── 一行一个 JSON ──▶ 解释器
```

| | 动态库插件 | 脚本插件 |
|---|---|---|
| 进程 / 边界 / 语言 | 与宿主同进程；C ABI 函数调用；Rust / C / C++ | 一个插件一个子进程；stdin/stdout 一行一个 JSON；Python（`python`/`python3`/`py`）、JS（`node`/`bun`）、TS（Node ≥ 23.6 类型剥离） |
| 脚本定位 | 不适用 | **脚本必须与 cdylib 同名同目录**（`find_script`；cargo 的 `lib` 前缀会被去掉） |
| 启动时序 / 崩溃 | 同步；崩了拖垮宿主 | `plugin_start` 阻塞等脚本报 `ready`；子进程死，宿主记一条错 |
| 已知天花板 | panic 靠 `C-unwind` + `catch_unwind` 接住 | 管道是同步写的：脚本一直不读 stdin 会拖住宿主那条线程 |

脚本侧协议见 `fuyubot-sdk-script` 的 crate 文档；两门语言共用同一份 Rust 桥，
所以「Python SDK 与 JS SDK 对 ABI 的理解」不可能漂移。

## 6. 四大机制与内核（阶段 2/3 的产物）

```
core：lib.rs（Kernel：Created → Started → Stopped）/ context.rs（Arc<RwLock<HashMap<TypeId, Arc<dyn Any+Send+Sync>>>>）
      effect.rs（Box<dyn FnOnce()+Send> 回滚守卫，#[must_use]，merge() 后注册者先撤销）/ plugin.rs（Plugin trait + define_plugin!）
      manager.rs（PluginManager：load/load_all/unload/unload_all/reload + DFS 三色拓扑排序）/ error.rs（KernelError）
event：EventBus / Filter / SubscriptionHandle / EventContextExt        service：ServiceContainer / ServiceHandle / ServiceContextExt
tool：ToolDescriptor / ToolRegistry / ToolHandle / ffi 模块 / ToolContextExt
middleware：Stage / Middleware / Next / Pipeline / MiddlewareHandle / MiddlewareContextExt
```
共用的三条约定：**回调错误类型由 core 定**（四个 crate 不能互相依赖，所以 `fuyubot_core::Result<T>` = `Box<dyn Error + Send + Sync>`，这是全仓唯一允许 boxed error 的地方）；**与 Context 的集成 = `install(ctx)` + 扩展 trait**（返回的 `Effect` 要一直持有，否则服务立刻从 Context 消失；服务缺失时错误消息里写明该怎么 install）；**所有注册都返回句柄且 `#[must_use]`**（四个 Handle 内部都包着 `core::Effect`，drop 即回滚）。

各机制的语义要点：

- **事件**：`emit` 在锁**外面**调回调，所以回调里再 `on`/`emit`/`off` 不会自锁；某个回调返回 `Err` 会**中断**本次广播并把错误抛出去。`Filter` 用点分路径（`"message.chat.id"`）做字段相等匹配。
- **服务**：命名容器（跨语言插件报不出 Rust 的 `TypeId`）。同名重复注册报错；`get::<T>` 类型不匹配返回 `None`，用 `contains` + `type_of` 区分「没注册」和「类型不对」。
- **工具**：`call_tool` = 查表 → 权限门 → input_schema → 处理器 → output_schema。`handler: Option<ToolHandlerFn>`，`None` 表示「只有元数据的声明型工具」；注册/注销用 `on_change` 广播（**不认识事件系统**，要不要转成事件总线由宿主层做）。
- **中间件**：洋葱模型，`next.run(ctx)` 前是进入段、后是退出段。`run(stage, ctx)` 会**先复制层列表再执行**，所以中间件里再 `use_middleware` 不会自锁；不调 `next.run` 就是不往下走。

内核几条语义：**`Context` 只有 `&self`**（`provide` 返回 `Effect`，drop 即注销，必须持有；重复注册同一类型直接报错，不静默覆盖）；**`Effect::merge` 是 LIFO**（`a.merge(b)` 撤销顺序 `b → a`）；**加载顺序由依赖决定，卸载是加载顺序的倒序**（`load_all` 报错会把本次已加载的卸掉，不留半加载）；**有依赖者时 `unload` 拒绝**（`HasDependents`，不级联）；`reload` 能工作是因为管理器存的是 `Arc<dyn Plugin>` 而不是 `Box`。

## 7. `fuyubot-tdlib-sys` + `fuyubot-tdlib`（阶段 1 + 2026 拆分）

```
crates/fuyubot-tdlib-sys/  ← 绑定层（-sys）：只出类型，不做加载
  build.rs：必需输入 td_api.tl 的获取 + 编译期两份产物（无条件执行，没有 feature 早退）
  src/：build_support.rs（两个构建脚本共享的缓存/下载设施，#[path] 同一份；cfg(test) 下跑自带测试）、
        ffi.rs（函数指针 + 符号名 + 指针形状 const assert）、tl_schema.rs（解析与 JSON 互转，build.rs 与运行时共用）、
        tl_codegen.rs + docs.rs（→ Rust 类型 / TL 标记 → rustdoc）、schema.rs（TdApiSchema）、td_api.rs（include! 生成物）
  tests/：generated_types.rs（生成类型自检）
crates/fuyubot-tdlib/  ← 运行时半边（依赖并重导出 sys）
  build.rs：只管 vendored 动态库 + 把库 staging 到可执行文件旁边（gaps 缺口 3）
  src/：loader.rs（四优先级；resolve / resolve_with / versioned_candidates / resolve_symbols）、
        loader/vendored.rs（资产映射、库文件名、sha256、tar.gz 解包、staging）、
        client.rs（TdClient、RequestId、PendingRequest、TdResponse/TdUpdate、route、spawn_receive_loop）、
        authorize.rs（log_in / log_in_with / question_for / request_for）
  tests/：integration.rs（真库 #[ignore]）+ sibling_dependencies.rs（独占进程）+ release_assets.rs
```

`fuyubot_tdlib::` 根上重导出 sys 的 `ffi` / `schema` / `td_api` / `tl_schema` 模块与 `TdApiSchema` /
`EntryKind` / `TlEntry` / `TlParam` / `SchemaError` / `TdApi` / `DEFAULT_LOG_VERBOSITY`——**既有调用方的
路径全部不变**。`src/build_support.rs` 与 `src/loader/vendored.rs` 都被**两边共用**（各自的 build.rs 用
`#[path]` 引同一份源码），所以它们只能依赖 std 与可选依赖；`[package.metadata.fuyubot]` 有两份，值必须
一致（守卫测试 `metadata_is_in_sync_between_the_two_crates`）。

运行时加载优先级（`loader::resolve`）：`TDLIB_TDJSON_PATH` 文件/目录 → **vendored 目录 → 可执行文件
所在目录（ExeDir）** → Linux 带版本号系统库 → 裸库名（真的去 dlopen 试一次）；全失败返回
`TdlibLoadError::NotFound`，消息里逐条列出 `[1]`…`[4]` 的尝试与原因（`fuyubot` 启动时会打印，可直接
拿来排查）。真正加载走 `loader::open_library`（Windows 绝对路径走 `LOAD_WITH_ALTERED_SEARCH_PATH`），
**不是 `Library::new`**——新增任何加载动态库的地方都走它（pitfalls 坑 43）。
`sys/src/ffi.rs` 的符号名是 per-client 那套（`td_json_client_*`，`execute` 两参数，坑 42）——编译期不校验
符号（没有 `#[link]`），只有真库能发现写错。`@extra` 是**字符串**（`RequestId::to_extra()`），`route()`
遇到非数字 `@extra` 会退化成更新而不是 panic。编译期注入两个环境常量：`FUYUBOT_TDLIB_VERSION`（无条件）与 `FUYUBOT_TDLIB_VENDORED_DIR`（仅 vendored 成功时）。

### `td_api.tl` 转成了什么（**只在编译期**）

```
              ┌─ tl_schema::parse ──> tl_codegen::generate ──> $OUT_DIR/td_api_generated.rs
td_api.tl ────┤                                                    └─ include! ─> fuyubot_tdlib_sys::td_api（Rust 类型）
（TDLIB_TD_API_TL                              └─> $OUT_DIR/td_api.json
  或缓存/Releases 下载）                        └─ include_str! ─> TdApiSchema::compiled（可校验元数据）
```

两份产物来自同一个解析结果，而且 `tl_schema.rs` 被 build.rs 用 `#[path]` 引同一份实现，所以「编译期
写出来的形状」与「运行时读回去的形状」不可能漂移。生成规则：类 → 带 `#[serde(tag = "@type")]` 的 enum；
函数 → 带 `METHOD` / `RESULT` 常量的 struct（派生 `Default`，没填的字段不写进请求）；类字段一律
`Option<Box<T>>`（断互递归 + 对上「缺字段就是没有」）；认不出的类型降级成 `serde_json::Value`；Rust
关键字转义（`r#type`）；**文档四级齐全**（类 / 变体 / 函数 / 字段），HTML 标记经 `docs.rs` 转 rustdoc
Markdown，真文件产物里 `<b>/<code>/<a>/<br>` 残留 **0**。`check_request` 那套元数据仍然保留——它做的
是生成类型做不到的本地校验（生成类型只能保证**结构**对，挡不住「用错方法名」）。归类只认
`---functions---` / `---types---` 分区标记（细节见 pitfalls 坑 40/41），认不出来时**构建直接失败**。
生成码由 `fuyubot-tdlib-sys/tests/generated_types.rs` 自检（只依赖 `message` / `messageText` /
`sendMessage` / `getMe` 四个真文件与样例都有的东西，换哪份 `td_api.tl` 都能跑）。

## 8. 关键类型对照

| 类型 / 函数 | 位置 | 作用 |
|---|---|---|
| `HostApiVTable` / `PluginHandle` / `HostApi` | `fuyubot-abi` | 函数表（18 字段） / `plugin_init` 的返回值（`abi_version == 0` = 失败） / 插件侧门面（解包一次，之后没有 `Option`） |
| `HostExt` / `Bridge` | `fuyubot-plugin-api` / `fuyubot-sdk-script`（**FuyuBot-SDK 仓库**） | 日志简写、`subscribe_json`、`wait_for_update_json`、`invoke_value` / 脚本插件的桥（spawn 解释器 + pump + 订阅转发） |
| `HostRuntime` / `UpdatePipeline` / `PluginLoader` / `LoadedPlugin` | `fuyubot-loader` | 15 个桥的落点、请求留档、响应回投、`rollback_owner` / 六阶段（实跑 4 个）+ 双通道路由 + `attach_tdlib`/`spawn_dispatch` / dlopen、`plugin_init`/`plugin_start`/`plugin_stop`、drop 即卸载 |
| `TdClient` / `Routed` / `ReceiveLoop` | `fuyubot-tdlib` | TDLib 客户端；`route()` 分出 `Update`/`Response`；停止把手（`stop()`/`join()`，drop **不会**停） |
| `Simplify` | `fuyubot-simplify` | `send_message` / `send_message_album` / `edit_message_*` / `delete_message` / `formatted`（Markdown→entities，走同步 `td_execute`）/ `wait_response` / `wait_for_update` / `resolve_entities` / `download_file`；参数是**每个方法一个结构体**（`SendMessageParams` 等），调用方不拼 TDLib 嵌套 JSON |
| `Bridge` | `fuyubot-sdk-script`（**FuyuBot-SDK 仓库**） | 脚本插件的桥：spawn 解释器 + pump + 订阅转发 |
| `TdApiSchema::compiled()` | `fuyubot-tdlib-sys` | 编译期烘焙的元数据；`check_request` 做本地方法名/参数名校验 |

**没验证的部分**全在 [`gaps.md`](gaps.md)（那里是唯一台账，本文不再复制一份）。
