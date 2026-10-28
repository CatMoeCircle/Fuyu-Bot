# fuyubot-abi

宿主与插件之间唯一的稳定边界：`HostApiVTable`/`ServiceVTable` 的 `#[repr(C)]` 定义、插件必须导出的**七个**符号（v3 的四件套 `plugin_init` / `plugin_start` / `plugin_stop` / `plugin_free_string`，加 v4 的 `plugin_manifest` / `fuyubot_abi_version` / `plugin_free_handle`）、插件侧 `HostApi` 门面、`define_plugin!` 宏，以及 Rust 侧的 `ResponseRouter` / `ResponseFuture` / `UpdateFuture`（可 `.await` 的响应与更新等待；不改 C 布局）。
