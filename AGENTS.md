# AGENTS.md — fuyubot 仓库约定

基于 TDLib 的 Telegram bot/user 框架，Cargo workspace，插件通过 C ABI 动态库加载。
**本仓（Fuyu-Bot）只放运行时**：内核、TDLib 接入层、四大机制、插件 ABI、宿主运行时、
TDLib 便捷封装、更新分发管线、`fuyubot` CLI 与内置插件。
代码分三个仓库（2026 拆分）：

| 仓库 | 放什么 |
| --- | --- |
| **Fuyu-Bot**（本仓） | 运行时全部 + `fuyubot-abi` + `fuyubot-tdlib-sys`（后两个发 crates.io） |
| FuyuBot-SDK | `fuyubot-plugin-api`、`fuyubot-sdk-script`、`fuyubot-sdk-cli`（`fuyubot-sdk` CLI）+ 五种语言示例 |
| FuyuBot-Docs | Fumadocs 文档站（`website/` 已整体搬过去） |

跨仓依赖只有一条、单向：SDK 通过 **crates.io** 依赖本仓的 `fuyubot-abi` / `fuyubot-tdlib-sys`；
**本仓不得依赖 SDK 仓库的任何 crate**（见约束 51）。
`fuyubot-plugin-demo` 会被集成测试真的 dlopen 起来，走一遍「VTable → 更新管线 → 响应回投」。
**真 TDLib 已经跑通**（本机 1.8.68 的 `tdjson.dll` + 真 `td_api.tl`）：编译期转换出三千多条、
`getMe` 往返、同步 `execute`、以及「插件 `td_invoke` → 真库 → 响应按归属回投」都验过；
没接 TDLib 时请求仍只留档 + 合成请求号，整条链路照样能被测通。
（条数**取决于构建时用的那份 `td_api.tl`**：本机 `.tdlib-cache/1.8.68` 这份是 3325 条
（函数 1065 / 构造器 2257 / 未归类 3）；1.8.66 是 3178，以前还有一份 3214——换文件条数就变，
别把某个数字当成常量写进断言。）
`td_api.tl` 是构建的必需输入，所以**先读 [`docs/HANDOFF.md`](docs/HANDOFF.md) 开头的「当前的构建契约」**，
再看 [`docs/SESSION-SUMMARY.md`](docs/SESSION-SUMMARY.md)（最近一次工作的账：需求 → 证据 → 被推翻的决策）。

## 硬约束（改动前必须确认，不要"顺手优化"）

1. **工具链**：Rust 1.99.0 stable + Edition 2024。禁止任何 nightly feature，禁止 `#![feature(...)]`。
2. **依赖方向**（`cargo tree` 必须能验证）：
   - `fuyubot-core` **不得**依赖 `fuyubot-tdlib`
   - `fuyubot-abi` **不得**依赖 `fuyubot-core`（也不依赖任何机制 crate）
   - `fuyubot-event`/`service`/`tool`/`middleware` 只依赖 `fuyubot-core`（见约束 12）
   - `fuyubot-tdlib-sys` 是**最底层的绑定 crate**：不得依赖任何 fuyu crate，正常依赖只有
     serde / serde_json / thiserror（ureq/toml 只在它的构建期，见约束 9）
   - `fuyubot-tdlib` 依赖 `fuyubot-tdlib-sys`（类型/元数据/FFI 都在 sys，它自己只剩加载与循环），
     并**重导出** sys 的公开 API——已有调用方的 `fuyubot_tdlib::TdApiSchema` 等路径不变
   - `fuyubot-loader` 是唯一的宿主层，依赖 `fuyubot-abi` + `fuyubot-core` + 四个机制 crate + `fuyubot-tdlib`（见约束 17）
3. **依赖版本只写在根 `Cargo.toml` 的 `[workspace.dependencies]`**，成员 crate 一律用 `serde.workspace = true` 这种写法。
   根 `[workspace.package]` 已固定 `edition` / `rust-version` / `version`，成员用 `xxx.workspace = true` 继承。
4. **profile 只写在根 `Cargo.toml`**（Cargo 规定）。`[profile.debug]` 是自定义 profile，靠 `inherits = "dev"` 工作，用 `cargo build --profile debug`。
5. **每个 crate 都要有 `README.md`**，一句话说明职责。
6. JSON Schema 只用 `fuyubot-tool`；动态库加载只用 `fuyubot-loader`；`cbindgen` 只是 `fuyubot-abi` 的 build-dependency。
7. **`fuyubot-tdlib` 是运行时加载，不是静态链接**：禁止给 tdjson 加 `#[link(name = ...)]`，禁止让任何二进制在编译期依赖 tdjson 存在。符号一律走 `ffi::TdApi` + `loader::resolve_symbols`。
8. **`fuyubot-tdlib` 的加载优先级顺序不要重排**：`TDLIB_TDJSON_PATH` → vendored →
   **可执行文件所在目录** → Linux 带版本号的系统库 → 裸库名。改它必须同步改 `loader::tests` 的断言（见约束 48）。
9. **`tar`/`flate2`/`sha2` 只服务 vendored 的动态库；`ureq` 是常开的 build-dependency**（因为 `td_api.tl` 是必需输入，任何构建都要能下载它，见约束 25）。运行时的**默认依赖图里不能出现它们**——构建期依赖不进运行时：
   - `cargo tree -p fuyubot-tdlib -e normal` 应当只有 fuyubot-tdlib-sys / libloading / serde_json / thiserror / tokio；
   - `cargo tree -p fuyubot-tdlib-sys -e normal` 应当只有 serde / serde_json / thiserror。
   另外 sys 的 `http_get` 标了 `#[cfg(not(test))]`：`build_support` 在测试下编译进来只为跑缓存布局测试，
   别把 ureq（连带 ring 的 C 代码）拖进测试编译。
