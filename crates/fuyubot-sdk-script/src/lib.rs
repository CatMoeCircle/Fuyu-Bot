//! 脚本插件桥：**一个 cdylib 把 Python / JS 脚本接进 fuyubot**。
//!
//! 插件作者写脚本（逻辑），本 crate 提供契约映射（胶水）。Python 与 JS
//! **共用这一份实现**，所以「两份 SDK 对 ABI 的理解」不可能漂移。
//!
//! ## 它是怎么跑起来的
//!
//! ```text
//! fuyubot 宿主 ──dlopen──> 本 crate 编出来的 cdylib ──spawn──> python/node <script>
//!        ▲                                                        │
//!        └──────────── HostApiVTable 调用 ◄── 一行一个 JSON ───────┘
//! ```
//!
//! - `plugin_start` 时才起解释器（`plugin_init` 只登记，宿主此刻还没准备好收注册）；
//! - 宿主 → 脚本：脚本的 stdin，每行一个 JSON；
//! - 脚本 → 宿主：脚本的 stdout，每行一个 JSON。**脚本的 stderr 原样透传到宿主控制台**，
//!   Python / Node 的报错直接可见；
//! - `plugin_stop` 杀掉解释器并回收订阅。
//!
//! ## 协议（一行一个 JSON 对象，`op` 是动作名）
//!
//! 脚本 → 宿主：
//!
//! | 行 | 作用 | 宿主回什么 |
//! |---|---|---|
//! | `{"op":"ready"}` | 告诉宿主脚本起来了 | 无 |
//! | `{"op":"log","level":"info","message":"..."}` | 打日志；`level` 可给 `error/warn/info/debug/trace` 或 1..5 | 无 |
//! | `{"op":"on","tag":1,"event":"updateNewMessage","filter":{...}}` | 订阅，`filter` 可省 | `{"op":"on","tag":1,"handle":N}` |
//! | `{"op":"off","handle":N}` | 退订 | 无 |
//! | `{"op":"invoke","tag":2,"method":"sendMessage","params":{...}}` | 发 TDLib 请求 | `{"op":"invoked","tag":2,"request_id":R}` |
//! | `{"op":"emit","event":"x","payload":{...}}` | 发一条自定义事件 | 无 |
//! | `{"op":"tool","descriptor":{...}}` | 登记工具元数据 | `{"op":"tool","tag":n,"handle":N}` |
//!
//! 宿主 → 脚本：
//!
//! | 行 | 作用 |
//! |---|---|
//! | `{"op":"event","event":"updateNewMessage","payload":{...}}` | 你订阅的事件来了 |
//! | 上面那几种回执 | 你给的 `tag` 会原样带回，方便脚本自己配对 |
//!
//! ## 用法（插件 crate 的全部代码）
//!
//! ```ignore
//! fuyubot_sdk_script::script_plugin!("fuyubot.python.weather", python, "weather.py");
//! ```
//!
//! 脚本路径：`plugin_start` 收到的配置 JSON 里的 `"script"`（绝对路径或相对当前目录），
//! 没有就用宏里写的那个名字。解释器：Python 依次试 `python`/`python3`/`py`，
//! JS 依次试 `node`/`bun`。
//!
//! 已知上限：一个插件 = 一个解释器子进程；宿主与它之间的管道是同步写的，
//! 脚本若一直不读 stdin，宿主那条线程会在写管道时被拖住。

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use fuyubot_plugin_api::{HostApi, HostExt, LogLevel, Subscription};
use serde_json::{Value, json};

/// `plugin_start` 最多等脚本报 `ready` 多久。
///
/// 10 秒对「解释器冷启动 + import」是宽裕的；真超时通常是脚本忘了在最后调
/// `host.run()` / `host.run()`，错误信息里会直说。
const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);

/// 脚本运行时要启动的解释器。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Runtime {
    /// 依次尝试 `python` / `python3` / `py`。
    Python,
    /// 依次尝试 `node` / `bun`。
    Node,
}

impl Runtime {
    fn candidates(self) -> &'static [&'static str] {
        match self {
            Self::Python => &["python", "python3", "py"],
            Self::Node => &["node", "bun"],
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Python => "python",
            Self::Node => "js",
        }
    }
}

/// 一条通往解释器的**可写**通道。
///
/// 抽成 trait 而不是直接抱 `ChildStdin`：测试要能用内存缓冲顶替真实子进程，
/// 否则这一层就只能靠「真的起一个 python」来验。
trait LineSink: Send + Sync {
    fn write_line(&self, line: &str);
}

/// 真实子进程的 stdin。
struct ChildSink {
    stdin: Mutex<ChildStdin>,
    alive: AtomicBool,
}

