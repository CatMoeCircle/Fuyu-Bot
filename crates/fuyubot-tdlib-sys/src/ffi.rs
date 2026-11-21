//! TDLib C ABI 的函数指针类型与符号名。
//!
//! 库路径运行时才确定，所以这里只给类型与名字，符号解析在
//! `fuyubot-tdlib` 的 `loader::resolve_symbols` 里做。
//!
//! ## 两套 API 不要混（真库把这个坑埋得很深）
//!
//! tdjson 同时导出**两套**接口：
//!
//! - **per-client（本 crate 用的）**：`td_json_client_create` 返回一个句柄，
//!   `td_json_client_send/receive/execute/destroy` 都带上它。
//! - **多客户端**：`td_create_client_id()` 给一个 **int** id，`td_send(id, request)`、
//!   `td_receive(timeout)`（**没有**句柄参数）、`td_execute(request)`。
//!
//! 两套都在，所以**写错名字不会解析失败**，只会在运行时静默错投：
//! `td_send` 的第一个参数是 `int32 client_id`，把我们那个指针的低 32 位当 id 用，
//! TDLib 回一条 `400 Invalid TDLib instance specified`——签名照样「对得上」，
//! 只有真库能暴露。`td_receive(timeout)` 少一个参数更隐蔽：超时从没被真正传进去。

use core::ffi::{c_char, c_void};

pub type TdJsonClientCreateFn = unsafe extern "C" fn() -> *mut c_void;
pub type TdJsonClientDestroyFn = unsafe extern "C" fn(*mut c_void);
pub type TdSendFn = unsafe extern "C" fn(*mut c_void, *const c_char);
pub type TdReceiveFn = unsafe extern "C" fn(*mut c_void, f64) -> *const c_char;
pub type TdExecuteFn = unsafe extern "C" fn(*mut c_void, *const c_char) -> *const c_char;

/// 日志详细度。**这是全局设置**（TDLib 的内部静态状态），不带 client 句柄，
/// 而且**可以在建客户端之前就调**——所以它是「默认日志等级」的正确落点。
pub type TdSetLogVerbosityFn = unsafe extern "C" fn(i32);

/// TDLib 内部日志回调：`(verbosity_level, message)`。
///
/// `message` 是以 NUL 结尾的 UTF-8 字符串，**只在回调期间有效**（TDLib 随后就能
/// 复用那块缓冲），回调里必须立刻拷走。
pub type TdLogMessageCallbackFn = unsafe extern "C" fn(i32, *const c_char);

/// 安装 / 替换 TDLib 的内部日志回调；传 `None` 恢复默认（stderr）。
///
/// `max_verbosity_level` 与 `td_set_log_verbosity_level` 是同一套数字：
/// 0 致命 / 1 错误 / 2 警告 / 3 信息 / 4 调试 / 5 详细调试。
///
/// ⚠️ 回调是**追加**在默认日志流之后的：TDLib 先写默认流（stderr / 文件），再调
/// 回调（`tdutils/td/utils/logging.cpp` 的 `LogInterface::append`）。所以要真正止住
/// TDLib 往终端写，还得把默认流换成 `logStreamEmpty`——两步在 `fuyubot-tdlib::log`
/// 里一起做，详见那边的模块文档。
pub type TdSetLogMessageCallbackFn = unsafe extern "C" fn(i32, Option<TdLogMessageCallbackFn>);

pub const SYM_CREATE: &str = "td_json_client_create";
pub const SYM_DESTROY: &str = "td_json_client_destroy";
pub const SYM_SEND: &str = "td_json_client_send";
pub const SYM_RECEIVE: &str = "td_json_client_receive";
pub const SYM_EXECUTE: &str = "td_json_client_execute";
pub const SYM_SET_LOG_VERBOSITY: &str = "td_set_log_verbosity_level";
/// 可选符号：老的 / 精简的 tdjson 可能没导出它。
pub const SYM_SET_LOG_MESSAGE_CALLBACK: &str = "td_set_log_message_callback";

/// 默认日志详细度：**1**。
///
/// TDLib 自己的默认值更啰嗦（连 `Begin to wait for updates with timeout…` 这种
/// 每轮轮询的 internal 日志都会打到 stderr，`[ 3]` 那一堆），对「跑起来看结果」是纯噪音。
/// 1 保留警告与错误，够定位问题；要更详细就把 `[tdlib] log_verbosity` 调大。
pub const DEFAULT_LOG_VERBOSITY: i32 = 1;

/// 已解析的一组符号。
///
/// 用结构体而不是裸 `extern "C"` 声明：库在运行时才加载，且测试可以注入替代实现。
#[derive(Clone, Copy)]
pub struct TdApi {
    pub create: TdJsonClientCreateFn,
    pub destroy: TdJsonClientDestroyFn,
    pub send: TdSendFn,
    pub receive: TdReceiveFn,
    pub execute: TdExecuteFn,
    pub set_log_verbosity: TdSetLogVerbosityFn,
    /// 可选：拿不到时 TDLib 的日志继续由它自己写 stderr（老库 / 精简构建）。
    pub set_log_message_callback: Option<TdSetLogMessageCallbackFn>,
}

// C ABI 两侧的指针形状必须一致；不一致的话这里直接编译不过。
const _: () = {
    assert!(core::mem::size_of::<*const c_char>() == core::mem::size_of::<*mut c_void>());
    assert!(core::mem::align_of::<*const c_char>() == core::mem::align_of::<*mut c_void>());
    assert!(core::mem::size_of::<*mut c_void>() == core::mem::size_of::<usize>());
    assert!(core::mem::size_of::<TdApi>() == 7 * core::mem::size_of::<usize>());
};