10. **`Context` 只暴露 `&self`**：注册一律走 `provide` 并**持有**返回的 `Effect`（drop 即注销）。禁止为了“方便”给 `Context` 加 `&mut` 接口或 `insert/get_mut` 那一套——`Plugin::apply` 的签名就是 `&Context`，加回去整个内核就不成立了。
11. **`Effect` 是 `#[must_use]`，不要在测试或生产代码里让它被无意丢弃**：要么 `let guard = ...` 存起来，要么显式 `drop(guard)` / `guard.rollback()`。`let _ =` 会立刻触发回滚。
12. **`fuyubot-event` / `fuyubot-service` / `fuyubot-tool` / `fuyubot-middleware` 只依赖 `fuyubot-core`，彼此不许依赖**。校验方式：`cargo tree -p <crate> -e normal --depth 1` 里除了 `fuyubot-core` 只应看到各自的外部依赖。它们之间的组合（比如把工具注册广播转成事件总线的消息）由宿主层做。
13. **与 `Context` 的集成一律走「`install(ctx)` + 扩展 trait」**：`EventContextExt` / `ServiceContextExt` / `ToolContextExt` / `MiddlewareContextExt`。对应的服务不在 Context 里时，扩展方法必须报一条**能指路**的错误（消息里带上 `install` 那一行该怎么写）。
14. **扩展 trait 的方法名不能和 `Context` 的固有方法重名**：固有方法优先于 trait 方法，重名会让 trait 方法根本调不到。类型化服务已经占了 `provide`/`get`，所以命名服务叫 `provide_named`/`resolve`——不要为了"对齐需求文档"把它们改回去。
15. **`HostApiVTable` 的每个函数第一个参数都是 `host: *mut c_void`，而且每个函数指针字段都是 `Option<...>`**。前者是因为 Rust 的裸函数指针没有捕获环境；后者是因为 C 侧传 NULL 合法，非 `Option` 字段收到 NULL 就是 UB。两者都**不要**为了"看起来干净"改掉。
16. **跨 ABI 的字符串：谁分配谁释放**。插件返回的指针宿主一律不 free（插件用 `plugin_free_string` 自己放）；宿主返回给插件的字符串是**线程局部缓冲**，只在下一次宿主调用之前有效——所以 `HostApiVTable` 里没有 host 侧的 free 函数。
17. **`fuyubot-loader` 是唯一的宿主层**：它是唯一允许同时依赖 `fuyubot-event`/`service`/`tool`/`middleware`/`tdlib` 的 crate，`HostApiVTable` 的每个函数都在那里桥到 Context。**新的桥一律放这里**，不许为了图方便让两个机制 crate 互相依赖（约束 12 不变）。
18. **新增公开 ABI 类型时必须同步 `crates/fuyubot-abi/build.rs` 的 `export.include`**：cbindgen 只导出被函数引用到的类型，漏了 C 头文件里就没有它。
19. **事件归属（scope）的语义是「额外定向」，不是「排除广播」**：`emit` 发给**所有**订阅者，`emit_scoped(scope)` 发给「全局 + 该归属」。改成「全局 emit 只发 scope 0」会让插件收不到更新（集成测试会立刻红）。
20. **更新同时发到两条通道**：具体 `@type`（`updateNewMessage` 等）与兜底通道 `"update"`。订阅者按 `@type` 精确订阅、按 `"update"` 收全部。
21. **过滤器里的 `event_type` 键是选通道用的，不参与负载匹配**。`wait_for_update` 靠它决定订阅哪条通道（例如 `{"event_type":"response","request_id":5}` 等响应）。
22. **插件的回调与中间件回调必须是 `C-unwind`**：`extern "C"` 边界上逃出去的 panic 直接 abort，宿主**来不及** `catch_unwind`。同理 `define_plugin!` 必须自己把插件代码的 panic 包在 `catch_unwind` 里。
23. **一个 crate 只能展开一次 `define_plugin!`**：四个导出符号名是 ABI 定死的，展开两次就是重复定义。一个 cdylib 就是一个插件，这是设计而不是限制。
24. **`td_api.tl` 的「函数 vs 构造器」只能靠 `---functions---` / `---types---` 分区标记判定**。不要用「名字首字母大写是否等于返回类型」这类启发式：`updateNewMessage ... = Update` 是构造器，但大写之后是 `UpdateNewMessage` ≠ `Update`，任何这种启发式都会把它误判成函数。没有标记时归类为 `EntryKind::Unknown`——宁可不判也不猜。
    **真实 TDLib 的文件只有 `---functions---` 一个标记**，它前面的一切都是类型段；所以「有函数标记、又没有显式 `---types---`」时，标记之前的条目按类型段算。另外文件开头有一段**内建前奏**（`double ? = Double;`、`vector {t:Type} # [ t ] = Vector t;`），按**语法形状**（裸 `?` 参数、`{}`/`#`/`[]`）判成 `Unknown`，不当类生成——判据是形状，不是名字。
25. **`td_api.tl` 是构建的必需输入，不属于 vendored 那一套**。`fuyubot-tdlib-sys/build.rs` 会**无条件**去拿它：
    `TDLIB_TD_API_TL`（显式指定，优先级最高）→ 缓存（`<缓存根>/<版本>/td_api.tl`）→ 从 Releases 下载。
    下载成功即入缓存，一台机器只下这一次。**拿不到就让构建失败**——不存在「没有就不转换」这条路；
    `ureq` 因此是**常开**的 build-dependency（只在构建期，不进运行时依赖图）。它平台无关，缓存路径不带 `os-arch`。
    **缓存根是一串候选**（`build_support::cache_roots()`，在 `crates/fuyubot-tdlib-sys/src/build_support.rs`，
    两个 build.rs 用 `#[path]` 共享**同一份**文件，查找与落盘用**同一串**，顺序即优先级）：
    `FUYUBOT_TDLIB_VENDORED_CACHE` → `$CARGO_HOME/fuyubot-tdlib-vendored` →
    **项目本地 `<工作区根>/.tdlib-cache`**（往上找带 `[workspace]` 的 `Cargo.toml` 才认）。
    ⚠️ **`$OUT_DIR` 只能当最后保底，不能当缓存**：cargo 每次重跑构建脚本都会清掉它，
    于是「退到 OUT_DIR」= 每次构建**重新下载**（用户 2026 报的正是这个：`$CARGO_HOME` 在
    DSH 沙箱里只读）。项目本地那一档才是受限环境里“下过一次就不再下”的答案。
    缓存根全写不进去也**不让构建失败**（见 [`docs/pitfalls.md`](docs/pitfalls.md) 坑 80），但会明确警告「下次还会重下」。
26. **转换只在编译期发生，产出两份东西，形状只有一份实现**：`fuyubot-tdlib-sys/build.rs` 解析 `td_api.tl`，写出
    `$OUT_DIR/td_api.json`（运行时元数据，`TdApiSchema::compiled`）和 `$OUT_DIR/td_api_generated.rs`
    （Rust 类型，sys 的 `td_api` 模块 `include!`）。**运行时不读文件、不解析、不生成**；解析与 JSON 互转只有
    `crates/fuyubot-tdlib-sys/src/tl_schema.rs` 一份实现（build.rs 用 `#[path]` 引同一文件），
    **改形状要同时想清楚两边**。
    因为转换是必需的，`TdApiSchema::compiled()` 返回 `&'static Self` 而不是 `Option`。
27. **转换失败 = 构建失败**：文件读不了、解析不了、没有 `---functions---` / `---types---` 分区标记，
    全都是 `panic!`（错误信息里写清原因和三条修法）。`bake_schema` 在 `fuyubot-tdlib-sys/build.rs` 里
    **无条件**执行（那个脚本已经没有任何 feature 早退）。历史教训仍要记住——踩过一次：
    旧 build.rs 里 `CARGO_FEATURE_VENDORED` 为空时的 early return 把转换整个挡在门外，
    症状是「找不到 td_api.json」这种完全不指向原因的编译错误。
28. **生成器里几条不能动的规则**（都在 `tl_codegen`，改之前先读那段文档）：
    类类型的字段一律 `Option<Box<T>>`（`Box` 断互递归——TL 的环一定经过类；`Option` 对上 TDLib
    「缺字段就是没有」的语义，也让 `#[serde(default)]` 成立）；
    认不出的类型降级成 `serde_json::Value` 而不是让生成码编译不过；
    类 enum 的变体字段**不能**写 `pub`（E0449），函数 struct 的字段**必须**写 `pub`；
    没有分区标记时 `generate` 返回空——但这条在 `bake_schema` 里已经被更早的 `panic!` 挡住了。
29. **tdjson 导出两套 API，只能用 per-client 那套**：`td_json_client_create` 给句柄，
    `td_json_client_send/receive/execute/destroy` 都带句柄；另一套是多客户端
    （`td_create_client_id()` 给 **int** id、`td_send(id, request)`、`td_receive(timeout)` 没有句柄参数、
    `td_execute(request)` 单参数）。**两套符号都在，写错名字不会解析失败**，只会静默错投：
    把 `td_send` 当 `td_json_client_send` 用时，我们那个指针的低 32 位会被当 `client_id`，
    只回一条 `400 Invalid TDLib instance specified`。`TdExecuteFn` 是**两参数**
    （`(client, request)`，不是老的 `td_execute`）。改了 `crates/fuyubot-tdlib-sys/src/ffi.rs` 的符号名/arity，
    必须拿真库跑 `crates/fuyubot-tdlib/tests/integration.rs` 的 `-- --ignored`（测试仍在 tdlib 侧）。
    **还有第三类符号：全局的，不带 client 句柄**——`td_set_log_verbosity_level`（我们在用）、
    `td_set_log_file_path`、`td_set_log_message_callback` 等。它们属于**同一个库**，
    但**可以在建客户端之前调**（`TdClient::new` 就是这么把日志压到
    [`DEFAULT_LOG_VERBOSITY`] = 1 的，否则 TDLib 自己的默认会把每轮 `td_receive` 的
    超时都打到 stderr）。⚠️ 往 `TdApi` 加字段时**必须同步 sys 的 `src/ffi.rs` 末尾那条
    `size_of::<TdApi>()` 断言**（5 → N）与测试里那个 `TdApi { .. }` 字面量——
    断言是故意写成硬编码的，就是逼你改。
