//! `libfuyubotjni.so`：把整个 fuyubot 宿主编成 Android 的 JNI 运行时。
//!
//! **只交付 `.so`**：不出 APK、不出 UI、不出 Java 代码。APK 作者 `System.loadLibrary("fuyubotjni")`
//! 之后调这里导出的 native 方法；保活（前台 Service / 通知 / WakeLock）是他那一侧的事。
//!
//! 两套入口、一份实现：
//! - JNI：`JNI_OnLoad` 里 `RegisterNatives` 绑到 [`NATIVE_CLASS`]
//!   （默认 `com/catmoecircle/fuyubot/runtime/Native`，构建期可用 `FUYUBOT_JNI_CLASS` 覆盖）。
//!   **不再用 `Java_...` 名字绑定**：包名是常量，不是符号名的一部分。
//! - C：`fuyubot_*`（逃生门：不想用 Java 就自己写 20 行 JNI / `dlsym`）
//!
//! 宿主契约以 `include/fuyubot_jni.h` 为准：导出符号、线程约定、options JSON 与方法表
//! 都写在那里（它同时是 cbindgen 之外唯一手写的对外头）。
//!
//! ## 这里的纪律
//!
//! 1. **panic 不许过 FFI 边界**：每个导出函数体都 `catch_unwind`，返回 [`state::PANIC`]。
//! 2. `start` **立即返回**：真正干活在 worker 线程里（`boot::run` 是阻塞的）。
//! 3. `stop` 是**唯一的停车方式**：写停止位 + 等线程收摊（≤ 8s），不粗暴打断。
//! 4. `last_error` 是**线程局部**的：下周同线程再调本库前有效（与插件 ABI 同一条纪律）。

mod api;
mod callback;
mod host;
mod live;
mod log;
mod options;
mod run;
mod state;

use std::ffi::{CStr, c_char, c_int, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Mutex;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use jni::JNIEnv;
use jni::JavaVM;
use jni::objects::{JObject, JString};
use jni::sys::{JNI_VERSION_1_6, jboolean, jint, jlong, jstring};

use options::Options;
use state::{ARG, BUSY, INTERNAL, OK, PANIC, TIMEOUT};

/// 运行时 ABI 版本。头文件里的 `FUYUBOT_JNI_ABI` 必须与它相同。
///
/// **v1**：六个生命周期/问答方法 + 插件列表、插件详情、两个 TDLib 状态查询
/// （授权 / 连接）、TDLib JSON 请求、本线程最后一次错误，共十二个。
///
/// 这个 crate 从来没有发布过（既没进过 Release 资产，也没在 crates.io 上），
/// 所以编号一直从 1 起：**v1 就是首个对外版本**，包含下面全部 12 个方法；
/// 新增的两个状态查询属于 v1，不单独占一个版本号。
///
/// 开发期短暂存在过的内存接口（`nativeMemory` / `nativeMemoryOf`）**没进 v1**：
/// 内存采集不归框架，宿主（APK）自己用 `Debug.MemoryInfo` 或读
/// `/proc/<pid>/smaps`。以后真发过版再加方法，才按「只加不改」升 v2。
const ABI_VERSION: i32 = 1;

/// JNI 绑定类的**内部名**（`/` 分隔，不是点分名）。
///
/// 默认 `com/catmoecircle/fuyubot/runtime/Native`。构建时用
/// `FUYUBOT_JNI_CLASS=com/example/runtime/Native` 覆盖即可换包名（**必须 `/` 分隔**，
/// 写成点分名 `FindClass` 会找不到类）。它只是 `FindClass` 的入参，所以换包名
/// 不需要改任何函数名、也不会改变导出符号。
pub const NATIVE_CLASS: &str = match option_env!("FUYUBOT_JNI_CLASS") {
    Some(name) => name,
    None => "com/catmoecircle/fuyubot/runtime/Native",
};

/// `stop` 等 worker 收摊的上限：与 `fuyubot stop` 的 8 秒兜底同量级。
const STOP_BUDGET: Duration = Duration::from_secs(8);

/// 一次 `start` 起的那个工作线程。
///
/// 轮次号不放在这里：收尾要比对的 `generation` 由线程闭包自己捕获，跟
/// [`Lifecycle::generation`] 比——上一轮的收尾只有号对得上才清 `RUNNING`。
struct Worker {
    handle: JoinHandle<()>,
}

/// 进程级的生命周期闸门：谁在跑、谁在停。
///
/// 为什么不能只有运行位 + 一个 `Option<JoinHandle>`：`stop` 会把句柄**取出去**再 join，
/// 那段时间 `Option` 是空的而 `RUNNING` 还是真的——并发的 `start` 于是看到
/// 「运行位在、句柄没了」，判定成「上一轮已经自己收尾」再起一个 worker。
/// 两个 worker = 两个 tokio runtime + 两个 TDLib 客户端抢同一个数据库目录。
/// 所以判状态、起线程、取句柄都在同一把锁里，另有 `stopping` 把「正在 join」标出来。
struct Lifecycle {
    worker: Option<Worker>,
    /// 已经有线程拿着句柄在等它结束。这期间不许 `start`，也不许第二个 `stop` 同时 join。
    stopping: bool,
    /// 轮次号，只增。
    generation: u64,
}

static LIFECYCLE: Mutex<Lifecycle> = Mutex::new(Lifecycle {
    worker: None,
    stopping: false,
    generation: 0,
});

/// 工作线程退出时的收尾：只清**本轮**的运行位。
fn finish(generation: u64) {
    let life = LIFECYCLE.lock().unwrap_or_else(|e| e.into_inner());
    if life.generation == generation {
        state::set_running(false);
    }
}

/// 句柄槽的三种样子（`start` 的准入只关心这个）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    /// 从来没起过，或者上一轮的句柄已经回收。
    Empty,
    /// 上一轮的线程已经结束（句柄还在，join 一下就能回收）。
    Finished,
    /// 线程还活着。
    Alive,
}

