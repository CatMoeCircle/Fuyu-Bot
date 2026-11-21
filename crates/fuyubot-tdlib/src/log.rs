//! TDLib 自己的日志（不是 API 更新）的去处。
//!
//! TDLib 默认把内部日志**直接写 stderr**。在终端里跑没问题，但 TUI 进了
//! alternate screen 之后，TDLib 的一行错误（TLS 警告、`getMe` 失败、`td_receive`
//! 超时……）就会盖在界面上，把整个面板顶乱——它不是 `tracing` 发的，
//! `logging::enter_tui` 那条「日志改写到文件」的路根本拦不住。
//!
//! 所以客户端一建好就装 `td_set_log_message_callback`，把内部日志交给宿主：
//! 宿主用 [`set_log_sink`] 注册落点（CLI / Android 都接到自己的 `tracing` 上，
//! TUI 期间自然落进 `logs/fuyubot.log`）。**没有注册落点时退回 stderr**，
//! 保持 TDLib 原本的行为，不把测试 / 裸宿主的日志吞掉。
//!
//! ⚠️ **光装回调不够**：TDLib 的 `LogInterface::append` 是「先写默认流，再调回调」，
//! 回调是**追加**不是替换（1.8.70 真库实测：装了回调，stderr 里照样出现
//! `[ 3][t …]` 原样行）。默认流就是 stderr，所以还要用 `setLogStream` +
//! `logStreamEmpty` 把它换成空实现，回调才是唯一落点（[`silence_default_stream`]）。
//!
//! 级别沿用 TDLib 自己的数字：0 致命 / 1 错误 / 2 警告 / 3 信息 / 4 调试 /
//! 5 详细调试（与 `td_set_log_verbosity_level` 同一套，见
//! [`DEFAULT_LOG_VERBOSITY`](crate::DEFAULT_LOG_VERBOSITY)）。
//!
//! ⚠️ 回调是**全局**的（TDLib 只有一个日志器）：同一进程里所有客户端共用一份落点，
//! 与「库加载之后不卸载」是同一条边界。

use std::ffi::{CStr, c_char};
use std::sync::Mutex;

use crate::client::TdClient;
use crate::ffi::TdApi;

/// 宿主接走 TDLib 内部日志的钩子：`(TDLib 级别, 单行内容)`。
///
/// 多行消息会按行拆开逐条回调，落点不需要自己再拆。
pub type LogSink = fn(i32, &str);

static SINK: Mutex<Option<LogSink>> = Mutex::new(None);

/// 注册 TDLib 内部日志的落点；重复注册以最后一次为准。
///
/// 回调在 `TdClient::new` 里装，这里只记「往哪送」，所以**建客户端之前或之后**
/// 都可以调——之后再来一条日志就按新落点走。
pub fn set_log_sink(sink: LogSink) {
    *SINK.lock().unwrap_or_else(|error| error.into_inner()) = Some(sink);
}

fn sink() -> Option<LogSink> {
    *SINK.lock().unwrap_or_else(|error| error.into_inner())
}

/// 装回调（`td_set_log_message_callback`），返回是否装上了。
///
/// `max_verbosity` 是「最多报到哪一级」，与 `td_set_log_verbosity_level` 共用数字。
/// 返回 `false` = 这份 tdjson 没导出该符号（老版本 / 精简构建），TDLib 继续自己写
/// stderr——宁可多一行终端输出，也不能因为一个**日志**符号拒绝整份库。
pub(crate) fn install(api: &TdApi, max_verbosity: i32) -> bool {
    let Some(set_callback) = api.set_log_message_callback else {
        return false;
    };
    // SAFETY: 符号来自已加载的 TDLib（`TdClient::new` 刚解析过）；回调是
    // `extern "C"`、不 panic，字符串生命周期由 TDLib 保证（见 `on_tdlib_log`）。
    unsafe { set_callback(max_verbosity.max(0), Some(on_tdlib_log)) };
    true
}