30. **加载动态库时必须让系统先查库自己所在的目录**（走 `fuyubot_tdlib::loader::open_library`，
    Windows 上是 `LOAD_WITH_ALTERED_SEARCH_PATH`）。`libloading::Library::new` 传 `flags = 0`，只查
    「exe 目录 + System32 + PATH」；tdjson 的依赖（libssl/libcrypto/zlib 之类）和它并排放在同一个目录时
    就会 `LoadLibraryExW { source: 126 }`（`ERROR_MOD_NOT_FOUND`）——报的是「库不在」，实际缺的是它的依赖。
    裸库名（优先级 4）没有「自己所在的目录」，保持 `Library::new`。
    **这条对插件同样成立**：`fuyubot-loader` 加载插件 cdylib 也走同一个 helper
    （且在调用前先 `std::fs::canonicalize`，因为这个 flag 只对**绝对路径**生效）。
    踩过一次：pitfalls 坑 43 当时只在 tdlib 那侧修了，插件侧还是 `Library::new`，
    于是「插件把兄弟 DLL 放在自己旁边」的部署形态静默失败。**新增任何加载动态库的地方都走它。**
31. **`td_receive` 的接线有两个入口，而且必须按顺序用**：
    - `UpdatePipeline::attach_tdlib` —— 只把 `TdClient` 挂进 `HostRuntime`（**不起循环**）；
    - `UpdatePipeline::spawn_dispatch` —— 才开始接收循环，把 `Routed::Update` 送进
      `process_value`（六阶段 + 双通道）、`Routed::Response` 送进 `deliver_response`（按 `@extra`
      投给发起者），并**返回 [`ReceiveLoop`] 停止把手**。
    **为什么拆成两步**：插件 `start` 期发的请求要能进真库（所以 client 必须**先**挂上），
    而更新又不能在插件加载完之前开始分发（否则插件漏订阅）。`fuyubot run` 的顺序因此是
    「问凭据 → attach → **加载全部插件** → 等 `WaitTdlibParameters` 发 `setTdlibParameters` → 登录
    → 最后 `spawn_dispatch`」。
    ⚠️ 登录窗口里 TDLib 已经在发东西了（`updateOption` 等），所以驱动登录必须走
    `authorize::log_in_with` 并把它不认的那些经 `bypass` 交给 `pipeline.dispatch`——
    用 `log_in` 会把它们**吞掉**（那条路传的是空 `bypass`）。
    **停止位必须显式持有、显式停**：`ReceiveLoop::stop()` 之后最长还要 1s（轮询周期）才真停，
    要确认就接着 `join().await`；**drop 它不会停**（刻意与 `Effect`/`Subscription` 的
    「drop 即注销」相反）。调用方必须一直留着那个句柄，否则进程退不掉、也做不了
    「退回登录 → 重开分发」。**Ctrl-C 仍是硬杀**（捕获它要 tokio 的 `signal` feature，会带进
    `mio` + `signal-hook-registry` + `errno`，依赖白名单不放行——要用户点头）。
32. **公开 ABI 类型不许用 `macro_rules!` 生成**：cbindgen **不展开宏**，宏里造的类型不会出现在
    C 头文件里，而 VTable 的字段又用到它们——头文件会「用了没声明的类型」，`-fsyntax-only` 报一串
    完全指不到原因的语法错误（五个句柄就这么消失过一次）。所以句柄都是显式 `struct`，
    只有 impl 那半走宏。新增公开 ABI 类型时除了同步 `build.rs` 的 `export.include`（约束 18），
    **还要真的把头文件编译一次**。
33. **cbindgen 的 `after_includes` 插在所有类型定义之前**：那里只能放宏和注释，任何「用到某个 ABI
    类型」的代码都要放 `config.trailer`（导出符号声明就在那儿）。
34. **VTable / 服务结构里 `Option<fn>` 的签名必须内联写**，不能写 `Option<EventCallbackFn>` 这种
    别名形式：cbindgen 只在签名内联时才生成「可空的函数指针」，走别名会退化成**不完整的结构体按值
    传参**（C 编译器直接报错）。Rust 里两者是同一个类型，所以这不影响 ABI。
35. **`fuyubot_plugin.h` 是产物，不是源码**：不许签进仓库。权威副本是
    `fuyubot_abi::PLUGIN_HEADER`（`include_str!(OUT_DIR/…)`，跟着 crate 分发），
    谁要落盘就从它写（`fuyubot-sdk init --lang c`、`fuyubot-sdk codegen --lang c` 都这么做）。
36. **脚本插件（Python / JS / TS）的四条硬约定**，少一条就会静默丢事件或卡住加载：
    ① `plugin_start` 必须**等**脚本发 `{"op":"ready"}` 才能返回（否则宿主已经开始分发、脚本还没订阅）；
    ② 宿主发给脚本的事件必须带**订阅号** `handle`（同一事件可以有多条带不同过滤器的订阅，
    按事件名分发会让不该响的回调也响）；
    ③ 两侧 stdio 必须钉成 **UTF-8**（Windows 上 Python 默认按系统代码页读管道，中文直接变乱码）；
    ④ 解释器起不来/脚本早退要让 `plugin_start` 返回错误，而不是留一个「加载成功但什么都不干」的插件。
    **脚本认哪些扩展名**：`find_script` 依次试 `.py` / `.js` / `.ts`（`.js` 优先于 `.ts`），
    拷同伴文件的白名单是 `py | js | ts | pyi`——**注意 `Path::extension("x.d.ts")` 是 `ts`**，
    别把多段后缀写进白名单（写过一次，那个分支是死代码，`.d.ts` 从来没被拷过，见 [`docs/pitfalls.md`](docs/pitfalls.md) 坑 65）。
    **TypeScript 靠 Node 自带类型剥离跑**（`node x.ts`，Node ≥ 23.6），
    所以 TS 插件不需要 `tsc`，但只能用能剥掉的语法（注解 / `interface` / `type` / `as`，
    **不能用** `enum` / `namespace` / 构造器参数属性）。示例：`examples/ts-counter`（SDK 仓库）。
37. **脚本插件的 crate 只依赖 `fuyubot-sdk-script`**（该约束适用于 **FuyuBot-SDK 仓库**）：
    `script_plugin!` 宏内部一律走 `$crate::__macro_support::…`，不许写 `::fuyubot_plugin_api::…`
    ——那样生成的插件还得自己再依赖一个 crate，宏的卖点（一行接入）就没了。
    依赖方向：`fuyubot-plugin-api` → `fuyubot-abi`；`fuyubot-sdk-script` → `fuyubot-plugin-api`。
38. **`fuyubot run --replay` 不许接 TDLib**：回放是「离线验插件」用的，接了就要账号/网络，
    而它本来要能在任何机器上跑。⚠️ **注意这条的理由变过**：原来是因为接收循环停不下来
    （[`docs/gaps.md`](docs/gaps.md) 缺口 8，`tokio` 运行时 drop 时会一直等它、进程退不出去）；**停止位已经做好了**
    （约束 31），所以现在「停不下来」不再是理由——但「回放必须离线」这条**依然成立**。
    回放要等「安静窗口」再打印结果（脚本插件的反应是异步的，`wait_until_quiet`）。
39. **CLI 的模板与 SDK 只有一份来源**（SDK 仓库内）：Python / JS 的桥与类型声明用 `include_str!` 直接引
    `crates/fuyubot-sdk-script/python|js/…`，C 头文件用 `fuyubot_abi::PLUGIN_HEADER`。
    **不许复制第二份**——复制就等于两个 SDK 会漂移，而这正是「所有 SDK 从同一份 ABI 生成」要避免的。
