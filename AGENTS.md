# AGENTS.md — fuyubot

## 这是什么

基于 TDLib 的 Telegram bot / user 框架：**一个宿主进程 + N 个动态库插件**。
插件是 cdylib（`.dll` / `.so` / `.dylib`），只认四个导出符号与一张 `HostApiVTable`（ABI 版本 6）。
TDLib 在**运行时**动态加载（`fuyubot-tdlib::loader`），`td_api.tl` 在**编译期**转成 Rust 类型与可校验元数据。

四个仓库（本仓只放运行时）：

| 仓库 | 放什么 |
| --- | --- |
| **Fuyu-Bot**（本仓） | 运行时全部 + `fuyubot-abi` + `fuyubot-tdlib-sys`（后两个发 crates.io） |
| FuyuBot-Script | 一个脚本插件宿主 `fuyubot-script`（Python + JS/TS 同一个 dll）+ 两个薄 SDK（PyPI / npm）。没有 CLI、没有示例 |
| FuyuBot-Docs | 文档站（<https://catmoecircle.github.io/FuyuBot-Docs/>，源在 `content/docs/**`） |
| [FuyuBot-for-Android](https://github.com/CatMoeCircle/FuyuBot-for-Android) | Android 图形客户端（fuyuTD，Kotlin + Compose）。**不含内核**：APK 由本仓 `release-bot.yml` 编出来 |

跨仓依赖只有一条、单向：FuyuBot-Script 通过 crates.io 依赖本仓的 `fuyubot-abi`。
**crates.io 上本仓发 `fuyubot-abi` 与 `fuyubot-tdlib-sys` 两个包**；SDK 时代的其余 crate 已删除（仓库内移除 + crates.io yank）。

## 结构

| crate | 一句话 |
| --- | --- |
| `fuyubot-cli` | 二进制 `fuyubot`：`init` / `run` / `status` / `stop` / `config` / `plugin`（clap） |
| `fuyubot-core` | Cordis 内核：`Context`（按**名字**寻址的服务表）/ `Effect` / `Plugin`（含 `inject`）/ `PluginManager`（Fiber 生命周期与依赖跟踪）。不认识 Telegram |
| `fuyubot-abi` | C ABI：`HostApiVTable`（2 个 u32 + `host_ctx` + 27 个函数指针）、`PluginHandle`、`define_plugin!`、cbindgen 头文件 |
| `fuyubot-tdlib-sys` | 绑定层：`td_api.tl` → 类型 + 元数据 + FFI 声明（无 tokio / 无网络） |
| `fuyubot-tdlib` | `loader`（四级定位）+ `client`（`td_receive` 循环、`ReceiveLoop`）+ `authorize`（授权状态机）+ `session`（建客户端 / 登录 / `getMe` / 关会话） |
| `fuyubot-event` / `service` / `tool` / `middleware` | 四个机制 crate，只依赖 `core`，彼此不依赖 |
| `fuyubot-loader` | 唯一宿主层：VTable 桥、更新管线、插件加载、注册回滚 |
| `fuyubot-app` | 宿主编排层：把「内核 → 宿主 → 接 TDLib → 装插件 → 登录 → 分发 → 收摊」写在一处；交互全走 `AppHost` trait。CLI 与 Android JNI 共用 |
| `fuyubot-android` | Android 的 JNI 运行时（`cdylib`，`libfuyubotjni.so`）：只交付 `.so`，不出 APK / UI；保活归 APK 作者 |
| `fuyubot-simplify` | TDLib 便捷封装（发消息 / 编辑 / Markdown → entities / 命令解析）；`ops` 模块是它的 JSON 操作表，宿主 ABI v5 的 `simplify_invoke*` 就跑它 |
| `plugins/` · `examples/` · `tests/` | 按角色分目录：`plugins/` = 三个内置插件（`/start`、`/help`、`/plugins`）；`examples/fuyubot-plugin-demo` = 最小示例（也是加载器夹具）；`tests/` = ABI / 清单 / 作用域夹具（`badabi`、`manifest-fixture`、`scope-fixture`） |

数据流：插件的 `on/invoke/emit/middleware` → `HostApiVTable` → `HostRuntime`（事件总线 / 服务 / 工具 / 管线 / TdClient）
→ 更新走六阶段 + 双通道（`"update"` 与具体 `@type`），响应按 `@request_id` 回到发起插件。
细节都在文档站的[架构页](https://catmoecircle.github.io/FuyuBot-docs/docs/dev/architecture/)。

## 构建

`td_api.tl` 是**必需输入**：`TDLIB_TD_API_TL` → **包内 `vendor/<tdlib-version>/td_api.tl`**（随 crate 发布，crates.io 下游与 docs.rs 靠它，不联网）→ 缓存（`<缓存根>/<版本>/td_api.tl`）→ 从 Releases 下载。
缓存根顺序：`FUYUBOT_TDLIB_VENDORED_CACHE` → `$CARGO_HOME/fuyubot-tdlib-vendored` → 项目本地 `.tdlib-cache/`。
仓里的 `.cargo/config.toml` 已经把缓存指向仓内 `.tdlib-cache`（`relative = true`），本地直接敲命令即可。

```powershell
cargo build --workspace --offline     # 0 退出、0 warning
cargo test  --workspace --offline     # 341 passed / 0 failed / 12 ignored
cargo tree  --workspace --offline     # 无环
cargo fmt --all --check               # 0 diff

cargo bot                             # = fuyubot run（当前目录）
cargo dev                             # 开发跑 = cargo bot + 完整日志（--log-level trace + --trace-updates）
cargo package-bot                     # release：fuyubot.exe + 三个内置插件（tdjson 由 build.rs 放到 exe 同级）
```

## 改动后怎么验证

除默认自检外，按改动面补跑：

```powershell
# 真库（真 tdjson + 真 schema）：改 ffi 符号/arity、loader、tl_schema/tl_codegen、登录授权后必跑
$env:TDLIB_TDJSON_PATH = "F:\Rust\FuyuBot\.tdlib-cache\1.8.70\windows-x86_64"
cargo test -p fuyubot-tdlib --test integration --offline -- --ignored
cargo test -p fuyubot-tdlib --test sibling_dependencies --offline -- --ignored   # 必须单独跑（独占进程）

# vendored 分支（缓存命中就不联网）
cargo test  -p fuyubot-tdlib --features vendored --offline
cargo build -p fuyubot-cli   --features vendored --offline

# CLI 全流程（不需要账号）
target\debug\fuyubot.exe init --dir <项目> --name demo --api-id 1
target\debug\fuyubot.exe run --replay examples\replay\new_message.json --plugin-dir <项目>\plugins

# 文档站（改 content/docs 后）
cd F:\Rust\FuyuBot-Docs
$env:NEXT_PUBLIC_BASE_PATH='/FuyuBot-docs'; $env:NEXT_PUBLIC_SITE_URL='https://catmoecircle.github.io/FuyuBot-docs'
pnpm build        # push main 自动构建发布到 gh-pages（pages.yml）
```

真实账号登录（要真凭据，凭据只走环境变量或交互输入）：

```powershell
Remove-Item -Recurse -Force target\tdlib-login-test
$env:TG_API_ID="…"; $env:TG_API_HASH="…"; $env:TG_BOT_TOKEN="…"
cargo test -p fuyubot-tdlib --test integration --offline -- --ignored real_bot_login --nocapture
```

## 硬约束（编号是跨文件引用锚，保持不变）

1. **Rust 1.99 stable + Edition 2024**，不许 nightly feature。
2. **依赖方向**：`core` 不依赖 `tdlib`；`abi` 不依赖 `core`；`sys` 不依赖任何 fuyu crate；`tdlib` 依赖并重导出 `sys`；`loader` 是唯一同时依赖四机制 + `tdlib` + `abi` + `core` 的 crate。
3. **依赖版本只写在根 `Cargo.toml` 的 `[workspace.dependencies]`**，成员用 `x.workspace = true`。
4. **profile 只写在根 `Cargo.toml`**（`[profile.debug]` 靠 `inherits = "dev"`）。
5. **每个 crate 都要有 `README.md`**，一句话职责。
6. JSON Schema 只用 `fuyubot-tool`；动态库加载只用 `fuyubot-loader`；`cbindgen` 只是 `abi` 的 build-dependency。
7. **tdjson 运行时加载**：禁止 `#[link(name = ...)]`，符号一律走 `ffi::TdApi` + `loader::resolve_symbols`。
8. **加载优先级不要重排**：`TDLIB_TDJSON_PATH` → vendored → exe 同级目录 → Linux 带版本系统库 → 裸库名。
9. `tar`/`flate2`/`sha2` 只服务 vendored；`ureq` 是常开 build-dependency；**运行时依赖图里不许出现它们**（`cargo tree -p fuyubot-tdlib -e normal` 只有 sys / libloading / serde_json / thiserror / tokio）。
   ⚠️ 2026 起例外只有一处：`fuyubot-cli` 的 **TUI 商店**（`panel/store.rs`）可以依赖
   `ureq` / `sha2` / `zip`——它们是 CLI 的依赖，不在 `fuyubot-tdlib` 的 normal 树里，不算违反本条。
10. **`Context` 只暴露 `&self`**：注册走 `provide` 并**持有** `Effect`，不加 `&mut` 接口。
    ⚠️ **2026 P1 起它有了作用域链**：`Context::new()` 永远造**根**（68 个调用点都靠这个），
    子作用域必须显式 `child()`。查找**沿父链**走（自己这层 → 父 → …），注册**只写自己那层**；
    `isolate(name)` 把某个名字从当前作用域及以下的父链查找里摘掉。
    两个刻意的区分：`names()` 报的是「**能看见**什么」（含父级、去掉被隔离的），
    `len()` / `is_empty()` 报的是「**这一层**存了几个」。
    ✅ **2026 起 dylib 插件有各自的作用域**：`HostRuntime` 在 `begin_plugin` 时给每个 owner 建一份
    `ctx.child()`（存 `plugin_contexts`）；插件的 `provide_service` 落自己那层、要跨插件共享得用
    `provide_service_shared` 显式写根、`get_service` 从自己那层沿父链查，`rollback_owner` 连 child
    一起丢弃。⚠️ 但 `PluginManager` 传给**内置 Rust 插件** `apply` 的仍是共享的根（它们本来就是宿主
    的一部分，不是外部插件）；`isolate` 仍然只服务测试替身与宿主自己的代码。
11. **`Effect` 是 `#[must_use]`**：`let _ =` 会立刻回滚；要么存起来，要么显式 drop / rollback。
12. **四个机制 crate 只依赖 `core`，彼此不许依赖**；组合由宿主层做。
13. **与 Context 集成 = `install(ctx)` + 扩展 trait**；服务缺失时报错要带「该怎么 install」。
14. **扩展 trait 不许与 `Context` 固有方法重名**：命名服务叫 `provide_named` / `resolve`
    —— ⚠️ **2026 Cordis 化之后反过来了**：`Context` 自己有了 `provide_named` / `resolve`
    这两个固有方法（服务表现在按名字寻址），所以 `fuyubot-service` 的扩展 trait
    改叫 **`provide_service` / `get_service`**。规矩本身没变：固有方法永远优先。
    ⚠️ **P1 又占了四个名字**：`child` / `isolate` / `is_isolated` / `has_parent`。
    起新扩展方法前先 `grep` 一下 `crates/fuyubot-core/src/context.rs` 的 `pub fn`。
15. **VTable 每个函数首参是 `host: *mut c_void`，每个函数指针字段都是 `Option<...>`**。
16. **跨 ABI 字符串谁分配谁释放**：宿主不 free 插件的指针；宿主返回的是线程局部缓冲，下次调用前有效。
17. **新的宿主桥一律放 `fuyubot-loader`**。
18. 新增公开 ABI 类型要同步 `crates/fuyubot-abi/build.rs` 的 `export.include`，并真的编译一次 C 头文件。
19. **scope 是「额外定向」**：`emit` 发所有人，`emit_scoped` 发「全局 + 该归属」。
20. **更新发两条通道**：具体 `@type` 与兜底 `"update"`。
21. **过滤器里的 `event_type` 只选通道**，不参与负载匹配。
22. **插件与中间件回调必须 `C-unwind`**；`define_plugin!` 自己也要包 `catch_unwind`。
23. **一个 crate 只能展开一次 `define_plugin!`**（四个符号名是 ABI 定死的）。
24. **`td_api.tl` 的函数/构造器只认 `---functions---` / `---types---` 标记**，不做首字母启发式。
25. **`td_api.tl` 是必需输入**，拿不到就构建失败；顺序是 `TDLIB_TD_API_TL` → **包内 `vendor/<tdlib-version>/td_api.tl`** → 缓存 → 下载，缓存根顺序见上文。升 `tdlib-version` 时记得一起加 `vendor/<新版本>/`，否则下游又变回「要联网」。
26. **转换只在编译期**：`td_api.json` + `td_api_generated.rs` 来自同一份 `tl_schema` 实现。
27. **转换失败 = 构建失败**（读不了 / 解析不了 / 没分区标记都 panic）。
28. 生成器规则：类字段 `Option<Box<T>>`、认不出的类型降级 `serde_json::Value`、类 enum 变体字段不写 `pub`、函数 struct 字段写 `pub`。
29. **tdjson 只用 per-client 那套符号**；`TdExecuteFn` 是两参数；往 `TdApi` 加字段要同步 `size_of` 断言。
    **加载之后不卸载**：`TdClient::drop` 只发 `destroy`，`Library` 故意 `forget` 到进程结束——
    TDLib 在第一次 `create` 时就起了自己的 manager / scheduler 线程、活到进程结束，
    `dlclose` 之后它们再跑一步就是访问违例（Android 上表现为「stop 之后整个 App 崩」，2026 定位）。
30. **加载动态库走 `loader::open_library`**（Windows `LOAD_WITH_ALTERED_SEARCH_PATH`），它先查库自己所在目录；插件加载也走它。
31. **`attach_tdlib` 先、`spawn_dispatch` 后**；停止位必须显式持有并 `stop()` + `join().await`
    （正常收摊的唯一正路）。`ReceiveLoop` 的 `Drop` 只做**兜底置位**：把手在错误路径 / panic 里被丢掉时，
    那个 `spawn_blocking` 任务最迟 1s 后自己退出——否则 `tokio::runtime::Runtime::drop` 会**无限期**
    等它，进程卡死在退出那一刻（2026 实测）。别把 `Drop` 当收摊：它不会 join、也不刷盘。
32. **公开 ABI 类型不许用 `macro_rules!` 生成**（cbindgen 不展开宏）。
33. cbindgen 的 `after_includes` 只放宏与注释；用到 ABI 类型的代码放 `config.trailer`。
34. **`Option<fn>` 的签名要内联写**，不能走类型别名（会退化成不完整结构体）。
35. **`fuyubot_plugin.h` 是产物**，权威副本是 `fuyubot_abi::PLUGIN_HEADER`，别签进仓库。
36. **脚本插件四条硬约定**：`plugin_start` 等 `ready`、事件带订阅号、stdio 钉 UTF-8、起不来就报错。
37. **薄 SDK 的协议实现只有一份**：Python / JS 都走 `fuyubot-sdk-native`，语言侧只做「回调 ↔ C 函数指针」的转换；`script_plugin!` 已删（脚本不再各自带 cdylib）。
38. **`run --replay` 不接 TDLib**：回放必须离线可跑。
39. CLI 的模板只有一份来源（`include_str!`），不许复制第二份。
40. **`clap` 只允许出现在 CLI crate 里**，库 crate 不许依赖它。
41. **Bot Token / 手机号 / 两步验证密码一律不落盘**：只走 `TG_BOT_TOKEN` 或交互输入。
    `api_id` / `api_hash` 是**例外**：它们**每次冷启动**都要随 `setTdlibParameters` 发出去
    （session 有效也一样），不记住就得每次手输——`run` 交互问过之后**明文写回**
    `fuyubot.toml` 的 `[telegram]` 段（`api_hash` 不进 `Config` 树，插件 `get_config` 读不到；
    插件写配置只动 `plugin-config/`，**用户配置文件运行时永不重写**，所以不必也不许再补回凭据）。
    写回只发生在「这次真问过」时。
42. **TDLib 字段路径先问真库**（如 `file.id`；`user` 没有 `is_bot` / `username`）。
43. **TDLib 方法名 ≠ Bot API**：发送一律 `sendMessage` + `input_message_content`；参数名以 `td_api.tl` 为准，并留一条 `check_request` 测试。
44. **插件导出表只静态读、绝不 dlopen**：ABI 版本只能由 `plugin_init` 返回的 handle 在运行时判定（静态检查工具已随 SDK 仓删除）。
45. **改文本用编辑器工具**，不要用 shell 读写源码（PowerShell 会把 UTF-8 当 cp936，毁中文并吞换行）。
46. **发布资产契约**：tag `TDLib-v<版本>`、资产名 `TDLib-<os>-<arch>`（cargo 口径）、内容 `.tar.gz`；加平台要同时改 `ASSETS` 与 CI matrix。
47. **实例状态是 `.fuyubot/` 三个文件**：`lock`（只用于独占）、`status`（启动后发布）、`stop`（优雅停止请求）；另有可选的 `log-level`（运行期调插件日志级别的控制文件，新实例拿锁时清掉）。**不要合并**。
48. **构建期把 tdjson 放到可执行文件同级**（`build.rs` 的 staging），改任一侧要同步 `loader::tests`。
49. **发行形态是「下载即用」**：`fuyubot-<版本>-<OS>-<平台>.zip`（`<OS>` = windows / linux / macos，
    `<平台>` = x86_64 / aarch64），**解压出来就是本体**（zip 里不再套一层目录），
    内容是 exe + tdjson + `plugins/`；内置插件**不单独打包**，中间 tar.gz 只留在 Actions artifacts。
50. **Go 的 c-shared 插件不能卸载**：识别 `Go buildinf` 魔数并故意不卸载；同进程重复加载不可靠。
51. **发布顺序**：本仓发 `fuyubot-abi` / `fuyubot-tdlib-sys` →
    FuyuBot-Script 发各平台 dll 资产（不进 crates.io）。不要把 patch 写进下游的 `Cargo.toml`。
52. **插件配置段是 `[plugin.<插件名>]`**（ABI v4）：插件名点分 → TOML 嵌套表，宿主把插件名当**点路径**在 `plugin` 表里下钻（`fuyubot.manage` → `plugin.fuyubot.manage`）。`get_config(section, key)` 的 `section` 按**配置文件里的段名**寻址（`parse_section`）：`plugin.fuyubot.music`、`[plugin.fuyubot.music]`、裸插件名、顶层 `[bot]` 都认；**空串 = 自己那段**（走 `executor_name`，与 `set_config` 同一个默认）。`set_config(key, value)` **没有 section 参数**，只写**执行者**自己那段（靠 `owner_names` 映射判定，插件无法伪造）。要写别人的段走 `set_config_section(section, key, value)`（v6 尾部追加）：目标显式、**只接受 `plugin.*`**（顶层归用户，写了会被拒）、规则相同、宿主记一行「谁改了谁」。
    **配置是三层叠出来的**（后者只填前者的缺失键）：包默认（`plugin.json` 的 `config`，`fuyubot-loader` 在 `DylibPlugin::new` 里合并）< 运行时落盘（`<root>/plugin-config/<插件名>.toml`）< 用户配置（`fuyubot.toml` 的 `[plugin.<名>]`，**最高**）。
    **`set_config` 只写自己那一个文件**：段级、原子替换（同目录 `.toml.tmp` → `rename`）、只落这次改的键；`fuyubot.toml` **运行时永不重写**（注释与 `api_hash` 不会被卷走，`persist_toml` 已删除）。用户配置里钉住的键**拒绝写**（返回错误），不许「写完下次启动被静默盖回去」；没有配置文件（`config_path = None`）时同样报错，不许静默只写内存。写盘失败要还原内存。
    脚本插件侧同步入口是 `plugin_config()` / `pluginConfig()`（中间插件在 spawn 前把自己的段塞进 `config` 的 `plugin_config` 键）。
53. **依赖声明以 `plugin_manifest` 为准**：`DylibPlugin::inject()` / `soft_inject()` 先读 manifest（probe 阶段 `read_manifest` 拿到，`plugin_free_string` 归还），为空才退回 `[plugin.<名>].inject` / `.soft_inject`。别把配置当唯一来源——那会让「换台机器忘了抄」变成静默的加载顺序错误。
54. **`reconcile` 不许在 `apply` 期间递归推进**：`apply` 里注册服务会同步触发服务变更钩子；若钩子就地递归，提供者还在 `Activating`（`start` 没跑完）依赖方就被激活，软依赖「基础设施先起来」直接作废、硬依赖也会提前放人。钩子只置 `Progress::needed` 并返回，由外层 `reconcile_loop` 接着推进（`Progress::running` 是闸门，`RunningGuard` 保证 panic 也不焊死）。
55. **新增插件导出符号要两处同步**：`fuyubot-abi` 的 `SYM_*` 常量、`build.rs` 的 `TRAILER`（手写声明）。约束 18 只管**类型**（`export.include`），不管函数声明——ABI v4 的 `plugin_manifest` / `fuyubot_abi_version` / `plugin_free_handle` 就漏在头文件外过一轮，C 作者据此**根本不知道要写**。`TRAILER` 里用到 ABI 类型的声明必须留在 trailer（`after_includes` 在所有类型定义之前，放那儿 `-fsyntax-only` 直接报错，约束 33）。
56. **`plugin_manifest` 的 `version` 取 `env!("CARGO_PKG_VERSION")`**（宏在调用点展开，所以是**插件 crate** 的版本，不是 abi 的）：它是真值，不是 `"0.0.0"` 占位。所有字符串过 `quote()` 转义（`"` / `\` / `< 0x20` → `\u00XX`），非 ASCII 原样写 UTF-8。宿主侧 `declared()` 的优先级是「manifest 该侧非空 → 用它，否则回退 `[plugin.<名>].inject`」，**覆盖而非合并、且逐侧判定**——所以插件一旦声明 `inject`，配置里那份就静默失效。
57. **`plugin_free_handle` 的签名永久冻结**：`void plugin_free_handle(PluginHandle)` 只接 handle 本身、**不碰 VTable**，宿主正是在「版本对不上、VTable 一律不可信」那一刻调它（`probe` 与 `load_with_config` 两条路径都必须调，否则 `init` 里分配的状态就漏了）。所以它不能加参数、不能改语义——改了等于让老宿主在未来的插件上乱调。没导出它的 v3 插件只能像以前一样把状态留在那儿，`load` 照样返回 `AbiMismatch`。

58. **simplify 下沉成 op JSON 分发，不是一个方法一个函数指针**（ABI v5）：
    宿主侧实现在 `fuyubot-simplify::ops`，`HostRuntime::run_simplify` 调它——与 Rust 插件用的是
    **同一个** `Simplify`，不存在第二套行为。加操作 = 往 `SYNC_OPS` / `ASYNC_OPS` 与 `dispatch`
    里加一项，**不动 ABI**。异步 op 只能走 `simplify_invoke`（宿主起工作线程跑，结果照 `@request_id`
    回投）；同步 op（`parse_text_entities` / `rich_from_markdown` / `list_ops`）走
    `simplify_invoke_result`，回调里安全，传异步 op 返回 `code:"async_op"`。
    ⚠️ op 工作线程的归属是**宿主自己**（`owner = 0`），它顺手发出的 TDLib 请求的中间响应只到
    全局订阅者——插件只该收到最后那一条 `simplifyResult`。

59. **插件分发格式 v1（bundle）**：一个插件 = `plugins/<id>/`（脚本插件仍在
    `plugins/scripts/<id>/`），根上的 `plugin.json` 是**权威静态元数据**；契约见文档站
    [插件分发格式 v1](https://catmoecircle.github.io/FuyuBot-docs/docs/plugin/bundle/)。
    `plugin_entries` 只扫一层：bundle 按 id 在前、旧形态
    平铺 dll 按文件名在后；`.disabled/` / `<id>.disable` 一律不当候选。
    加载器必须能**不 dlopen** 就拿到 `id/version/kind/entry/depends`（`probe_bundle`），
    真正 `dlopen` 只发生在 `DylibPlugin::apply`；`plugin_files` 只服务旧形态，
    新代码不要再用它扫插件目录。bundle 的 `plugin_init` 名字与清单 id 不一致要拒绝加载。
60. **Go 的 c-shared 没有 `plugin.json` 就拒绝**：`PluginLoader::probe` 在读文件魔数阶段
    直接返回 `GoRequiresManifest`，**不 dlopen**——它一旦映射就永远不能卸载，必须在
    映射前就知道依赖。唯一例外是 `probe_for_migration`（`plugin migrate` 用，进程即将退出），
    调用方必须提示「本次迁移进程会永久映射」。bundle 里的 Go 允许，但
    `App::load_plugins` 必须把它排在队列最后；同进程别放多个会装信号处理器的 Go 插件。
61. **商店边界**：`fuyubot-loader` / 运行时**永不**下载、解压、读索引——这些只在 CLI 的
    TUI 商店层（`crates/fuyubot-cli/src/panel/store.rs`），且**不单独建 crate**（用户 2026
    拍板：完整功能只集成在 TUI）。其它宿主（APK 等）自己做商店，只复用 bundle 契约。
    `plugin install` 的 CLI 只装本地目录/包，不认 zip。

62. **APK 在 `release-bot.yml` 的 `build-apk` job 里编，壳不在本仓**：
    壳是 [FuyuBot-for-Android](https://github.com/CatMoeCircle/FuyuBot-for-Android)（Kotlin + Compose），
    按 `main` 取；内核是同一 tag 的 `build-android` job 编出的 JNI 包。⚠️ **JNI 包只作为
    Actions artifact 喂给 APK job，不再作为 Release 资产发布**（Release 只挂 `.apk`）。
    契约有四条，改之前先看 `.github/scripts/build-android-apk.sh`：
    - **运行期调日志不用加 JNI 方法**：壳写 `<root>/.fuyubot/log-level`（一行级别名），
      `run` 每 500ms 轮询，与 CLI 同一个控制文件协议。订阅者**不许再挂级别过滤**——
      `tracing_subscriber::fmt()` 默认上限是 `INFO`，会把 `debug` / `trace` 在格式化前丢掉。
    - **指定目录是 `app/src/main/assets/fuyubot-kernel/`**（不是 `jniLibs/`）。它被上游
      `.gitignore` 排除，缺了也照样编得出 APK——只是装到设备上首次启动报「资产中缺少内核目录」。
      所以判据只能是「APK 里真有 `assets/fuyubot-kernel/lib*.so`」，不能只看 `assembleRelease` 绿了。
    - **版本要注入进去**：上游 `CURRENT_KERNEL_VERSION`（`KernelManager` 拿它决定要不要重装内核，
      相等就跳过）与 `versionCode` / `versionName` 都得按本仓的 tag 改写。漏改的症状是
      「APK 更新了但还在跑旧内核」，且**不会报错**。
    - **资产名是 `fuyubot-<版本>-<abi>.apk`**（例 `fuyubot-v0.1.3-arm64-v8a.apk`）。两个 ABI 的 `.so`
      文件名完全相同，**不能装进同一个 APK**，所以必须分开发、文件名里带 abi。
      不加 `-android-` 段不撞名：可执行文件那批是 `fuyubot-<版本>-android-<平台>.zip`，
      JNI 包不进 Release。⚠️ publish 的下载表因此要**按扩展名分派**标签，
      只按基名匹配的话 `.apk` 与 `.zip` 的 `-x86_64` 会串味。
    签名走 `apksigner`（不用 AGP 的 injected signing——那要四个 `-P` 属性齐全，缺一个就**静默不签**）。
    keystore 从 Secrets 还原，四个都要在：`ANDROID_KEYSTORE_BASE64` / `ANDROID_KEYSTORE_PASSWORD` /
    `ANDROID_KEY_ALIAS` / `ANDROID_KEY_PASSWORD`。**同一个 keystore 不能换**，换了老用户装不上新 APK。

63. **服务调用经过宿主（ABI v6）**：`HostApi::call_service` 走 VTable 尾部的 `call_service` 桥，
    不再由插件自己取 vtable 直调。宿主在调用期间压一张**调用者栈**（值取自调用方那份
    `PluginHost.owner`，与线程无关），`current_plugin` 优先读它、其次才退回线程作用域栈——
    所以插件在自己的线程上调服务也认得出调用方，且无法自报。`set_config` 按**执行者栈**
    （服务调用期间是提供方，其余时候是线程作用域栈）判定「谁的段」，两张栈别混。v5 → v6 的迁移说明在文档站
    [ABI v5 → v6 迁移](https://catmoecircle.github.io/FuyuBot-docs/docs/plugin/abi-v6/)。
    同一次 v6 里还尾部追加了三个入口（**不跳版本号**、靠 `size` 协商）：`last_error`（`call_service`
    失败原因，插件侧已自动拼进错误）、`list_services`（看得见的服务清单：名字 / 版本 / 方法 / 提供方）、
    `set_config_section`（跨插件写配置）。服务表里存的不再只是 vtable，还带**提供方 owner**：
    调用期间压一层「执行者」，所以服务方法里的 `set_config` 落提供方自己的段（约束 52）。

64. **命令回复统一走宿主**：插件回复命令用 `Command::reply_then`（Rust）/ `fuyubot.host.command`
    service（其它语言），**不要自己拼 `sendMessage`**。入参与 `SendMessageParams` 相同
    （`text` / `media` / `reply_markup` / `link_preview` / `topic_id` / `timeout_ms`），
    `reply_to_message_id` 由宿主覆盖成命令消息。宿主决定：bot 账户 / user 账户**白名单用户**
    触发 → `sendMessage` + `reply_to`；user 账户**自己**触发 → 编辑原命令消息（文本
    `editMessageText`、富媒体 `editMessageMedia`）。成功返回 TDLib 的最终 `Message`，失败返回
    `{"@type":"error",…}`；`reply` **阻塞**，命令回调里必须用 `reply_then`（句柄在 `stop()`
    里 join）。命令消息带 `ephemeral_message_id > 0`、当前账号是 bot（`getMe` 的
    `userTypeBot`，不用配置兜底）、且**不是私聊**（群/频道 chat_id 为负）时，回复改走
    `sendEphemeralMessage` + `inputMessageReplyToEphemeralMessage`，`receiver_user_id` =
    命令发送者；任一条件不满足就退回普通消息/编辑，**不尝试 ephemeral**（退回普通消息时
    也不 `reply_to` 那条 ephemeral 命令）。被拦下的命令只从
    `updateNewMessage` 通道消失，`update` 通用通道照发。`[command].prefix` 是**数组**：
    不写按账号默认（bot `/`、user `.`），写了就**整套替换**（bot 不再自动保留 `/`）；
    user 账户下 `/` 即使写进数组也始终不生效。非 `/` 命令头放行后归一化成 `/`。
    **user 账户默认白名单模式**（只认自己 + `whitelist`）；`[command].whitelist_only = false`
    可关，但模板 / 文档的注释必须写明「⚠️ 不建议关闭」——关掉后任何人都能触发插件。
    bot 账户的 `whitelist` 规则不受 `whitelist_only` 影响。

## 文档

公开文档只在**文档站**（`FuyuBot-Docs/content/docs/**`）；改 ABI / CLI / 行为时同步那边。