/// 把 TDLib 自己的默认日志流换成空实现（`setLogStream` + `logStreamEmpty`）。
///
/// 只装回调是**止不住 stderr** 的（见模块文档）：默认流还在，TDLib 先写它再调回调。
/// 换成空实现之后，回调成为唯一落点，一行也不会再落到终端上。
///
/// 同步方法，`td_json_client_execute` 直接跑，不需要登录。失败不致命：老库 / schema
/// 不认这个方法时，最坏退回「TDLib 自己写 stderr」——与修之前一样，而不是把日志吞掉。
pub(crate) fn silence_default_stream(client: &TdClient) {
    let _ = client.execute(serde_json::json!({
        "@type": "setLogStream",
        "log_stream": { "@type": "logStreamEmpty" },
    }));
}

/// TDLib 的 C 回调。**不能 panic / unwind**：跨过 C 栈就是未定义行为。
unsafe extern "C" fn on_tdlib_log(verbosity: i32, message: *const c_char) {
    if message.is_null() {
        return;
    }
    // SAFETY: TDLib 保证 message 是 NUL 结尾的 UTF-8，且只在这次调用期间有效。
    let text = unsafe { CStr::from_ptr(message) }.to_string_lossy();
    match sink() {
        Some(sink) => {
            // TDLib 的详细日志偶尔带换行（栈回溯之类）：按行拆开、一行一个事件，
            // 日志页 / 文件里才不会出现「半行接时间戳」的脏行。
            for line in text.lines() {
                sink(verbosity, line);
            }
        }
        // 没有宿主接手：维持 TDLib 原本的 stderr 行为。
        None => eprintln!("[tdlib] {text}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

    use crate::ffi::TdLogMessageCallbackFn;

    static CAPTURED: StdMutex<Vec<(i32, String)>> = StdMutex::new(Vec::new());
    static INSTALLED_MAX: AtomicI32 = AtomicI32::new(-1);
    static INSTALLED_CALLBACK: AtomicBool = AtomicBool::new(false);

    fn capture(verbosity: i32, line: &str) {
        CAPTURED.lock().unwrap().push((verbosity, line.to_owned()));
    }

    unsafe extern "C" fn fake_set_log_callback(max: i32, callback: Option<TdLogMessageCallbackFn>) {
        INSTALLED_MAX.store(max, Ordering::SeqCst);
        INSTALLED_CALLBACK.store(callback.is_some(), Ordering::SeqCst);
    }

    unsafe extern "C" fn noop_create() -> *mut std::ffi::c_void {
        std::ptr::null_mut()
    }
    unsafe extern "C" fn noop_one_ptr(_: *mut std::ffi::c_void) {}
    unsafe extern "C" fn noop_send(_: *mut std::ffi::c_void, _: *const c_char) {}
    unsafe extern "C" fn noop_receive(_: *mut std::ffi::c_void, _: f64) -> *const c_char {
        std::ptr::null()
    }
    unsafe extern "C" fn noop_execute(_: *mut std::ffi::c_void, _: *const c_char) -> *const c_char {
        std::ptr::null()
    }
    unsafe extern "C" fn noop_verbosity(_: i32) {}

    fn api(set_log_message_callback: Option<crate::ffi::TdSetLogMessageCallbackFn>) -> TdApi {
        TdApi {
            create: noop_create,
            destroy: noop_one_ptr,
            send: noop_send,
            receive: noop_receive,
            execute: noop_execute,
            set_log_verbosity: noop_verbosity,
            set_log_message_callback,
        }
    }

    #[test]
    fn missing_symbol_means_no_callback() {
        assert!(!install(&api(None), 3));
    }

    #[test]
    fn install_clamps_the_level_and_passes_our_callback() {
        assert!(install(&api(Some(fake_set_log_callback)), 3));
        assert_eq!(INSTALLED_MAX.load(Ordering::SeqCst), 3);
        assert!(INSTALLED_CALLBACK.load(Ordering::SeqCst));
        // 负数钳到 0：TDLib 的 0 是「只报致命错误」，不是「什么都不报」。
        assert!(install(&api(Some(fake_set_log_callback)), -2));
        assert_eq!(INSTALLED_MAX.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn callback_splits_lines_and_forwards_to_the_sink() {
        set_log_sink(capture);
        let message = CString::new("boom\nsecond line").unwrap();
        // SAFETY: `message` 在调用期间存活，且是 NUL 结尾。
        unsafe { on_tdlib_log(2, message.as_ptr()) };
        let captured = CAPTURED.lock().unwrap();
        assert_eq!(
            *captured,
            vec![(2, "boom".to_owned()), (2, "second line".to_owned())]
        );
    }
}