40. **`clap` 只允许出现在两个 CLI crate 里**：`fuyubot-cli`（二进制 `fuyubot`，运行时，本仓）与
    `fuyubot-sdk-cli`（二进制 `fuyubot-sdk`，插件开发，**FuyuBot-SDK 仓库**）。**任何库 crate 都不许依赖 clap**，
    也不许为了「顺手」把 CLI 逻辑下沉到库里。
    两个 CLI 的职责严格分离、**都不参与构建/打包**（见 [`docs/decisions.md`](docs/decisions.md) 变更 4/5）：
    打包由插件作者用自己的语言工具链完成。**`fuyubot-cli` 里不许再出现「调 cargo / 调 C 编译器」
    的代码**——过渡期的 `build-plugin` 已经拆掉（2026），要装插件只能 `fuyubot plugin install`
    **复制**一个已经编好的产物；`examples/` 的产物用作者工具链编（`cargo build -p …` / `cl /LD`）。
    生成的插件 crate 一律是**独立 crate**（`Cargo.toml` 开头那个空 `[workspace]` 不能省）。
41. **凭据一律不许落盘**：`api_hash`、bot token、手机号、验证码、两步验证密码
    **不得**写进仓库任何文件、`fuyubot.toml`、测试夹具或日志。
    只允许两个来源：**环境变量**（测试/验收用 `TG_API_ID` / `TG_API_HASH` / `TG_BOT_TOKEN`）
    与**交互式输入**（CLI 登录）。session 由 TDLib 自己落在 `database_dir`，配置文件不记任何凭据
    ——这是新规格的核心原则（见 [`docs/decisions.md`](docs/decisions.md) 变更 2/3）。
    相应地，含真实登录态的 `target/tdlib-login-test/` **不是测试夹具**，不许提交、不许当预置数据。
42. **TDLib 的字段路径必须拿真库问一次，别按文档猜**：已经踩过两次——
    `updateFile` 的 file id 在 `file.id` 而不是顶层 `file_id`；`user` **没有** `is_bot` / `username`，
    bot 判定在 `type.@type == "userTypeBot"`、用户名在 `usernames.active_usernames[0]`。
    凡是要读 TDLib 对象的嵌套字段（尤其是「分类/身份」这类判断），先跑一次 `-- --ignored` 的
    真库自检把真实 JSON 打出来，再写断言。
43. **TDLib 的方法名/参数名 ≠ Telegram Bot API 的**（踩过：`fuyubot-simplify` 整个发送路径
    按 Bot API 拼，真发必错）。
    `sendPhoto` / `sendDocument` / `sendLocation` 之类在 TDLib 里**不存在**——那是 Bot API
    （HTTP 接口）的方法名；TDLib 里一切发送都是 `sendMessage` + `input_message_content`
    （`inputMessageText` / `inputMessagePhoto` / `inputMessageDocument` / `inputMessageLocation`）。
    参数名也不同：`topic_id` / `reply_to` / `options: messageSendOptions`（不是
    `message_thread_id` / `reply_to_message_id` / 顶层 `disable_notification`），
    `caption` 在内容类型里，`formattedText` 没有 `parse_mode`。
    **规矩**：① 写 TDLib 的 JSON 先拿真定义核对
    （`Select-String '^sendMessage ' .tdlib-cache/1.8.68/td_api.tl` 最快）；
    ② 硬编码了 TDLib 方法名/参数名的模块，**必须留一条消费 `TdApiSchema::compiled().check_request`
    的测试**（离线可跑，写错名字立刻红）——`fuyubot-simplify` 里
    `every_request_simplify_builds_passes_the_real_schema` 就是模板。
44. **`fuyubot-sdk check` 必须**静态**读文件，不许 dlopen**：规格要求「不加载、不运行」——
    `dlopen` / `LoadLibrary` 会在宿主进程里跑插件的初始化代码（Windows 上就是 `DllMain`），
    那已经算「加载」了。所以走 `object` 解析 PE / ELF / Mach-O 的导出表。
    由此**静态看不到 ABI 版本**：版本在 `PluginHandle.abi_version` 里，是 `plugin_init` 的
    **返回值**而不是静态符号——`check` 必须如实报「只能运行时判定」，**不许编一个看起来可信的结论**。
    `object` 只允许出现在 **FuyuBot-SDK 仓库的** `fuyubot-sdk-cli`，**不许进运行时依赖图**
    （`cargo tree -p fuyubot-cli` 里不该看到它——和约束 9 一个道理）。

45. **改文本一律用编辑器工具，不许用 shell 去读写源码**：`read` / `write` / `edit` / `grep` / `glob`
    这些工具是按 UTF-8 处理的，**任何 shell 的文本通道都可能重编码**。
    踩过大坑：PowerShell 的 `Get-Content` 在本机把 UTF-8 当 **GBK(936)** 解码，`Set-Content` 再写回去
    ——中文注释全毁，而且 GBK 解码会**吞换行**导致行号错位（按「删 96–222 行」删到了别的代码）。
    更糟的是**当时这仓库不是 git 仓库**，没有 `git checkout` 可救（见 [`docs/pitfalls.md`](docs/pitfalls.md) 坑 63）。
    PowerShell 只用来**跑命令、读程序输出**（那没问题，命令的 stdout 解码是对的）；
    涉及文件内容的读写，走编辑器工具。
    **shell 选择**：本机 git 装在 `E:\Git\`。
    - `E:\Git\git-bash.exe` 是 **mintty 的 GUI 启动器**，会脱离 stdio（拿不到输出），**不要用它做自动化**；
    - 要跑 bash 用 **`E:\Git\bin\bash.exe -lc '…'`**；
    - **别用 `C:\Windows\system32\bash.exe`**——那是 **WSL 的桥**，会换一套文件系统与工具链。
    ⚠️ 但在 DSH 的受限沙箱下 bash **根本起不来**（MSYS2 要命名管道做信号处理，被沙箱拒），
    详见 [`docs/pitfalls.md`](docs/pitfalls.md) 坑 67。**当前会话只能用 PowerShell 当命令启动器**，等沙箱放开再切 bash。
46. **发布资产的 tag / 文件名 / 格式是「CI ↔ loader」之间的契约**，改一边必须同步另一边。
    **消费方**：`crates/fuyubot-tdlib/src/loader/vendored.rs` 的
    **`ASSETS: &[(os, arch, 资产名)]` 表**（下载 URL 由 `crates/fuyubot-tdlib-sys/src/build_support.rs`
    的 `release_url()` 拼，两个构建脚本共用：`https://github.com/<repo>/releases/download/TDLib-v<版本>/<资产名>`）。
    **生产方**：`.github/workflows/build-tdlib.yml` + `.github/scripts/*.sh`。四条硬要求：
    ① tag 必须是 **`TDLib-v<版本>`**（tag 不含空格，避免 GitHub Release 创建时报 `tag_name is not well-formed`）；
    ② 资产名必须**精确等于** `ASSETS` 表里的值，所以**不带扩展名**；
       **命名一律 `TDLib-<os>-<arch>`，`arch` 用 cargo `CARGO_CFG_TARGET_ARCH` 的口径**
       （`x86_64` / `aarch64`）——**别混用 `arm64`，也别一会 arch-os 一会 os-arch**
       （原来就是这么乱的：`TDLib-x86_64-linux` 对 `TDLib-macos-arm64`，踩过）；
    ③ 内容必须是 **`.tar.gz`**——`.zip` 会被 `extract_library` **当成裸库原样写出**，
       得到一个坏文件而且**不报错**（`verify-tdlib.sh` 因此按内容判格式并直接拒绝 zip）；
    ④ **加平台只动 `ASSETS` 表 + CI 的 matrix，两边必须同时加**——表里有而 CI 不建、
       或 CI 建了而表里没有，都是故障（后者就是 `TDLib-windows-arm64` 那个错配）。
       `macos/x86_64`（Intel Mac）**故意不在表里**：CI 没有那个 runner，
       进了表只会把「构建期明确报错」变成「运行期 404」。
    `crates/fuyubot-tdlib/tests/release_assets.rs` 会**读 workflow 来双向比对**（表→CI、CI→表），
    漂了就红。踩过一次：CI 从别的项目搬来，tag / 资产名 / 扩展名 / 架构名**四处都不同**。
    改完记得用 `git show <旧提交>:<workflow>` 跑一次负向对照（**两个方向都要**）。
    两条**只在 Windows runner 上才会犯**的坑（都踩过，见 [`docs/pitfalls.md`](docs/pitfalls.md) 坑 77）：
    - **给 `tar` 的路径必须是 POSIX 形态**（`/d/a/...`）：GNU tar 把 `host:path` 当**远程归档**，
      `D:/a/...` 会被理解成「连主机 D」而直接失败。脚本里用 `cygpath -u`，**别用 `cygpath -m`**。
    - **workflow 里别手抄 matrix 的内容**（runner label、资产名…）：手抄的那份和 matrix 对不上过一次
      （汇总表写 `windows-latest`，matrix 里是另一个 label）。要用就从 matrix 现算。

