//! __NAME__ 的 ABI 胶水。逻辑全在 `__SCRIPT__` 里。
//!
//! 这一行展开出 `Plugin` 实现和四个 C ABI 导出符号；宿主加载本 cdylib 之后，
//! 它会启动 node（退路是 bun）去跑 `__SCRIPT__`，两边用一行一个 JSON 说话。

fuyubot_sdk_script::script_plugin!("__NAME__", js, "__SCRIPT__");
