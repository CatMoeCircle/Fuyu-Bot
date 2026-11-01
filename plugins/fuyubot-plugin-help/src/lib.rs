//! 内置插件：`/help` 列出可用命令。
//!
//! 命令表现在是**动态**的：任何插件都能在 `start()` 里经进程级共享服务
//! `fuyubot.help.commands` 的 `register` 方法，把自己的命令登记进 `/help`，
//! 不必改这个插件半行代码。静态的 `BUILTIN_COMMANDS` 只兜住三个内置命令，
//! 其余命令由各插件自行为自己注册。
//!
//! 这是插件之间「直接调用彼此能力」的标准桥（`provide_service_shared` +
//! `call_service`，见 `fuyubot-abi` 的 `ServiceVTable`）：help 插件把命令注册表
//! 暴露成一个服务，别的插件按名字调它的 `register` 方法，当场拿到 `{"ok":true}`。

use std::cell::RefCell;
use std::ffi::{CStr, CString, c_char, c_void};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::thread_local;

use fuyubot_abi::{HostApi, LogLevel, Plugin, ServiceMethod, ServiceVTable, define_plugin};
use fuyubot_simplify::{
    Command, CommandSubscription, SendMessageParams, Simplify, Text, on_command,
};
use serde_json::{Value, json};

/// 动态命令注册表：名字 → 说明。`/help` 渲染时和静态基线合并去重后展示。
///
/// 它藏在 help 插件自己的动态库里（其它插件看不见这个符号），只通过下面的共享服务暴露出去。
static REGISTRY: OnceLock<Mutex<Vec<(String, String)>>> = OnceLock::new();

fn registry() -> &'static Mutex<Vec<(String, String)>> {
    REGISTRY.get_or_init(|| Mutex::new(Vec::new()))
}

/// 把一条命令塞进注册表（按名字去重：已存在就更新说明）。
fn register_command(name: &str, description: &str) {
    if name.is_empty() {
        return;
    }
    let mut guard = registry().lock().expect("help 注册表被毒化");
    match guard.iter_mut().find(|(n, _)| n == name) {
        Some(entry) => entry.1 = description.to_owned(),
        None => guard.push((name.to_owned(), description.to_owned())),
    }
}

/// 进程级共享服务名：其它插件经它把自己的命令登记进 `/help`。
const SERVICE_NAME: &str = "fuyubot.help.commands";

/// 静态兜底的命令（内置三件套）。动态注册表里不该再出现这些名字，
/// 但 `collect_commands` 会按名字去重，所以重复也无害。
const BUILTIN_COMMANDS: &[(&str, &str)] = &[
    ("start", "开始使用"),
    ("help", "这条帮助"),
    ("plugins", "看看装了哪些插件"),
];

// 服务方法返回给调用方的字符串由**提供方持有**：这里用线程局部缓冲，调用方读完即弃。
// 宿主侧 `call_service` 同步调用并立刻把结果拷走，所以每个线程的缓冲在下次同线程调用前
// 都不会被读到的那一份覆盖——安全。
thread_local! {
    static RESPONSE: RefCell<CString> = RefCell::new(CString::new("").unwrap());
}

/// 把一段文本放进线程局部缓冲并返回指针（调用方须在读走前不再调同名方法）。
fn reply(text: &str) -> *const c_char {
    // 说明里理论不该含 NUL；保险起见坏字符兜底成空串。
    let c = match CString::new(text) {
        Ok(c) => c,
        Err(_) => CString::new("").unwrap(),
    };
    RESPONSE.with(|buf| {
        *buf.borrow_mut() = c;
        buf.borrow().as_ptr()
    })
}