impl LineSink for ChildSink {
    fn write_line(&self, line: &str) {
        if !self.alive.load(Ordering::Relaxed) {
            return;
        }
        let mut stdin = self.stdin.lock().unwrap_or_else(|e| e.into_inner());
        if writeln!(stdin, "{line}")
            .and_then(|()| stdin.flush())
            .is_err()
        {
            // 解释器没了：记一次就够了，之后所有写入直接丢掉。
            self.alive.store(false, Ordering::Relaxed);
        }
    }
}

/// 宿主与脚本之间的这条管道。
struct Chat {
    sink: Box<dyn LineSink>,
}

impl Chat {
    fn send(&self, value: &Value) {
        self.sink.write_line(&value.to_string());
    }
}

/// 一条订阅在宿主侧的私有数据（回调的 `user_data`）。
struct SubState {
    chat: Arc<Chat>,
    event: String,
    /// 宿主的订阅号。事件要把它带给脚本：脚本可能给同一个事件注册了多条订阅
    /// （各自带不同过滤器），**只有订阅号能告诉它这条事件属于哪一个回调**。
    ///
    /// 注册是同步的、句柄要等宿主返回才知道，所以先放 0、拿到句柄再填。
    /// 极端情况下（另一条线程正好在 `on` 返回与这次写入之间投递）事件会带着
    /// 0 出去，脚本那边认不出订阅号就会丢掉它——窗口是同一线程上的两条指令，
    /// 丢的也只是「订阅刚建立那一瞬」的事件。要彻底消掉得让宿主回调把句柄
    /// 传进来，那是 ABI 变更，不值得为这个窗口改。
    handle: AtomicU64,
}

impl SubState {
    fn forward(&self, payload: *const core::ffi::c_char) {
        // SAFETY: 宿主按 ABI 契约传的是临时 NUL 结尾字符串。
        let payload = unsafe { fuyubot_plugin_api::parse_payload(payload) };
        self.chat.send(&json!({
            "op": "event",
            "event": self.event,
            "handle": self.handle.load(Ordering::Relaxed),
            "payload": payload,
        }));
    }
}

/// 脚本启动到哪一步了。///
/// `plugin_start` 必须**等脚本报 `ready` 才能返回**：脚本里的
/// `host.on(...)` 是一条条写到宿主这边来的，宿主没法知道「它注册完了没有」。
/// 早返回的后果是——宿主已经回放/分发更新了，脚本的订阅才刚刚建起来，
/// 于是第一批事件静悄悄地丢了（这个坑是例子插件在 `--replay` 下暴露出来的）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Startup {
    Waiting,
    Ready,
    /// 脚本还没说 ready 就退出了（语法错、import 失败、缺依赖……）。
    Dead,
}

/// 宿主侧的全部共享状态：pump 线程与 [`Bridge`] 都靠它。
struct Shared {
    host: HostApi,
    chat: Arc<Chat>,
    /// 生效中的订阅：`Subscription` drop 即退订。
    subs: Mutex<HashMap<u64, Subscription>>,
    /// 每条订阅的私有数据，必须活到插件停止。
    states: Mutex<Vec<Arc<SubState>>>,
    startup: Mutex<Startup>,
    startup_signal: Condvar,
}

impl Shared {
    fn new(host: HostApi, chat: Arc<Chat>) -> Self {
        Self {
            host,
            chat,
            subs: Mutex::new(HashMap::new()),
            states: Mutex::new(Vec::new()),
            startup: Mutex::new(Startup::Waiting),
            startup_signal: Condvar::new(),
        }
    }

    fn mark_ready(&self) {
        *self.startup.lock().unwrap_or_else(|e| e.into_inner()) = Startup::Ready;
        self.startup_signal.notify_all();
    }

    fn mark_dead(&self) {
        let mut state = self.startup.lock().unwrap_or_else(|e| e.into_inner());
        // 已经 ready 过就保持 ready：脚本跑着跑着退出是另一回事，由写管道时的失败去报。
        if *state == Startup::Waiting {
            *state = Startup::Dead;
        }
        drop(state);
        self.startup_signal.notify_all();
    }

    fn wait_startup(&self, timeout: Duration) -> Startup {
        let deadline = Instant::now() + timeout;
        let mut state = self.startup.lock().unwrap_or_else(|e| e.into_inner());
        while *state == Startup::Waiting {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let (guard, _) = self
                .startup_signal
                .wait_timeout(state, deadline - now)
                .unwrap_or_else(|e| e.into_inner());
            state = guard;
        }
        *state
    }

    fn reply(&self, tag: Option<u64>, op: &str, mut extra: Value) {
        if !extra.is_object() {
            extra = json!({});
        }
        let object = extra.as_object_mut().expect("上面保证是对象");
        object.insert("op".to_owned(), json!(op));
        if let Some(tag) = tag {
            object.insert("tag".to_owned(), json!(tag));
        }
        self.chat.send(&extra);
    }

