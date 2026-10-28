# fuyubot-plugin-manifest-fixture

测试夹具：一个声明了硬/软依赖、别的什么都不做的空插件，只服务 `fuyubot-loader` 的
`plugin_manifest` 端到端测试。它的版本号 `2.3.4` 故意与 workspace 不同，用来证明
manifest 里的 `version` 取自插件 crate 而不是 `fuyubot-abi`。
