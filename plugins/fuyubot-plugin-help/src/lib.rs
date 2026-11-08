//! 内置插件：`/help` 列出可用命令。
//!
//! 命令表现在是**动态**的：任何插件都能在 `start()` 里经进程级共享服务
//! `fuyubot.help.commands` 的 `register` 方法，把自己的命令登记进 `/help`，
//! 不必改这个插件半行代码。静态的 `BUILTIN_COMMANDS` 只兜住三个内置命令，
//! 其余命令由各插件自行为自己注册。
//!
//! 每条命令还带**归属插件**：服务方法里回问宿主 `HostApi::current_plugin()`，
//! 由宿主按本线程作用域栈判定（与 `set_config` 的越权判定同一份映射），所以是
//! 权威来源、插件伪造不了。`register` 的返回与 `list` 数组里都带 `plugin` 字段。
//!
//! 这是插件之间「直接调用彼此能力」的标准桥（`provide_service_shared` +
//! `call_service`，见 `fuyubot-abi` 的 `ServiceVTable`）：help 插件把命令注册表
//! 暴露成一个服务，别的插件按名字调它的 `register` 方法，当场拿到
//! `{"ok":true,"plugin":"…"}`。

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

/// 动态命令注册表。`/help` 渲染时和静态基线合并去重后展示。
///
/// 它藏在 help 插件自己的动态库里（其它插件看不见这个符号），只通过下面的共享服务暴露出去。
static REGISTRY: OnceLock<Mutex<Vec<CommandEntry>>> = OnceLock::new();

/// 注册表里的一条：命令名、说明，以及**登记它的插件名**。
///
/// `plugin` 是宿主判定的（[`caller`]），`None` 表示宿主自己 / 名字还没登记——
/// 不是「插件自报为空」，两者都不该在 `/help` 里编造归属。
struct CommandEntry {
    name: String,
    description: String,
    plugin: Option<String>,
}

fn registry() -> &'static Mutex<Vec<CommandEntry>> {
    REGISTRY.get_or_init(|| Mutex::new(Vec::new()))
}

/// 把一条命令塞进注册表（按名字去重：已存在就更新说明与归属）。
fn register_command(name: &str, description: &str, plugin: Option<&str>) {
    if name.is_empty() {
        return;
    }
    let mut guard = registry().lock().expect("help 注册表被毒化");
    match guard.iter_mut().find(|entry| entry.name == name) {
        Some(entry) => {
            entry.description = description.to_owned();
            entry.plugin = plugin.map(str::to_owned);
        }
        None => guard.push(CommandEntry {
            name: name.to_owned(),
            description: description.to_owned(),
            plugin: plugin.map(str::to_owned),
        }),
    }
}

/// 进程级共享服务名：其它插件经它把自己的命令登记进 `/help`。
const SERVICE_NAME: &str = "fuyubot.help.commands";

/// 静态兜底的命令（内置三件套），只在对应插件**没有**动态登记时补位。
///
/// 基线里没有来源（`plugin`），所以渲染时不标归属——这是「不知道就别编」，
/// 而不是替 start / manage 声明它们。
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

/// 问宿主「现在是谁在调我」：`context` 是 `build_service` 存进去的那份 `HostApi`。
///
/// 返回的是**权威**来源（宿主按本线程作用域栈判定），不是入参里自报的字段。
/// context 为空（老宿主 / 直接调用）时返回 `None`，`/help` 就不标归属。
fn caller(context: *mut c_void) -> Option<String> {
    if context.is_null() {
        return None;
    }
    // SAFETY: `build_service` 把 `vtable.context` 指向 `HelpService` 自持的
    // `Box<HostApi>`，只要 `HelpService` 活着就一直有效（宿主回滚注册先于它被放下）。
    let host = unsafe { &*(context as *const HostApi) };
    host.current_plugin()
}

/// 服务方法 `register`：入参 `{"name": "...", "description": "..."}`。
///
/// 其它插件在自己的 `start()` 里调它登记命令；成功回
/// `{"ok":true,"plugin":"登记者插件名"}`（宿主问不到身份时 `plugin` 是 `null`），
/// 入参非法回 `{"error":"…"}`。
unsafe extern "C" fn svc_register(
    context: *mut c_void,
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
    // 归属由宿主判，不看入参——插件没法伪造成别人。
    let plugin = caller(context);
    register_command(name, &description, plugin.as_deref());
    let body = match &plugin {
        Some(plugin) => json!({ "ok": true, "plugin": plugin }),
        None => json!({ "ok": true, "plugin": Value::Null }),
    };
    reply(&body.to_string())
}

