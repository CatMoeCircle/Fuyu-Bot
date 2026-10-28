# fuyubot-tdlib-sys

TDLib 的类型与 FFI 绑定：`build.rs` 在编译期把 `td_api.tl` 转成 Rust 类型（`td_api`）与可校验元数据（`schema`），运行时不做任何生成、不加载任何原生库。

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

schema 来自 TDLib（**Boost Software License 1.0**），来源与说明见 `vendor/README.md`。
