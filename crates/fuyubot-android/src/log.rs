//! 日志落盘：把 stderr 接到 `<root>/logs/fuyubot.log`，再装一个朴素订阅者。
//!
//! **为什么必须重定向**：Android 应用进程的 stdout / stderr 是 `/dev/null`，
//! 而 `tracing` 的订阅者默认写 stderr——不接走的话，日志、TDLib 的抱怨、panic 信息
//! 全部消失。CLI 的 `logging::init` 是私有的、格式也偏终端，所以这里自己装一个
//! 更低要求的：不带颜色、不带时间戳以外的花活（时间戳由 `fmt` 默认给）。
//!
//! 刻意**不链 `liblog`**：为几行日志拉一个系统库不值，`DT_NEEDED` 只剩
//! `libc / libm`，打包校验也好写。

use std::path::Path;

/// 把 fd 2 接到 `path`（追加、不存在就建）。
///
/// 失败不致命：继续用原来的 stderr（桌面上看得见，Android 上=看不见，但至少不挂）。
pub fn redirect_stderr(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;

        // SAFETY: 声明的是 libc 的 dup2；两个参数都是本进程里有效的 fd。
        unsafe extern "C" {
            fn dup2(old: std::os::raw::c_int, new: std::os::raw::c_int) -> std::os::raw::c_int;
        }

        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            Ok(file) => {
                // SAFETY: `file` 在这一行期间有效；dup2 把它的 fd 复制到 2。
                // 之后 `file` drop 只关它自己那个 fd，2 仍然指着同一个打开的文件。
                unsafe {
                    dup2(file.as_raw_fd(), 2);
                }
            }
            // 这条错误要打在**原来的** stderr 上（Android 上是 /dev/null，桌面看得见）。
            Err(error) => eprintln!("打不开日志 {}：{error}", path.display()),
        }
    }
    #[cfg(not(unix))]
    {
        // Windows 上本 crate 只为「能编译、能被 cargo build --workspace 检查」而存在，
        // 真正的产物只有 Android。不折腾 fd 重定向。
        let _ = path;
    }
}

/// 装全局订阅者。重复调用（测试、或者宿主自己已经装过）只保留第一个。
///
/// ⚠️ **必须放到 TRACE**：`tracing_subscriber::fmt()` 的默认上限是 `INFO`
/// （`Subscriber::DEFAULT_MAX_LEVEL`），不放开的话 `log_level = "debug" / "trace"`
/// 在插件侧被 `HostRuntime::set_log_verbosity` 放行了，却会在订阅者这层被丢掉，
/// `fuyubot.log` 里永远看不到 DEBUG / TRACE。CLI 的 `logging::init` 同样不挂级别过滤
/// （那边写明了「级别是 HostRuntime 的事」），这里对齐。
///
/// 噪音不用担心：宿主的 `trace` 事件只有 `--trace-updates` 打开时才有，Android 不开；
/// 插件日志仍由运行时的门槛在**源头**挡。
pub fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(std::io::stderr)
        .try_init();
}
