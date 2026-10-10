# 插件 ABI 契约（`fuyubot-abi`）

结论先说：这是**唯一**的跨语言契约，形状只有 `fuyubot-abi/src/lib.rs` 一份实现，
C 头文件 `fuyubot_plugin.h` 由 cbindgen 从它生成（`crates/fuyubot-abi/build.rs`）。
宿主侧的 14 个桥全在 `fuyubot-loader/src/runtime.rs`。**改 ABI 就是改这个文件 + 加版本号。**

## 1. `HostApiVTable` 的 17 个字段

布局由 `const _: () = { … }` 里的 `offset_of!` / `size_of` 断言钉死：
`version` 在 0、`size` 在 4、`host_ctx` 在 8，总大小 = `2 * size_of::<u32>() + 15 * size_of::<*const c_void>()`。

| # | 字段 | 签名（省略第一个参数 `host: *mut c_void` 之外的形态） | 宿主实现 | 说明 |
|---|---|---|---|---|
| — | `version` | `u32` | `ABI_VERSION` | 必须等于 `ABI_VERSION`；首个字段就是它，插件能在读其它任何字段之前先比一次 |
| — | `size` | `u32` | `size_of::<HostApiVTable>()` | 老宿主 + 新插件时先比 `size` 再读尾部字段，读越界就是 UB |
| — | `host_ctx` | `*mut c_void` | `&HostRuntime`（`Arc::new_cyclic` 里取的 `weak.as_ptr()`） | 每个函数都原样回传；插件作者看不见它（`HostApi` 自动带上） |
| 1 | `on` | `(update_type: *const c_char, filter_json: *const c_char, callback: Option<EventCallbackFn>, user_data: *mut c_void) -> SubscriptionHandle` | `api_on` | 订阅。`filter_json` 里的 `event_type` 键选通道，其余键做负载匹配；`NULL`/空串/`"null"` = 收全部 |
| 2 | `off` | `(handle: SubscriptionHandle)` | `api_off` | 退订；句柄 drop 时宿主也会整批回滚 |
| 3 | `emit` | `(event_type: *const c_char, payload_json: *const c_char) -> i32` | `api_emit` | 发自定义事件；`emit_scoped(current_owner, …)`，插件自己发的事件只送给它自己 + 全局订阅者 |
| 4 | `register_tool` | `(descriptor_json: *const c_char) -> ToolHandle` | `api_register_tool` | 只登记元数据。本版 ABI 没有处理器指针，登记出来的一律是 `handler: None` 的声明型工具 |
| 5 | `call_tool` | `(name: *const c_char, input_json: *const c_char) -> RequestId` | `api_call_tool` | 走 `ToolRegistry::call_tool`（查表 → 权限门 → schema → 处理器）；没有处理器就返回无效请求号 |
| 6 | `list_tools` | `() -> *const c_char` | `api_list_tools` | 全部工具的元数据 JSON 数组。**宿主线程局部缓冲**，读完即弃 |
| 7 | `provide_service` | `(name: *const c_char, vtable: *const ServiceVTable) -> ServiceHandle` | `api_provide_service` | 注册一个命名服务；`vtable` 必须活到注销为止 |
| 8 | `get_service` | `(name: *const c_char) -> *const ServiceVTable` | `api_get_service` | 取回同一个指针；没有就是 `NULL` |
| 9 | `td_invoke` | `(method_json: *const c_char) -> RequestId` | `api_td_invoke` | 发完整 TDLib 请求 JSON（含 `@type`） |
| 10 | `td_invoke_simplified` | `(method_name: *const c_char, params_json: *const c_char) -> RequestId` | `api_td_invoke_simplified` | 给参数补上 `@type = method_name`；参数不是对象就整份替换成 `{"@type": …}` |
| 11 | `use_middleware` | `(stage: u32, callback: Option<MiddlewareCallbackFn>, user_data: *mut c_void) -> MiddlewareHandle` | `api_use_middleware` | 挂在某个阶段；回调返回非 0 → 本阶段中断 |
| 12 | `wait_for_update` | `(filter_json: *const c_char, timeout_ms: u64) -> WaitHandle` | `api_wait_for_update` | **阻塞**当前线程。`timeout_ms == 0` 表示只检查当前状态 |
| 13 | `cancel_wait` | `(handle: WaitHandle)` | `api_cancel_wait` | 从另一个线程取消等待 |
| 14 | `log` | `(level: u32, message: *const c_char)` | `api_log` | 落进宿主环形缓冲（`LOG_CAPACITY = 256`）并打一行到 stderr；非法 level 按 `Info` 处理 |