    fn fail(&self, tag: Option<u64>, message: &str) {
        self.reply(tag, "error", json!({ "message": message }));
    }

    /// 处理脚本发来的一行。
    fn handle_line(&self, line: &str) {
        let value: Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(e) => {
                self.fail(None, &format!("这一行不是合法 JSON ({e}): {line}"));
                return;
            }
        };
        let tag = value.get("tag").and_then(Value::as_u64);
        let Some(op) = value.get("op").and_then(Value::as_str) else {
            self.fail(tag, "缺 op 字段");
            return;
        };

        match op {
            "ready" => self.mark_ready(),

            "log" => {
                let text = value.get("message").and_then(Value::as_str).unwrap_or("");
                self.host.log(level_of(value.get("level")), text);
            }

            "on" => {
                let Some(event) = value.get("event").and_then(Value::as_str) else {
                    self.fail(tag, "on 缺 event 字段");
                    return;
                };
                let filter = match value.get("filter") {
                    None | Some(Value::Null) => None,
                    Some(filter) => Some(filter.to_string()),
                };
                let state = Arc::new(SubState {
                    chat: Arc::clone(&self.chat),
                    event: event.to_owned(),
                    handle: AtomicU64::new(0),
                });
                // Arc 的堆分配地址稳定，塞进 `states` 之后这个指针照样有效。
                let pointer = Arc::as_ptr(&state) as *mut core::ffi::c_void;
                let handle = self.host.on(event, filter.as_deref(), forward, pointer);
                let Some(subscription) = Subscription::new(self.host, handle) else {
                    self.fail(tag, &format!("订阅 {event} 失败（过滤器不合法？）"));
                    return;
                };
                // 句柄要写进 `state` 才能带进每一条事件，所以先填再搬进去。
                state.handle.store(handle.id, Ordering::Relaxed);
                self.states
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(state);
                self.subs
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(handle.id, subscription);
                self.reply(tag, "on", json!({ "handle": handle.id }));
            }

            "off" => {
                if let Some(id) = value.get("handle").and_then(Value::as_u64) {
                    // 从表里摘掉 → `Subscription` drop → 调宿主的 off。
                    self.subs
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&id);
                }
            }

            "invoke" => {
                let Some(method) = value.get("method").and_then(Value::as_str) else {
                    self.fail(tag, "invoke 缺 method 字段");
                    return;
                };
                let params = value.get("params").cloned().unwrap_or(Value::Null);
                match self.host.invoke_value(method, params) {
                    Ok(request) => self.reply(tag, "invoked", json!({ "request_id": request.id })),
                    Err(e) => self.fail(tag, &format!("invoke {method} 失败: {e}")),
                }
            }

            "emit" => {
                let event = value.get("event").and_then(Value::as_str).unwrap_or("");
                let payload = value
                    .get("payload")
                    .map(Value::to_string)
                    .unwrap_or_else(|| "null".to_owned());
                if let Err(e) = self.host.emit(event, &payload) {
                    self.fail(tag, &format!("emit {event} 失败: {e}"));
                }
            }

            "tool" => {
                let descriptor = value
                    .get("descriptor")
                    .map(Value::to_string)
                    .unwrap_or_else(|| "{}".to_owned());
                let handle = self.host.register_tool(&descriptor);
                if handle.is_valid() {
                    self.reply(tag, "tool", json!({ "handle": handle.id }));
                } else {
                    self.fail(tag, "工具描述不合法");
                }
            }

            other => self.fail(tag, &format!("不认识的 op: {other}")),
        }
    }

    fn shutdown(&self) {
        self.subs.lock().unwrap_or_else(|e| e.into_inner()).clear();
        self.states
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }
}

fn level_of(value: Option<&Value>) -> LogLevel {
    match value {
        Some(Value::Number(number)) => LogLevel::from_u32(number.as_u64().unwrap_or(3) as u32),
        Some(Value::String(name)) => match name.as_str() {
            "error" => LogLevel::Error,
            "warn" => LogLevel::Warn,
            "debug" => LogLevel::Debug,
            "trace" => LogLevel::Trace,
            _ => LogLevel::Info,
        },
        _ => LogLevel::Info,
    }
}

/// 订阅回调：把事件原样写给脚本。
///
/// `C-unwind`：`extern "C"` 边界上逃出去的 panic 会直接 abort 整个进程（见 `fuyubot-abi`）。
unsafe extern "C-unwind" fn forward(
    user_data: *mut core::ffi::c_void,
    payload: *const core::ffi::c_char,
) -> i32 {
    // SAFETY: user_data 是 `Shared::handle_line` 里 `Arc::as_ptr` 出来的，活到 shutdown。
    let Some(state) = (unsafe { (user_data as *const SubState).as_ref() }) else {
        return -1;
    };
    state.forward(payload);
    0
}