47. **运行时的实例状态是 `<项目根>/.fuyubot/` 下的三个文件，三件事不许合并**（`fuyubot-cli/src/instance.rs`）：
    - `lock` —— **只用来独占**（`std::fs::File::try_lock`，drop / 进程死即释放，所以陈旧锁不需要谁清理）。
      ⚠️ **内容不能塞进这个文件**：Windows 的文件锁是**强制**的，字节范围被锁住时别的进程 `read`
      直接 `ERROR_LOCK_VIOLATION`，`status` 就永远读不到内容（Unix 的 `flock` 是建议锁，读得到——
      所以这个坑**只在 Windows 上现形**）。「能不能锁上」是一个比特，内容另放一处反而更干净。
    - `status` —— `key=value`（pid / started / version / plugins / name），**启动完成之后**才发布：
      `run --background` 的父进程等的就是它（拿上锁只说明进程活着，那时插件还没加载、登录还没走）。
    - `stop` —— 停止请求（内容是被请求的 PID）。`fuyubot stop` **先写它请实例自己收摊**
      （主循环每 500ms 看一次 → 停接收循环 → 卸载插件 → 停内核 → 放锁），8 秒不理会才硬杀兜底。
      ⚠️ **别退回「只有 `taskkill`」**：本机沙箱里 `taskkill` 直接被拒（`Access denied`，
      而 `.NET Process.Kill` 那条路是通的），而且硬杀跑不到插件的 `plugin_stop`。
      标记里带 PID 是必须的：不然「上一次留下的停止请求」会让新实例刚起来就自己停。
    陈旧状态**不需要谁清理**：`acquire` 顺手删掉上一个实例的 `status` / `stop`。
    `--replay` **不占锁、不发布状态**（离线开发路径不该和跑着的实例互斥）。
    任何新的「运行中实例」相关能力（`attach` / 热重载 / 优雅退出）都往这三个文件上长，
    不要在 CLI 里另发明一套。

48. **构建期必须把动态库放到「可执行文件同级目录」**：`crates/fuyubot-tdlib/build.rs` 的
    `stage_next_to_binary()` 把 `tdjson`（+ 那个目录里的兄弟动态库）复制到 `target/<profile>/`，
    `loader::resolve_with` 的优先级 2 认这一档（`LibrarySource::ExeDir`，**排在 vendored 之后**）。
    这是「把二进制拷走也能跑」的唯一收敛点——**改任一侧都要同步另一侧与 `loader::tests` 的断言**
    （两个纯函数 `vendored::binary_dir_from` / `stageable_library_files` 自带自检）。
    它只在**构建脚本管得到的目录**里生效：`cargo install` 的安装目录不受管辖，
    那种形态仍然得自己把库放过去（见 [`docs/gaps.md`](docs/gaps.md) 缺口 3）。

