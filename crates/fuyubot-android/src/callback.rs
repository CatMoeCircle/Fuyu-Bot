//! 回调 Java 侧要用到的全局状态：`JavaVM`、`Callback` 的 `GlobalRef`、提问的等待位。
//!
//! 三条纪律（照 JNI 的规矩来）：
//! 1. `JNIEnv*` **只在当前线程有效**，所以工作线程每次回调都要先 attach（拿 `JNIEnv`），
//!    绝不把 `JNIEnv` 存进 static。
//! 2. Java 传进来的对象出了调用就失效，必须 `NewGlobalRef` 存一份。
//! 3. 回调完要看 `ExceptionCheck`：带着异常再调 Java 是未定义行为。

use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use jni::objects::{GlobalRef, JObject, JValue};
use jni::{JNIEnv, JavaVM};

/// 单次提问的等待上限。真正的兜底是 `log_in_with` 的 600s 总预算。
pub const ASK_BUDGET: Duration = Duration::from_secs(300);

static VM: OnceLock<JavaVM> = OnceLock::new();
static CALLBACK: Mutex<Option<GlobalRef>> = Mutex::new(None);

/// 一次提问的等待位。`waiting` 从「有」变成 `None` 就是「答案到了 / 取消了」。
#[derive(Debug, Default)]
struct Prompt {
    next_id: i32,
    waiting: Option<i32>,
    answer: Option<String>,
}

static PROMPT: Mutex<Prompt> = Mutex::new(Prompt {
    next_id: 0,
    waiting: None,
    answer: None,
});
static ANSWERED: Condvar = Condvar::new();

/// `JNI_OnLoad` 里调一次。
pub fn install_vm(vm: JavaVM) {
    let _ = VM.set(vm);
}

/// 贴一份 `GlobalRef`。重复注册 = 换掉旧的（不会泄漏：旧 `GlobalRef` 的 `Drop` 会 Delete）。
pub fn register(env: &mut JNIEnv<'_>, callback: &JObject<'_>) -> Result<(), String> {
    let global = env
        .new_global_ref(callback)
        .map_err(|error| format!("new_global_ref 失败：{error}"))?;
    *CALLBACK.lock().unwrap_or_else(|e| e.into_inner()) = Some(global);
    Ok(())
}

fn callback() -> Option<GlobalRef> {
    CALLBACK.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// 拿到当前线程的 `JNIEnv`：已 attach 的线程直接 `GetEnv`，否则永久 attach
/// （回调很频繁，attach/detach 的抖动不值得；工作线程本来就活到收摊）。
fn with_env<R>(f: impl for<'env> FnOnce(&mut JNIEnv<'env>) -> R) -> Option<R> {
    let vm = VM.get()?;
    let mut env = vm
        .get_env()
        .or_else(|_| vm.attach_current_thread_permanently())
        .ok()?;
    let result = f(&mut env);
    if env.exception_check().unwrap_or(false) {
        // 不带着异常继续调 Java：清掉，把这次回调当失败。
        let _ = env.exception_clear();
        tracing::warn!(target: "fuyubot::jni", "Java 回调抛了异常（已清除）");
        return None;
    }
    Some(result)
}

/// `Callback.onState(int, String)`
pub fn state(state: i32, detail: &str) {
    let _ = with_env(|env| {
        let Some(callback) = callback() else {
            return;
        };
        let Ok(text) = env.new_string(detail) else {
            return;
        };
        let text: &JObject = text.as_ref();
        let _ = env.call_method(
            callback.as_obj(),
            "onState",
            "(ILjava/lang/String;)V",
            &[JValue::Int(state), JValue::Object(text)],
        );
    });
}

/// `Callback.onQr(String)`——只展示，不需要回答。
pub fn qr(link: &str) {
    let _ = with_env(|env| {
        let Some(callback) = callback() else {
            return;
        };
        let Ok(text) = env.new_string(link) else {
            return;
        };
        let text: &JObject = text.as_ref();
        let _ = env.call_method(
            callback.as_obj(),
            "onQr",
            "(Ljava/lang/String;)V",
            &[JValue::Object(text)],
        );
    });
}

/// 工作线程退出前调一次。
///
/// `attach_current_thread_permanently` 不会自己摘；ART 对「attach 了就直接退出」的线程
/// 会打警告（那点 TLS 也一直挂着）。收摊之后调它最干净。
pub fn detach_current_thread() {
    if let Some(vm) = VM.get() {
        // SAFETY: 只在**本线程**上摘自己（`jni` 把它标成 unsafe 是为了提醒这一点）；
        // 没 attach 过的话它是 no-op。
        unsafe {
            let _ = vm.detach_current_thread();
        }
    }
}

