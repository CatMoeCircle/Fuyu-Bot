# fuyubot-tdlib-sys

TDLib 的类型与 FFI 绑定（发布于 [crates.io](https://crates.io/crates/fuyubot-tdlib-sys)）：`build.rs` 在编译期把 `td_api.tl` 转成 Rust 类型（`td_api`）与可校验元数据（`schema`），运行时不做任何生成、不加载任何原生库。

[![](https://img.shields.io/badge/powered_by-dsh-4D6BFE?style=flat-square&logo=deepseek&logoColor=white)](https://github.com/deepseek-ai/deepseek-harness)

## 里面有什么

- **`td_api`** —— `td_api.tl` 的 Rust 镜像：类字段 `Option<Box<T>>`，认不出的类型降级 `serde_json::Value`。
- **`schema` / `tl_schema`** —— 可校验的类型元数据；`td_api.json` 与 `td_api_generated.rs` 出自同一份实现。
- **`ffi`** —— per-client 的 tdjson 符号声明（`TdExecuteFn` 两参数），加载在 `fuyubot-tdlib`，本 crate 只声明不链接。
- 本 crate 无 tokio、无网络。

## `td_api.tl` 从哪来

`build.rs` 在编译期需要它，按这个顺序找：

1. `TDLIB_TD_API_TL` —— 显式指定（文件或目录，最高优先级）。想用别的 TDLib 版本就设它。
2. **包内自带的 `vendor/<版本>/td_api.tl`** —— 1.8.69 已随包发布，所以从 crates.io
   装下来的下游（和 docs.rs）**不需要联网、也不需要先跑一次下载**。
3. 本地缓存 —— `FUYUBOT_TDLIB_VENDORED_CACHE` → `$CARGO_HOME/fuyubot-tdlib-vendored`
   → 项目本地 `.tdlib-cache/`（`<缓存根>/<版本>/td_api.tl`）。
4. 从 GitHub Releases 下载 —— 成功后写进缓存，一台机器只会真正下一次。

版本号取自 `[package.metadata.fuyubot] tdlib-version`；包内目录按版本号拼，
所以换了版本却没提供对应 `vendor/` 时会自动退到 3 / 4（不会静默编一份旧 schema）。

拿不到 `td_api.tl` = **构建失败**，没有降级路径。

schema 来自 TDLib（**Boost Software License 1.0**），来源与说明见 `vendor/README.md`。

## 相关

- [调用 TDLib](https://catmoecircle.github.io/FuyuBot-docs/docs/plugin/tdlib/)
- [仓库](https://github.com/CatMoeCircle/Fuyu-Bot)
