# fuyubot-loader

宿主层：`HostRuntime` 把 `HostApiVTable` 的每个函数桥到四大机制与 TDLib，`PluginLoader` 用 libloading 加载 cdylib 插件并做 ABI 版本协商。
