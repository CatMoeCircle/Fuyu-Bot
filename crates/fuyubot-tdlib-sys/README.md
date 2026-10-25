# fuyubot-tdlib-sys

TDLib 的类型与 FFI 绑定：`build.rs` 在编译期把 `td_api.tl` 转成 Rust 类型（`td_api`）与可校验元数据（`schema`），运行时不做任何生成、不加载任何原生库。