/// `start` 的准入结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Admission {
    /// 可以起。
    Start,
    /// 已经在跑，或者另一个线程正在 stop。→ [`BUSY`]
    Busy,
    /// 上一个线程还活着，但运行位是灭的（状态不一致）。→ [`INTERNAL`]
    Inconsistent,
}

/// `start` 失败的原因（决定了返回码与 `last_error` 的措辞）。
enum StartFailure {
    Busy,
    Inconsistent,
    Spawn(std::io::Error),
}

/// `start` 现在能不能起。**纯函数**：只看状态，不动生命周期（所以好测）。
fn admit_start(running: bool, slot: Slot, stopping: bool) -> Admission {
    // 先看 stopping：那时 `RUNNING` 可能已经是灭的（上一轮 worker 退了），
    // 但这一轮还没 join 干净，起第二个就会和它抢 TDLib 的数据库。
    if stopping {
        return Admission::Busy;
    }
    if running {
        return Admission::Busy;
    }
    match slot {
        Slot::Alive => Admission::Inconsistent,
        Slot::Empty | Slot::Finished => Admission::Start,
    }
}

/// `stop` 现在能不能取句柄。同样是纯函数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopGate {
    /// 可以停（句柄在）。
    Take,
    /// 没在跑：幂等返回 OK。
    Idle,
    /// 另一个线程正在 stop：别并发 join，回 [`BUSY`]。
    AlreadyStopping,
}

fn admit_stop(running: bool, slot: Slot, stopping: bool) -> StopGate {
    if stopping {
        return StopGate::AlreadyStopping;
    }
    if !running {
        return StopGate::Idle;
    }
    match slot {
        Slot::Alive | Slot::Finished => StopGate::Take,
        // 运行位在、句柄没了：线程正在退出（`finish` 会清位），别再动它。
        Slot::Empty => StopGate::Idle,
    }
}

// ── C 符号（逃生门）────────────────────────────────────────────────

/// 运行时 ABI 版本。
#[unsafe(no_mangle)]
pub extern "C" fn fuyubot_abi_version() -> c_int {
    ABI_VERSION
}

/// 起运行时。立即返回；成功 = [`OK`]（**不代表已登录**，看日志与 `onState`）。
///
/// # Safety
/// `options_json` 必须是 NUL 结尾的 UTF-8 C 字符串（可以是空指针 → 返回 [`ARG`]）。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fuyubot_start(options_json: *const c_char) -> c_int {
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        if options_json.is_null() {
            state::set_last_error("options_json 是空指针");
            return ARG;
        }
        // SAFETY: 调用方保证指针是 NUL 结尾的 C 字符串；只在本次调用里用。
        let text = unsafe { CStr::from_ptr(options_json) };
        let Ok(json) = text.to_str() else {
            state::set_last_error("options_json 不是合法 UTF-8");
            return ARG;
        };
        start_with_json(json)
    }));
    match outcome {
        Ok(code) => code,
        Err(_) => {
            state::fail("fuyubot_start 里 panic（已兜住）");
            PANIC
        }
    }
}

/// 优雅停车，最多等 8 秒。没在跑 = 直接 [`OK`]（幂等）。
#[unsafe(no_mangle)]
pub extern "C" fn fuyubot_stop() -> c_int {
    match catch_unwind(AssertUnwindSafe(stop_runtime)) {
        Ok(code) => code,
        Err(_) => {
            state::fail("fuyubot_stop 里 panic（已兜住）");
            PANIC
        }
    }
}

/// 线程起来了且还没收摊 = 1。
#[unsafe(no_mangle)]
pub extern "C" fn fuyubot_is_running() -> c_int {
    c_int::from(state::running())
}

/// 回答一次提问。没有这个 id 在等 → [`ARG`]。
///
/// # Safety
/// `text` 必须是 NUL 结尾的 UTF-8 C 字符串，或空指针（= 取消）。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fuyubot_answer(id: c_int, text: *const c_char) -> c_int {
    let answer = if text.is_null() {
        None
    } else {
        // SAFETY: 调用方保证 NUL 结尾；拷贝出来再返回，不留下借用。
        match unsafe { CStr::from_ptr(text) }.to_str() {
            Ok(value) => Some(value.to_owned()),
            Err(_) => {
                state::set_last_error("fuyubot_answer 的 text 不是合法 UTF-8");
                return ARG;
            }
        }
    };
    if callback::answer(id, answer) {
        OK
    } else {
        ARG
    }
}

/// 本线程最后一次错误的说明；没有错就是空串。
///
/// 有效性：**下次在同一线程调用本库之前**（与插件 ABI 的线程局部缓冲同一条纪律）。
#[unsafe(no_mangle)]
pub extern "C" fn fuyubot_last_error() -> *const c_char {
    state::last_error_ptr()
}

// ── 运行时查询 ────────────────────────────────────────────────────

/// 当前插件列表（JSON：清单 + 计数 + 跳过原因）。没在跑 = 空表（同样的形状）。
///
/// ```json
/// {"count":2,"scanned":3,"active":1,"pending":0,"activating":0,"failed":1,"unknown":0,
///  "skipped":1,"plugins":[…],"skipped_plugins":[{"path":"…","reason":"…"}]}
/// ```
///
/// 从「插件装好」那一刻起可用（**登录之前**）；每项的字段见 `include/fuyubot_jni.h`。
/// 返回的是**线程局部缓冲**：下次在**同一线程**调本库前有效——别 free、别存、别跨线程传。
#[unsafe(no_mangle)]
pub extern "C" fn fuyubot_plugins() -> *const c_char {
    match catch_unwind(AssertUnwindSafe(api::plugins_json)) {
        Ok(json) => state::store_output(&json),
        Err(_) => {
            state::fail("fuyubot_plugins 里 panic（已兜住）");
            std::ptr::null()
        }
    }
}