两条形状上的硬要求（AGENTS 约束 15）：

- **每个函数的第一个参数都是 `host: *mut c_void`**。Rust 的裸函数指针没有捕获环境，
  VTable 里放不了闭包；不用显式上下文参数就只能靠进程级全局，那样一个进程只能有一个宿主、测试也没法并行。
- **每个函数指针字段都是 `Option<...>`**。C 侧传 `NULL` 合法；非 `Option` 字段收到 `NULL` 就是 UB。
  `HostApi::from_raw` 会一次解包校验全部 14 个，之后调用路径上没有 `Option`、也没有 panic 分支。

相关类型：`SubscriptionHandle` / `ToolHandle` / `ServiceHandle` / `MiddlewareHandle` / `RequestId`
都是 `#[repr(C)]` + 单个 `u64`（断言钉住 `size_of == size_of::<u64>()`）；
`LogLevel` = 1..5（`Error`/`Warn`/`Info`/`Debug`/`Trace`），`Stage` = 0..5；
`WaitHandle` = `{ id: u64, matched: u32, _reserved: u32, payload_json: *const c_char }`。

## 2. 插件必须导出的四个符号

| 符号 | 类型 | 必须吗 | 说明 |
|---|---|---|---|
| `plugin_init` | `PluginInitFn = unsafe extern "C" fn(*const HostApiVTable) -> PluginHandle` | **必须** | 校验 VTable、构造插件状态。返回失败句柄（`abi_version == 0`）表示拒绝加载 |
| `plugin_start` | `PluginStartFn = unsafe extern "C" fn(PluginHandle, *const c_char) -> i32` | 可选 | 返回 0 表示成功；只登记元数据的插件可以不写 |
| `plugin_stop` | `PluginStopFn = unsafe extern "C" fn(PluginHandle)` | 可选 | 卸载前调一次；Rust 宏在这一步把状态收回 `Box` 释放 |
| `plugin_free_string` | `PluginFreeStringFn = unsafe extern "C" fn(*mut c_char)` | **必须** | 释放**插件自己**分配的字符串。宿主从不 free 插件的指针 |

符号名由常量钉住（`SYM_PLUGIN_INIT` / `SYM_PLUGIN_START` / `SYM_PLUGIN_STOP` / `SYM_PLUGIN_FREE_STRING`），
`PluginLoader::load_with_config` 按这些名字 `lib.get(...)`。C++ 里必须放在 `extern "C"` 块里。

`PluginHandle` = `{ abi_version: u32, size: u32, context: *mut c_void, name: *const c_char }`；
`is_valid()` 的判据是「`abi_version != 0` 且 `context` 非空」，所以插件的 `context` 不能是 `NULL`。
`PluginHandle::FAILED` 是全零版本号，`const _` 里有 `assert!(PluginHandle::FAILED.abi_version == 0)`。

## 3. 内存所有权：三条硬规矩

抄自头文件（`build.rs` 的 `HEADER`，改契约只改这一处）：

1. **谁分配谁释放**。插件通过 `plugin_init` 返回的 `name`、以及插件自己分配的任何字符串，
   都由插件持有；宿主绝不 free 它们。插件要给宿主一个需要接管的临时字符串，只能走
   `plugin_free_string` 那套约定。
2. **宿主返回的字符串是线程局部缓冲**。`list_tools`、`get_service`、`WaitHandle::payload_json`
   指向宿主的一块复用缓冲（`thread_local! HOST_STRING`），**只在下一次调用宿主函数之前有效**。
   要留着就当场拷走，并且**不要试图释放它**——宿主 VTable 里没有 host 侧的 free 函数，
   这是设计，不是遗漏。
3. **`ServiceMethodFn` 的返回值由提供方持有**。调用方读完即弃。

## 4. 版本协商