/// 服务方法 `register`：入参 `{"name": "...", "description": "..."}`。
///
/// 其它插件在自己的 `start()` 里调它登记命令；成功回 `{"ok":true}`，入参非法回 `{"error":"…"}`。
unsafe extern "C" fn svc_register(
    _context: *mut c_void,
    input_json: *const c_char,
) -> *const c_char {
    let input = match unsafe { CStr::from_ptr(input_json) }.to_str() {
        Ok(s) => s,
        Err(_) => return reply(r#"{"error":"非 UTF-8 入参"}"#),
    };
    let value: Value = match serde_json::from_str(input) {
        Ok(v) => v,
        Err(_) => return reply(r#"{"error":"不是合法 JSON"}"#),
    };
    let name = match value.get("name").and_then(Value::as_str) {
        Some(n) if !n.is_empty() => n,
        _ => return reply(r#"{"error":"缺 name 或为空"}"#),
    };
    let description = value
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    register_command(name, &description);
    reply(r#"{"ok":true}"#)
}

/// 服务方法 `list`：回 `[{"name":"…","description":"…"}, …]`（静态 + 动态合并去重）。
unsafe extern "C" fn svc_list(_context: *mut c_void, _input_json: *const c_char) -> *const c_char {
    let commands = collect_commands();
    let body = serde_json::to_string(&commands).unwrap_or_else(|_| "[]".to_owned());
    reply(&body)
}

/// 合并静态基线与动态注册表，按名字去重，静态优先、各自保持原顺序。
fn collect_commands() -> Vec<Value> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    let mut push = |name: &str, description: &str| {
        if seen.insert(name.to_owned()) {
            out.push(json!({ "name": name, "description": description }));
        }
    };
    for (name, description) in BUILTIN_COMMANDS {
        push(name, description);
    }
    let guard = registry().lock().expect("help 注册表被毒化");
    for (name, description) in guard.iter() {
        push(name, description);
    }
    out
}

/// 把 `ServiceVTable` 与其指向的名字 / 方法数组打包在一起，保证后者活得一样久。
///
/// 注册进宿主的是 `vtable` 的裸指针，而它的 `name` / `methods` / 各方法 `name`
/// 都指向同结构体里的 `names` 与 `methods` 缓冲——只要 `HelpService` 活着就有效。
/// 插件卸载时 `stop()` 把 `Box` 放下，缓冲随之释放（届时宿主已回滚该服务注册）。
struct HelpService {
    vtable: ServiceVTable,
    // 下面两个字段从不被代码「读」，但它们的缓冲被 `vtable` 的裸指针引用，
    // 必须随 `HelpService` 一起活着——靠持有而非读取来保持存活，故跳过 dead_code。
    #[allow(dead_code)]
    methods: Vec<ServiceMethod>,
    #[allow(dead_code)]
    names: Vec<CString>,
}

// `ServiceVTable` / `ServiceMethod` 含裸指针，Rust 默认判为 `!Send + !Sync`。
// 但这里它只是「指针 + 进程级只读数据（CString 缓冲、函数代码）」的打包，
// 在进程内任意线程间移动 / 共享都安全——宿主经函数指针调 `svc_register` / `svc_list`，
// 这两个函数只碰 `Mutex` 保护的 `REGISTRY`。手动标注。
unsafe impl Send for HelpService {}
unsafe impl Sync for HelpService {}

/// 构造并自持有 `fuyubot.help.commands` 的服务表。
fn build_service() -> Box<HelpService> {
    let name_service = CString::new(SERVICE_NAME).expect("服务名不该含 NUL");
    let name_register = CString::new("register").expect("方法名不该含 NUL");
    let name_list = CString::new("list").expect("方法名不该含 NUL");
    // 顺序固定：names[0]=服务名，names[1]=register，names[2]=list。
    let names = vec![name_service, name_register, name_list];
    let methods = vec![
        ServiceMethod {
            name: names[1].as_ptr(),
            func: Some(svc_register),
        },
        ServiceMethod {
            name: names[2].as_ptr(),
            func: Some(svc_list),
        },
    ];
    let vtable = ServiceVTable {
        name: names[0].as_ptr(),
        version: 1,
        size: core::mem::size_of::<ServiceVTable>() as u32,
        context: core::ptr::null_mut(),
        methods: methods.as_ptr(),
        method_count: methods.len(),
    };
    Box::new(HelpService {
        vtable,
        methods,
        names,
    })
}

struct HelpPlugin {
    host: HostApi,
    subscription: Option<CommandSubscription>,
    /// 还在跑的异步发送（`stop()` 里 join 掉：卸载之后不能再跑库里的代码）。
    sending: Arc<Mutex<Vec<JoinHandle<()>>>>,
    /// 持有的服务表；`stop()` 里放下即释放（与宿主回滚注册同步）。
    service: Option<Box<HelpService>>,
}

impl Plugin for HelpPlugin {
    fn init(host: HostApi) -> Result<Self, String> {
        Ok(Self {
            host,
            subscription: None,
            sending: Arc::new(Mutex::new(Vec::new())),
            service: None,
        })
    }

    fn start(&mut self, _config_json: &str) -> Result<(), String> {
        let host = self.host;

        // 1) 把命令注册表暴露成进程级共享服务，让其它插件能 `call_service` 进来。
        let service = build_service();
        let handle = host.provide_service_shared(SERVICE_NAME, &service.vtable);
        if !handle.is_valid() {
            host.log(
                LogLevel::Error,
                "help 插件注册 fuyubot.help.commands 共享服务失败：其它插件将无法登记命令",
            );
        }
        self.service = Some(service);

        // 2) 先把三个内置命令塞进注册表（避免「/help 不显示自己」这种自举真空）。
        for (name, description) in BUILTIN_COMMANDS {
            register_command(name, description);
        }

        // 3) 订阅 /help。
        let sending = Arc::clone(&self.sending);
        let subscription = on_command(host, move |command: &Command| {
            if !command.is("help") {
                return;
            }
            let params = SendMessageParams {
                text: Some(Text::markdown(help_text())),
                reply_to_message_id: Some(command.message_id),
                ..SendMessageParams::default()
            };
            // 命令回调跑在接收循环线程上：用 then，别在回调里等结果。
            let handle =
                Simplify::new(host).send_message_then(command.chat_id, &params, move |result| {
                    if let Err(error) = result {
                        host.log(LogLevel::Error, &format!("help 插件回复失败: {error}"));
                    }
                });
            if let Ok(mut sending) = sending.lock() {
                sending.push(handle);
            }
        });
        let subscription = subscription.ok_or_else(|| "订阅 updateNewMessage 失败".to_owned())?;
        self.subscription = Some(subscription);
        Ok(())
    }

    fn stop(&mut self) {
        self.subscription = None;
        self.service = None;
        let mut sending = self
            .sending
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        for handle in sending.drain(..) {
            let _ = handle.join();
        }
    }
}

/// 帮助文本（Markdown：命令用 `/x`，粗体是 `*x*`）。
fn help_text() -> String {
    let mut text = String::from("*可用命令*\n");
    for command in collect_commands() {
        let name = command.get("name").and_then(Value::as_str).unwrap_or("?");
        let description = command
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("");
        text.push_str(&format!("/{name} — {description}\n"));
    }
    text
}

define_plugin!(HelpPlugin, "fuyubot.help");