/// 单个插件的 JSON；没这个插件 / 没在跑 = `NULL`（**不设** `last_error`：找不到不是错）。
///
/// 返回值同样是线程局部缓冲。
///
/// # Safety
/// `name` 必须是 NUL 结尾的 UTF-8 C 字符串，或空指针（→ `NULL`）。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fuyubot_plugin_info(name: *const c_char) -> *const c_char {
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        if name.is_null() {
            return None;
        }
        // SAFETY: 调用方保证 NUL 结尾、且只在本次调用里用。
        let Ok(name) = (unsafe { CStr::from_ptr(name) }).to_str() else {
            state::set_last_error("fuyubot_plugin_info 的 name 不是合法 UTF-8");
            return None;
        };
        api::plugin_json(name)
    }));
    match outcome {
        Ok(Some(json)) => state::store_output(&json),
        Ok(None) => std::ptr::null(),
        Err(_) => {
            state::fail("fuyubot_plugin_info 里 panic（已兜住）");
            std::ptr::null()
        }
    }
}

/// TDLib **授权状态**（登录走到哪一步）的 JSON。
///
/// ```json
/// {"known":true,"state":"authorizationStateReady","ready":true,"needs_re_login":false}
/// ```
///
/// 运行时没起来 / 一次更新都还没收到 = 同一份形状的 `known:false`、`state:null`。
/// 返回线程局部缓冲：下次在**同一线程**调本库前有效——别 free、别存、别跨线程传。
#[unsafe(no_mangle)]
pub extern "C" fn fuyubot_auth_state() -> *const c_char {
    match catch_unwind(AssertUnwindSafe(api::auth_state_json)) {
        Ok(json) => state::store_output(&json),
        Err(_) => {
            state::fail("fuyubot_auth_state 里 panic（已兜住）");
            std::ptr::null()
        }
    }
}

/// TDLib **连接状态**（到 Telegram 的链路）的 JSON。
///
/// ```json
/// {"known":true,"state":"connectionStateReady","ready":true}
/// ```
///
/// 五种状态：`connectionStateWaitingForNetwork` / `connectionStateConnectingToProxy` /
/// `connectionStateConnecting` / `connectionStateUpdating` / `connectionStateReady`。
/// 运行时没起来 / 一次更新都还没收到 = 同一份形状的 `known:false`、`state:null`。
/// 返回线程局部缓冲，纪律同上。
#[unsafe(no_mangle)]
pub extern "C" fn fuyubot_connection_state() -> *const c_char {
    match catch_unwind(AssertUnwindSafe(api::connection_state_json)) {
        Ok(json) => state::store_output(&json),
        Err(_) => {
            state::fail("fuyubot_connection_state 里 panic（已兜住）");
            std::ptr::null()
        }
    }
}

/// 发一条原始 td_api JSON 并等它自己的响应，返回响应 JSON。
///
/// `timeout_ms <= 0` = 默认 30s，上限 600s。**阻塞**：在专门的工作线程上调。
/// 失败（没在跑 / JSON 非法 / 超时 / TDLib 报错到发不出去）返回 `NULL`，
/// 细节看 [`fuyubot_last_error`]；TDLib 的业务错误是**正常的响应 JSON**
/// （`{"@type":"error",…}`），不是 `NULL`。
///
/// # Safety
/// `request_json` 必须是 NUL 结尾的 UTF-8 C 字符串，或空指针（→ `NULL`）。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fuyubot_td_request(
    request_json: *const c_char,
    timeout_ms: c_int,
) -> *const c_char {
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        if request_json.is_null() {
            state::set_last_error("request_json 是空指针");
            return None;
        }
        // SAFETY: 调用方保证 NUL 结尾；拷贝成 &str 只在本次调用里用。
        let Ok(text) = (unsafe { CStr::from_ptr(request_json) }).to_str() else {
            state::set_last_error("request_json 不是合法 UTF-8");
            return None;
        };
        match api::td_request(text, i64::from(timeout_ms)) {
            Ok(json) => Some(json),
            Err(error) => {
                state::set_last_error(&error);
                None
            }
        }
    }));
    match outcome {
        Ok(Some(json)) => state::store_output(&json),
        Ok(None) => std::ptr::null(),
        Err(_) => {
            state::fail("fuyubot_td_request 里 panic（已兜住）");
            std::ptr::null()
        }
    }
}

// ── JNI 绑定（RegisterNatives）──────────────────────────────────────

/// `JNI_OnLoad`：缓存 `JavaVM`，并把十二个 native 方法绑到 [`NATIVE_CLASS`]。
///
/// 绑定失败**不阻止加载**（照样返回 1.6）：第一次调 native 方法时 Java 会抛
/// `UnsatisfiedLinkError`，消息里带类名与方法名——比 `JNI_OnLoad` 返回 `JNI_ERR`
/// （只报 “bad version”）好排查；纯 C 宿主也不会被牵连。
#[unsafe(no_mangle)]
pub extern "system" fn JNI_OnLoad(vm: JavaVM, _reserved: *mut c_void) -> jint {
    // 先注册（要借 `&vm`），再把 `vm` 交给回调模块——`JavaVM` 没有 `Clone`。
    let registration = register_natives(&vm);
    callback::install_vm(vm);
    if let Err(error) = registration {
        tracing::error!(target: "fuyubot::jni", "JNI 绑定失败：{error}");
    }
    JNI_VERSION_1_6
}