| 位置 | 谁比 | 不匹配的后果 |
|---|---|---|
| `ABI_VERSION`（当前 `1`） | 编译期常量，两侧共同引用 | — |
| `HostApiVTable::version` | 插件侧 `HostApi::from_raw` | `AbiError::VersionMismatch { expected, found }` |
| `HostApiVTable::size` | 同上 | `AbiError::VTableTooSmall { needed, found }` |
| 某个函数指针是 `NULL` | 同上 | `AbiError::MissingFunction("<字段名>")` |
| `PluginHandle::abi_version` | 宿主侧 `PluginLoader::load_with_config` | `LoadError::AbiMismatch { plugin, host }` |

版本不兼容时宿主**不能**调 `plugin_stop`——那是对一个不认识的 ABI 下手，
所以插件在 `init` 里分配的状态这一版回收不了（HANDOFF 缺口 7）。
字段只增不改，新增一律追加在尾部。

## 5. 为什么本 ABI 一个可变参数函数指针都没有

Rust 1.99 的 `extern "C" fn(.., ...)` 确实稳定（能写、cbindgen 也能生成），
但**本 ABI 一个都没用**，理由只有一条：`...` 会把**参数类型和个数校验全部丢掉**。
VTable 里 14 个函数的参数都是固定的，用 `...` 只会让 C 侧传错参数从编译错误降级成运行期 UB。

将来真要加，正确的声明形状是（C 侧）：

```c
typedef void (*FuyubotLogFn)(uint32_t level, const char *format, ...);
// Rust 侧对应：unsafe extern "C" fn(level: u32, format: *const c_char, ...)
```

两条必须知道的事：

- 变参部分**不能**是唯一的参数——C 要求 `...` 之前至少有一个具名参数。
- **变参函数指针跨不了 ABI 的 `Option` 优化**：`Option<fn(..., ...)>` 不是 niche 优化，
  占**两个指针宽**，必须显式判空。本 crate 里那条
  `size_of::<Option<unsafe extern "C" fn()>>() == size_of::<*const c_void>()` 断言
  只对**非变参**函数指针成立；普通函数指针字段能享受 niche 优化（`NULL` 就是 `None`），
  变参的不能——这也是「整个 VTable 所有字段都能是 `Option<...>` 且不撑大布局」的前提之一。

## 6. 五种语言各自声明的兼容 ABI 版本

| 语言 | 声明在哪 | 值 | 怎么生效 |
|---|---|---|---|
| C | 头文件 `fuyubot_plugin.h` 里的 `#define ABI_VERSION 1` | 1 | 模板里写 `if (host == NULL \|\| host->version != ABI_VERSION) return failed;` |
| C++ | 同一份 `fuyubot_plugin.h` | 1 | 同上（四个符号放 `extern "C"`） |
| Rust | `fuyubot_plugin_api::SDK_ABI_VERSION`（= `ABI_VERSION`）、`COMPATIBLE_HOST_ABI_VERSIONS = &[ABI_VERSION]`、`is_compatible(host_abi_version)` | 1 | `define_plugin!` 把它填进 `PluginHandle::abi_version`；宿主拿这个值跟自己的 `ABI_VERSION` 比 |
| Python | **没有自己的常量** | — | 走同一条 Rust 桥：`.py` 由 `fuyubot-sdk-script` 编出的 cdylib 驱动，`abi_version` 由那个 cdylib 填 |
| JS / TS | **没有自己的常量** | — | 同上，与 Python 共用 `fuyubot-sdk-script` 这一份实现 |

也就是说：脚本侧 SDK（`fuyubot_plugin.py` / `fuyubot_plugin.js` 与它们的类型声明）
声明的是**「一行一个 JSON」的协议**，不是 ABI 版本；ABI 版本永远由承载它的那份
Rust cdylib 声明。Python 与 JS 共用同一份桥，所以「两个 SDK 对 ABI 的理解」不可能漂移。

## 7. 回调的 ABI：`C-unwind`

`EventCallbackFn` 与 `MiddlewareCallbackFn` 都是 `unsafe extern "C-unwind" fn(...)`。
原因：`extern "C"` 边界上逃出去的 panic 会直接 **abort 整个进程**，宿主来不及 `catch_unwind`。
`C-unwind` 允许 unwind 穿过边界，宿主就能接住、记一条日志、继续跑下一条更新。
对 C/C++ 而言两者 ABI 完全一致；C++ 异常同样会穿过这个边界，所以请在回调内部处理完异常再返回。

`define_plugin!` 生成的 `plugin_init` / `plugin_start` / `plugin_stop` 自己也把插件代码包在
`catch_unwind` 里，两层都做是必要的。
