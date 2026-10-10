//! weather 示例插件的 ABI 胶水。**逻辑全在 `weather.py` 里。**
//!
//! 宿主 dlopen 这个 cdylib，`plugin_start` 时它启动 python 解释器执行 `weather.py`；
//! 两边用「一行一个 JSON」说话（协议见 `fuyubot-sdk-script` 的 crate 文档）。
//!
//! 宿主怎么知道 `weather.py` 在哪？`fuyubot run` 会找与 cdylib 同名的脚本，
//! 把绝对路径写进插件的 config JSON（见 `find_script`）。

fuyubot_sdk_script::script_plugin!("fuyubot.example.weather", python, "weather.py");