/// 十二个 native 方法的「名字 + 签名 + 函数指针」。
///
/// `nativeRegister` 的参数是 `Ljava/lang/Object;`：回调只按**方法名 + 签名**调
/// （`onState` / `onAsk` / `onQr` / `onPlugins`），不认具体接口——所以 `Callback` 可以放在 APK
/// 自己的包里，Kotlin 侧把参数声明成 `Any` 即可（契约见头文件）。
fn jni_methods() -> [jni::NativeMethod; 12] {
    [
        jni::NativeMethod {
            name: "nativeVersion".into(),
            sig: "()I".into(),
            fn_ptr: native_version as *const () as *mut c_void,
        },
        jni::NativeMethod {
            name: "nativeRegister".into(),
            sig: "(Ljava/lang/Object;)I".into(),
            fn_ptr: native_register as *const () as *mut c_void,
        },
        jni::NativeMethod {
            name: "nativeStart".into(),
            sig: "(Ljava/lang/String;)I".into(),
            fn_ptr: native_start as *const () as *mut c_void,
        },
        jni::NativeMethod {
            name: "nativeStop".into(),
            sig: "()I".into(),
            fn_ptr: native_stop as *const () as *mut c_void,
        },
        jni::NativeMethod {
            name: "nativeIsRunning".into(),
            sig: "()Z".into(),
            fn_ptr: native_is_running as *const () as *mut c_void,
        },
        jni::NativeMethod {
            name: "nativeAnswer".into(),
            sig: "(ILjava/lang/String;)I".into(),
            fn_ptr: native_answer as *const () as *mut c_void,
        },
        jni::NativeMethod {
            name: "nativePlugins".into(),
            sig: "()Ljava/lang/String;".into(),
            fn_ptr: native_plugins as *const () as *mut c_void,
        },
        jni::NativeMethod {
            name: "nativePluginInfo".into(),
            sig: "(Ljava/lang/String;)Ljava/lang/String;".into(),
            fn_ptr: native_plugin_info as *const () as *mut c_void,
        },
        jni::NativeMethod {
            name: "nativeAuthState".into(),
            sig: "()Ljava/lang/String;".into(),
            fn_ptr: native_auth_state as *const () as *mut c_void,
        },
        jni::NativeMethod {
            name: "nativeConnectionState".into(),
            sig: "()Ljava/lang/String;".into(),
            fn_ptr: native_connection_state as *const () as *mut c_void,
        },
        jni::NativeMethod {
            name: "nativeTdRequest".into(),
            sig: "(Ljava/lang/String;J)Ljava/lang/String;".into(),
            fn_ptr: native_td_request as *const () as *mut c_void,
        },
        jni::NativeMethod {
            name: "nativeLastError".into(),
            sig: "()Ljava/lang/String;".into(),
            fn_ptr: native_last_error as *const () as *mut c_void,
        },
    ]
}

/// 逐个注册（**不是**整表注册）。
///
/// `RegisterNatives` 遇到类里没有的方法（名字 / 签名不符）会失败；整表注册时，
/// **新版 `.so` + 旧版 Java 类**（比如类里没有 `nativeTdRequest` 声明）
/// 会连 `nativeLastError` 都绑不上。逐个注册把影响收窄到「少的那几个方法」，
/// 其余照常可用；没绑上的会在 `JNI_OnLoad` 的日志里列出来。
fn register_natives(vm: &JavaVM) -> Result<(), String> {
    let mut env = vm
        .get_env()
        .map_err(|error| format!("当前线程拿不到 JNIEnv：{error}"))?;
    let mut missing = Vec::new();
    for method in jni_methods() {
        let name = method.name.to_string_lossy().into_owned();
        if env
            .register_native_methods(NATIVE_CLASS, &[method])
            .is_err()
        {
            // `FindClass` / `RegisterNatives` 失败会留下 pending 异常；不清掉就会被带进
            // 下一次 JNI 调用（未定义行为）。清掉，把名字记下来。
            let _ = env.exception_clear();
            missing.push(name);
        }
    }
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "{NATIVE_CLASS} 没绑上的方法：{}（Java 类是不是旧版？）",
            missing.join("、")
        ))
    }
}

extern "system" fn native_version(_env: JNIEnv<'_>, _this: JObject<'_>) -> jint {
    ABI_VERSION
}

/// 贴住 Java 的回调对象。**可重复调**（换掉旧的）。
extern "system" fn native_register(
    mut env: JNIEnv<'_>,
    _this: JObject<'_>,
    callback: JObject<'_>,
) -> jint {
    match catch_unwind(AssertUnwindSafe(|| {
        match callback::register(&mut env, &callback) {
            Ok(()) => OK,
            Err(error) => {
                state::set_last_error(&error);
                ARG
            }
        }
    })) {
        Ok(code) => code,
        Err(_) => {
            state::fail("nativeRegister 里 panic（已兜住）");
            PANIC
        }
    }
}

extern "system" fn native_start(
    mut env: JNIEnv<'_>,
    _this: JObject<'_>,
    options: JString<'_>,
) -> jint {
    let outcome = catch_unwind(AssertUnwindSafe(|| match read_string(&mut env, options) {
        Ok(json) => start_with_json(&json),
        Err(error) => {
            state::set_last_error(&format!("options {error}"));
            ARG
        }
    }));
    match outcome {
        Ok(code) => code,
        Err(_) => {
            state::fail("nativeStart 里 panic（已兜住）");
            PANIC
        }
    }
}

