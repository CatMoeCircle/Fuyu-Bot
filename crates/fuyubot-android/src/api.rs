//! 三个「运行时查询」接口的实现：插件列表 / 插件详情 / TDLib JSON 请求。
//!
//! C 符号与 JNI 方法都只是薄壳（见 [`crate::lib`] 里的导出）；逻辑放这儿，
//! 好在没有 JVM 的机器上直接测——本仓的 Android 单测本来就跑在桌面。
//!
//! 三个接口共用一条纪律：**运行时没起来不报错，返回空结果**。`plugins` 是空表，
//! `plugin_info` 是 `None`，`td_request` 带一句可读的原因。这样 Java 侧在启动过程中
//! 随手调一下也不会崩。

use std::time::Duration;

use crate::live;

/// `timeout_ms <= 0` 时的默认等待。
pub const DEFAULT_TD_TIMEOUT: Duration = Duration::from_secs(30);
/// `timeout_ms` 的上限：再长也只是把调用线程焊在那儿（TDLib 的慢请求也不该等这么久）。
pub const MAX_TD_TIMEOUT: Duration = Duration::from_secs(600);

/// 插件列表 JSON。没在跑 = 空表（**不是错误**）。
///
/// 空表与正常返回**同一份形状**（含全部计数字段）：形状由 `fuyubot-app` 的
/// `empty_plugins_json()` 生成，客户端不用为「还没起来」写第二套解析。
pub fn plugins_json() -> String {
    match live::get() {
        Some(view) => view.plugins_json(),
        None => fuyubot_app::empty_plugins_json(),
    }
}

/// 单个插件的信息；没在跑或没这个插件 = `None`。
pub fn plugin_json(name: &str) -> Option<String> {
    live::get()?.plugin_json(name)
}

/// 发一条原始 td_api JSON 并等响应。返回响应 JSON 文本（`@type: "error"` 也是它）。
pub fn td_request(request: &str, timeout_ms: i64) -> Result<String, String> {
    // Java 在 `onAsk` / `onState` 回调里同步调它时，当前就是工作线程：等下去只会
    // 自己等自己（响应要工作线程去收）。立刻报错，别让调用方白等一个超时。
    if crate::state::on_worker_thread() {
        return Err(
            "不能在工作线程的回调里等 TDLib 响应（onAsk / onState 会在工作线程上跑）；换一个线程调"
                .to_owned(),
        );
    }
    let Some(view) = live::get() else {
        return Err("运行时还没起来（先 start）".to_owned());
    };
    view.td_request(request, timeout_from_ms(timeout_ms))
}

/// 毫秒 → 等待预算：非正数走默认，超过上限就钳住。
fn timeout_from_ms(timeout_ms: i64) -> Duration {
    if timeout_ms <= 0 {
        DEFAULT_TD_TIMEOUT
    } else {
        Duration::from_millis(timeout_ms as u64).min(MAX_TD_TIMEOUT)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 没装运行时（本机单测就是这个状态）：空表 + 空详情 + 可读的错误。
    #[test]
    fn the_empty_state_is_well_formed() {
        let parsed: serde_json::Value = serde_json::from_str(&plugins_json()).unwrap();
        assert_eq!(parsed["count"], serde_json::json!(0));
        assert_eq!(parsed["plugins"], serde_json::json!([]));
        assert_eq!(parsed["scanned"], serde_json::json!(0));
        assert_eq!(parsed["skipped"], serde_json::json!(0));
        // 空表就是「零个插件的报告」，不是另一种形状。
        assert_eq!(
            parsed,
            serde_json::from_str::<serde_json::Value>(&fuyubot_app::empty_plugins_json()).unwrap()
        );
        assert!(plugin_json("fuyubot.demo").is_none());
        let error = td_request(r#"{"@type":"getMe"}"#, 1000).unwrap_err();
        assert!(error.contains("还没起来"), "{error}");
    }

    #[test]
    fn timeout_falls_back_to_the_default_and_is_capped() {
        assert_eq!(timeout_from_ms(0), DEFAULT_TD_TIMEOUT);
        assert_eq!(timeout_from_ms(-5), DEFAULT_TD_TIMEOUT);
        assert_eq!(timeout_from_ms(1500), Duration::from_millis(1500));
        assert_eq!(timeout_from_ms(i64::MAX), MAX_TD_TIMEOUT);
    }

    /// 在工作线程（`onAsk` / `onState` 回调就在这条线程上）里等响应 = 自己等自己：
    /// 必须立刻报错，而不是挂到超时。
    #[test]
    fn td_request_refuses_to_block_the_worker_thread() {
        crate::state::set_worker_thread(Some(std::thread::current().id()));
        let error = td_request(r#"{"@type":"getMe"}"#, 1000).unwrap_err();
        crate::state::set_worker_thread(None);
        assert!(error.contains("工作线程"), "{error}");
    }
}
