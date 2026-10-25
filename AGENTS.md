# AGENTS.md — fuyubot

## 这是什么

基于 TDLib 的 Telegram bot / user 框架：**一个宿主进程 + N 个动态库插件**。
插件是 cdylib（`.dll` / `.so` / `.dylib`），只认四个导出符号与一张 `HostApiVTable`（ABI 版本 2）。
TDLib 在**运行时**动态加载（`fuyubot-tdlib::loader`），`td_api.tl` 在**编译期**转成 Rust 类型与可校验元数据。

三个仓库（本仓只放运行时）：

| 仓库 | 放什么 |
| --- | --- |
| **Fuyu-Bot**（本仓） | 运行时全部 + `fuyubot-abi` + `fuyubot-tdlib-sys`（后两个发 crates.io） |
| FuyuBot-SDK | `fuyubot-plugin-api`、`fuyubot-sdk-script`、`fuyubot-sdk` CLI + 五种语言示例 |
| FuyuBot-Docs | 文档站（<https://catmoecircle.github.io/FuyuBot-docs/>，源在 `content/docs/**`） |

跨仓依赖只有一条、单向：SDK 通过 crates.io 依赖本仓的 `fuyubot-abi` / `fuyubot-tdlib-sys`。

## 结构

| crate | 一句话 |
| --- | --- |
| `fuyubot-cli` | 二进制 `fuyubot`：`init` / `run` / `status` / `stop` / `config` / `plugin`（clap） |
| `fuyubot-core` | Cordis 内核：`Context` / `Effect` / `Plugin` / `PluginManager`。不认识 Telegram |
| `fuyubot-abi` | C ABI：`HostApiVTable`（18 字段）、`PluginHandle`、`define_plugin!`、cbindgen 头文件 |
| `fuyubot-tdlib-sys` | 绑定层：`td_api.tl` → 类型 + 元数据 + FFI 声明（无 tokio / 无网络） |
| `fuyubot-tdlib` | `loader`（四级定位）+ `client`（`td_receive` 循环、`ReceiveLoop`）+ `authorize`（授权状态机） |
| `fuyubot-event` / `service` / `tool` / `middleware` | 四个机制 crate，只依赖 `core`，彼此不依赖 |
| `fuyubot-loader` | 唯一宿主层：VTable 桥、更新管线、插件加载、注册回滚 |
| `fuyubot-simplify` | TDLib 便捷封装（发消息 / 编辑 / Markdown → entities / 命令解析） |
| `fuyubot-plugin-*` | demo / badabi 测试插件 + 三个内置插件（`/start`、`/help`、`/plugins`） |