extern "system" fn native_stop(_env: JNIEnv<'_>, _this: JObject<'_>) -> jint {
    match catch_unwind(AssertUnwindSafe(stop_runtime)) {
        Ok(code) => code,
        Err(_) => {
            state::fail("nativeStop 里 panic（已兜住）");
            PANIC
        }
    }
}

extern "system" fn native_is_running(_env: JNIEnv<'_>, _this: JObject<'_>) -> jboolean {
    jboolean::from(state::running())
}

/// `text = null` = 取消。
extern "system" fn native_answer(
    mut env: JNIEnv<'_>,
    _this: JObject<'_>,
    id: jint,
    text: JString<'_>,
) -> jint {
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        // 先拿裸指针判断 null：`JString` 没有 `is_null`，但 `into_raw` 有。
        let raw = text.into_raw();
        let answer = if raw.is_null() {
            None
        } else {
            // SAFETY: raw 是 JNI 刚传进来的局部引用，本次调用期间有效。
            let text = unsafe { JString::from_raw(raw) };
            env.get_string(&text)
                .ok()
                .map(|value| value.to_string_lossy().into_owned())
        };
        if callback::answer(id, answer) {
            OK
        } else {
            ARG
        }
    }));
    match outcome {
        Ok(code) => code,
        Err(_) => {
            state::fail("nativeAnswer 里 panic（已兜住）");
            PANIC
        }
    }
}

// ── 实现（JNI 与 C 两条入口共用）──────────────────────────────────

/// 把 Rust 字符串变成 Java `String`；失败（OOM 之类）返回 `null` 并记一条错。
///
/// `new_string` 失败会留下 pending 异常，必须清掉——带着异常继续用 `JNIEnv` 是
/// 未定义行为（与 `register_natives` 里那条同一个道理）。
fn new_java_string(env: &mut JNIEnv<'_>, text: &str) -> jstring {
    match env.new_string(text) {
        Ok(value) => value.into_raw(),
        Err(error) => {
            state::set_last_error(&format!("new_string 失败：{error}"));
            let _ = env.exception_clear();
            std::ptr::null_mut()
        }
    }
}

/// 把一个 Java `String` 参数读成 Rust 字符串。
///
/// **先判 null**：`JString` 可以是空引用（Kotlin 侧写了 `nativeTdRequest(null, …)`），
/// 把空引用交给 `GetStringUTFChars` 是未定义行为——CheckJNI 直接 abort，
/// 非 CheckJNI 上可能返回空指针，紧接着 `to_str()` 就是一次空指针解引用。
/// 所以这里一律转成可读的错误，让调用方回 [`ARG`]。
fn read_string(env: &mut JNIEnv<'_>, value: JString<'_>) -> Result<String, String> {
    let raw = value.into_raw();
    if raw.is_null() {
        return Err("是 null".to_owned());
    }
    // SAFETY: raw 是 JNI 刚交进来的局部引用，本次调用期间有效。
    let value = unsafe { JString::from_raw(raw) };
    env.get_string(&value)
        .map(|text| text.to_string_lossy().into_owned())
        .map_err(|error| format!("不是合法 UTF-8：{error}"))
}

/// 当前插件列表 JSON。运行时没起来 = 空表（与正常返回同一份形状，**不是** `null`）。
extern "system" fn native_plugins(mut env: JNIEnv<'_>, _this: JObject<'_>) -> jstring {
    match catch_unwind(AssertUnwindSafe(api::plugins_json)) {
        Ok(json) => new_java_string(&mut env, &json),
        Err(_) => {
            state::fail("nativePlugins 里 panic（已兜住）");
            std::ptr::null_mut()
        }
    }
}

/// 单个插件 JSON；没在跑或没这个插件 = `null`。
extern "system" fn native_plugin_info(
    mut env: JNIEnv<'_>,
    _this: JObject<'_>,
    name: JString<'_>,
) -> jstring {
    let outcome = catch_unwind(AssertUnwindSafe(|| match read_string(&mut env, name) {
        Ok(name) => api::plugin_json(&name),
        Err(error) => {
            state::set_last_error(&format!("name {error}"));
            None
        }
    }));
    match outcome {
        Ok(Some(json)) => new_java_string(&mut env, &json),
        Ok(None) => std::ptr::null_mut(),
        Err(_) => {
            state::fail("nativePluginInfo 里 panic（已兜住）");
            std::ptr::null_mut()
        }
    }
}

/// TDLib 授权状态 JSON；运行时没起来 = `known:false`（同形状，不是 `null`）。
extern "system" fn native_auth_state(mut env: JNIEnv<'_>, _this: JObject<'_>) -> jstring {
    match catch_unwind(AssertUnwindSafe(api::auth_state_json)) {
        Ok(json) => new_java_string(&mut env, &json),
        Err(_) => {
            state::fail("nativeAuthState 里 panic（已兜住）");
            std::ptr::null_mut()
        }
    }
}

/// TDLib 连接状态 JSON；运行时没起来 = `known:false`（同形状，不是 `null`）。
extern "system" fn native_connection_state(mut env: JNIEnv<'_>, _this: JObject<'_>) -> jstring {
    match catch_unwind(AssertUnwindSafe(api::connection_state_json)) {
        Ok(json) => new_java_string(&mut env, &json),
        Err(_) => {
            state::fail("nativeConnectionState 里 panic（已兜住）");
            std::ptr::null_mut()
        }
    }
}