/// `Callback.onAsk(int, int, String)`，然后阻塞等 `answer(id, …)`。
///
/// 没有注册回调（纯 C 宿主）→ 直接 `None`：登录只可能走 bot token 那条。
pub fn ask(kind: i32, prompt: &str) -> Option<String> {
    if callback().is_none() {
        return None;
    }

    let id = {
        let mut slot = PROMPT.lock().unwrap_or_else(|e| e.into_inner());
        slot.next_id = slot.next_id.wrapping_add(1).max(1);
        slot.waiting = Some(slot.next_id);
        slot.answer = None;
        slot.next_id
    };

    let delivered = with_env(|env| {
        let Some(callback) = callback() else {
            return false;
        };
        let Ok(text) = env.new_string(prompt) else {
            return false;
        };
        let text: &JObject = text.as_ref();
        env.call_method(
            callback.as_obj(),
            "onAsk",
            "(IILjava/lang/String;)V",
            &[JValue::Int(id), JValue::Int(kind), JValue::Object(text)],
        )
        .is_ok()
    })
    .unwrap_or(false);

    if !delivered {
        cancel_waiting(id);
        return None;
    }
    wait_answer(id, ASK_BUDGET)
}

/// `Native.nativeAnswer(id, text)` 落到这里。返回「有没有这个 id 在等」。
pub fn answer(id: i32, text: Option<String>) -> bool {
    let mut slot = PROMPT.lock().unwrap_or_else(|e| e.into_inner());
    if slot.waiting != Some(id) {
        return false;
    }
    slot.waiting = None;
    slot.answer = text;
    ANSWERED.notify_all();
    true
}

fn cancel_waiting(id: i32) {
    let mut slot = PROMPT.lock().unwrap_or_else(|e| e.into_inner());
    if slot.waiting == Some(id) {
        slot.waiting = None;
        slot.answer = None;
    }
}

fn wait_answer(id: i32, budget: Duration) -> Option<String> {
    let deadline = Instant::now() + budget;
    let mut slot = PROMPT.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        // 已经被回答（或取消）：`waiting` 不再是这个 id。
        if slot.waiting != Some(id) {
            return slot.answer.take();
        }
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        let remaining = deadline - now;
        let (next, _) = ANSWERED
            .wait_timeout(slot, remaining)
            .unwrap_or_else(|e| e.into_inner());
        slot = next;
    }
    // 超时：把等待位收掉，免得之后 `answer(id)` 打进一个没人管的位置。
    slot.waiting = None;
    slot.answer = None;
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `PROMPT` 是进程级全局：三个用例都要独占它，串行跑。
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    /// 没有 VM / 没注册回调时，`ask` 必须**立刻**返回 `None`（不能傻等 5 分钟）。
    #[test]
    fn ask_without_a_callback_returns_immediately() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let started = Instant::now();
        assert!(ask(0, "Bot Token").is_none());
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    /// 等待位被 `answer` 填上之后，`wait_answer` 能拿到字符串。
    #[test]
    fn answer_unblocks_the_waiter() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let id = {
            let mut slot = PROMPT.lock().unwrap();
            slot.next_id += 1;
            slot.waiting = Some(slot.next_id);
            slot.answer = None;
            slot.next_id
        };

        let waiter = std::thread::spawn(move || wait_answer(id, Duration::from_secs(5)));
        std::thread::sleep(Duration::from_millis(50));
        assert!(answer(id, Some("123456".to_owned())));
        assert_eq!(waiter.join().unwrap().as_deref(), Some("123456"));
    }

    /// 回答一个没人等的 id 必须报「没对上」，而不是污染下一次提问。
    #[test]
    fn answer_for_an_unknown_id_is_rejected() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        assert!(!answer(9999, Some("boom".to_owned())));
    }

    /// 取消（`text = None`）也要把人叫醒，且结果是 `None`。
    #[test]
    fn cancelling_wakes_the_waiter_with_none() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let id = {
            let mut slot = PROMPT.lock().unwrap();
            slot.next_id += 1;
            slot.waiting = Some(slot.next_id);
            slot.answer = None;
            slot.next_id
        };

        let waiter = std::thread::spawn(move || wait_answer(id, Duration::from_secs(5)));
        std::thread::sleep(Duration::from_millis(50));
        assert!(answer(id, None));
        assert_eq!(waiter.join().unwrap(), None);
    }
}
