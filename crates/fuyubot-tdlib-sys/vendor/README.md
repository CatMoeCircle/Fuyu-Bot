# vendor

`<版本>/td_api.tl` 是从 TDLib 对应版本原样拷来的 schema，**只为让下游不必联网**。

- **来源**：本仓 Release `TDLib-v1.8.69` 下的 `td_api.tl` 资产（由 `build-tdlib.yml` 产出），
  也可以从 <https://github.com/tdlib/td> 对应 tag 的 `td/generate/scheme/td_api.tl` 取。
- **许可**：TDLib 整体是 **Boost Software License 1.0**（BSL-1.0），这份 schema 随它一起分发。
- **用途**：`build.rs` 在 `TDLIB_TD_API_TL` 没设时优先读这里，于是
  crates.io 下游首次构建不用下载 1.2MB、docs.rs 也能直接构建文档页。

⚠️ 升 `[package.metadata.fuyubot] tdlib-version` 时要一起加 `<新版本>/td_api.tl`。
`build.rs` 按版本号拼路径，缺了就退回「缓存 → 下载」——不会静默编一份旧 schema，
但下游又会变成要联网。
