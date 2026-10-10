# fuyubot-abi

宿主与插件之间唯一的稳定边界：`HostApiVTable`/`ServiceVTable` 的 `#[repr(C)]` 定义、插件必须导出的四个符号、插件侧 `HostApi` 门面，以及 `define_plugin!` 宏。
