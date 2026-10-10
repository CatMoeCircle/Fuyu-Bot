//! counter 示例插件的 ABI 胶水。**逻辑全在 `counter.ts` 里。**
//!
//! 宿主 dlopen 这个 cdylib，`plugin_start` 时它启动 node 执行 `counter.ts`；
//! 两边用「一行一个 JSON」说话（协议见 `fuyubot-sdk-script` 的 crate 文档）。
//!
//! `counter.ts` 靠 Node 自带的**类型剥离**直接跑（Node ≥ 23.6），
//! 所以这个示例不需要 `tsc`——见 `counter.ts` 顶部的说明。
//!
//! 宿主怎么知道脚本在哪？`fuyubot run` 会找与 cdylib 同名的脚本（`.py` / `.js` / `.ts`），
//! 把绝对路径写进插件的 config JSON（见 `find_script`）。

fuyubot_sdk_script::script_plugin!("fuyubot.example.counter", js, "counter.ts");
