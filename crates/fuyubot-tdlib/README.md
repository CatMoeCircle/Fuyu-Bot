# fuyubot-tdlib

TDLib 接入层的**运行时半边**：`loader` 按四优先级定位并加载原生库（vendored 资产表、版本协商、`open_library`），`client` 提供 `TdClient`（`@extra` 注入 + `td_receive` 循环 + `ReceiveLoop` 停止把手），`authorize` 是登录状态机。

类型绑定（`td_api` / `schema` / `ffi` / `tl_schema`）住在 [`fuyubot-tdlib-sys`](../fuyubot-tdlib-sys)：`td_api.tl` 在那边**编译期**转成 Rust 类型与可校验元数据（拿不到文件就构建失败，运行时不读文件、不生成）。本 crate 依赖并重导出 sys 的公开 API，`fuyubot_tdlib::TdApiSchema` 这些路径因此不变。
