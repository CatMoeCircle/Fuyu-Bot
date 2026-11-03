//! 进程内状态：运行位、停止位、对外状态机、返回码、线程局部错误串。
//!
//! 这一层**不认识 Android**：它只被 `lib.rs` 的导出符号与 `boot.rs` 的编排读改写，
//! 真正的「JS 侧可见」由 [`crate::callback`] 投出去。

use std::cell::RefCell;
use std::ffi::{CString, c_char};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

/// 返回码。数值与 `include/fuyubot_jni.h` 一字不差（改这里必须同步改头文件）。
pub const OK: i32 = 0;
/// 参数错：JSON 非法、缺字段、root 不是绝对路径。
pub const ARG: i32 = 1;
/// 已经在跑。
pub const BUSY: i32 = 2;
/// `stop` 等收摊超时。
pub const TIMEOUT: i32 = 3;
/// 本库内部 panic（已兜住，进程还在）。
pub const PANIC: i32 = 4;
/// 内部错：TDLib 未就绪、登录失败…细节看 [`last_error_ptr`]。
pub const INTERNAL: i32 = 5;

/// `onState(state)` 的取值。
///
/// **不在这里另立一份**：直接用编排层的 [`fuyubot_app::AppState`]，它的数值就是
/// `onState(int)` 的契约（0 STARTING / 1 READY / 2 STOPPING / 3 STOPPED / 4 FAILED≈ERROR）。
pub use fuyubot_app::AppState as State;

static RUNNING: AtomicBool = AtomicBool::new(false);
static STOP: AtomicBool = AtomicBool::new(false);
static WORKER_THREAD: Mutex<Option<std::thread::ThreadId>> = Mutex::new(None);

/// 记下工作线程（跑登录 / 接收循环 / 状态回调的那条）。线程收尾时清掉。
///
/// 用途只有一个：`td_request` 在工作线程上被同步调用会**自己等自己**
/// （Java 在 `onAsk` / `onState` 回调里顺手发请求），要能立刻报错而不是等 30 秒。
pub fn set_worker_thread(id: Option<std::thread::ThreadId>) {
    *WORKER_THREAD.lock().unwrap_or_else(|e| e.into_inner()) = id;
}

/// 当前线程是不是工作线程。
pub fn on_worker_thread() -> bool {
    let current = std::thread::current().id();
    WORKER_THREAD
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        == Some(&current)
}

/// 线程活着且还没收摊。
pub fn running() -> bool {
    RUNNING.load(Ordering::SeqCst)
}

pub fn set_running(running: bool) {
    RUNNING.store(running, Ordering::SeqCst);
}

/// `stop()` 请求过没有。`boot.rs` 每 500ms 看一次。
pub fn stop_requested() -> bool {
    STOP.load(Ordering::SeqCst)
}

pub fn request_stop() {
    STOP.store(true, Ordering::SeqCst);
}

/// 下一次 `start` 之前必须清掉（`stop` 是粘性的，不然第二次 start 一起来就收摊）。
pub fn reset_stop() {
    STOP.store(false, Ordering::SeqCst);
}

/// 收摊走到了哪一步（`App::shutdown` 里每一段上报一次）。
///
/// 为什么要有它：`fuyubot_stop()` 等 worker 的上限是 8s，超时只能返回 `TIMEOUT`；
/// 那时 worker 还卡在**某一步**里，日志还没落盘。把这一步记成**进程级**（不是线程局部）
/// 的字符串，`stop_runtime` 超时后就能在 `last_error()` 里写清楚「卡在哪」——
/// App 弹窗一显示，就不用猜是「还在停接收循环」还是「还在刷盘」。
static STOP_PHASE: Mutex<String> = Mutex::new(String::new());

pub fn set_stop_phase(detail: &str) {
    *STOP_PHASE.lock().unwrap_or_else(|e| e.into_inner()) = detail.to_owned();
}

/// 当前收摊阶段；空串 = 还没上报（启动中 / 已经收完）。
pub fn stop_phase() -> String {
    STOP_PHASE.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

pub fn clear_stop_phase() {
    STOP_PHASE.lock().unwrap_or_else(|e| e.into_inner()).clear();
}

/// 记一条状态：本地日志 + 投给 Java 的 `onState`。
pub fn emit(state: State, detail: &str) {
    if state == State::Failed {
        tracing::error!(target: "fuyubot::jni", "{detail}");
    } else {
        tracing::info!(target: "fuyubot::jni", "{detail}");
    }
    crate::callback::state(state.as_i32(), detail);
}

/// 失败：错误串落进线程局部缓冲（供 `last_error()` 读），再投一次 `FAILED`。
pub fn fail(detail: &str) {
    set_last_error(detail);
    emit(State::Failed, detail);
}

thread_local! {
    /// 本线程最后一次错误。`CString` 而不是 `String`：`last_error()` 要直接交指针。
    static LAST_ERROR: RefCell<CString> = RefCell::new(CString::new("").unwrap_or_default());
}

/// 覆盖本线程的错误串。内部 `\0` 会被换成空格——`CString` 装不下它。
pub fn set_last_error(message: &str) {
    let cleaned = message.replace('\0', " ");
    LAST_ERROR.with(|slot| {
        if let Ok(text) = CString::new(cleaned) {
            *slot.borrow_mut() = text;
        }
    });
}

/// 本线程错误串的裸指针。
///
/// 有效性：**下次在同一线程调用本库之前**（届时会被覆盖，旧缓冲随之释放）。
/// 与插件 ABI 的「谁分配谁释放」同一条纪律——调用方别 free、别存。
pub fn last_error_ptr() -> *const c_char {
    LAST_ERROR.with(|slot| slot.borrow().as_ptr())
}

/// 本线程错误串的拷贝（JNI 侧要把它变成 Java `String`，不能交裸指针）。
pub fn last_error_string() -> String {
    LAST_ERROR.with(|slot| slot.borrow().to_string_lossy().into_owned())
}

thread_local! {
    /// 本线程最后一次**返回给宿主**的 JSON（`fuyubot_plugins` / `_plugin_info` /
    /// `_td_request` 的结果）。
    ///
    /// 与 [`LAST_ERROR`] 同一条纪律：下次在**同一线程**调本库前有效。
    /// 用一个独立缓冲而不是复用错误串：调用方完全可能先读结果、再读 `last_error`。
    static OUTPUT: RefCell<CString> = RefCell::new(CString::new("").unwrap_or_default());
}

/// 把一段文本放进线程局部输出缓冲，返回裸指针。
///
/// 内部 `\0` 换成空格（`CString` 装不下它）；返回的指针**只在下次同线程调用前有效**。
pub fn store_output(text: &str) -> *const c_char {
    let cleaned = text.replace('\0', " ");
    OUTPUT.with(|slot| {
        if let Ok(value) = CString::new(cleaned) {
            *slot.borrow_mut() = value;
        }
        slot.borrow().as_ptr()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 收摊阶段是**进程级**的（不是线程局部）：worker 写、Java 调用线程读，
    /// 超时消息才能拼出「卡在哪一步」。
    #[test]
    fn stop_phase_round_trips_across_threads() {
        clear_stop_phase();
        assert_eq!(stop_phase(), "");

        let writer = std::thread::spawn(|| set_stop_phase("TDLib 关会话（等数据库刷盘）"));
        writer.join().unwrap();

        assert_eq!(stop_phase(), "TDLib 关会话（等数据库刷盘）");
        clear_stop_phase();
        assert_eq!(stop_phase(), "");
    }
}