/// 发一条 td_api JSON、等响应（**阻塞**：从工作线程调）。
extern "system" fn native_td_request(
    mut env: JNIEnv<'_>,
    _this: JObject<'_>,
    request: JString<'_>,
    timeout_ms: jlong,
) -> jstring {
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        let request = match read_string(&mut env, request) {
            Ok(request) => request,
            Err(error) => {
                state::set_last_error(&format!("request {error}"));
                return None;
            }
        };
        match api::td_request(&request, timeout_ms) {
            Ok(response) => Some(response),
            Err(error) => {
                state::set_last_error(&error);
                None
            }
        }
    }));
    match outcome {
        Ok(Some(json)) => new_java_string(&mut env, &json),
        Ok(None) => std::ptr::null_mut(),
        Err(_) => {
            state::fail("nativeTdRequest 里 panic（已兜住）");
            std::ptr::null_mut()
        }
    }
}

/// 本线程最后一次错误（`fuyubot_last_error` 的 Java 版）。没有错 = 空串。
///
/// `nativeTdRequest` / `nativePluginInfo` 返回 `null` 时，原因在这里——
/// 与 C 侧同一条纪律：**线程局部**，只对「刚才那次调用所在线程」有意义。
extern "system" fn native_last_error(mut env: JNIEnv<'_>, _this: JObject<'_>) -> jstring {
    let text = match catch_unwind(AssertUnwindSafe(state::last_error_string)) {
        Ok(text) => text,
        Err(_) => {
            state::fail("nativeLastError 里 panic（已兜住）");
            String::new()
        }
    };
    new_java_string(&mut env, &text)
}

fn start_with_json(json: &str) -> c_int {
    // 幂等：已经在跑就说忙，别起第二个线程去抢同一个 TDLib 数据库。
    if state::running() {
        return BUSY;
    }

    let options = match Options::parse(json) {
        Ok(options) => options,
        Err(error) => {
            state::set_last_error(&error);
            return ARG;
        }
    };

    // 判状态、点运行位、起线程、存句柄**全在同一把锁里**：中间不留「运行位亮着但
    // 还没有句柄」的空窗（并发 `stop` 会把那当成「线程已经自己收尾」）。
    // 锁里不做任何会回调 Java 的事——`onState` 里 App 可能顺手调 `nativeStop`，
    // 那是同一把锁，重入即死锁。
    let outcome = {
        let mut life = LIFECYCLE.lock().unwrap_or_else(|e| e.into_inner());
        let slot = match life.worker.as_ref() {
            None => Slot::Empty,
            Some(worker) if worker.handle.is_finished() => Slot::Finished,
            Some(_) => Slot::Alive,
        };

        match admit_start(state::running(), slot, life.stopping) {
            Admission::Busy => Err(StartFailure::Busy),
            Admission::Inconsistent => Err(StartFailure::Inconsistent),
            Admission::Start => {
                // 上一轮的句柄已经结束：回收，别留着占内存。
                life.worker = None;
                life.generation = life.generation.wrapping_add(1).max(1);
                let generation = life.generation;

                state::reset_stop();
                state::clear_stop_phase();
                // 先点亮再起线程：线程万一起得极快、立刻退出，`finish` 也只在
                // `generation` 对得上时才清位，不会把这一轮按掉。
                state::set_running(true);

                match std::thread::Builder::new()
                    .name("fuyubot-android".to_owned())
                    .spawn(move || {
                        // 先登记自己：Java 在 onAsk / onState 回调里同步调 `nativeTdRequest`
                        // 时，得能认出「这是工作线程，等下去等于自己等自己」。
                        state::set_worker_thread(Some(std::thread::current().id()));
                        match run::run(&options) {
                            // `App::shutdown` 已经发过 `onState(STOPPED)`，这里不再重复。
                            Ok(()) => {}
                            Err(error) => state::fail(&error),
                        }
                        // 工作线程 attach 过 JVM（登录问答要回调 Java）：退出前摘掉。
                        callback::detach_current_thread();
                        state::set_worker_thread(None);
                        finish(generation);
                    }) {
                    Ok(handle) => {
                        life.worker = Some(Worker { handle });
                        Ok(())
                    }
                    Err(error) => {
                        state::set_running(false);
                        Err(StartFailure::Spawn(error))
                    }
                }
            }
        }
    };

    match outcome {
        Ok(()) => {
            state::emit(state::State::Starting, "运行时线程已起");
            OK
        }
        Err(error) => {
            state::set_running(false);
            match error {
                StartFailure::Busy => {
                    state::set_last_error(
                        "上一次 start 还没收干净（正在停止 / 线程还活着），别立刻再 start",
                    );
                    BUSY
                }
                StartFailure::Inconsistent => {
                    state::set_last_error("上一次 start 的线程还活着（状态不一致）");
                    INTERNAL
                }
                StartFailure::Spawn(error) => {
                    state::fail(&format!("起不了运行时线程：{error}"));
                    INTERNAL
                }
            }
        }
    }
}

/// `stop` 超时时写给 `last_error()` 的文案；阶段名来自 `App::shutdown` 的逐段上报。
///
/// 抽成纯函数只为单测：这句话是崩溃前唯一能被 App 弹出来的现场，措辞不能漂。
fn stop_timeout_message(phase: &str) -> String {
    let where_ = if phase.is_empty() {
        "（kernel 还没上报收摊阶段：可能还在登录 / 启动，收摊还没开始）".to_owned()
    } else {
        format!("（kernel 停在这一步：{phase}）")
    };
    format!(
        "stop 等收摊超时（{STOP_BUDGET:?}）：worker 线程还在跑{where_}。\
         进程没收干净，别立刻再 start；日志见 <root>/logs/fuyubot.log"
    )
}

