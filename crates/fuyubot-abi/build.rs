//! 生成 `fuyubot_plugin.h`，供 C/C++/Zig 插件作者使用。
//!
//! 头文件里除了 cbindgen 生成的类型，还有三段**手写**内容（契约文档，不是代码）：
//! 内存所有权、插件导出符号声明、以及「本 ABI 为什么没有可变参数函数指针」。
//! 它们都从这里注入，改契约只改这一处。

/// 文件开头的契约说明。
const HEADER: &str = r#"/**
 * fuyubot 插件 ABI（C 侧视图）。
 *
 * 本文件由 cbindgen 从 fuyubot-abi/src/lib.rs 生成，**不要手改**：
 * 改 ABI 请改 Rust 定义，然后重新构建（build.rs 会重新生成）。
 *
 * ## 内存所有权（只有三条，全部是硬规矩）
 *
 * 1. **谁分配谁释放。** 插件通过 `plugin_init` 返回的 `name`、以及插件自己
 *    分配的任何字符串，都由插件持有；宿主绝不会 free 它们。插件若要给出一个
 *    临时的、需要宿主接管的字符串，只能走 `plugin_free_string` 那套约定。
 * 2. **宿主返回的字符串是线程局部缓冲。** `list_tools` 与
 *    `WaitHandle::payload_json` 指向宿主的一块复用缓冲，
 *    **只在下一次调用宿主函数之前有效**。要留着就必须当场拷走，
 *    并且**不要试图释放它**——宿主 VTable 里没有 host 侧的 free 函数，
 *    这是设计，不是遗漏。
 * 3. **`get_service` 返回的不是缓冲**，而是提供方注册时给的那个
 *    `ServiceVTable` 指针：在服务被注销之前一直有效，可以放心留着
 *    （但它指向的内存属于提供方，不要写）。
 * 4. **`ServiceMethodFn` 的返回值由提供方持有。** 调用方读完即弃。
 *
 * ## 版本协商
 *
 * `HostApiVTable::version` 必须等于 FUYUBOT 的 `ABI_VERSION`（本文件里的 `#define`）。首个字段就是它，
 * 所以插件可以在读其它任何字段之前先比一次；`size` 用来判断宿主的表够不够大
 * （老宿主 + 新插件：够小就先别读尾部字段，读越界就是 UB）。
 * 插件自己声明的版本写在 `plugin_init` 返回的 `PluginHandle::abi_version` 里。
 *
 * ## 为什么这里没有可变参数函数指针
 *
 * Rust 1.99 的 `extern "C" fn(.., ...)` 已经稳定（可以写、cbindgen 也能生成），
 * 但**本 ABI 一个都没用**：`...` 会把参数类型和个数校验全部丢掉，而 VTable 里
 * 15 个函数的参数都是固定的，用 `...` 只会让 C 侧传错参数从编译错误降级成运行期 UB。 * 如果将来真要加，正确的 C 声明形状是：
 *
 *     typedef void (*FuyubotLogFn)(uint32_t level, const char *format, ...);
 *     // Rust 侧对应：unsafe extern "C" fn(level: u32, format: *const c_char, ...)
 *
 * 注意两点：变参部分**不能**是虚参之后的唯一参数（C 要求至少一个具名参数），
 * 而且变参函数指针**跨不了 ABI 的 `Option` 优化**——`Option<fn(..., ...)>` 不是
 * niche 优化，占两个指针宽，必须显式判空。
 */"#;

/// 放在 include 之后：可见性宏 + 回调 ABI 的提醒。
///
/// **不能把导出符号声明放这里**：cbindgen 把 `after_includes` 插在
/// `#include` 之后、**所有类型定义之前**，那里 `PluginHandle` 还没声明，
/// `-fsyntax-only` 会报「identifier 语法错误」。声明只能放 `trailer`。
const AFTER_INCLUDES: &str = r#"/**
 * 插件导出符号用的可见性宏。
 *
 * 生成动态库时必须让这 4 个符号可见：Windows 上靠 `__declspec(dllexport)`，
 * 类 Unix 系统默认全导出（这里留空）。宿主（比如 C++ 写的）要包含本头文件时，
 * 先自己定义 `FUYUBOT_EXPORT` 成 `__declspec(dllimport)` 之类即可。
 */
#ifndef FUYUBOT_EXPORT
#  if defined(_WIN32) || defined(_WIN64)
#    define FUYUBOT_EXPORT __declspec(dllexport)
#  else
#    define FUYUBOT_EXPORT
#  endif
#endif

/**
 * 回调必须是**能 unwind** 的 ABI：Rust 侧声明为 `extern "C-unwind"`。
 * 对 C/C++ 而言它与 `extern "C"` 完全一致；区别只在于用 Rust 写宿主时，
 * `extern "C"` 边界上逃出去的 panic 会直接 abort 整个进程，宿主来不及用
 * catch_unwind 接住。所以插件回调里抛出的 C++ 异常同样会穿过这个边界——
 * 请在回调内部处理完异常再返回。
 */"#;

/// 文件末尾：插件必须导出的符号（这里所有类型都已经声明好了）。
const TRAILER: &str = r#"/**
 * 插件必须导出的符号（C++ 下请放在 `extern "C"` 块里）。
 *
 * 名字与 arity 是 ABI 定死的，宿主按这些名字 dlsym。
 * `plugin_start` / `plugin_stop` 可以省略（只做元数据登记的插件不写），
 * `plugin_init` 和 `plugin_free_string` 必须有。
 */
FUYUBOT_EXPORT PluginHandle plugin_init(const HostApiVTable *host);
FUYUBOT_EXPORT int32_t plugin_start(PluginHandle handle, const char *config_json);
FUYUBOT_EXPORT void plugin_stop(PluginHandle handle);
FUYUBOT_EXPORT void plugin_free_string(char *ptr);"#;

fn main() {
    let mut config = cbindgen::Config::default();
    config.language = cbindgen::Language::C;
    config.include_guard = Some("FUYUBOT_PLUGIN_H".to_owned());
    config.header = Some(HEADER.to_owned());
    config.after_includes = Some(AFTER_INCLUDES.to_owned());
    config.trailer = Some(TRAILER.to_owned());
    // cbindgen 默认只导出被函数引用到的类型，这里显式列出 ABI 的全部公开面；
    // 新增公开 ABI 类型时必须同步加进这个列表，否则头文件里不会出现。
    config.export.include = [
        "ABI_VERSION",
        "LogLevel",
        "Stage",
        "SubscriptionHandle",
        "ToolHandle",
        "ServiceHandle",
        "MiddlewareHandle",
        "RequestId",
        "WaitHandle",
        "EventCallbackFn",
        "MiddlewareCallbackFn",
        "ServiceMethodFn",
        "ServiceMethod",
        "ServiceVTable",
        "HostApiVTable",
        "PluginHandle",
        "PluginInitFn",
        "PluginStartFn",
        "PluginStopFn",
        "PluginFreeStringFn",
    ]
    .iter()
    .map(|name| (*name).to_owned())
    .collect();

    // 解析源文件而非 `with_crate`：不跑 cargo metadata，构建不依赖网络与依赖图。
    let bindings = cbindgen::Builder::new()
        .with_config(config)
        .with_src("src/lib.rs")
        .generate()
        .expect("cbindgen 生成头文件失败");

    let out = std::path::Path::new(&std::env::var("OUT_DIR").unwrap()).join("fuyubot_plugin.h");
    bindings.write_to_file(out);
    println!("cargo:rerun-if-changed=src/lib.rs");
}