49. **面向用户的发行形态是「下载即用」，不是「自己编」**：`.github/workflows/release-bot.yml` 在 `v*` tag 上
    按平台**分开打包再整合**——`Package` 产两份中间 `tar.gz`（本体 / 内置插件），`publish`
    把两份叠成**各平台一个 zip** 挂 Release。本仓**只发运行时**：`fuyubot-sdk` 的 zip 由
    **FuyuBot-SDK 仓库**的 `release-sdk.yml` 发（不塞进本体包）。
    最终资产：`fuyubot-<os>-<arch>.zip`（解压得到 `<资产名>/`：可执行文件 **+ 本平台的 `tdjson`**
    + `plugins/` 内置插件 + `README.txt`）。
    中间产物（`fuyubot-*.tar.gz` / `fuyubot-plugins-*.tar.gz`）只留在 Actions
    artifacts，**不挂 Release**——它们是构建产物，不是发布形态。平台集合固定为 `ASSETS` 表那 5 个：
    **Windows / Linux 各 x86_64 + aarch64，macOS 只有 aarch64**（Intel Mac 没有 runner）；
    release-bot 的 matrix 用 `build-tdlib.yml` 同一套 runner 标签（`windows-11-vs2026-arm` /
    `ubuntu-24.04-arm` / `macos-14`），加平台时两处 + `ASSETS` 一起加。五条契约，改一边要同步其余：
    ① 资产名里的 `<os>-<arch>` 用 cargo 口径（`x86_64` / `aarch64`），与 `vendored.rs` 的 `ASSETS` 同序同名；
    ② 用户包一律 `.zip`，**保留顶层目录** `<资产名>/`；zip 只在 `publish`（Linux runner）里生成
    （Windows runner 的 bash 没有 `zip`）——别为平台分叉；
    ③ `tdjson` 必须与可执行文件同级 —— 靠的就是约束 48 那一档，包里的布局就是唯一用户可见的部署形态；
    ④ 内置插件必须落在 `plugins/`（`fuyubot init` / `run` 会从这里复制进项目）——
    这条只对用户包成立；**TDLib 构建期资产（约束 46）仍是 `.tar.gz`**，两者别混；
    ⑤ Release notes 的下载表**从实际产物生成**（`publish` 里遍历 `dist/*.zip`），不要手抄平台列表——
    加平台时手抄的那份必然漂。
    **文档必须跟着变**：[FuyuBot-Docs](https://github.com/CatMoeCircle/FuyuBot-Docs) 的
    `content/docs/usage/index.mdx` 是「下载解压运行」，源码构建单独在 `usage/build.mdx`；主 README 开头同理。
    ⚠️ GitHub Actions 这条路**在本机验不了**，所以改 workflow 的组包步骤时，把 `Package` + `Merge`
    两段 bash 原样抠出来在本机跑一次，解压产物验证 `--version` / `init` / `run --replay` 全通。

50. **Go 的 c-shared 插件**（`examples/go-ping`，在 **FuyuBot-SDK 仓库**）：Go 用 `-buildmode=c-shared`
    编出来的库**不能卸载**——`FreeLibrary` / `dlclose` 之后进程直接 access violation（`0xC0000005`，
    而且崩在退出那一刻）。所以 `fuyubot-loader` 用文件里的 `\xff Go buildinf:` 魔数认出 Go 产物，
    对这类库照常 `plugin_stop` + 回滚注册，但**故意 `mem::forget` 那个 `Library`**。
    三条规矩：
    ① **判据只能是这个魔数**：Go 的 c-shared 只导出 `//export` 的函数 + `_cgo_dummy_export`，
       `crosscall2` 之类不在导出表里；PE 的 buildinfo 也不在文件末尾（在中间数据段，因此整份扫描）；
    ② 因此**同一进程内反复加载同一个 Go 插件不可靠**（热重载路径），先 `fuyubot stop` 再 `run`；
    ③ 插件侧写法固定成「Go 写逻辑 + 一层 C 薄壳导出四个符号」：`PluginHandle` 是按值返回的结构体，
       由 C 构造最稳，也避免把 VTable 函数指针搬进 Go。C 头文件仍由
       `fuyubot-sdk codegen --lang c` 落盘（约束 35/39 不变，**没有** `--lang go` 脚手架模板）。

51. **三个仓库的边界与发布顺序**（2026 拆仓）：
    - 本仓 = 运行时 + `fuyubot-abi` + `fuyubot-tdlib-sys`；**不得**依赖 SDK 仓库的任何 crate，
      也不得把 SDK 的源码/crate 复制回本仓；
    - **FuyuBot-SDK** 通过 **crates.io** 依赖本仓的 `fuyubot-abi` / `fuyubot-tdlib-sys`，
      反向依赖不存在；
    - **FuyuBot-Docs** 只放文档站，不参与 Rust 构建。
    **发布顺序**：本仓先发 `cargo publish -p fuyubot-abi`、`-p fuyubot-tdlib-sys`（二者都要有
    `license` / `repository` 元数据）→ SDK 仓库再发 `fuyubot-plugin-api` → `fuyubot-sdk-script`。
    本机/CI 在发布前验证 SDK 时用 `--config 'patch.crates-io.…path="…"'` 指回本仓源码
    （见 HANDOFF 的拆仓验证记录），**不要把 patch 写进 SDK 仓库的 Cargo.toml**——那会让发布形态
    依赖本机路径。

## 构建与验证

```powershell
# 本地开发用仓里的 vendored 缓存（真 td_api.tl + 真 tdjson.dll + 它的依赖）。
# 不设也能构建：build.rs 会从 Releases 下 td_api.tl，**缓存根写不进去只警告、不失败**
# （顺序：`FUYUBOT_TDLIB_VENDORED_CACHE` → `$CARGO_HOME/…` → **项目本地 `<工作区>/.tdlib-cache`**
#  → `$OUT_DIR`（最后保底，但**那个目录每次重跑构建脚本都会被清空 = 会重下**，见约束 25 与 pitfalls 坑 80）。
# ⚠️ 必须写**绝对路径**：build.rs 的 CWD 是 crate 根，相对路径会被解析到
# crates/fuyubot-tdlib/ 下面，然后 404。
# 仓里的 .cargo/config.toml 已经代劳这一条（`relative = true`，相对仓库根），并给了两个别名：
#   cargo bot（一键启动） / cargo package-bot（本地打包，tdjson 自动落到 exe 同级）
# SDK 不在本仓（在 FuyuBot-SDK），它的构建/发布命令见那边仓库。
$env:FUYUBOT_TDLIB_VENDORED_CACHE = "F:\Rust\FuyuBot\.tdlib-cache"

cargo build --workspace --offline          # 必须 0 退出、0 warning
cargo test  --workspace --offline          # 所有自检必须绿（180 passed；另有 9 个 #[ignore]）
cargo tree  --workspace --offline          # 必须无环

# 插件开发 CLI（`fuyubot-sdk`）在 **FuyuBot-SDK 仓库**（不在本仓）：
#   cargo run -q -p fuyubot-sdk-cli -- init demo --lang rust   # 在 SDK 仓库根敲
# 本机还没把 crates.io 依赖发出去时，用 patch 指回本仓源码（见 HANDOFF 的拆仓验证记录）：
#   FUYUBOT_ABI_SRC=F:\Rust\FuyuBot\crates\fuyubot-abi
#   FUYUBOT_TDLIB_SYS_SRC=F:\Rust\FuyuBot\crates\fuyubot-tdlib-sys
#   cargo build --offline --config 'patch.crates-io.fuyubot-abi.path="…"' …
# 生成的插件默认写 crates.io 版本；设 FUYUBOT_SDK_PATH 指向 SDK 仓库的 crates/ 才写成 path。

# CLI 全流程自检（不需要账号）
cargo build -p fuyubot-cli --offline      # ⚠️ `cargo test` **不会**刷新 target\debug\fuyubot.exe
target\debug\fuyubot.exe run --replay examples\replay\new_message.json --plugin-dir <插件目录>

# 内置插件（/start、/help、/plugins）的离线自检：三个插件 + 一份回放
# `cargo build --workspace` 之后插件就在 target\debug\ 里；`init` 会把它们复制进 <项目>\plugins\
target\debug\fuyubot.exe init --dir <项目> --name demo --api-id 1
target\debug\fuyubot.exe run --replay examples\replay\builtin_commands.json --plugin-dir <项目>\plugins

# 实例三件套（单实例锁 / 后台运行 / 看状态 / 停）：一个目录一个实例
target\debug\fuyubot.exe run --config <目录>\fuyubot.toml --background   # 日志进 <目录>\logs\fuyubot.log
target\debug\fuyubot.exe status --config <目录>\fuyubot.toml             # 运行中 / 陈旧 / 没有
target\debug\fuyubot.exe stop   --config <目录>\fuyubot.toml             # 优雅：等它自己收摊，8s 才硬杀

# 脚本/TS 示例工程在 SDK 仓库（FuyuBot-SDK/examples/），回放数据在本仓 examples/replay/。
# 真 TDLib 的端到端自检：真 schema + 真动态库 + 真请求往返 + 登录窗口的旁路
$env:TDLIB_TDJSON_PATH = ".tdlib-cache\1.8.68\windows-x86_64"
cargo test -p fuyubot-tdlib --test integration --offline -- --ignored
# ⚠️ 「兄弟依赖定位」**单独一个测试二进制**，必须单独跑：它有个负向对照
#    （样本旁边有兄弟 DLL 时 `Library::new` 必须失败；CI 的 tdjson 是静态链接，
#    那种样本上会明确打印「跳过」而不是硬造失败），前提是进程里没加载过那些 DLL——
#    和别的测试同进程跑就会被污染（踩过一次，见 tests/sibling_dependencies.rs 的文件头）。
cargo test -p fuyubot-tdlib --test sibling_dependencies --offline -- --ignored

# 真 Bot 登录 + session 复用（要真凭据，凭据只走环境变量，见约束 41）
# ⚠️ 先删 session 目录，否则验的是「复用」不是「首次登录」
Remove-Item -Recurse -Force target\tdlib-login-test
$env:TG_API_ID="…"; $env:TG_API_HASH="…"; $env:TG_BOT_TOKEN="…"
cargo test -p fuyubot-tdlib --test integration --offline -- --ignored real_bot_login --nocapture

# vendored 分支：会尝试访问 GitHub Releases，失败只警告、不 panic
cargo test  -p fuyubot-tdlib --features vendored --offline
cargo build -p fuyubot-cli   --features vendored --offline
```

**默认自检不碰任何外部文件**：真 tdjson、真 `td_api.tl`（用 `TDLIB_TD_API_TL` 顶替时）、真 Releases 下载
全部藏在 `#[ignore]` 后面（`-- --ignored` 显式打开，见 `crates/fuyubot-tdlib/tests/integration.rs`）。
所以「全绿」只证明代码自洽，**与真实 TDLib 对不对得上只有那些 `--ignored` 能回答**
（分布在 `tests/integration.rs` 与 `tests/sibling_dependencies.rs` 两个二进制里）——
改 `ffi.rs` 的符号名/arity、`loader::open_library`（**或任何加载动态库的地方**）、
`tl_schema`/`tl_codegen`，或碰任何登录/授权相关代码之后，必须跑一次。

**别把 `.tdlib-cache/` 当成源码**：它是本机开发缓存（28MB 的 tdjson + 它的几个依赖 DLL），
构建与端到端自检都靠它，但没有任何构建依赖它存在——`.cargo/fuyubot-tdlib-vendored/` 下放同样布局也行。

`crates/fuyubot-loader/tests/plugins.rs` 会自己调 `cargo build` 把 `fuyubot-plugin-demo` / `fuyubot-plugin-badabi`
编成 cdylib 再 dlopen——嵌套 cargo 是可行的（会先打印一句等锁，然后继续），不用改目标目录。

**联网注意**：本会话的 DSH 沙箱下 cargo 的 TLS 连不上 crates.io（`schannel ... SEC_E_NO_CREDENTIALS`）。
`Cargo.lock` 已落盘，`--offline` 就够。只有**改依赖版本**时才需要联网，那时向用户申请一次完整权限跑 `cargo fetch`。

## 代码风格

- **先想能不能不写**：能复用已有的 helper/类型就别新建；能一行别五十行。不引入无人使用的抽象（单实现的 trait、单产品的 factory）。
- **不新增依赖**，尤其是 `anyhow` / `async-trait` / `once_cell`——先看这条需求能不能用标准库或现有依赖表达。
  **例外只有两个**（都是用户明确批准的）：
  - `clap`：CLI 用 clap，只允许出现在本仓的 `fuyubot-cli` 与 SDK 仓库的 `fuyubot-sdk-cli`（约束 40）；
  - `object`（0.40，关掉默认特性里的 `compression`）：`fuyubot-sdk check` 要**不加载**就读出
    动态库的导出符号（约束 44），只允许出现在 **SDK 仓库的** `fuyubot-sdk-cli` 里（本仓已不依赖它）。
- 非平凡逻辑（分支、循环、解析、钱/安全路径）留**一个**可运行的自检：`#[cfg(test)]` 里一个 `assert` 测试即可，不引测试框架。
- **测试必须自洽：不依赖 shell、不依赖环境变量、不依赖「先跑过别的命令」。**
  具体一条硬规矩——**临时目录不许用 `%TEMP%`**（`std::env::temp_dir()` / `tempfile`），
  一律放 workspace 的 `target/test-tmp/<crate>/<名字>` 下
  （照 `fuyubot-cli` 的 `test_support::temp` 或 `fuyubot-loader/tests/plugins.rs` 的 `Scratch` 写）。
  理由：MSYS（git bash）把 `TEMP` 改成 `/tmp`，而受限环境里往真实 `%TEMP%` 建目录会被拒
  （`OS error 5`）——**一批测试会换个 shell 就红**（踩过两次，见 [`docs/pitfalls.md`](docs/pitfalls.md) 坑 70）。
  **测试里嵌套调用 cargo / 编译器时，也要把子进程的 `TEMP`/`TMP` 指到那里**：
  MSVC 的 `link.exe` 建不了 `%TEMP%` 里的 `lnk*.tmp` 会直接 `LNK1104`，插件/夹具根本编不出来，
  依赖它们的一批测试会一起红（踩过，见 [`docs/pitfalls.md`](docs/pitfalls.md) 坑 73/76）。
- 刻意留下的简化用 `# ponytail: <天花板是什么>，<什么时候升级>` 注释标记，别让后手猜。
- **文档里出现的命令必须在本仓库真跑过**，带输出的示例要**抄真实输出**（README / `docs/*.md` 都算）。
  踩过两次，都是「写文档时顺手跑一遍」才发现的：文档里写了 `init --api-hash`，真跑才发现它把凭据
  **落盘**（违反约束 41）；写了 `plugin install examples/ts-counter`，真跑才发现它报错
  （workspace 成员的产物在仓库根 `target/` 下）。**没跑过的命令不要写进文档**——
  读文档的人会比你更相信它。
- 错误类型用 `thiserror`，不要 `Box<dyn Error>`。**唯一例外**是回调签名统一的 `fuyubot_core::Result<T>` / `fuyubot_core::BoxError`：四个机制 crate 不能互相依赖，回调的错误类型只能由 core 定。
- Edition 2024 细节：`#[unsafe(no_mangle)]`（不是 `#[no_mangle]`）、`unsafe extern "C" { ... }` 块。

## 交接文档：每次实质改动都要维护，不许只改代码

这个仓库靠**三份**文档接力。**任何有实质产出的回合结束时，相关文档都必须是新的。**

- [`docs/ROADMAP.md`](docs/ROADMAP.md) —— **进度台账**：新规格（多语言 SDK + 两个 CLI）与
  「阶段 0–6 验收补跑」的每一项都列在这里，做完就勾掉并把证据写进 `docs/acceptance-phase-N.md`。
  **未完成的事不许只留在聊天记录里**——先写进这个文件。对话里出现的新规格/新决策，
  第一件事是落到这里，再动手。
- [`docs/HANDOFF.md`](docs/HANDOFF.md) —— **入口**：当前状态、构建契约、每个 crate「已实现 / 还是空的」。
  新增能力、改了对外行为，第一件事是同步它；**踩到的坑**写进 [`docs/pitfalls.md`](docs/pitfalls.md)
  （编号即引用锚），**新缺口**写进 [`docs/gaps.md`](docs/gaps.md)，被拍板的决定进
  [`docs/decisions.md`](docs/decisions.md)，欠账进 [`docs/tech-debt.md`](docs/tech-debt.md)；
  **面向使用者的文档**（架构、ABI 契约、插件写法、TDLib 封装）只在 **FuyuBot-Docs 文档站**
  （`content/docs/**`）——改这些行为要同步那边，仓库里不放副本。
  同时**删掉已经过时的说法**（过期的“下一步”比没有更坑人）。
- `AGENTS.md`（本文件）—— 只放**长期成立**的规范与约束。
  本阶段新出现的、后续也必须遵守的约束（依赖方向、加载优先级、禁止进入默认特性图的依赖……）要提炼进「硬约束」；
  只对本阶段有效的细节留在 HANDOFF，不要往这里堆。

顺序：先把事实写进 `docs/HANDOFF.md`，再把其中真正长期成立的部分提炼进本文件。
发现自己违反了下面任何一条时，先改代码，再把当时的判断补进「踩过的坑」——下一个 AI 会踩同一个坑。

## 目录职责

> `fuyubot-plugin-api` / `fuyubot-sdk-script` / `fuyubot-sdk-cli` 在 **FuyuBot-SDK** 仓库，
> 不列在这里（约束 51）。下表只列本仓的 crate。

| crate | 职责 |
|---|---|
| `fuyubot-cli` | 二进制 `fuyubot`：`init` / `run`（`--background` 后台运行、`--replay` 离线回放）/ `status` / `stop` / `config`（打印**生效值**，没有 `get`/`set`，理由见 ROADMAP 第 8 项）/ `plugin list` / `plugin install`（clap）。`run` 的顺序：**自动 init → 拿单实例锁 → 交互式问 api_id/api_hash → attach TDLib → 加载全部插件 → 等 `WaitTdlibParameters` 设参数并登录 → `getMe` 打印账户 → 发布状态 → 才开始分发**；运行期掉出 `Ready` 会退回登录（`--no-login` 可跳过登录那几步），看到 `.fuyubot/stop` 会优雅收摊（`--replay` 不占锁、不发布状态）。**不实现任何宿主函数**——那些都在 `fuyubot-loader`；**也不编译/打包插件**（见约束 40）。插件的脚手架与 5 种语言模板在 **FuyuBot-SDK** 仓库的 `fuyubot-sdk-cli`（本仓没有 `new-plugin` / `build-plugin`） |
| `fuyubot-core` | Cordis 内核：`Context`（Arc 服务表）、`Plugin` trait、`Effect` 回滚守卫、`PluginManager`（拓扑排序加载 / 倒序卸载）、`Kernel` 阶段。**不认识 Telegram** |
| `fuyubot-abi` | `HostApiVTable`（15 个桥 + version/size/host_ctx）、`ServiceVTable`、`PluginHandle`、插件侧 `HostApi` 门面、`Plugin` trait 与 `define_plugin!` 宏、cbindgen 生成 `fuyubot_plugin.h`（并有 `PLUGIN_HEADER` 嵌入副本）。**不认识 core 与任何机制 crate** |
| `fuyubot-tdlib-sys` | **绑定层（`-sys`）**：`td_api.tl` 的编译期转换——`tl_schema`（解析与 JSON 互转，build.rs 与运行时共用）+ `tl_codegen`（生成 Rust 类型）+ `docs`（TL 文档标记 → rustdoc Markdown）+ `td_api`（生成的类型，`include!` 展开）+ `schema`（烘焙元数据 `TdApiSchema`）+ `ffi`（C 函数指针与符号名）。`build.rs` 无条件拿 `td_api.tl` 并烘焙；`build_support.rs` 是两个构建脚本共享的缓存/下载设施。**没有 tokio / libloading / 网络**——下游只要类型提示就依赖它 |
| `fuyubot-tdlib` | `loader`（四优先级定位/加载/版本协商 + **exe 同级目录那一档** + vendored 资产表）+ `client`（`TdClient`、`@extra`、`td_receive` 循环、**`ReceiveLoop` 停止把手**）+ **`authorize`（授权状态机：`AccountType` / `TdlibParameters` / `AuthState` / `Prompter` / `log_in` / **`log_in_with`（登录窗口的旁路）**；真 13 个状态、`setTdlibParameters` 真 15 个字段；纯函数 `question_for`/`request_for` 可离线拿真 schema 校验）**。绑定（`ffi`/`schema`/`td_api`/`tl_schema`）整体住在 `fuyubot-tdlib-sys`，本 crate **重导出**其公开 API，`fuyubot_tdlib::TdApiSchema` 这些路径因此不变 |
| `fuyubot-event` | `EventBus`：`on(event_type, Filter, callback)` / `emit` / `off`，订阅句柄 drop 即退订；`EventContextExt` |
| `fuyubot-service` | 命名 `ServiceContainer`：`provide(名字, 服务)` / `get::<T>(名字)`，`ServiceHandle` drop 即注销；`ServiceContextExt` |
| `fuyubot-tool` | `ToolDescriptor` + `ToolRegistry`：schema 编译校验、权限门、`call_tool`、`list/query_tools`、`on_change` 广播、C ABI `ffi` 模块；`ToolContextExt` |
| `fuyubot-middleware` | `Stage` 六阶段 + `Pipeline` 洋葱模型（`Next::run`）+ `MiddlewareHandle`；`MiddlewareContextExt` |
| `fuyubot-loader` | **宿主层**：`HostRuntime`（VTable 桥 + 注册作用域回滚 + 请求留档 + 响应按归属回投）、`UpdatePipeline`（六阶段中间件 + 类型/过滤路由 + `attach_tdlib`/`dispatch` 接 `td_receive`）、`PluginLoader`/`LoadedPlugin`。也提供插件文件级工具（`DLL_EXTENSION` / `plugin_files`）。**插件名的归一化不在这里**：`plugin_file_name` 只有开发 CLI 一个调用者，它留在 SDK 仓库的 `fuyubot-sdk-cli`——放进宿主层等于让插件作者的依赖图白拖一整套 event/service/tool/middleware/tdlib |
| `fuyubot-plugin-demo` | 最小 Rust 插件（cdylib）：`Plugin` + `define_plugin!`，订阅更新、发请求、收自己的响应；被 loader 的集成测试真的加载 |
| `fuyubot-plugin-badabi` | 手写 C ABI 导出的样例：故意报不兼容 ABI 版本，供加载器测试 |
| `fuyubot-plugin-start` / `-help` / `-manage` | **内置插件**（三个 cdylib，`define_plugin!` 各一次）：`/start` 欢迎语、`/help` 命令表、`/plugins` 列出插件目录里的插件。用 `fuyubot-simplify` 的 `on_command`（订阅 `updateNewMessage` → 解出 `Command`）与 `SendMessageParams`。**打包时跟可执行文件一起**（`cargo package-bot` 与 release 流程都编它们），`fuyubot init` / `run` 把它们**复制**进项目的 `plugins/`（只装缺的，不覆盖）——`scaffold::install_builtins` |
| `fuyubot-simplify` | TDLib 便捷封装（`Simplify`）：发消息 / **相册** / **编辑消息（文本 / 标题 / 媒体）/ 删除消息** / 等更新 / 内联键盘 / 批量解析 / 下载进度。**参数形状是「一个方法一个参数结构体 + 短字段名」**（`SendMessageParams { text, media, reply_to_message_id, topic_id, reply_markup, link_preview, timeout_ms }` 这种），调用方永远不拼 TDLib 的嵌套 JSON；**文本格式转换经同步的 `td_execute` 直接问 TDLib 的 `parseTextEntities`**（没有第二套 Markdown 解析器）。**发送一律是 `sendMessage` / `sendMessageAlbum`**（TDLib 没有 `sendPhoto` 之类，见约束 43），内容走 `input_message_content`。运行时依赖 `fuyubot-tdlib-sys` 并**重导出 `td_api`**，调用方拿 `Value` 后能直接反序列化成类型 |

## 提交前检查清单

- [ ] **文档按分工更新**：入口 `docs/HANDOFF.md`、坑 `docs/pitfalls.md`、缺口 `docs/gaps.md`、
      台账 `docs/ROADMAP.md`（验收类还要写 `docs/acceptance-phase-N.md`），并删掉过时的说法
- [ ] 没有把凭据写进任何文件（约束 41）；真登录留下的 `target/tdlib-login-test/` 没被当成夹具
- [ ] **本文件的「硬约束」与「目录职责」已同步**：本回合新出现的长期约束已提炼进来
- [ ] `cargo build --workspace --offline` 0 warning（`#[allow]` 不算消 warning，能改就改）
- [ ] `cargo test --workspace --offline` 全绿
- [ ] **`cargo fmt --all --check` 为 0 diff**（整仓已是 rustfmt-clean，别再让它退化；
      要格式化就**单独一个提交**，别和逻辑改动混在一起）
- [ ] 构建用的 `td_api.tl` 有来源（`.tdlib-cache/` 或 `TDLIB_TD_API_TL`）
- [ ] 动了 `tl_schema` / `tl_codegen` / `docs`（都在 `fuyubot-tdlib-sys`）时，拿真 `td_api.tl` 构建过（生成码只有那时才被编译器检查）
- [ ] 动了 `fuyubot-tdlib-sys/src/ffi.rs` 的符号名/arity 或 `loader::open_library`（**或任何加载动态库的地方**）时，
      跑过 `--test integration -- --ignored` **和** `--test sibling_dependencies -- --ignored`
- [ ] 新增的跨 crate 依赖没违反上面的方向约束
- [ ] 新 crate 有 `Cargo.toml`（全部 `.workspace = true`）+ `src/lib.rs` 或 `src/main.rs` + `README.md`
- [ ] 新增的简化点已用 `# ponytail:` 标注
- [ ] 动了 `fuyubot-tdlib-sys/build.rs`、`build_support.rs` 或 `fuyubot-tdlib` 的 `build.rs` / `src/loader/vendored.rs` 时，
      `--features vendored` 那条测试也跑过
- [ ] **`README.md`（仓库根）已同步**：它是对外的第一份文档，只讲**项目是什么 + 用户怎么用**
      （下载 / `init` / `run` / 命令表 / 配置 / 插件）。构建契约、crate 表、进度与缺口留在
      `docs/HANDOFF.md` / `docs/ROADMAP.md` / `docs/gaps.md`，**别再搬回 README**；
      里面每条命令都必须是真跑过的（见「代码风格」里那条规矩）
- [ ] `[package.metadata.fuyubot]` 里的值仍与事实一致——**有两份**（`fuyubot-tdlib-sys` 与 `fuyubot-tdlib`
      各一份，守卫测试 `metadata_is_in_sync_between_the_two_crates` 比对）：`releases-repo` 要等于
      **`git remote origin`**（现在是 `CatMoeCircle/Fuyu-Bot`），`tdlib-version` 要等于**CI 跑 build-tdlib 时填的
      `tdlib_version`**（现在 `1.8.68`——资产放在 tag `TDLib-v<tdlib-version>` 下，填错就是 404）