fn stop_runtime() -> c_int {
    // 幂等：没在跑 = 已经是停的。别因为重复调用就报错——宿主在 onDestroy 里可能调多次。
    if !state::running() {
        // 上一轮可能已经自己退出、句柄还没回收：顺手 join 掉（它已经 finished，不会等）。
        let finished = {
            let mut life = LIFECYCLE.lock().unwrap_or_else(|e| e.into_inner());
            match life.worker.as_ref() {
                Some(worker) if worker.handle.is_finished() => life.worker.take(),
                _ => None,
            }
        };
        if let Some(worker) = finished {
            let _ = worker.handle.join();
        }
        return OK;
    }

    // 取句柄 + 打「停止中」标记。这两步必须一起做：句柄一旦离开 `Lifecycle`，
    // 并发的 `start` 就会把「运行位在、句柄没了」误判成「上一轮已经收尾」。
    let worker = {
        let mut life = LIFECYCLE.lock().unwrap_or_else(|e| e.into_inner());
        let slot = match life.worker.as_ref() {
            None => Slot::Empty,
            Some(worker) if worker.handle.is_finished() => Slot::Finished,
            Some(_) => Slot::Alive,
        };
        match admit_stop(true, slot, life.stopping) {
            StopGate::AlreadyStopping => {
                state::set_last_error("另一个线程正在 stop（等它返回；这期间不要再 start）");
                return BUSY;
            }
            // 运行位在、句柄却没了：线程正在退出，`finish` 会清位。别再动它。
            StopGate::Idle => return OK,
            StopGate::Take => {
                let Some(worker) = life.worker.take() else {
                    return OK;
                };
                life.stopping = true;
                worker
            }
        }
    };

    // 在自己的回调线程里调 stop：worker 正卡在这次调用里，等它等于等自己。
    if worker.handle.thread().id() == std::thread::current().id() {
        let mut life = LIFECYCLE.lock().unwrap_or_else(|e| e.into_inner());
        life.worker = Some(worker);
        life.stopping = false;
        state::set_last_error("不能在 fuyubot 的工作线程里调 stop（会自己等自己）");
        return TIMEOUT;
    }

    state::emit(state::State::Stopping, "收到停止请求");
    state::request_stop();

    let deadline = Instant::now() + STOP_BUDGET;
    while !worker.handle.is_finished() {
        if Instant::now() >= deadline {
            // 还没停完：句柄放回去（清掉 stopping，允许下一次 stop 接着等它）。
            let mut life = LIFECYCLE.lock().unwrap_or_else(|e| e.into_inner());
            life.worker = Some(worker);
            life.stopping = false;
            drop(life);
            state::set_last_error(&stop_timeout_message(&state::stop_phase()));
            return TIMEOUT;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = worker.handle.join();
    {
        let mut life = LIFECYCLE.lock().unwrap_or_else(|e| e.into_inner());
        life.stopping = false;
        // join 完这一轮的线程一定已经退出：位灭了才准下一次 start。
        state::set_running(false);
    }
    state::clear_stop_phase();
    OK
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 绑定表必须与头文件 / 文档里的**十二个**方法一一对应：名字或签名写错，
    /// `RegisterNatives` 在真机上才会失败（本机没有 JVM，只能静态对齐）。
    #[test]
    fn jni_method_table_matches_the_documented_contract() {
        let table: Vec<(String, String)> = jni_methods()
            .iter()
            .map(|method| {
                (
                    method.name.to_string_lossy().into_owned(),
                    method.sig.to_string_lossy().into_owned(),
                )
            })
            .collect();
        assert_eq!(
            table,
            vec![
                ("nativeVersion".to_owned(), "()I".to_owned()),
                (
                    "nativeRegister".to_owned(),
                    "(Ljava/lang/Object;)I".to_owned()
                ),
                ("nativeStart".to_owned(), "(Ljava/lang/String;)I".to_owned()),
                ("nativeStop".to_owned(), "()I".to_owned()),
                ("nativeIsRunning".to_owned(), "()Z".to_owned()),
                (
                    "nativeAnswer".to_owned(),
                    "(ILjava/lang/String;)I".to_owned()
                ),
                (
                    "nativePlugins".to_owned(),
                    "()Ljava/lang/String;".to_owned()
                ),
                (
                    "nativePluginInfo".to_owned(),
                    "(Ljava/lang/String;)Ljava/lang/String;".to_owned()
                ),
                (
                    "nativeAuthState".to_owned(),
                    "()Ljava/lang/String;".to_owned()
                ),
                (
                    "nativeConnectionState".to_owned(),
                    "()Ljava/lang/String;".to_owned()
                ),
                (
                    "nativeTdRequest".to_owned(),
                    "(Ljava/lang/String;J)Ljava/lang/String;".to_owned()
                ),
                (
                    "nativeLastError".to_owned(),
                    "()Ljava/lang/String;".to_owned()
                ),
            ]
        );
    }

    /// 头文件里的 `FUYUBOT_JNI_ABI` 必须与 `ABI_VERSION` 一致——两边都是手写的，
    /// 只靠注释约束不住。头文件不在 crate 源码里也能读：`include_str!` 在编译期绑定。
    #[test]
    fn the_header_declares_the_same_abi_version() {
        let header = include_str!("../include/fuyubot_jni.h");
        let declared = header
            .lines()
            .find_map(|line| line.trim().strip_prefix("#define FUYUBOT_JNI_ABI "))
            .expect("头文件里应有 FUYUBOT_JNI_ABI");
        assert_eq!(declared.trim(), ABI_VERSION.to_string());
    }

    /// 头文件里的 C 符号必须与 lib.rs 的导出逐个对上（改名 / 漏写都要在这儿响）。
    #[test]
    fn the_header_lists_every_exported_symbol() {
        let header = include_str!("../include/fuyubot_jni.h");
        for symbol in [
            "fuyubot_abi_version",
            "fuyubot_start",
            "fuyubot_stop",
            "fuyubot_is_running",
            "fuyubot_answer",
            "fuyubot_last_error",
            "fuyubot_plugins",
            "fuyubot_plugin_info",
            "fuyubot_auth_state",
            "fuyubot_connection_state",
            "fuyubot_td_request",
        ] {
            assert!(
                header.contains(&format!("{symbol}(")),
                "头文件缺 {symbol} 的声明"
            );
        }
    }

    /// 插件报告是对外契约：计数字段与可选回调 `onPlugins` 必须写在头文件里，
    /// 否则 APK 作者只会看到旧的 `{"count":n,"plugins":[…]}`，继续猜「失败在哪」。
    #[test]
    fn the_header_documents_the_plugin_report_contract() {
        let header = include_str!("../include/fuyubot_jni.h");
        for token in [
            "onPlugins",
            "skipped_plugins",
            "\"scanned\"",
            "\"failed\"",
            "\"skipped\"",
        ] {
            assert!(header.contains(token), "头文件缺 {token} 的说明");
        }
    }

    /// 两个状态查询的 JSON 契约必须写在头文件里：APK 作者只看得到头文件，
    /// 少一个字段名就只能靠猜（`known` / `needs_re_login` 尤其容易漏）。
    #[test]
    fn the_header_documents_the_td_state_contract() {
        let header = include_str!("../include/fuyubot_jni.h");
        for token in [
            "fuyubot_auth_state",
            "fuyubot_connection_state",
            "nativeAuthState",
            "nativeConnectionState",
            "\"needs_re_login\"",
            "connectionStateUpdating",
            "authorizationStateReady",
        ] {
            assert!(header.contains(token), "头文件缺 {token} 的说明");
        }
    }

    /// `onPlugins` 是**可选**回调：没有 JVM（本仓单测就是这样）时调一次不能 panic，
    /// 没注册回调（纯 C 宿主）时也一样。
    #[test]
    fn plugin_report_without_a_jvm_is_a_no_op() {
        use fuyubot_app::AppHost;
        crate::host::JniHost.plugin_report(r#"{"count":0,"plugins":[]}"#);
    }

    /// `FindClass` 只认 `/` 分隔的内部名；写成点分名会在真机上找不到类。
    /// 默认值与 `FUYUBOT_JNI_CLASS` 覆盖值都得守住这条。
    #[test]
    fn native_class_is_a_jni_internal_name() {
        assert!(!NATIVE_CLASS.contains('.'));
        assert!(!NATIVE_CLASS.contains(';'));
        assert!(!NATIVE_CLASS.starts_with('/'));
        assert!(!NATIVE_CLASS.ends_with('/'));
        assert!(NATIVE_CLASS.contains('/'));
    }

    /// 超时文案必须点名「卡在哪一步」——App 弹出来就靠它区分停循环 / 刷盘 / 卸插件。
    #[test]
    fn stop_timeout_message_names_the_stuck_stage() {
        let message = stop_timeout_message("TDLib 关会话（等数据库刷盘）");
        assert!(message.contains("8s"), "{message}");
        assert!(message.contains("TDLib 关会话"), "{message}");
    }

    /// 还没上报阶段（卡在登录 / 启动）时也要说人话，不能是空括号。
    #[test]
    fn stop_timeout_message_falls_back_without_a_phase() {
        let message = stop_timeout_message("");
        assert!(message.contains("登录 / 启动"), "{message}");
    }

    /// 起第二轮的唯一前提是「上一轮真的收干净了」：运行位灭、没人在 stop、
    /// 线程槽不是活着的。任何一条不满足都不许起——两个 worker 就是两个 TDLib
    /// 客户端抢同一个数据库目录（Android 上整进程崩的那一类）。
    #[test]
    fn start_is_refused_until_the_previous_round_is_really_over() {
        // 正常：没跑过。
        assert_eq!(admit_start(false, Slot::Empty, false), Admission::Start);
        // 上一轮自己结束了（句柄还在，但人走了）：可以起，起的时候顺手回收句柄。
        assert_eq!(admit_start(false, Slot::Finished, false), Admission::Start);
        // 还在跑。
        assert_eq!(admit_start(true, Slot::Alive, false), Admission::Busy);
        // 线程活着但运行位灭了：状态不一致，别硬起第二个。
        assert_eq!(
            admit_start(false, Slot::Alive, false),
            Admission::Inconsistent
        );
        // 另一个线程正在 join 上一轮：哪怕运行位已经灭了也不许起。
        assert_eq!(admit_start(false, Slot::Empty, true), Admission::Busy);
        assert_eq!(admit_start(false, Slot::Finished, true), Admission::Busy);
    }

    /// 并发 `stop` 不能两个线程一起去 join：第一个拿着句柄，第二个必须回 BUSY
    /// （以前它会「取不到句柄 → 清运行位 → 回 OK」，顺手把 start 的闸门打开）。
    #[test]
    fn concurrent_stops_do_not_both_join() {
        assert_eq!(admit_stop(true, Slot::Alive, false), StopGate::Take);
        assert_eq!(
            admit_stop(true, Slot::Alive, true),
            StopGate::AlreadyStopping
        );
        // 幂等：没在跑就是 OK。
        assert_eq!(admit_stop(false, Slot::Empty, false), StopGate::Idle);
        // 运行位在、句柄没了：线程正在退出，`finish` 会清位——不去动它。
        assert_eq!(admit_stop(true, Slot::Empty, false), StopGate::Idle);
    }
}