数据流：插件的 `on/invoke/emit/middleware` → `HostApiVTable` → `HostRuntime`（事件总线 / 服务 / 工具 / 管线 / TdClient）
→ 更新走六阶段 + 双通道（`"update"` 与具体 `@type`），响应按 `@request_id` 回到发起插件。
细节都在文档站的[架构页](https://catmoecircle.github.io/FuyuBot-docs/docs/dev/architecture/)。

## 构建

`td_api.tl` 是**必需输入**：`TDLIB_TD_API_TL` → 缓存（`<缓存根>/<版本>/td_api.tl`）→ 从 Releases 下载。
缓存根顺序：`FUYUBOT_TDLIB_VENDORED_CACHE` → `$CARGO_HOME/fuyubot-tdlib-vendored` → 项目本地 `.tdlib-cache/`。
仓里的 `.cargo/config.toml` 已经把缓存指向仓内 `.tdlib-cache`（`relative = true`），本地直接敲命令即可。

```powershell
cargo build --workspace --offline     # 0 退出、0 warning
cargo test  --workspace --offline     # 180 passed / 0 failed / 9 ignored
cargo tree  --workspace --offline     # 无环
cargo fmt --all --check               # 0 diff

cargo bot                             # = fuyubot run（当前目录）
cargo package-bot                     # release：fuyubot.exe + 三个内置插件（tdjson 由 build.rs 放到 exe 同级）
```

## 改动后怎么验证

除默认自检外，按改动面补跑：

```powershell
# 真库（真 tdjson + 真 schema）：改 ffi 符号/arity、loader、tl_schema/tl_codegen、登录授权后必跑
$env:TDLIB_TDJSON_PATH = "F:\Rust\FuyuBot\.tdlib-cache\1.8.68\windows-x86_64"
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
pnpm build        # 再把 out/ 推到 gh-pages（手动，没有 CI 自动部署）
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
10. **`Context` 只暴露 `&self`**：注册走 `provide` 并**持有** `Effect`，不加 `&mut` 接口。
11. **`Effect` 是 `#[must_use]`**：`let _ =` 会立刻回滚；要么存起来，要么显式 drop / rollback。
12. **四个机制 crate 只依赖 `core`，彼此不许依赖**；组合由宿主层做。
13. **与 Context 集成 = `install(ctx)` + 扩展 trait**；服务缺失时报错要带「该怎么 install」。
14. **扩展 trait 不许与 `Context` 固有方法重名**：命名服务叫 `provide_named` / `resolve`。
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
25. **`td_api.tl` 是必需输入**，拿不到就构建失败；缓存根顺序见上文。
26. **转换只在编译期**：`td_api.json` + `td_api_generated.rs` 来自同一份 `tl_schema` 实现。
27. **转换失败 = 构建失败**（读不了 / 解析不了 / 没分区标记都 panic）。
28. 生成器规则：类字段 `Option<Box<T>>`、认不出的类型降级 `serde_json::Value`、类 enum 变体字段不写 `pub`、函数 struct 字段写 `pub`。
29. **tdjson 只用 per-client 那套符号**；`TdExecuteFn` 是两参数；往 `TdApi` 加字段要同步 `size_of` 断言。
30. **加载动态库走 `loader::open_library`**（Windows `LOAD_WITH_ALTERED_SEARCH_PATH`），它先查库自己所在目录；插件加载也走它。
31. **`attach_tdlib` 先、`spawn_dispatch` 后**；停止位必须显式持有并 `stop()`，drop 不会停。
32. **公开 ABI 类型不许用 `macro_rules!` 生成**（cbindgen 不展开宏）。
33. cbindgen 的 `after_includes` 只放宏与注释；用到 ABI 类型的代码放 `config.trailer`。
34. **`Option<fn>` 的签名要内联写**，不能走类型别名（会退化成不完整结构体）。
35. **`fuyubot_plugin.h` 是产物**，权威副本是 `fuyubot_abi::PLUGIN_HEADER`，别签进仓库。
36. **脚本插件四条硬约定**：`plugin_start` 等 `ready`、事件带订阅号、stdio 钉 UTF-8、起不来就报错。
37. `script_plugin!` 内部走 `$crate::__macro_support::…`，不额外要求插件依赖别的 crate。
38. **`run --replay` 不接 TDLib**：回放必须离线可跑。
39. CLI 的模板与 SDK 只有一份来源（`include_str!`），不许复制第二份。
40. **`clap` 只允许出现在两个 CLI crate 里**，库 crate 不许依赖它。
41. **凭据一律不落盘**：只走 `TG_API_ID` / `TG_API_HASH` / `TG_BOT_TOKEN` 或交互输入。
42. **TDLib 字段路径先问真库**（如 `file.id`；`user` 没有 `is_bot` / `username`）。
43. **TDLib 方法名 ≠ Bot API**：发送一律 `sendMessage` + `input_message_content`；参数名以 `td_api.tl` 为准，并留一条 `check_request` 测试。
44. **`fuyubot-sdk check` 静态读导出表**，绝不 dlopen；ABI 版本只能运行时判定。
45. **改文本用编辑器工具**，不要用 shell 读写源码（PowerShell 会把 UTF-8 当 cp936，毁中文并吞换行）。
46. **发布资产契约**：tag `TDLib-v<版本>`、资产名 `TDLib-<os>-<arch>`（cargo 口径）、内容 `.tar.gz`；加平台要同时改 `ASSETS` 与 CI matrix。
47. **实例状态是 `.fuyubot/` 三个文件**：`lock`（只用于独占）、`status`（启动后发布）、`stop`（优雅停止请求）；不要合并。
48. **构建期把 tdjson 放到可执行文件同级**（`build.rs` 的 staging），改任一侧要同步 `loader::tests`。
49. **发行形态是「下载即用」**：`fuyubot-<版本>-<OS>-<平台>.zip`（`<OS>` = windows / linux / macos，
    `<平台>` = x86_64 / aarch64），**解压出来就是本体**（zip 里不再套一层目录），
    内容是 exe + tdjson + `plugins/`；内置插件**不单独打包**，中间 tar.gz 只留在 Actions artifacts。
50. **Go 的 c-shared 插件不能卸载**：识别 `Go buildinf` 魔数并故意不卸载；同进程重复加载不可靠。
51. **三仓发布顺序**：本仓先发 `fuyubot-abi` / `fuyubot-tdlib-sys` → SDK 发 `fuyubot-plugin-api` → `fuyubot-sdk-script`；不要把 patch 写进 SDK 的 `Cargo.toml`。

## 文档

公开文档只在**文档站**（`FuyuBot-Docs/content/docs/**`）；改 ABI / CLI / 行为时同步那边。
本仓 `docs/` 下是随源码分发的工程台账（HANDOFF / ROADMAP / gaps / pitfalls / decisions / tech-debt），不入库，也不再要求随每次改动更新。