/// 脚本插件的桥。插件 crate 只管用 [`script_plugin!`] 生成，不用直接碰它。
pub struct Bridge {
    host: HostApi,
    name: &'static str,
    runtime: Runtime,
    script: &'static str,
    shared: Option<Arc<Shared>>,
    child: Option<Child>,
    pump: Option<std::thread::JoinHandle<()>>,
}

impl Bridge {
    pub fn new(host: HostApi, name: &'static str, runtime: Runtime, script: &'static str) -> Self {
        Self {
            host,
            name,
            runtime,
            script,
            shared: None,
            child: None,
            pump: None,
        }
    }

    /// 起解释器、执行脚本，然后把它接进宿主。
    ///
    /// **返回之前会等脚本报 `ready`**：脚本的订阅是一条条写到宿主这边来的，
    /// 不等就容易「宿主已经开始分发更新，脚本还没订阅上」。等多久见
    /// [`STARTUP_TIMEOUT`]。
    pub fn start(&mut self, config_json: &str) -> Result<(), String> {
        let script = resolve_script(config_json, self.script);
        let mut tried = Vec::new();
        for candidate in self.runtime.candidates() {
            let mut child = match spawn(candidate, &script, config_json) {
                Ok(child) => child,
                Err(e) => {
                    tried.push(format!("{candidate}: {e}"));
                    continue;
                }
            };

            // 语法错会让解释器在几十毫秒内退出。先快速看一眼，把最常见的那个错误
            // 变成一条能指着看的消息（脚本自己的报错在 stderr 上）。
            std::thread::sleep(Duration::from_millis(50));
            if let Ok(Some(status)) = child.try_wait() {
                return Err(format!(
                    "{candidate} 跑 {} 之后立刻退出（{status}）；上面是它的报错",
                    script.display()
                ));
            }

            let stdout = child.stdout.take().expect("spawn 时要求了管道");
            let stdin = child.stdin.take().expect("spawn 时要求了管道");
            let chat = Arc::new(Chat {
                sink: Box::new(ChildSink {
                    stdin: Mutex::new(stdin),
                    alive: AtomicBool::new(true),
                }),
            });
            let shared = Arc::new(Shared::new(self.host, chat));

            let pumped = Arc::clone(&shared);
            let pump = std::thread::spawn(move || pump(&pumped, stdout));

            match shared.wait_startup(STARTUP_TIMEOUT) {
                Startup::Ready => {}
                Startup::Dead => {
                    reap(child, pump);
                    return Err(format!(
                        "{} {} 在报告 ready 之前就退出了；它的报错在上面",
                        self.runtime.label(),
                        script.display()
                    ));
                }
                Startup::Waiting => {
                    reap(child, pump);
                    return Err(format!(
                        "{} {} 起了 {} 秒还没报告 ready（脚本开头没调 run()？）",
                        self.runtime.label(),
                        script.display(),
                        STARTUP_TIMEOUT.as_secs()
                    ));
                }
            }

            self.shared = Some(shared);
            self.child = Some(child);
            self.pump = Some(pump);
            self.host.info(&format!(
                "{}: {candidate} 已经在跑（{}）",
                self.name,
                script.display()
            ));
            return Ok(());
        }

        Err(format!(
            "找不到 {} 解释器（试过：{}）。脚本：{}",
            self.runtime.label(),
            tried.join("; "),
            script.display()
        ))
    }

    /// 杀掉解释器、回收订阅。
    pub fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        // 子进程死了，stdout 关掉，pump 线程随之退出。
        if let Some(pump) = self.pump.take() {
            let _ = pump.join();
        }
        if let Some(shared) = self.shared.take() {
            shared.shutdown();
        }
        self.host.info(&format!("{}: 停了", self.name));
    }
}

/// 读脚本的 stdout，一行一个 JSON，逐条执行。
///
/// 退出（脚本死了或管道关了）时把 [`Startup`] 标成 `Dead`，让还在等 ready 的
/// `plugin_start` 立刻醒过来，而不是干等满超时。
fn pump(shared: &Shared, stdout: ChildStdout) {
    for line in BufReader::new(stdout).lines() {
        match line {
            Ok(line) if !line.trim().is_empty() => shared.handle_line(&line),
            Ok(_) => {}
            Err(_) => break,
        }
    }
    shared.mark_dead();
}

/// 收掉一个没起来的脚本进程：先杀再 join，别留着线程。
fn reap(mut child: Child, pump: std::thread::JoinHandle<()>) {
    let _ = child.kill();
    let _ = child.wait();
    let _ = pump.join();
}