/// 服务方法 `list`：回 `[{"name":"…","description":"…","plugin":"…"}, …]`。
///
/// 动态登记优先（带宿主判定的 `plugin`），静态基线只补缺（无 `plugin` 字段）。
unsafe extern "C" fn svc_list(_context: *mut c_void, _input_json: *const c_char) -> *const c_char {
    let commands = collect_commands();
    let body = serde_json::to_string(&commands).unwrap_or_else(|_| "[]".to_owned());
    reply(&body)
}

/// 合并静态基线与动态注册表，按名字去重，**动态优先**、各自保持原顺序。
///
/// 动态优先的理由是来源：动态项带宿主判定的 `plugin`，比硬编码基线可信；
/// 基线只补「插件没登记」的缺（比如某个内置插件没装）。
fn collect_commands() -> Vec<Value> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    {
        let guard = registry().lock().expect("help 注册表被毒化");
        for entry in guard.iter() {
            if !seen.insert(entry.name.clone()) {
                continue;
            }
            let mut value = json!({
                "name": &entry.name,
                "description": &entry.description,
            });
            if let Some(plugin) = &entry.plugin {
                value["plugin"] = json!(plugin);
            }
            out.push(value);
        }
    }
    for (name, description) in BUILTIN_COMMANDS {
        if seen.insert((*name).to_owned()) {
            out.push(json!({ "name": name, "description": description }));
        }
    }
    out
}

/// 把 `ServiceVTable` 与其指向的名字 / 方法数组打包在一起，保证后者活得一样久。
///
/// 注册进宿主的是 `vtable` 的裸指针，而它的 `name` / `methods` / 各方法 `name`
/// 都指向同结构体里的 `names` 与 `methods` 缓冲——只要 `HelpService` 活着就有效。
/// 插件卸载时 `stop()` 把 `Box` 放下，缓冲随之释放（届时宿主已回滚该服务注册）。
struct HelpService {
    /// 服务方法回问宿主「谁在调我」用的主机句柄；`vtable.context` 指向它。
    ///
    /// 单独 `Box` 一层是为了地址稳定：`HelpService` 本身会在 `Box` 里移动，
    /// 但内部这个 `Box` 指向的堆地址不随外层移动改变。
    #[allow(dead_code)]
    host: Box<HostApi>,
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
// 这两个函数只碰 `Mutex` 保护的 `REGISTRY` 与宿主的线程局部身份。手动标注。
unsafe impl Send for HelpService {}
unsafe impl Sync for HelpService {}

/// 构造并自持有 `fuyubot.help.commands` 的服务表。
///
/// `host` 存进 `context`：服务方法被调用时用它回问「当前调用者是谁」。
fn build_service(host: HostApi) -> Box<HelpService> {
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
    // 先装箱再取地址：`Box<HostApi>` 的堆地址不会因为外层 `HelpService` 搬家而变。
    let host = Box::new(host);
    let vtable = ServiceVTable {
        name: names[0].as_ptr(),
        version: 1,
        size: core::mem::size_of::<ServiceVTable>() as u32,
        context: (&*host as *const HostApi).cast_mut().cast::<c_void>(),
        methods: methods.as_ptr(),
        method_count: methods.len(),
    };
    Box::new(HelpService {
        host,
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
        //    服务表带着 `host`：方法被调用时要回问宿主「谁在登记」。
        let service = build_service(host);
        let handle = host.provide_service_shared(SERVICE_NAME, &service.vtable);
        if !handle.is_valid() {
            host.log(
                LogLevel::Error,
                "help 插件注册 fuyubot.help.commands 共享服务失败：其它插件将无法登记命令",
            );
        }
        self.service = Some(service);

        // 2) 登记 /help 自己（其它命令由各插件经上面的服务登记）。来源写死自己：
        //    这条不走服务调用，问不到「调用者」，但 help 确实就是它的提供方。
        register_command("help", "这条帮助", Some("fuyubot.help"));

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
        match command.get("plugin").and_then(Value::as_str) {
            // 归属显示短名（`fuyubot.start` → `start`），完整名留在 `list` 里给机器读。
            Some(plugin) => {
                let short = plugin.rsplit('.').next().unwrap_or(plugin);
                text.push_str(&format!("/{name} — {description}（来自 {short}）\n"));
            }
            None => text.push_str(&format!("/{name} — {description}\n")),
        }
    }
    text
}

define_plugin!(HelpPlugin, "fuyubot.help");

#[cfg(test)]
mod tests {
    use super::*;

    /// 没有 context（老宿主 / 宿主自己直接调）时，不去编造归属。
    #[test]
    fn caller_without_context_is_none() {
        assert!(caller(core::ptr::null_mut()).is_none());
    }
}
