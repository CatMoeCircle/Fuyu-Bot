# fuyubot-tdlib

TDLib 接入层的**运行时半边**：加载原生库、跑 `td_receive` 循环、走授权状态机。

[![](https://img.shields.io/badge/powered_by-dsh-4D6BFE?style=flat-square&logo=deepseek&logoColor=white)](https://github.com/deepseek-ai/deepseek-harness)

## 里面有什么

- **`loader`** —— 按四级优先级定位并加载 `tdjson`：`TDLIB_TDJSON_PATH` → vendored → exe 同级目录 → Linux 带版本系统库 → 裸库名；vendored 资产表、版本协商、`open_library` 都在这儿。
- **`client`** —— `TdClient`：`@extra` 注入 + `td_receive` 循环 + `ReceiveLoop` 停止把手 + `send_and_wait` 的 oneshot 等待。
- **`authorize`** —— 登录状态机（bot token / 手机号验证码 / 扫码），登录态由 TDLib 落进 `data/`。

类型绑定（`td_api` / `schema` / `ffi` / `tl_schema`）住在 [`fuyubot-tdlib-sys`](../fuyubot-tdlib-sys)：`td_api.tl` 在那边**编译期**转成 Rust 类型与可校验元数据，运行时不读文件、不生成。本 crate 依赖并重导出 sys 的公开 API，`fuyubot_tdlib::TdApiSchema` 这些路径因此不变。

tdjson 是**运行时**动态加载的——代码里没有 `#[link]`，符号一律经 `ffi::TdApi`。

## 相关

- [调用 TDLib](https://catmoecircle.github.io/FuyuBot-docs/docs/plugin/tdlib/)
- [仓库](https://github.com/CatMoeCircle/Fuyu-Bot)