/// 起一个解释器进程。stdin/stdout 都接管道，stderr 留给用户看。
fn spawn(program: &str, script: &std::path::Path, config_json: &str) -> std::io::Result<Child> {
    let mut command = Command::new(program);
    command
        .arg(script)
        .arg(config_json)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    // Python 往管道写时会开块缓冲，不关掉的话脚本的第一条消息要等到缓冲满才出来。
    // `PYTHONUTF8` 是给「脚本被别的东西启动」兜底的——`fuyubot_plugin.py` 自己也会
    // 把 stdio 钉成 UTF-8，两边都做是因为 Windows 默认代码页真的会把中文拆坏。
    if program.contains("py") {
        command.env("PYTHONUNBUFFERED", "1").env("PYTHONUTF8", "1");
    }
    command.spawn()
}

/// 脚本路径：配置里的 `script` 优先，否则用宏里写的名字。
fn resolve_script(config_json: &str, default_script: &str) -> std::path::PathBuf {
    let configured = serde_json::from_str::<Value>(config_json)
        .ok()
        .and_then(|value| {
            value
                .get("script")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .filter(|script| !script.is_empty())
        .unwrap_or_else(|| default_script.to_owned());
    std::path::PathBuf::from(configured)
}

/// 宏展开要用到的名字。
///
/// 插件作者**只依赖 `fuyubot-sdk-script`**，所以宏里不能直接写
/// `::fuyubot_plugin_api::...`（那样生成的插件还得自己再依赖一个 crate）。
/// 走 `$crate::__macro_support::` 就把这层转发掉了。别直接用这个模块。
#[doc(hidden)]
pub mod __macro_support {
    pub use fuyubot_plugin_api::{HostApi, Plugin, define_plugin};
}

/// 插件的全部代码就是这一行。
///
/// ```ignore
/// fuyubot_sdk_script::script_plugin!("fuyubot.python.weather", python, "weather.py");
/// fuyubot_sdk_script::script_plugin!("fuyubot.js.hello", js, "hello.js");
/// ```
#[macro_export]
macro_rules! script_plugin {
    (@build $name:literal, $runtime:expr, $script:literal) => {
        /// 脚本插件：逻辑在 `.py` / `.js` 里，这里只有 ABI 胶水。
        struct ScriptPlugin($crate::Bridge);

        impl $crate::__macro_support::Plugin for ScriptPlugin {
            fn init(host: $crate::__macro_support::HostApi) -> Result<Self, String> {
                Ok(Self($crate::Bridge::new(host, $name, $runtime, $script)))
            }

            fn start(&mut self, config_json: &str) -> Result<(), String> {
                self.0.start(config_json)
            }

            fn stop(&mut self) {
                self.0.stop();
            }
        }

        $crate::__macro_support::define_plugin!(ScriptPlugin, $name);
    };

    ($name:literal, python, $script:literal) => {
        $crate::script_plugin!(@build $name, $crate::Runtime::Python, $script);
    };

    ($name:literal, js, $script:literal) => {
        $crate::script_plugin!(@build $name, $crate::Runtime::Node, $script);
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::{CStr, c_char, c_void};
    use std::sync::atomic::AtomicUsize;

    /// 记下宿主收到了什么（真实 VTable 的桩）。
    #[derive(Default)]
    struct FakeHost {
        subscriptions: AtomicUsize,
        unsubscriptions: AtomicUsize,
        invokes: Mutex<Vec<(String, String)>>,
        logs: Mutex<Vec<String>>,
        tools: AtomicUsize,
        /// 最后一次 `on` 收到的回调。**故意不在 `on` 里就调它**：真实宿主
        /// 不可能在 `on` 返回之前投递事件（句柄还没发出去），桩也不该替它造这个场景。
        callback: Mutex<Option<(usize, fuyubot_plugin_api::EventCallbackFn)>>,
    }

    impl FakeHost {
        /// 像真实宿主那样发一条事件给最后那个订阅。
        fn fire(&self, payload: &str) {
            let stored = *self.callback.lock().unwrap();
            let (user_data, callback) = stored.expect("先得有人订阅");
            let payload = std::ffi::CString::new(payload).unwrap();
            // SAFETY: 回调按 ABI 契约接受 (user_data, NUL 结尾字符串)；
            // user_data 是订阅方给的 SubState，还活着。
            unsafe { callback(user_data as *mut c_void, payload.as_ptr() as *const c_char) };
        }
    }

    /// 假的宿主实例就从 VTable 的 `host_ctx` 里取——那正是这个字段存在的理由。
    ///
    /// **不能用 `static`**：测试是并行跑的，共用 static 会让断言互相干扰（阶段 4 踩过）。
    ///
    /// # Safety
    /// `host` 必须是 `fake_vtable` 填进去的那个 `FakeHost`，且调用期间仍然活着。
    unsafe fn host_of<'a>(host: *mut c_void) -> &'a FakeHost {
        unsafe { &*(host as *const FakeHost) }
    }

    unsafe extern "C" fn on(
        host: *mut c_void,
        _event: *const c_char,
        filter: *const c_char,
        callback: Option<fuyubot_plugin_api::EventCallbackFn>,
        user_data: *mut c_void,
    ) -> fuyubot_plugin_api::SubscriptionHandle {
        // SAFETY: 由调用方（`fake_vtable`）保证。
        let fake = unsafe { host_of(host) };
        // 真实宿主也这么干：过滤器不是 JSON 对象就拒绝这次订阅。
        if !filter.is_null() {
            // SAFETY: ABI 契约保证是 NUL 结尾字符串。
            let text = unsafe { CStr::from_ptr(filter) }.to_string_lossy();
            let ok = serde_json::from_str::<Value>(&text).is_ok_and(|value| value.is_object());
            if !ok {
                return fuyubot_plugin_api::SubscriptionHandle::INVALID;
            }
        }
        fake.subscriptions.fetch_add(1, Ordering::SeqCst);
        // 只登记回调，不在 `on` 里就调它——真实宿主发不出一个还没有句柄的订阅的事件。
        *fake.callback.lock().unwrap() = callback.map(|callback| (user_data as usize, callback));
        fuyubot_plugin_api::SubscriptionHandle { id: 7 }
    }

    unsafe extern "C" fn off(host: *mut c_void, _handle: fuyubot_plugin_api::SubscriptionHandle) {
        // SAFETY: 同上。
        unsafe { host_of(host) }
            .unsubscriptions
            .fetch_add(1, Ordering::SeqCst);
    }

    unsafe extern "C" fn log(host: *mut c_void, _level: u32, message: *const c_char) {
        // SAFETY: ABI 契约保证 message 是 NUL 结尾字符串。
        let text = unsafe { CStr::from_ptr(message) }
            .to_string_lossy()
            .into_owned();
        // SAFETY: 同上。
        unsafe { host_of(host) }.logs.lock().unwrap().push(text);
    }

    unsafe extern "C" fn invoke(
        host: *mut c_void,
        method: *const c_char,
        params: *const c_char,
    ) -> fuyubot_plugin_api::RequestId {
        // SAFETY: 同上。
        let method = unsafe { CStr::from_ptr(method) }
            .to_string_lossy()
            .into_owned();
        // SAFETY: 同上。
        let params = unsafe { CStr::from_ptr(params) }
            .to_string_lossy()
            .into_owned();
        // SAFETY: 同上。
        unsafe { host_of(host) }
            .invokes
            .lock()
            .unwrap()
            .push((method, params));
        fuyubot_plugin_api::RequestId { id: 99 }
    }

    unsafe extern "C" fn register_tool(
        host: *mut c_void,
        _descriptor: *const c_char,
    ) -> fuyubot_plugin_api::ToolHandle {
        // SAFETY: 同上。
        unsafe { host_of(host) }
            .tools
            .fetch_add(1, Ordering::SeqCst);
        fuyubot_plugin_api::ToolHandle { id: 3 }
    }

    unsafe extern "C" fn emit(_h: *mut c_void, _e: *const c_char, _p: *const c_char) -> i32 {
        0
    }
    unsafe extern "C" fn call_tool(
        _h: *mut c_void,
        _n: *const c_char,
        _i: *const c_char,
    ) -> fuyubot_plugin_api::RequestId {
        fuyubot_plugin_api::RequestId::INVALID
    }
    unsafe extern "C" fn list_tools(_h: *mut c_void) -> *const c_char {
        core::ptr::null()
    }
    unsafe extern "C" fn provide(
        _h: *mut c_void,
        _n: *const c_char,
        _v: *const fuyubot_plugin_api::ServiceVTable,
    ) -> fuyubot_plugin_api::ServiceHandle {
        fuyubot_plugin_api::ServiceHandle::INVALID
    }
    unsafe extern "C" fn get(
        _h: *mut c_void,
        _n: *const c_char,
    ) -> *const fuyubot_plugin_api::ServiceVTable {
        core::ptr::null()
    }
    unsafe extern "C" fn td(_h: *mut c_void, _m: *const c_char) -> fuyubot_plugin_api::RequestId {
        fuyubot_plugin_api::RequestId::INVALID
    }
    unsafe extern "C" fn middleware(
        _h: *mut c_void,
        _s: u32,
        _c: Option<fuyubot_plugin_api::MiddlewareCallbackFn>,
        _u: *mut c_void,
    ) -> fuyubot_plugin_api::MiddlewareHandle {
        fuyubot_plugin_api::MiddlewareHandle::INVALID
    }
    unsafe extern "C" fn wait(
        _h: *mut c_void,
        _f: *const c_char,
        _t: u64,
    ) -> fuyubot_plugin_api::WaitHandle {
        fuyubot_plugin_api::WaitHandle::INVALID
    }
    unsafe extern "C" fn cancel(_h: *mut c_void, _w: fuyubot_plugin_api::WaitHandle) {}
    unsafe extern "C" fn execute(_h: *mut c_void, _r: *const c_char) -> *const c_char {
        core::ptr::null()
    }

    type VTable = fuyubot_plugin_api::HostApiVTable;

    /// 造一份假 VTable + 它背后的 `FakeHost`（返回的 Arc 要一直拿住）。
    fn fake_vtable() -> (HostApi, Arc<FakeHost>) {
        let fake = Arc::new(FakeHost::default());
        let vtable: &'static VTable = Box::leak(Box::new(VTable {
            version: fuyubot_plugin_api::ABI_VERSION,
            size: core::mem::size_of::<VTable>() as u32,
            host_ctx: Arc::as_ptr(&fake) as *mut c_void,
            on: Some(on),
            off: Some(off),
            emit: Some(emit),
            register_tool: Some(register_tool),
            call_tool: Some(call_tool),
            list_tools: Some(list_tools),
            provide_service: Some(provide),
            get_service: Some(get),
            td_invoke: Some(td),
            td_invoke_simplified: Some(invoke),
            td_execute: Some(execute),
            use_middleware: Some(middleware),
            wait_for_update: Some(wait),
            cancel_wait: Some(cancel),
            log: Some(log),
        }));
        // SAFETY: vtable 是 Box::leak 出来的，布局与 ABI 契约一致。
        let api = unsafe { HostApi::from_raw(vtable) }.expect("假宿主必须能被解包");
        (api, fake)
    }

    /// 把写出去的行收在内存里，好断言。
    #[derive(Default)]
    struct VecSink(Mutex<Vec<String>>);

    impl LineSink for VecSink {
        fn write_line(&self, line: &str) {
            self.0.lock().unwrap().push(line.to_owned());
        }
    }

    /// `Chat` 独占 sink，测试另留一份共享句柄看内容。
    struct SharingSink(Arc<VecSink>);

    impl LineSink for SharingSink {
        fn write_line(&self, line: &str) {
            self.0.write_line(line);
        }
    }

    fn shared() -> (Arc<Shared>, Arc<VecSink>, Arc<FakeHost>) {
        let (api, fake) = fake_vtable();
        let sink = Arc::new(VecSink::default());
        let chat = Arc::new(Chat {
            sink: Box::new(SharingSink(Arc::clone(&sink))),
        });
        (Arc::new(Shared::new(api, chat)), sink, fake)
    }

    #[test]
    fn every_op_reaches_the_host() {
        let (shared, sink, fake) = shared();

        shared.handle_line(r#"{"op":"log","level":"warn","message":"hello"}"#);
        shared.handle_line(r#"{"op":"on","tag":1,"event":"updateNewMessage","filter":{"a":1}}"#);
        // 订阅建好之后再投递（真实宿主就是这个顺序），事件要带订阅号。
        fake.fire(r#"{"@type":"updateNewMessage"}"#);
        shared.handle_line(
            r#"{"op":"invoke","tag":2,"method":"sendMessage","params":{"chat_id":1}}"#,
        );
        shared.handle_line(r#"{"op":"tool","descriptor":{"name":"demo"}}"#);
        shared.handle_line(r#"{"op":"off","handle":7}"#);

        assert_eq!(fake.subscriptions.load(Ordering::SeqCst), 1);
        assert_eq!(
            fake.unsubscriptions.load(Ordering::SeqCst),
            1,
            "off 必须让宿主的订阅真的退掉"
        );
        assert_eq!(fake.tools.load(Ordering::SeqCst), 1);
        assert!(
            fake.logs.lock().unwrap().iter().any(|line| line == "hello"),
            "{:?}",
            fake.logs.lock().unwrap()
        );
        let invokes = fake.invokes.lock().unwrap();
        assert_eq!(invokes.len(), 1);
        assert_eq!(invokes[0].0, "sendMessage");
        assert!(invokes[0].1.contains("chat_id"));

        let written = sink.0.lock().unwrap();
        // 订阅建立时宿主立刻回推了一条事件，脚本那侧要能收到；
        // **必须带上订阅号**，否则脚本分不清这条事件属于哪条订阅
        // （同一个事件可以有带不同过滤器的多条订阅）。
        assert!(
            written
                .iter()
                .any(|line| line.contains("\"op\":\"event\"") && line.contains("\"handle\":7")),
            "{written:?}"
        );
        // 每条带 tag 的请求都要有回执。
        assert!(
            written.iter().any(|line| line.contains("\"op\":\"on\"")),
            "{written:?}"
        );
        assert!(
            written
                .iter()
                .any(|line| line.contains("\"op\":\"invoked\"")),
            "{written:?}"
        );
    }

    #[test]
    fn bad_lines_answer_with_an_error_instead_of_dying() {
        let (shared, sink, _fake) = shared();

        shared.handle_line("not json at all");
        shared.handle_line(r#"{"op":"wat","tag":5}"#);
        shared.handle_line(r#"{"no_op":true}"#);

        let written = sink.0.lock().unwrap();
        assert_eq!(written.len(), 3, "{written:?}");
        assert!(written.iter().all(|line| line.contains("\"op\":\"error\"")));
        assert!(written[1].contains("\"tag\":5"), "{written:?}");
    }

    #[test]
    fn a_bad_filter_is_reported_instead_of_subscribing() {
        let (shared, sink, fake) = shared();
        // 过滤器必须是 JSON 对象；数组会被宿主的 on 拒绝。
        shared.handle_line(r#"{"op":"on","tag":9,"event":"update","filter":[1,2]}"#);
        assert_eq!(
            fake.subscriptions.load(Ordering::SeqCst),
            0,
            "被拒绝的订阅不该记成成功"
        );
        let written = sink.0.lock().unwrap();
        assert!(
            written.iter().any(|line| line.contains("\"op\":\"error\"")),
            "{written:?}"
        );
    }

    #[test]
    fn shutdown_unsubscribes_everything() {
        let (shared, _sink, fake) = shared();
        shared.handle_line(r#"{"op":"on","tag":1,"event":"update"}"#);
        shared.handle_line(r#"{"op":"on","tag":2,"event":"response"}"#);
        assert_eq!(fake.subscriptions.load(Ordering::SeqCst), 2);

        shared.shutdown();
        assert_eq!(
            fake.unsubscriptions.load(Ordering::SeqCst),
            2,
            "停插件必须把订阅全部退掉"
        );
    }

    #[test]
    fn startup_waits_for_the_scripts_ready() {
        let (shared, _sink, _fake) = shared();
        // 没 ready 之前不能放行：这正是「宿主已经分发更新、脚本还没订阅上」那个坑。
        assert_eq!(
            shared.wait_startup(Duration::from_millis(30)),
            Startup::Waiting
        );
        shared.handle_line(r#"{"op":"ready"}"#);
        assert_eq!(
            shared.wait_startup(Duration::from_millis(30)),
            Startup::Ready
        );
    }

    #[test]
    fn a_dead_script_unblocks_the_waiter() {
        let (shared, _sink, _fake) = shared();
        shared.mark_dead();
        assert_eq!(
            shared.wait_startup(Duration::from_secs(5)),
            Startup::Dead,
            "脚本死了要立刻醒，不能干等满超时"
        );

        // ready 之后再死不该把状态翻回 Waiting/Dead：那是「跑起来之后又退出」，
        // 由写管道失败去反映，不能把已经成功的启动说成失败。
        shared.mark_ready();
        shared.mark_dead();
        assert_eq!(
            shared.wait_startup(Duration::from_millis(10)),
            Startup::Ready
        );
    }

    #[test]
    fn script_path_follows_the_config() {
        assert_eq!(
            resolve_script(r#"{"script":"/tmp/a.py"}"#, "default.py"),
            std::path::PathBuf::from("/tmp/a.py")
        );
        assert_eq!(
            resolve_script("", "default.py"),
            std::path::PathBuf::from("default.py")
        );
        // 空串按「没配」处理，而不是把工作目录当成脚本。
        assert_eq!(
            resolve_script(r#"{"script":""}"#, "default.py"),
            std::path::PathBuf::from("default.py")
        );
    }

    /// 脚本那一侧的分发逻辑自带 `_selftest`：解释器在就跑一遍。
    ///
    /// 找不到解释器就**跳过**——自检不该让构建环境对 python/node 有硬依赖。
    #[test]
    fn the_script_helpers_pass_their_own_self_check() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        for (program, script) in [
            ("python", "python/fuyubot_plugin.py"),
            ("node", "js/fuyubot_plugin.js"),
        ] {
            let path = dir.join(script);
            let output = match std::process::Command::new(program).arg(&path).output() {
                Ok(output) => output,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    eprintln!("跳过 {program} 自检：这台机器上找不到它");
                    continue;
                }
                Err(e) => panic!("起不了 {program}: {e}"),
            };
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                output.status.success(),
                "{program} 自检失败：\n{stdout}\n{stderr}"
            );
            assert!(
                stdout.contains("self-test ok"),
                "{program}: {stdout}{stderr}"
            );
        }
    }
}
