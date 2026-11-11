//! fuyubot 命令行入口（**运行时** CLI）。
//!
//! 这里不实现任何宿主函数：`HostApiVTable` 的实现全在 `fuyubot-loader` 里，
//! 本文件只负责把内核、宿主运行时、插件目录接起来。
//!
//! **插件开发的事不在这里**：没有插件开发 CLI，写法与示例都在文档站（见 `docs/decisions.md` 变更 12）。
//!
//! ```text
//! fuyubot                                   不带子命令 = 直接开**终端操作面板**（同 `run --tui`）
//! fuyubot init                              生成 fuyubot.toml + plugins/
//! fuyubot run [--background]                起框架、加载插件（后台运行 = 日志进 logs/fuyubot.log）
//! fuyubot login                             只登录：建 client → 走完授权 → 打印账户 → 退出（不加载插件）
//! fuyubot logout [--yes]                    清掉本地 TDLib 会话目录（默认只打印将要删什么）
//! fuyubot status                            看这个目录上的实例在不在跑
//! fuyubot run --tui                          终端操作面板（版本栏 + 导航 + 工作区 + 资源占用）
//! fuyubot config                            打印**生效的**配置值（含默认值）
//! fuyubot stop                              停掉它
//! fuyubot plugin list                       列出插件目录里的插件
//! fuyubot plugin install <路径>             把一个插件装进插件目录
//! ```
//!
//! **本 CLI 不编译、不打包插件**（`docs/decisions.md` 变更 4/5）：插件由作者自己的语言工具链编，
//! 编好之后 `fuyubot plugin install <产物>` 只是**复制**进插件目录。以前的 `build-plugin`
//! 已按新规格拆掉。

/// CLI 输出统一走 `tracing`：命令结果与运行期诊断同一条通道、同一份格式。
///
/// 目标固定为 `fuyubot::cli`，控制台来源列显示 `cli`；消息本身不带级别以外的前缀。
macro_rules! cli {
    ($level:ident, $($arg:tt)*) => {
        tracing::$level!(target: "fuyubot::cli", $($arg)*)
    };
}

mod instance;
mod logging;
mod logout;
mod panel;
mod scaffold;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clap::{Args, Parser, Subcommand};
use fuyubot_loader::{BundleManifest, LogLevel, PluginEntry, UpdatePipeline, find_script};
use fuyubot_simplify::{CONNECTION_UPDATE, ConnectionState};
// 扩展 trait 要在作用域里才能对 `ConsolePrompter` 调 `ask`（固有方法不覆盖它）。
use fuyubot_tdlib::{AuthState, Prompter as _};

/// 收摊时等 TDLib 走完 `close` → `authorizationStateClosed` 的预算。
///
/// 上限压在 `fuyubot stop` 的 8 秒兜底（`instance.rs`）之下：插件 `plugin_stop` 已经先跑过，
/// 这里只留给「关数据库、刷盘」；等不到就照常销毁句柄，不让停止命令被 TDLib 拖死。
const TD_CLOSE_TIMEOUT: Duration = Duration::from_secs(3);

/// `fuyubot`：基于 TDLib 的插件化 bot 框架。
#[derive(Parser, Debug)]
#[command(name = "fuyubot", version, about, long_about = None)]
struct Cli {
    /// 省略 = 直接开**终端操作面板**（与 `run --tui` 同一条路）。
    ///
    /// 「下载即用」的形态里，双击 exe 就是不带参数的一次调用：那里不该是一屏
    /// usage，而该是能直接按 `s` 启动 bot 的面板。
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// 初始化一个 fuyubot 项目（生成 fuyubot.toml 与插件目录）
    Init(InitArgs),
    /// 起框架、加载插件
    Run(RunArgs),
    /// 只登录，不起框架：建 TDLib 客户端 → 走完授权 → 打印账户 → 退出。
    ///
    /// **不加载插件、不开接收循环**（缺口 22）。登录完 session 落在 `data/`，
    /// 之后 `fuyubot run`（含 `--background`）起来就直接进 `Ready`。
    Login(LoginArgs),
    /// 清掉本地 TDLib 会话目录（= 退出登录；下次要重新登录）。
    ///
    /// 默认**只打印将要删除什么**，加 `--yes` 才真删。实例在跑时拒绝。
    Logout(LogoutArgs),
    /// 看这个目录上的实例在不在跑（读 `.fuyubot/`）
    Status(StatusArgs),
    /// 停掉这个目录上跑着的实例
    Stop(StatusArgs),
    /// 看配置：打印**生效值**（默认值 + 向上找到的那份文件）
    Config(ConfigArgs),
    /// 插件管理
    Plugin(PluginArgs),
}

#[derive(Args, Debug)]
struct InitArgs {
    /// 项目目录
    #[arg(long, default_value = ".")]
    dir: PathBuf,
    /// bot 名字（只影响配置里的一行）
    #[arg(long, default_value = "fuyubot")]
    name: String,
    /// TDLib api_id（到 https://my.telegram.org 申请）。**不是**秘密，可以写进配置。
    #[arg(long, default_value_t = 0)]
    api_id: i64,
    /// TDLib api_hash；给了就明文写进 `[telegram].api_hash`，不给就留空等首次 run 问。
    #[arg(long, value_name = "HASH")]
    api_hash: Option<String>,
}

#[derive(Args, Debug)]
struct RunArgs {
    /// 配置文件
    #[arg(long, default_value = scaffold::CONFIG_FILE)]
    config: PathBuf,
    /// 覆盖配置里的 `api_id`（**不是**秘密，可以写在命令行 / 脚本里）。
    ///
    /// 优先级：**本参数 → 环境变量 `TG_API_ID` → 配置 `[telegram].api_id` → 交互提问**。
    /// `api_hash` 是凭据，**没有**对应参数（硬约束 41）：只走 `TG_API_HASH` 或交互输入。
    #[arg(long, value_name = "ID")]
    api_id: Option<i64>,
    /// 覆盖配置里的插件目录
    #[arg(long)]
    plugin_dir: Option<PathBuf>,
    /// 不接 TDLib，只加载插件
    #[arg(long)]
    no_tdlib: bool,
    /// 只接 TDLib、**不走登录流程**（CI / 容器要「起得来但不交互」时用）。
    /// 已有的 session 仍然会被 TDLib 打开，所以已登录过的实例照样能跑。
    #[arg(long)]
    no_login: bool,
    /// 放到后台跑：自己派生出没有终端的子进程，stdout/stderr 追加进 `logs/fuyubot.log`，
    /// 父进程等它真的起来（`.fuyubot/status` 发布）再返回。
    ///
    /// ⚠️ 后台跑**没有 stdin**：要交互式登录的场合（还没有 session、也没有 `TG_BOT_TOKEN`）
    /// 会直接失败并留在日志里——先用前台 `fuyubot run` 登一次，session 会落在 `data/`。
    /// 首次还要问 api_id / api_hash，同样问不了；问过一次它们会写回 `fuyubot.toml`
    /// （或事先设 `TG_API_ID` / `TG_API_HASH`），之后就不用再问。
    #[arg(long)]
    background: bool,
    /// 打开**终端操作面板**（顶部版本栏 + 左侧导航 + 中央工作区 + 底部两行），而不是在前台跑 bot。
    ///
    /// 面板自己**不接 TDLib、不加载插件**：TDLib 的数据目录同一时刻只能一个进程打开；
    /// 面板里的「启动」会派生 `fuyubot run --background`，
    /// 所以它显示的就是真正在跑的那个实例。退出面板不会停掉已启动的实例。
    #[arg(
        long = "tui",
        short = 't',
        conflicts_with_all = ["background", "replay"]
    )]
    tui: bool,
    /// 把一批更新的 JSON 灌进管线后退出：**不接 TDLib、不用账号**，纯离线验插件
    #[arg(long)]
    replay: Option<PathBuf>,
    /// 回放后等插件的静默窗口（毫秒）。不填就按有没有脚本插件自动选
    /// （脚本插件 2000，原生插件 150）。插件要发慢速网络请求时把它调大。
    #[arg(long, value_name = "MS")]
    replay_wait: Option<u64>,
    /// 覆盖 `[plugins].log_level`：error / warn / info / debug / trace。
    ///
    /// 调试时不用改配置就能把插件日志打到最细（trace = 全打）。
    /// 与配置共用同一份解析，写错会当场报错指路。
    #[arg(long, value_name = "LEVEL")]
    log_level: Option<String>,
    /// 只看这些插件打出来的日志（可重复；宿主 / 管线日志不受影响）。
    ///
    /// 名字以 manifest 里的插件名为准（如 `fuyubot.start`），也可以写逗号分隔的多个。
    /// 不传 = 全收。
    #[arg(long = "log-plugin", value_name = "NAME")]
    log_plugin: Vec<String>,
    /// 调试用：把 TDLib 发来的**每一条**原始更新 / 响应 JSON 打到 stderr。
    ///
    /// 里面含消息内容，别在正式环境开；要查「某个错误为什么没有 code」这类问题时
    /// 它就是现场——`cargo dev` 已经默认带上这个开关。
    #[arg(long)]
    trace_updates: bool,
}

impl RunArgs {
    /// 不带子命令时（`./fuyubot`）用的那一份：**全默认值**，等价于 `run --tui`。
    ///
    /// 只给面板入口用：`config` 用模板里的默认文件名，其余开关全关
    /// （`--api-id` / `--log-level` / `--replay` 这些只有显式写命令的人才会给）。
    fn default_for_tui() -> Self {
        Self {
            config: PathBuf::from(scaffold::CONFIG_FILE),
            api_id: None,
            plugin_dir: None,
            no_tdlib: false,
            no_login: false,
            background: false,
            tui: true,
            replay: None,
            replay_wait: None,
            log_level: None,
            log_plugin: Vec::new(),
            trace_updates: false,
        }
    }
}

#[derive(Args, Debug)]
struct LoginArgs {
    /// 配置文件
    #[arg(long, default_value = scaffold::CONFIG_FILE)]
    config: PathBuf,
    /// 覆盖配置里的 `api_id`（**不是**秘密）。
    ///
    /// 优先级：**本参数 → 环境变量 `TG_API_ID` → 配置 → 交互提问**。
    /// `api_hash` 没有对应参数（硬约束 41）：只走 `TG_API_HASH` 或交互输入。
    #[arg(long, value_name = "ID")]
    api_id: Option<i64>,
}

#[derive(Args, Debug)]
struct LogoutArgs {
    /// 配置文件（会话目录就是它旁边的 `[telegram].data_dir`，默认 `<项目根>/data`）
    #[arg(long, default_value = scaffold::CONFIG_FILE)]
    config: PathBuf,
    /// 真的删除。**不加只打印「将要删除什么」**（默认是预览，不是删除）。
    ///
    /// 三道守卫与这个开关无关：实例在跑 → 一律拒绝；目录不在项目根内 → 额外醒目警告；
    /// 目录里没有 TDLib 的会话标记（`td.binlog`）→ 一律拒绝（那多半不是 TDLib 数据目录）。
    #[arg(long)]
    yes: bool,
}

#[derive(Args, Debug)]
struct StatusArgs {
    /// 配置文件（实例状态放在它旁边的 `.fuyubot/` 里）
    #[arg(long, default_value = scaffold::CONFIG_FILE)]
    config: PathBuf,
}

#[derive(Args, Debug)]
struct ConfigArgs {
    /// 配置文件（找不到就往上找；都没有就按默认值报）
    #[arg(long, default_value = scaffold::CONFIG_FILE)]
    config: PathBuf,
}

#[derive(Args, Debug)]
struct PluginArgs {
    #[command(subcommand)]
    command: PluginCommand,
}

#[derive(Subcommand, Debug)]
enum PluginCommand {
    /// 静态列出插件目录里的插件（不 dlopen、不建 Kernel）
    List(PluginDirArgs),
    /// 把一个插件装进插件目录（目录 = 插件包 / 脚本插件，文件 = 原生插件）
    Install(InstallArgs),
    /// 卸载一个插件（包按 id，脚本按目录名，旧形态按动态库名）
    Uninstall(UninstallArgs),
    /// 把旧形态插件迁成插件包（`plugins/<id>/plugin.json`）
    ///
    /// 平铺 dll 会**允许 dlopen 一次**来问名字 / 版本 / 依赖（用户显式触发，进程即将退出）；
    /// `scripts/<dir>` 只是补一份 `plugin.json`，目录不动。`--dry-run` 只打印计划。
    Migrate(MigrateArgs),
}

#[derive(Args, Debug)]
struct PluginDirArgs {
    #[arg(long, default_value = scaffold::CONFIG_FILE)]
    config: PathBuf,
    #[arg(long)]
    plugin_dir: Option<PathBuf>,
}

#[derive(Args, Debug)]
struct InstallArgs {
    /// 插件包目录（根部有 plugin.json）、脚本插件目录（package.json / pyproject.toml），
    /// 或一个编好的动态库
    path: PathBuf,
    #[arg(long, default_value = scaffold::CONFIG_FILE)]
    config: PathBuf,
    #[arg(long)]
    plugin_dir: Option<PathBuf>,
}

#[derive(Args, Debug)]
struct MigrateArgs {
    /// 只打印计划，不动任何文件
    #[arg(long)]
    dry_run: bool,
    #[arg(long, default_value = scaffold::CONFIG_FILE)]
    config: PathBuf,
    #[arg(long)]
    plugin_dir: Option<PathBuf>,
}

#[derive(Args, Debug)]
struct UninstallArgs {
    /// 插件名：脚本插件是 `plugins/scripts/` 下的目录名，原生插件是动态库文件名主干
    name: String,
    #[arg(long, default_value = scaffold::CONFIG_FILE)]
    config: PathBuf,
    #[arg(long)]
    plugin_dir: Option<PathBuf>,
}

fn main() {
    // **所有**输出都走 `tracing`：命令结果、运行期诊断、致命错误同一条通道（见 `logging.rs`）。
    logging::init();
    if let Err(error) = dispatch(Cli::parse()) {
        tracing::error!(target: "fuyubot::cli", "{error}");
        std::process::exit(1);
    }
}

/// 按子命令分派；`Err` = 这条命令失败，由 [`main`] 记一条 error 日志后以 1 退出。
fn dispatch(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    // 不带子命令 = 开面板：`./fuyubot` 与 `./fuyubot run --tui` 是同一个入口，
    // 其余子命令（含 `run` 自己的 `--background` / `--replay`）一律原样走下面。
    let Some(command) = cli.command else {
        return run_tui(RunArgs::default_for_tui());
    };
    match command {
        Command::Init(args) => {
            let config = scaffold::init_with_api_hash(
                &args.dir,
                &args.name,
                args.api_id,
                args.api_hash.as_deref(),
            )?;
            cli!(info, "已生成 {}", config.display());
            cli!(info, "插件目录：{}", args.dir.join("plugins").display());
            cli!(
                info,
                "下一步：装 fuyubot-script 插件，或按文档站示例自己写插件（plugins/ 或 plugins/scripts/）"
            );
        }
        Command::Run(args) if args.tui => run_tui(args)?,
        Command::Run(args) if args.background => run_background(args)?,
        Command::Run(args) => run(args)?,
        Command::Login(args) => login(args)?,
        Command::Logout(args) => logout::run(&args)?,
        Command::Status(args) => status(&args)?,
        Command::Stop(args) => stop(&args)?,
        Command::Config(args) => config_list(&args)?,
        Command::Plugin(args) => match args.command {
            PluginCommand::List(args) => plugin_list(args)?,
            PluginCommand::Install(args) => plugin_install(args)?,
            PluginCommand::Uninstall(args) => plugin_uninstall(args)?,
            PluginCommand::Migrate(args) => migrate_plugins(args)?,
        },
    }
    Ok(())
}

/// `fuyubot status`：只读 `.fuyubot/`，**不加载插件、不接 TDLib**。
fn status(args: &StatusArgs) -> Result<(), Box<dyn std::error::Error>> {
    let root = existing_root(&args.config)?;
    for line in instance::status_lines(&root) {
        cli!(info, "{line}");
    }
    cli!(info, "  日志       {}", instance::log_path(&root).display());
    Ok(())
}

/// `fuyubot config`：打印**生效值**。不加载插件、不接 TDLib、不建任何东西。
///
/// 存在的理由只有一条：配置有默认值、还会往上级目录找文件，用户改了一行之后
/// 「到底生效了没有」没人回答。`get` / `set` / `path` 不做——改配置用编辑器，
/// 路径 `status` 第一行已经打了。
///
/// 找不到文件也照常打印（全是默认值）：`run` 就是这样起的，如实反映才不骗人。
fn config_list(args: &ConfigArgs) -> Result<(), Box<dyn std::error::Error>> {
    let start = parent_or_dot(&args.config);
    let path = scaffold::find_config(&start).unwrap_or_else(|| args.config.clone());
    let config = scaffold::load_config(&path)?;
    for line in scaffold::config_lines(&path, &parent_or_dot(&path), &config) {
        cli!(info, "{line}");
    }
    Ok(())
}

/// `fuyubot stop`。
fn stop(args: &StatusArgs) -> Result<(), Box<dyn std::error::Error>> {
    let root = existing_root(&args.config)?;
    cli!(info, "{}", instance::stop(&root)?);
    Ok(())
}

/// 新后台实例从空日志开始：截断旧文件（父进程已确认没有实例在跑）。
fn reset_log_file(log: &Path) -> Result<(), String> {
    if let Some(parent) = log.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("建不了 {}: {e}", parent.display()))?;
    }
    std::fs::write(log, "").map_err(|e| format!("清不了 {}: {e}", log.display()))
}

/// 让本进程的 stdin / stdout / stderr 不再被**随后**派生的子进程继承（仅 Windows）。
///
/// 为什么需要：`CreateProcessW(bInheritHandles = TRUE)` 会把父进程里**所有**带
/// `HANDLE_FLAG_INHERIT` 的句柄都交给子进程，而 Rust 的 `Command` 现在正是这么调的
/// （std 只用一把锁避免并发 spawn，没有收窄成 handle list）。`run --background` 的
/// stdout / stderr 很可能就是**调用方的管道**——被 DETACHED 的 daemon 继承之后，调用方
/// 读管道永远等不到 EOF。面板按 `s` 正是这条链：`Command::output()` 迟迟不返回，
/// `busy` 一直停在「启动中…」，于是 `x` 被 `busy` 挡掉、`s` 也再起不来。
///
/// 只清 **launcher 自己** 标准句柄的继承位；daemon 的 stdio 仍由 `STARTUPINFO`
/// 单独指定（日志文件 / NUL），不受影响。
#[cfg(windows)]
mod windows_handle_inheritance {
    use std::ffi::c_void;

    type Handle = *mut c_void;

    const STD_INPUT_HANDLE: u32 = -10i32 as u32;
    const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
    const STD_ERROR_HANDLE: u32 = -12i32 as u32;
    const HANDLE_FLAG_INHERIT: u32 = 0x0000_0001;
    const INVALID_HANDLE_VALUE: Handle = -1isize as Handle;

    unsafe extern "system" {
        fn GetStdHandle(std_handle: u32) -> Handle;
        fn SetHandleInformation(object: Handle, mask: u32, flags: u32) -> i32;
        #[cfg(test)]
        fn GetHandleInformation(object: Handle, flags: *mut u32) -> i32;
    }

    /// 清掉 launcher 三个标准句柄的继承位（无效句柄跳过）。
    pub fn clear_std_inherit() {
        for id in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            // SAFETY: 两个调用都没有前置条件；GetStdHandle 只读取进程的 std 句柄表。
            let handle = unsafe { GetStdHandle(id) };
            let _ = clear_handle_inherit(handle);
        }
    }

    /// 清掉单个句柄的继承位。空句柄 / `INVALID_HANDLE_VALUE` / 系统拒绝都返回 `false`。
    pub fn clear_handle_inherit(handle: Handle) -> bool {
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            return false;
        }
        // SAFETY: 调用方给的是有效句柄；失败最多保留旧行为（子进程仍会继承）。
        unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) != 0 }
    }

    #[cfg(test)]
    pub fn make_inheritable(handle: Handle) -> bool {
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            return false;
        }
        // SAFETY: 同上，测试句柄来自仍然活着的 `File`。
        unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) != 0 }
    }

    #[cfg(test)]
    pub fn is_inheritable(handle: Handle) -> Option<bool> {
        let mut flags = 0u32;
        // SAFETY: `flags` 指向本栈上的 u32；句柄无效时系统调用失败、返回 `None`。
        let ok = unsafe { GetHandleInformation(handle, &mut flags) } != 0;
        ok.then(|| flags & HANDLE_FLAG_INHERIT != 0)
    }
}

/// `run --background` 的父进程：**派生一个没有终端的自己**，等它真起来再返回。
///
/// 不做成「后台线程 + 父进程不退出」：用户要的是「敲完命令就回到提示符」，
/// 也就是真进程分离（`run --background` 之后 `fuyubot status` 应该报「运行中」）。
///
/// ⚠️ 子进程的 stdin 是 `null`：**后台跑不能交互**。要登录（没有 session、也没有
/// `TG_BOT_TOKEN`）就会直接失败——父进程把日志尾巴打出来指路，而不是挂在那里等人输入。
fn run_background(args: RunArgs) -> Result<(), Box<dyn std::error::Error>> {
    // 参数错误要**在前台**报出来：子进程的报错只会留在日志里。
    let cli_api_id = checked_cli_api_id(args.api_id)?;
    let root = parent_or_dot(&args.config);
    // 先在本进程里把项目建好：两个进程同时 init 会撞车（一个成功一个报「已经存在」）。
    ensure_project(&root, &args.config, cli_api_id)?;
    // 新实例从空日志开始：旧日志不保留。必须在确认没有实例在跑之后再做，
    // 否则会把正在运行实例的日志清掉。
    refuse_while_running(&root)?;

    let log = instance::log_path(&root);
    reset_log_file(&log)?;
    let append = || {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log)
            .map_err(|e| format!("打不开 {}: {e}", log.display()))
    };

    let mut command = std::process::Command::new(std::env::current_exe()?);
    command.arg("run").arg("--config").arg(&args.config);
    if let Some(dir) = &args.plugin_dir {
        command.arg("--plugin-dir").arg(dir);
    }
    if args.no_tdlib {
        command.arg("--no-tdlib");
    }
    if args.no_login {
        command.arg("--no-login");
    }
    // `--api-id` 是**非秘密**的覆盖，后台子进程必须拿到同一份（否则前后台用两个 api_id）。
    if let Some(api_id) = args.api_id {
        command.arg("--api-id").arg(api_id.to_string());
    }
    if let Some(replay) = &args.replay {
        command.arg("--replay").arg(replay);
    }
    if let Some(wait) = args.replay_wait {
        command.arg("--replay-wait").arg(wait.to_string());
    }
    // 日志过滤也要跟着走：后台进程是另一个进程，不传就等于把过滤丢了。
    for name in &args.log_plugin {
        command.arg("--log-plugin").arg(name);
    }
    command
        .stdin(std::process::Stdio::null())
        .stdout(append()?)
        .stderr(append()?);
    #[cfg(windows)]
    {
        // DETACHED_PROCESS：子进程不要控制台（否则父进程退出 / 终端关掉会影响它）。
        // 没做 Unix 的 setsid（std 不提供）——那边靠「stdio 都重定向了、
        // 也不在终端的进程组前台」撑着，关终端发 SIGHUP 时可能被带走；要真守护进程再引守护库。
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        command.creation_flags(DETACHED_PROCESS);
        // ⚠️ 必须在 spawn 之前清：std 的 `Command` 用 `bInheritHandles = TRUE` 派生，
        // daemon 会把 launcher 从调用方继承来的 stdout / stderr 管道一起带走，
        // 调用方（面板的 `Command::output()`）就永远读不到 EOF。见模块文档。
        windows_handle_inheritance::clear_std_inherit();
    }

    let mut child = command
        .spawn()
        .map_err(|e| format!("起不了后台进程: {e}"))?;
    let pid = child.id();

    // 等它发布状态（拿上锁只说明进程活着：插件还没加载、登录还没走）。
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(status) = child.try_wait()? {
            return Err(format!(
                "后台进程退了（{status}）——日志：{}\n{}",
                log.display(),
                instance::tail(&log, 20)
            )
            .into());
        }
        if matches!(
            instance::read_status(&root),
            Some(instance::Info { pid: published, .. }) if published == pid
        ) {
            cli!(info, "已在后台运行：pid {pid}，日志 {}", log.display());
            cli!(info, "`fuyubot status` 看状态，`fuyubot stop` 停掉它");
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            cli!(info, "后台进程（pid {pid}）还在启动中（登录可能要一会儿）");
            cli!(info, "日志：{}；状态：`fuyubot status`", log.display());
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// 从 `start` 往上找 `fuyubot.toml`，**找不到就报错指路**（变更 6）。
///
/// `run` 不走这条：它会自动 init。`status` / `stop` 面对的是「已经存在的实例」，
/// 没有配置就说明这里根本不是 Bot 目录，凭空造一个只会更迷惑。
fn existing_root(config: &Path) -> Result<PathBuf, String> {
    let start = parent_or_dot(config);
    match scaffold::find_config(&start) {
        Some(found) => Ok(parent_or_dot(&found)),
        None => Err(format!(
            "{} 及其上级目录里没有 {}——先 `fuyubot init` 建一个",
            start.display(),
            scaffold::CONFIG_FILE
        )),
    }
}

/// 实例在跑就**拒绝**（`login` / `logout` 共用）。
///
/// 判据就是 [`instance::probe`] 的那把锁（不是「PID 还活着吗」）：拿不到锁 = 真有进程
/// 持有这个目录的实例。为什么这两条命令要挡：TDLib 的 sqlite + binlog 同一时刻只能
/// 一个进程打开，`login` 再开一次、`logout` 直接把目录删掉，都会把跑着的实例拆坏。
///
/// 回报的是「已经存在的实例」而不是「没实例」——`Stale`（锁文件在、没人持有）是安全的，
/// 那是上次没收摊，`run` 自己会接管。
fn refuse_while_running(root: &Path) -> Result<(), String> {
    match instance::probe(root) {
        instance::State::Running(Some(info)) => Err(format!(
            "{} 上还有实例在跑（{}，pid {}，已运行 {}）——先 `fuyubot stop` 再来；\
             运行中动 TDLib 的会话目录会把它的数据库拆坏",
            root.display(),
            info.name,
            info.pid,
            instance::uptime_text(info.started, instance::unix_now())
        )),
        instance::State::Running(None) => Err(format!(
            "{} 上还有一个实例正在启动（状态还没发布）——等它起来后 `fuyubot stop` 再来",
            root.display()
        )),
        instance::State::Absent | instance::State::Stale(_) => Ok(()),
    }
}

/// 把 `--config` 归一成**真正要读**的那份文件。
///
/// 默认名字（`fuyubot.toml`）是相对的、还要支持「从子目录往上找」，所以得用
/// [`scaffold::find_config`] 找出来的那份；`--config` 明确写了别的名字就是显式意图，
/// 原样用（文件不在 = 一份默认值，见 [`scaffold::load_config`]）。
///
/// `run` / `login` / `logout` 都走这一条：三条路读的配置必须是同一份，
/// 否则 `data_dir` 一差，登录与运行就会各写一个 TDLib 数据库。
fn resolved_config_path(configured: &Path, root: &Path) -> PathBuf {
    match scaffold::find_config(root) {
        Some(found) if configured == Path::new(scaffold::CONFIG_FILE) => found,
        _ => configured.to_path_buf(),
    }
}

/// 从 `start` 往上找带 `fuyubot.toml` 的目录。
fn project_root(start: &Path) -> Option<PathBuf> {
    scaffold::find_config(start).and_then(|path| path.parent().map(Path::to_path_buf))
}

/// `path` 的父目录；没有父目录（`fuyubot.toml` 这种纯文件名）就是当前目录。
///
/// ⚠️ `filter(is_empty)` 不能省：`Path::new("fuyubot.toml").parent()` 是 `Some("")` 而不是 `None`。
fn parent_or_dot(path: &Path) -> PathBuf {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
        .to_path_buf()
}

/// 把进程的工作目录切到项目根（`fuyubot.toml` 所在目录）。
///
/// 插件是**同进程**的动态库，它们算相对路径时只有 `current_dir()` 可用——宿主是在项目根、
/// 还是随便哪个目录被敲起来的，本来不该决定插件把缓存写到哪。`run` 在加载插件之前切一次，
/// 「相对路径」从此一律指项目根：插件配置里写 `cache` / `Cache` 就落在配置文件旁边，
/// 和 `[telegram].data_dir`（`root.join(...)`）是同一个基准。
///
/// ⚠️ 调用方必须**先**把手上的相对路径归一成绝对路径（`--config` / `--plugin-dir` /
/// `--replay`）：切完目录，同一个相对字符串就不再指向同一处了。
///
/// 回放（`--replay`）也走这条：它同样加载插件，缓存落点该和真机一致。
fn enter_project_root(root: &Path) -> Result<(), Box<dyn std::error::Error>> {
    if std::env::current_dir().ok().as_deref() == Some(root) {
        return Ok(());
    }
    std::env::set_current_dir(root).map_err(|e| format!("切不到项目根 {}: {e}", root.display()))?;
    cli!(info, "工作目录切到项目根：{}", root.display());
    Ok(())
}

// ── run / plugin list ──────────────────────────────────────────────

/// `fuyubot run --tui`：打开操作面板。
///
/// 与 `run` 共用「缺配置就自动 init」；面板本身**不接 TDLib、不加载插件**
/// （TDLib 数据目录同一时刻只能一个进程打开），启停都派生 / 通知子进程。
fn run_tui(args: RunArgs) -> Result<(), Box<dyn std::error::Error>> {
    // 先确认是终端：非 TTY 下不该为了报错而先把项目建出来（自动 init 有副作用）。
    panel::require_terminal()?;
    // 与 `run` 一样先验参数，再自动 init：别等进了面板才因为参数错而退出。
    let cli_api_id = checked_cli_api_id(args.api_id)?;
    if let Some(raw) = &args.log_level {
        scaffold::parse_log_level(raw)?;
    }
    let root = parent_or_dot(&args.config);
    ensure_project(&root, &args.config, cli_api_id)?;

    // 面板里按 `s` 起的后台实例要拿到同一组 `run` 参数（否则前后两次用不同的 api_id / 日志级别）。
    let spawn = panel::SpawnOptions {
        api_id: cli_api_id,
        no_tdlib: args.no_tdlib,
        no_login: args.no_login,
        log_level: args.log_level.clone(),
        log_plugins: args.log_plugin.clone(),
        trace_updates: args.trace_updates,
    };
    panel::run(&args.config, args.plugin_dir.clone(), spawn)
}

/// 读面板写的日志级别请求；没有 / 不认识就保持 `current`。
///
/// 抽成纯函数是为了能单独测：主循环本身要真 TDLib，测试里跑不起来。
/// 控制文件由 `instance::request_log_level` 校验后写入，外部写坏也只是「不生效」。
fn desired_log_level(root: &Path, current: LogLevel) -> LogLevel {
    instance::requested_log_level(root)
        .and_then(|raw| scaffold::parse_log_level(&raw).ok())
        .unwrap_or(current)
}

fn run(args: RunArgs) -> Result<(), Box<dyn std::error::Error>> {
    // 在起 tokio 之前装：`now_local()` 在 Unix 多线程下拿不到时区，只能退回 UTC。
    logging::init();
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(run_async(args))
}

/// `fuyubot login`：**只**把登录走完（缺口 22 的「独立登录命令」）。
///
/// 与 `run` 的差别只有两处：**不加载插件、不开接收循环**。其余（配置解析、自动 init、
/// 凭据来源、`resolve_parameters`、`drive_login`、账户打印）全部复用 `run` 的那一套——
/// 两条路各写一份的话，`data_dir` 或凭据优先级一旦漂移，就会出现「`run` 能登、`login`
/// 登不上」这种最难查的分叉。
///
/// 为什么需要它：`run --background` 没有 stdin，登录不了；容器 / CI 想先登录一次、
/// 之后只跑实例，也得有个只做登录的命令。登录完 session 落在 `data/`，
/// 之后 `fuyubot run --background` 起来就直接进 `Ready`（`drive_login` 会打那条判据）。
///
/// 实例在跑时拒绝并**拿锁**：登录可能要等验证码 / 扫码好几分钟，这期间
/// `fuyubot run` 不该挤进来抢同一个 TDLib 数据库（判据与 `logout` 共用）。
fn login(args: LoginArgs) -> Result<(), Box<dyn std::error::Error>> {
    logging::init();
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(login_async(args))
}

async fn login_async(args: LoginArgs) -> Result<(), Box<dyn std::error::Error>> {
    // 先验 `--api-id`（显式写错就立刻报）：别先建了项目再因为参数错而半途退出。
    let cli_api_id = checked_cli_api_id(args.api_id)?;
    let root = parent_or_dot(&args.config);
    // 配置与 `run` 同源：自动 init 会让「第一次就 `login`」也能直接工作。
    ensure_project(&root, &args.config, cli_api_id)?;
    let config_path = resolved_config_path(&args.config, &root);
    let root = parent_or_dot(&config_path);
    let config = scaffold::load_config(&config_path)?;

    refuse_while_running(&root)?;
    // 锁一直拿到本命令退出：登录期间 `fuyubot run` 会被这把锁挡在外面。
    let instance_lock = instance::InstanceLock::acquire(&root)?;

    // ⚠️ 这里**不**装插件、**不**开分发：登录只需要一个能收发 TDLib 更新的客户端，
    // 插件与分发都是 `run` 的事（不然登个录还要装/起一遍插件）。
    // `App::boot` = 内核 + 宿主 + 接 TDLib（不登录）；`login` 才是走授权状态机。
    let parameters = resolve_parameters(&config, &config_path, &root, cli_api_id)?;
    let account = config.telegram.account_type()?;
    cli!(
        info,
        "只登录：不加载插件、不开接收循环；登完就退出\
         （之后 `fuyubot run` 会直接用这份 session）"
    );

    let app_options = fuyubot_app::AppOptions {
        data_dir: parameters.data_dir.clone(),
        plugin_dir: config.plugin_dir(&root),
        log_level: config.plugins.log_level()?,
        log_plugins: Vec::new(),
        config: config.to_json()?,
        config_path: Some(config_path.clone()),
        tdjson_path: config.tdlib.path.as_deref().map(PathBuf::from),
        tdlib: Some(fuyubot_app::TdlibSettings {
            parameters: parameters.clone(),
            account,
            bot_token: std::env::var("TG_BOT_TOKEN")
                .ok()
                .filter(|token| !token.is_empty()),
            log_verbosity: Some(config.tdlib.log_verbosity()),
            login_budget: fuyubot_tdlib::LOGIN_BUDGET,
            login: true,
        }),
        host: Arc::new(ConsoleHost::new()),
    };

    let mut app = fuyubot_app::App::boot(app_options).await?;
    app.login()?;
    cli!(
        info,
        "登录完成：session 在 {}——`fuyubot run`（含 `--background`）起来会直接进 Ready",
        parameters.data_dir.display()
    );
    // 登录完也走一次优雅关闭：让 TDLib 把刚写下的 session 刷进 `database_dir` 再销毁，
    // 而不是只靠 destroy 收尾。
    if app.shutdown(None, TD_CLOSE_TIMEOUT).await? {
        cli!(info, "TDLib 已关闭（数据库已刷盘）");
    } else {
        cli!(
            warn,
            "TDLib 没在 {:?} 内确认关闭，直接销毁客户端",
            TD_CLOSE_TIMEOUT
        );
    }

    drop(instance_lock);
    Ok(())
}

async fn run_async(args: RunArgs) -> Result<(), Box<dyn std::error::Error>> {
    // 参数错误要**在动任何文件之前**报出来（自动 init 会建项目目录）。
    let cli_api_id = checked_cli_api_id(args.api_id)?;
    // 直接调 `run_async` 的路径（测试 / 将来的嵌入方）也要有订阅者；重复调用只保留第一个。
    logging::init();
    // 启动时刻：写进状态文件给 `status` 算「已运行多久」。
    let started_at = instance::unix_now();
    // 项目根目录：`fuyubot.toml` 所在目录（找不到就是当前目录）。
    let root = parent_or_dot(&args.config);

    // **自动 init**：缺配置文件就照模板生成一份，用户不用先手工跑一次 `fuyubot init`。
    //
    // ⚠️ **`--replay` 不自动 init**：回放是「离线验插件」的开发路径，不该因为跑一次就在
    //    当前目录里留下 `fuyubot.toml`（踩过一次：在仓库根跑回放，仓库里凭空多了个配置文件）。
    if args.replay.is_none() {
        ensure_project(&root, &args.config, cli_api_id)?;
    }

    let config_path = resolved_config_path(&args.config, &root);
    // 从这一刻起手上的路径都得是绝对的：下面要切工作目录，相对字符串切完就指到别处去了。
    let config_path = std::path::absolute(&config_path)?;
    let root = parent_or_dot(&config_path);
    let config = scaffold::load_config(&config_path)?;
    // `--plugin-dir` 是「相对用户敲命令的地方」的路径，切目录之前先归一；
    // 没给就用配置里的（`root` 已经绝对的，`root.join(dir)` 自然也是）。
    let plugin_dir = match args.plugin_dir.clone() {
        Some(dir) => std::path::absolute(dir)?,
        None => config.plugin_dir(&root),
    };
    // `--replay` 同理：回放文件是用户眼里的相对路径，切目录后就不再是那个意思了。
    let replay = args.replay.clone().map(std::path::absolute).transpose()?;
    // 插件是**同进程**的 dll，它们算相对路径用的是 `current_dir()`。宿主从哪个目录被敲起来
    // 是用户的自由，不该决定缓存落在哪儿——所以在加载插件之前把 CWD 钉到项目根
    // （配置文件所在目录）：插件配置里写成 `cache` / `Cache` 的相对路径就稳稳落在
    // `fuyubot.toml` 旁边，跟 `[telegram].data_dir` 的语义一致。
    enter_project_root(&root)?;
    // 插件目录不在就建一个空的：`plugin_entries` 对**不存在的目录**是 io 错误，
    // 而 `run` 不该因为「还没装过插件」就死在一个 `Os { code: 3 }` 上
    // （`init` 只在缺配置时才会建 `plugins/`，配置已存在而目录被删掉就轮到这条兜底）。
    std::fs::create_dir_all(&plugin_dir)
        .map_err(|e| format!("建不了插件目录 {}: {e}", plugin_dir.display()))?;
    // 内置插件（/start、/help、/plugins）跟可执行文件放在一起，装进插件目录就能用。
    // **只装缺的**，而且回放不碰用户目录（`--replay` 是离线验插件的开发路径）。
    if args.replay.is_none() {
        match scaffold::install_builtins(&plugin_dir) {
            Ok(staged) => {
                for path in &staged {
                    cli!(info, "已安装内置插件 {}", path.display());
                }
            }
            Err(error) => cli!(warn, "装内置插件失败（继续）: {error}"),
        }
    }

    // ── 单实例（变更 7）───────────────────────────────────────────────
    //
    // 一个目录一个实例：拿不到锁就说明已经有一个在跑（报错里带上它的 PID）。
    // ⚠️ **回放不占锁**：它是离线验插件的开发路径，跑着的实例不该挡住它。
    let instance_lock = match args.replay {
        Some(_) => None,
        None => Some(instance::InstanceLock::acquire(&root)?),
    };

    // ── ③～⑧ 交给编排层（`fuyubot-app`）───────────────────────────
    //
    // 「内核 → 宿主运行时 → 接 TDLib → 装插件 → 登录」这段顺序只有一份，在 `fuyubot-app` 里
    // （Android 的 JNI 运行时走的是同一份）。这里只留 CLI 自己的东西：
    // 实例锁、`.fuyubot/status`、回放、重登、插件日志门槛、`--trace-updates`。
    //
    // 装插件必须在登录**之前**：插件要在 TDLib 开始产出之前订阅好，否则登录窗口里的更新
    // 就漏了（窗口里收到的东西也不会被吞：`Session::login` 的 `bypass` 转给管线）。
    let replaying = args.replay.is_some();

    // ② 凭据先问清楚。`--no-login` 不问也不发参数——TDLib 会停在
    //    `authorizationStateWaitTdlibParameters`，**已有的 session 也不会被打开**
    //    （数据库目录正是在 `setTdlibParameters` 里给的）。它只适合「只想把插件跑起来」。
    let parameters = if args.no_login || args.no_tdlib || replaying {
        None
    } else {
        Some(resolve_parameters(
            &config,
            &config_path,
            &root,
            cli_api_id,
        )?)
    };

    // 回放 / `--no-tdlib` 不接 TDLib；否则先探一次库——**探不到就不接、但照常跑**
    // （与以前的 `attach_tdlib` 一致：插件与回放都不需要 TDLib）。
    let attach = if args.no_tdlib || replaying {
        if replaying {
            cli!(info, "--replay：本次不接 TDLib（回放是离线的，请求只留档）");
        } else {
            cli!(info, "--no-tdlib：跳过 TDLib，插件照常加载");
        }
        false
    } else {
        match fuyubot_tdlib::resolve() {
            Ok(_) => true,
            Err(error) => {
                cli!(warn, "TDLib 未就绪（不接更新）：{error}");
                false
            }
        }
    };

    // 插件日志门槛：`[plugins].log_level`，`--log-level` 覆盖它。**必须在插件加载之前**——
    // 插件 `init` / `start` 里打的日志也受它管。
    let mut log_level = match &args.log_level {
        Some(raw) => scaffold::parse_log_level(raw)?,
        None => config.plugins.log_level()?,
    };
    cli!(
        info,
        "插件日志级别：{}",
        scaffold::log_level_name(log_level)
    );
    if !args.log_plugin.is_empty() {
        cli!(info, "只看插件日志：{}", args.log_plugin.join(", "));
    }

    // `[tdlib].path` 指到不存在的路径就别硬塞给 loader：告诉用户，然后走自动查找。
    let tdjson_path = match config.tdlib.path.as_deref() {
        Some(path) if !path.trim().is_empty() => {
            let path = PathBuf::from(path);
            if path.exists() {
                cli!(info, "TDLib 用配置里的路径：{}", path.display());
                Some(path)
            } else {
                cli!(
                    warn,
                    "配置里的 tdlib.path 不存在：{}，回退到自动查找",
                    path.display()
                );
                None
            }
        }
        _ => None,
    };

    let data_dir = parameters
        .as_ref()
        .map(|parameters| parameters.data_dir.clone())
        .unwrap_or_else(|| root.join(config.telegram.data_dir.as_deref().unwrap_or("data")));

    let app_options = fuyubot_app::AppOptions {
        data_dir: data_dir.clone(),
        plugin_dir: plugin_dir.clone(),
        log_level,
        log_plugins: args.log_plugin.clone(),
        config: config.to_json()?,
        config_path: Some(config_path.clone()),
        tdjson_path,
        tdlib: if attach {
            Some(fuyubot_app::TdlibSettings {
                // `--no-login` 时参数只是占位（`login: false`，永远不会发给 TDLib）。
                parameters: parameters.unwrap_or_else(|| {
                    fuyubot_tdlib::TdlibParameters::new(data_dir, 0, String::new())
                }),
                account: config.telegram.account_type()?,
                bot_token: std::env::var("TG_BOT_TOKEN")
                    .ok()
                    .filter(|token| !token.is_empty()),
                log_verbosity: Some(config.tdlib.log_verbosity()),
                login_budget: fuyubot_tdlib::LOGIN_BUDGET,
                login: !args.no_login,
            })
        } else {
            None
        },
        host: Arc::new(ConsoleHost::new()),
    };

    let mut app = fuyubot_app::App::boot(app_options).await?;

    // ⑥ 装插件（**在发 `setTdlibParameters` 之前**）。
    app.load_plugins()?;
    // 运行期调日志级别 / `--trace-updates` 走同一条管线（`fuyubot-app` 已经把门槛与来源过滤设好）。
    let pipeline = Arc::clone(app.pipeline());
    if args.trace_updates {
        pipeline.runtime().set_trace_updates(true);
        cli!(
            info,
            "--trace-updates：每一条原始更新 / 响应都会打到 stderr"
        );
    }

    // ── TDLib 运行状态（面板顶部 / 概览页）─────────────────────────
    //
    // TDLib **没有**「现在连上了吗 / 登录到哪一步了」这种可查的方法：连接与授权都只有
    // `update*` 这一条路，而且只在状态变化时来一条。这里各订阅一次、只记最新那条，
    // 主循环再写进 `.fuyubot/status`——面板是**另一个进程**，只能通过这份文件看到它们
    // （约束 47：面板自己不接 TDLib）。
    //
    // ⚠️ 连接状态的订阅必须早于登录：登录窗口里 TDLib 就在发
    //    `updateConnectionState`，而且只在**变化**时发，等登录完再订阅就漏掉 `connectionStateReady`。
    //    授权状态正相反：`log_in_with` 把登录窗口里的 `updateAuthorizationState` **自己消费掉了**
    //    （只把与授权无关的更新转给管线），事件总线看不到最后那条 `authorizationStateReady`——
    //    所以登录成功后要补记一笔（见 `mark_authorized`）。`--no-login` 时 TDLib 会停在
    //    `authorizationStateWaitTdlibParameters`，那条会正常经管线到达。
    let connection: Arc<Mutex<Option<ConnectionState>>> = Arc::new(Mutex::new(None));
    let _connection_watch = {
        let slot = Arc::clone(&connection);
        app.runtime()
            .events()
            .on(CONNECTION_UPDATE, None, move |payload| {
                if let Some(state) = connection_update_state(payload) {
                    *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(state);
                }
                Ok(())
            })
    };
    // 授权状态另存一份**名字**（`authorizationStateReady` 等）。这条只负责「记录」；
    // 运行期「被下线」的报警由下面的 `_auth_watch` 单独负责——两者的时机与用途不同，
    // 混在一起会让「登录窗口里的 WaitCode」被误当成「被踢」。
    let authorization: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let _authorization_watch = {
        let slot = Arc::clone(&authorization);
        app.runtime()
            .events()
            .on("updateAuthorizationState", None, move |payload| {
                if let Some(name) = authorization_update_name(payload) {
                    *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(name);
                }
                Ok(())
            })
    };

    // ⑦ 现在才驱动登录：等「等参数」状态 → 设置 → 等授权状态 →（要登录就登录）→ Ready。
    //    账户信息（`getMe`）由 `App::login` 交给运行时：插件经 `fuyubot.host.identity`
    //    读到它，`on_command` 靠它把「点名给别人的命令」挡在插件外面。
    if app.should_login() {
        app.login()?;
        // 登录窗口里的授权更新被 `log_in_with` 自己消费了，事件总线收不到 Ready；
        // 它正常返回就等于授权状态到了 Ready，补记这一笔（否则 status 里会一直没有授权状态）。
        mark_authorized(&authorization);
    }

    // ⑧ 插件与登录都就位，现在才开接收循环（gaps 缺口 8：句柄要一直留着，否则停不掉）。
    let receive = app.dispatch();

    let bot_name = config
        .bot
        .name
        .clone()
        .unwrap_or_else(|| "fuyubot".to_owned());
    // 插件速览：`status` 是**另一个进程**，只能从这份文件读，所以要写下来。
    // （插件自己那条路走 `fuyubot.host.plugins` 服务，`load_plugins` 里已经装了活来源。）
    let detail = instance::PluginDetail::from_report(&app.kernel().report());
    cli!(
        info,
        "{bot_name}（fuyubot {}）已启动：插件 {} 个，工具 {} 个",
        env!("CARGO_PKG_VERSION"),
        detail.active.len(),
        app.runtime().tools().len()
    );
    // 发布 / 重发状态用同一份计算：`--background` 的父进程等的是**第一次**发布，
    // 而运行期插件上下线 / 日志级别变化之后要让 `status` 跟上——两处算的必须是同一个东西。
    //
    // `running` / `stopped_at` 只在**收摊成功之后**才写 `running=0` + 时间：崩溃 / 硬杀
    // 留下的最后一份仍是 `running=1`，面板据此区分「上次正常停止」与「上次异常退出」。
    //
    // `account` 由调用方算好传进来：宿主手里那份是**活的**（登录写一次，之后每条
    // `updateUser` 都刷新），闭包里不碰 `App` 才不会和后面的 `login` / `shutdown` 抢借用。
    let publish = |detail: instance::PluginDetail,
                   log_level: LogLevel,
                   running: bool,
                   stopped_at: Option<u64>,
                   account: Option<instance::Account>|
     -> Result<(), Box<dyn std::error::Error>> {
        let Some(lock) = instance_lock.as_ref() else {
            return Ok(());
        };
        lock.publish(&instance::Info {
            pid: std::process::id(),
            started: started_at,
            version: env!("CARGO_PKG_VERSION").to_owned(),
            log_level: scaffold::log_level_name(log_level).to_owned(),
            plugins: detail.active.len(),
            name: bot_name.clone(),
            running: Some(running),
            stopped_at,
            // 订阅没装（`--no-tdlib` / `--replay`）就一直 `None`：status 里没这一行。
            connection: (*connection.lock().unwrap_or_else(|e| e.into_inner()))
                .map(|state| state.type_name().to_owned()),
            authorization: (*authorization.lock().unwrap_or_else(|e| e.into_inner())).clone(),
            account,
            detail,
        })?;
        Ok(())
    };
    // 状态**这时候才发布**：`run --background` 的父进程等的就是它（拿上锁只说明进程活着）。
    publish(
        detail.clone(),
        log_level,
        true,
        None,
        current_account(app.runtime()),
    )?;
    // 运行循环用它判断「要不要重发」——不记下来的话，每圈都算得出差异、
    // 或者干脆每圈都写一遍。
    let mut last_detail = detail;
    let mut last_log_level = log_level;
    let mut last_connection = *connection.lock().unwrap_or_else(|e| e.into_inner());
    let mut last_authorization = (*authorization.lock().unwrap_or_else(|e| e.into_inner())).clone();
    let mut last_account = current_account(app.runtime());

    if let Some(replay) = replay {
        // 脚本插件在**另一个进程**里跑，反应要经管道回来，还可能捎带网络请求；
        // 纯 Rust/C 插件是同步的，回放完就已经尘埃落定。按这个差别选等待窗口。
        let scripted = has_script_plugins(&plugin_dir);
        replay_updates(
            &pipeline,
            &replay,
            app.runtime(),
            scripted,
            args.replay_wait,
        )?;
        app.shutdown(None, TD_CLOSE_TIMEOUT).await?;
        return Ok(());
    }

    // 没有 TDLib 就到此为止：接收循环没起来，本进程只是个「装好插件的空壳」。
    let Some(mut receive) = receive else {
        let account = current_account(app.runtime());
        app.shutdown(None, TD_CLOSE_TIMEOUT).await?;
        publish(
            last_detail.clone(),
            log_level,
            false,
            Some(instance::unix_now()),
            account,
        )?;
        return Ok(());
    };

    // ── 运行期盯授权状态（变更 3-b）──────────────────────────────────
    //
    // 订阅事件总线的 `updateAuthorizationState`：一旦进了「要用户再出一次凭据」的状态
    // （`needs_re_login`：WaitPhoneNumber / WaitCode / WaitPassword / 扫码确认），
    // 说明**被下线或封禁**了 → 把状态名丢进这个槽，由主任务处理。
    //
    // 为什么不在回调里直接处理：回调跑在**接收循环那个线程**上，而我们要做的第一件事
    // 就是停掉那个循环——在自己的线程里 stop + join 自己是死锁。
    let alert: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let _auth_watch = {
        let slot = Arc::clone(&alert);
        app.runtime()
            .events()
            .on("updateAuthorizationState", None, move |payload| {
                if let Some(state) = re_login_alert(payload) {
                    *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(state);
                }
                Ok(())
            })
    };

    cli!(
        info,
        "开始接收 TDLib 更新（Ctrl-C 退出，或在别处 `fuyubot stop`）"
    );
    //
    // ⚠️ Ctrl-C 目前仍是**硬杀**（进程被直接终止，插件的 `stop` 走不到）：要捕获它得开
    //    tokio 的 `signal` feature，那会带进 mio + signal-hook-registry + errno——AGENTS 的
    //    「不新增依赖」只放行 clap / object，所以这件事得用户点头（见 docs/decisions.md 变更 3-c）。
    //    要优雅退出就走 `fuyubot stop`（它写停止标记，我们在下面这一圈里看到就自己收摊）。
    loop {
        tokio::time::sleep(Duration::from_millis(500)).await;

        // 运行期调日志级别：面板在日志页写 `.fuyubot/log-level`，这里每圈读一次。
        // 只有**变了**才 set + 打一行；文件不存在 / 内容写坏都保持当前级别。
        let desired = desired_log_level(&root, log_level);
        if desired != log_level {
            pipeline.runtime().set_log_verbosity(desired);
            log_level = desired;
            cli!(
                info,
                "插件日志级别已切换为 {}",
                scaffold::log_level_name(desired)
            );
        }

        // 插件在运行期会自行上下线（依赖消失就退回 Pending，回来再自动激活），
        // 而 `fuyubot status` 是**另一个进程**、只能读这份文件。所以这一圈顺带把
        // 变了的状态重新发布一次。
        //
        // ⚠️ **只在真的变了才写**：`publish` 会整份重写文件，每 500ms 都写一遍
        // 是白费 I/O，还会让 `status` 的读和写撞上（Windows 上偶尔读到半份）。
        {
            let current = instance::PluginDetail::from_report(&app.kernel().report());
            let connection_now = *connection.lock().unwrap_or_else(|e| e.into_inner());
            let authorization_now =
                (*authorization.lock().unwrap_or_else(|e| e.into_inner())).clone();
            // 账户也会变：TDLib 用 `updateUser` 改名字 / 用户名，宿主已经收下最新的那份，
            // 这里只负责发现「变了」并把 status 重发一遍。
            let account_now = current_account(app.runtime());
            if current != last_detail
                || log_level != last_log_level
                || connection_now != last_connection
                || authorization_now != last_authorization
                || account_now != last_account
            {
                publish(current.clone(), log_level, true, None, account_now.clone())?;
                last_detail = current;
                last_log_level = log_level;
                last_connection = connection_now;
                last_authorization = authorization_now;
                last_account = account_now;
            }
        }

        // `fuyubot stop` 请我们停下：**优雅收摊**——停接收循环 → 卸载插件（跑 `plugin_stop`）
        // → 停内核 → 放锁（`InstanceLock` 随作用域 drop）。
        if instance_lock
            .as_ref()
            .is_some_and(instance::InstanceLock::stop_requested)
        {
            cli!(info, "收到停止请求：停接收循环、卸载插件");
            // 收摊前的账户先取出来：`shutdown` 会消费 `app`。
            let account = current_account(app.runtime());
            let closed = app.shutdown(Some(receive), TD_CLOSE_TIMEOUT).await?;
            if closed {
                cli!(info, "TDLib 已关闭（数据库已刷盘）");
            } else {
                cli!(
                    warn,
                    "TDLib 没在 {:?} 内确认关闭，直接销毁客户端",
                    TD_CLOSE_TIMEOUT
                );
            }
            if let Some(lock) = instance_lock.as_ref() {
                lock.clear_stop();
            }
            // 收摊成功之后才敢说「正常停止」：写 `running=0` + 时间，面板 / `status`
            // 才能和崩溃留下的 `running=1` 区分开。
            publish(
                last_detail.clone(),
                log_level,
                false,
                Some(instance::unix_now()),
                account,
            )?;
            return Ok(());
        }

        let Some(state) = alert.lock().unwrap_or_else(|e| e.into_inner()).take() else {
            continue;
        };
        // 退回登录要用启动时那套参数。`--no-login` 下我们根本没发过参数，也就没有可重登的
        // 东西——**这条必须在停循环之前判**：`shutdown` 会按值消费 `app`。
        if !app.should_login() {
            cli!(
                warn,
                "⚠️ 授权状态变成 {state}，但本次是 --no-login（没发过 setTdlibParameters），\
                 没有可用的重登参数——只能继续等"
            );
            continue;
        }

        cli!(
            warn,
            "⚠️ 授权状态变成 {state}：账号可能被下线或封禁 —— 停掉接收循环，退回登录流程"
        );
        receive.stop();
        receive.join().await;
        // 重登之后账户可能换了（换号 / 改名）：`App::login` 会把新的 `getMe` 交给宿主，
        // 否则 `fuyubot.host.identity` 与面板上的账户行还停在旧账号。
        app.login()?;
        // 与启动时同一个理由：登录窗口里的授权更新不进事件总线，成功返回就补记 Ready。
        mark_authorized(&authorization);
        // 登录完再开分发。**插件不重新加载**：它们还订阅着，重载会重复注册。
        receive = match app.dispatch() {
            Some(receive) => receive,
            None => unreachable!("刚登录完一定有 TDLib 客户端"),
        };
        cli!(info, "已重新进入运行状态");
    }
}

/// `fuyubot plugin list`：**纯静态**列出插件目录里的插件（不 dlopen、不建 Kernel）。
///
/// 四段（契约 §6）：
/// 1. 原生插件包：`id  version  ABI n|?  kind  path`；
/// 2. 旧形态 dll：`文件名  legacy  path`；
/// 3. 脚本插件：`scripts/<dir>` 有 `plugin.json` 就按它列，没有就按
///    `package.json` / `pyproject.toml` 的 fuyubot 身份段列；
/// 4. 已禁用：`.disabled/<id>/` 与 `*.disable`。
///
/// 单个包坏掉（清单读不了 / 入口不在 / 平台不匹配）只打一行原因，继续列其余的。
fn plugin_list(args: PluginDirArgs) -> Result<(), Box<dyn std::error::Error>> {
    let root = project_root(&parent_or_dot(&args.config)).unwrap_or_else(|| PathBuf::from("."));
    let config = scaffold::load_config(&args.config)?;
    let plugin_dir = args.plugin_dir.unwrap_or_else(|| config.plugin_dir(&root));

    let entries = match fuyubot_loader::plugin_entries(&plugin_dir) {
        Ok(entries) => entries,
        Err(error) => {
            cli!(warn, "读不了插件目录 {}: {error}", plugin_dir.display());
            Vec::new()
        }
    };
    let mut bundles = Vec::new();
    let mut legacy = Vec::new();
    for entry in entries {
        match entry {
            PluginEntry::Bundle(dir) => bundles.push(dir),
            PluginEntry::Legacy(path) => legacy.push(path),
        }
    }

    // ① 原生插件包
    cli!(info, "原生插件包（{} 个）：", bundles.len());
    if bundles.is_empty() {
        cli!(info, "  （无）");
    }
    for dir in &bundles {
        match BundleManifest::load(dir) {
            Ok(manifest) => {
                // 平台 / 入口在列举时就判：装了一个跑不起来的包，越早知道越好。
                if let Err(error) = manifest.validate(dir) {
                    cli!(warn, "  跳过 {}：{error}", dir.display());
                    continue;
                }
                let abi = manifest
                    .abi
                    .map(|abi| format!("ABI {abi}"))
                    .unwrap_or_else(|| "ABI ?".to_owned());
                cli!(
                    info,
                    "  {}\t{}\t{}\t{}\t{}",
                    manifest.id,
                    manifest.version,
                    abi,
                    manifest.kind.as_str(),
                    dir.display()
                );
            }
            Err(error) => cli!(warn, "  跳过 {}：{error}", dir.display()),
        }
    }

    // ② 旧形态平铺 dll（不探测：读不了元数据就直说，别 dlopen）
    cli!(
        info,
        "旧形态动态库（{} 个，建议 `fuyubot plugin migrate` 迁成插件包）：",
        legacy.len()
    );
    if legacy.is_empty() {
        cli!(info, "  （无）");
    }
    for path in &legacy {
        cli!(info, "  {}\tlegacy\t{}", file_name_of(path), path.display());
    }

    // ③ 脚本插件（由 fuyubot.script 插件托管；这里只读静态清单）
    let rows = script_plugin_rows(&plugin_dir.join(fuyubot_loader::SCRIPTS_DIR));
    cli!(
        info,
        "脚本插件（{} 个，由 fuyubot.script 托管）：",
        rows.len()
    );
    if rows.is_empty() {
        cli!(info, "  （无）");
    }
    for row in rows {
        cli!(info, "  {row}");
    }

    // ④ 已禁用
    print_disabled_plugins(&plugin_dir);
    Ok(())
}

/// 脚本插件的静态行：`id  version  kind  entry  path`。
///
/// 有 `plugin.json` 的按清单列；老脚本目录按 `package.json` / `pyproject.toml`
/// 的 fuyubot 身份段列（[`scaffold::script_metadata`]），都没有的目录不是插件。
fn script_plugin_rows(scripts_dir: &Path) -> Vec<String> {
    let mut rows = Vec::new();
    let mut seen = BTreeSet::new();
    for dir in fuyubot_loader::script_bundles(scripts_dir).unwrap_or_default() {
        seen.insert(dir.clone());
        match BundleManifest::load(&dir) {
            Ok(manifest) => rows.push(format!(
                "{}\t{}\t{}\t{}\t{}",
                manifest.id,
                manifest.version,
                manifest.kind.as_str(),
                manifest.entry,
                dir.display()
            )),
            Err(error) => cli!(warn, "  跳过 {}：{error}", dir.display()),
        }
    }
    let Ok(entries) = std::fs::read_dir(scripts_dir) else {
        return rows;
    };
    let mut rest: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_dir()
                && !seen.contains(path)
                && path.file_name().is_some_and(|name| {
                    let name = name.to_string_lossy();
                    !name.starts_with('.') && !name.ends_with(fuyubot_loader::DISABLED_SUFFIX)
                })
        })
        .collect();
    rest.sort();
    for dir in rest {
        if let Some(meta) = scaffold::script_metadata(&dir) {
            rows.push(format!(
                "{}\t{}\t{}\t{}\t{}",
                meta.id,
                meta.version,
                meta.kind.as_str(),
                meta.entry,
                dir.display()
            ));
        }
    }
    rows
}

/// 被禁用（`.disabled/<id>/` 或 `*.disable`）的插件单独列一段。
///
/// 不说的话，面板里关掉一个插件之后 `plugin list` 里它就凭空消失了——用户会以为
/// 文件丢了。这里不 dlopen 它们（禁用就是不想让它被加载），只读清单 / 文件名。
fn print_disabled_plugins(plugin_dir: &Path) {
    let mut lines = Vec::new();
    let mut dirs: Vec<PathBuf> = Vec::new();

    // 契约形态：整个包挪进 `plugins/.disabled/`。
    if let Ok(entries) = std::fs::read_dir(plugin_dir.join(fuyubot_loader::DISABLED_DIR)) {
        dirs.extend(
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| path.is_dir()),
        );
    }
    // 旧面板形态：目录改名为 `<id>.disable`，文件留在原地。
    if let Ok(entries) = std::fs::read_dir(plugin_dir) {
        dirs.extend(
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| {
                    path.is_dir()
                        && path.file_name().is_some_and(|name| {
                            name.to_string_lossy()
                                .ends_with(fuyubot_loader::DISABLED_SUFFIX)
                        })
                }),
        );
    }
    dirs.sort();
    dirs.dedup();

    for dir in dirs {
        let label = if fuyubot_loader::is_bundle_dir(&dir) {
            match BundleManifest::load(&dir) {
                Ok(manifest) => format!(
                    "{}\t{}\t{}",
                    manifest.id,
                    manifest.version,
                    manifest.kind.as_str()
                ),
                Err(error) => {
                    cli!(warn, "  跳过 {}：{error}", dir.display());
                    continue;
                }
            }
        } else if let Some(meta) = scaffold::script_metadata(&dir) {
            format!("{}\t{}\t{}", meta.id, meta.version, meta.kind.as_str())
        } else {
            file_name_of(&dir)
        };
        lines.push(format!("  {label}\t{}（已禁用）", dir.display()));
    }

    // 旧形态：`foo.dll.disable` 文件。
    if let Ok(entries) = std::fs::read_dir(plugin_dir) {
        let mut files: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.is_file() && fuyubot_loader::is_disabled(path))
            .collect();
        files.sort();
        for path in files {
            lines.push(format!(
                "  {}\tlegacy\t{}（已禁用）",
                file_name_of(&path),
                path.display()
            ));
        }
    }

    if lines.is_empty() {
        return;
    }
    cli!(
        info,
        "已禁用 {} 个（重启后生效；删掉 {} 或改成原名就恢复）：",
        lines.len(),
        fuyubot_loader::DISABLED_SUFFIX
    );
    for line in lines {
        cli!(info, "{line}");
    }
}

/// 路径的文件名（非 UTF-8 用替换字符，够展示）。
fn file_name_of(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn plugin_install(args: InstallArgs) -> Result<(), Box<dyn std::error::Error>> {
    let root = project_root(&parent_or_dot(&args.config)).unwrap_or_else(|| PathBuf::from("."));
    let config = scaffold::load_config(&args.config)?;
    let plugin_dir = args.plugin_dir.unwrap_or_else(|| config.plugin_dir(&root));

    for path in scaffold::install(&args.path, &plugin_dir)? {
        if path.is_dir() {
            cli!(
                info,
                "已安装插件包 {}（`fuyubot plugin list` 可见；重启后生效）",
                path.display()
            );
        } else {
            cli!(
                info,
                "已安装旧形态插件 {}（重启后生效；迁成插件包用 `fuyubot plugin migrate`）",
                path.display()
            );
        }
    }
    Ok(())
}

/// `fuyubot plugin uninstall <名字>`：删脚本目录或动态库（连同伴文件）。
///
/// **不接正在跑的机器人**：删掉文件之后，宿主侧的目录 watch 会自己发现
/// 「少了一个目录」并卸载对应脚本（设计文档 S3）。所以这条命令对运行中的
/// 实例同样安全，不需要先 stop。
fn plugin_uninstall(args: UninstallArgs) -> Result<(), Box<dyn std::error::Error>> {
    let root = project_root(&parent_or_dot(&args.config)).unwrap_or_else(|| PathBuf::from("."));
    let config = scaffold::load_config(&args.config)?;
    let plugin_dir = args.plugin_dir.unwrap_or_else(|| config.plugin_dir(&root));

    for path in scaffold::uninstall(&args.name, &plugin_dir)? {
        cli!(
            info,
            "已卸载 {}（正在跑的实例要重启才彻底放手）",
            path.display()
        );
    }
    Ok(())
}

/// `fuyubot plugin migrate [--dry-run]`：把旧形态插件迁成插件包。
///
/// 逻辑全在 [`scaffold::migrate`]（可单测）；这里只把报告逐行打出来。
fn migrate_plugins(args: MigrateArgs) -> Result<(), Box<dyn std::error::Error>> {
    let root = project_root(&parent_or_dot(&args.config)).unwrap_or_else(|| PathBuf::from("."));
    let config = scaffold::load_config(&args.config)?;
    let plugin_dir = args.plugin_dir.unwrap_or_else(|| config.plugin_dir(&root));

    if args.dry_run {
        cli!(info, "--dry-run：只打印计划，不改动任何文件");
    }
    for line in scaffold::migrate(&plugin_dir, args.dry_run)? {
        cli!(info, "{line}");
    }
    Ok(())
}

/// `run` / `login` 的自动初始化：缺配置文件就照模板写一份，插件目录不在就建。
///
/// **刻意不复用 `init()`**：那个命令在文件已存在时报错（对显式 `init` 是对的），
/// 而 `run` 要的是幂等——「有就直接跑，没有就建好再跑」，这样用户少一步。
///
/// 只处理**默认文件名**：`--config` 指了别的名字就是显式意图，不替用户凭空造一个。
///
/// `api_id_override` 是命令行的 `--api-id`（非秘密）：模板里顺手写上，省一次手填。
/// 优先级与 [`resolve_parameters`] 一致：`--api-id` → `TG_API_ID` → 0（让 `run` 再问）。
fn ensure_project(
    root: &Path,
    configured: &Path,
    api_id_override: Option<i64>,
) -> Result<(), Box<dyn std::error::Error>> {
    if configured.is_file() {
        return Ok(());
    }
    if configured != Path::new(scaffold::CONFIG_FILE) {
        return Err(format!(
            "{} 不存在（自动初始化只生成默认的 {}）",
            configured.display(),
            scaffold::CONFIG_FILE
        )
        .into());
    }
    // `TG_API_ID` 不是秘密，顺手带进模板省得用户手填；名字允许用 FUYUBOT_NAME 覆盖。
    let name = std::env::var("FUYUBOT_NAME").unwrap_or_else(|_| "fuyubot".to_owned());
    let api_id = api_id_override
        .or_else(|| {
            std::env::var("TG_API_ID")
                .ok()
                .and_then(|value| value.parse::<i64>().ok())
        })
        .unwrap_or(0);
    scaffold::init(root, &name, api_id)?;
    cli!(
        info,
        "没有配置，已按默认模板生成 {}（等价于先跑一次 `fuyubot init`）",
        root.join(scaffold::CONFIG_FILE).display()
    );
    Ok(())
}

/// 控制台交互式提问者：登录要问 Bot Token / 手机号 / 验证码 / 两步验证密码。
///
/// 凭据只有两个来源（硬约束 41）：**环境变量**或**交互式输入**。这里两者都支持：
/// - `TG_BOT_TOKEN` 有值时，Bot Token 那一问直接用环境变量、**不问**（CI / 验收用）。
/// - 其余一律读 stdin。
///
/// 凭据**不落盘、不进日志**：`ask` 的答案除了交给 TDLib 之外不做任何记录。
#[derive(Default)]
struct ConsolePrompter {
    /// 这次登录**有没有真的问过用户**。
    ///
    /// 用来区分「session 还有效 → TDLib 直接进 `Ready`」与「走完了完整登录」：
    /// 前者要日志提醒一句（变更 3-a 第 3 步 / 3-b 的登录期判据），否则用户会以为
    /// 自己刚输入的凭据被用了。
    asked: bool,
}

impl fuyubot_tdlib::Prompter for ConsolePrompter {
    fn ask(&mut self, question: &str) -> Result<String, fuyubot_tdlib::LoginError> {
        self.asked = true;
        // bot token 允许整个从环境变量来：CI / 无交互环境下也登得进去。
        if question.starts_with("Bot Token") {
            if let Ok(token) = std::env::var("TG_BOT_TOKEN") {
                if !token.trim().is_empty() {
                    cli!(info, "{question}：用环境变量 TG_BOT_TOKEN（不回显）");
                    return Ok(token);
                }
            }
        }
        print!("{question}：");
        let _ = std::io::Write::flush(&mut std::io::stdout());
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).unwrap_or(0) == 0 {
            return Err(fuyubot_tdlib::LoginError::NoInput {
                question: question.to_owned(),
            });
        }
        Ok(line.trim().to_owned())
    }

    /// 让用户挑登录方式（只有 user 账号会走到）。
    fn choose_login_method(
        &mut self,
    ) -> Result<fuyubot_tdlib::LoginMethod, fuyubot_tdlib::LoginError> {
        cli!(info, "这个账号是 user 类型，选登录方式：");
        cli!(info, "  1) 手机号（收验证码）");
        cli!(info, "  2) 二维码（手机 Telegram 扫码）");
        match self.ask("输入 1 或 2")?.trim() {
            "1" => Ok(fuyubot_tdlib::LoginMethod::PhoneNumber),
            "2" => Ok(fuyubot_tdlib::LoginMethod::QrCode),
            other => Err(fuyubot_tdlib::LoginError::NoInput {
                question: format!("登录方式只能选 1 或 2，给的是「{other}」"),
            }),
        }
    }

    fn notify(&mut self, message: &str) {
        cli!(info, "{message}");
    }
}

/// `fuyubot-app` 的控制台宿主：把「状态 / 提问 / 二维码」接到 stderr / stdin。
///
/// 与 [`ConsolePrompter`] 的关系：后者是 `fuyubot-tdlib` 的 `Prompter`（`login` 子命令
/// 里 `resolve_parameters` 提问 api_id / api_hash 还在用它）；这里是编排层要的
/// `AppHost`，内部直接复用同一套提问输出——同一份体验，没有第二套问法。
struct ConsoleHost {
    prompter: Mutex<ConsolePrompter>,
}

impl ConsoleHost {
    fn new() -> Self {
        Self {
            prompter: Mutex::new(ConsolePrompter::default()),
        }
    }
}

impl fuyubot_app::AppHost for ConsoleHost {
    fn state(&self, state: fuyubot_app::AppState, detail: &str) {
        if state == fuyubot_app::AppState::Failed {
            cli!(error, "{detail}");
        } else {
            cli!(info, "{detail}");
        }
    }

    fn ask(&self, _kind: fuyubot_app::AskKind, prompt: &str) -> Option<String> {
        let mut prompter = self.prompter.lock().unwrap_or_else(|e| e.into_inner());
        <ConsolePrompter as fuyubot_tdlib::Prompter>::ask(&mut prompter, prompt).ok()
    }

    fn qr(&self, link: &str) {
        cli!(info, "扫码登录：{link}");
    }
}

/// api_id 是从哪一路来的（只用来打一句「凭据已就位」与写单测）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApiIdSource {
    /// 命令行 `--api-id`（缺口 24）。
    Cli,
    /// 环境变量 `TG_API_ID`。
    Env,
    /// 配置文件 `[telegram].api_id`（旧位置 `[bot].api_id` 也算）。
    Config,
    /// 都没有，得交互式问。
    Asked,
}

impl ApiIdSource {
    fn describe(self) -> &'static str {
        match self {
            Self::Cli => "命令行 --api-id",
            Self::Env => "环境变量 TG_API_ID",
            Self::Config => "配置 [telegram].api_id",
            Self::Asked => "交互输入",
        }
    }
}

/// 按优先级挑 api_id：**命令行 → 环境变量 → 配置 → 问用户**（返回值 `0` = 得问）。
///
/// 纯函数：环境变量由调用方读好传进来。单测**不许**去改进程环境——`cargo test` 的
/// 多个测试线程共享同一个环境，`set_var` 会让别的测试随机变红。
///
/// `0` 一律当「没给」：模板里 `api_id = 0` 是占位，CI 里 `TG_API_ID=` 也常留空。
fn pick_api_id(cli: Option<i64>, env: Option<i64>, configured: i64) -> (i64, ApiIdSource) {
    let usable = |id: i64| id != 0;
    if let Some(id) = cli.filter(|id| usable(*id)) {
        return (id, ApiIdSource::Cli);
    }
    if let Some(id) = env.filter(|id| usable(*id)) {
        return (id, ApiIdSource::Env);
    }
    if usable(configured) {
        return (configured, ApiIdSource::Config);
    }
    (0, ApiIdSource::Asked)
}

/// 校验命令行的 `--api-id`：**显式写错就报错**，不静默退回配置。
///
/// `0` / 负数当成「填错了」而不是「没写」——用户既然写了参数，得到一个 0 只会更困惑
/// （`0` 在模板里是占位，在真实调用里必然失败）。
fn checked_cli_api_id(cli: Option<i64>) -> Result<Option<i64>, String> {
    match cli {
        Some(id) if id <= 0 => Err(format!(
            "`--api-id {id}` 不合法：api_id 是正整数（到 https://my.telegram.org 申请一个）"
        )),
        other => Ok(other),
    }
}

/// 把登录要用的 TDLib 参数凑齐：**缺什么就交互式问用户**，问到的**回写配置**。
///
/// 优先级（缺口 24 定的）：
/// - `api_id`：**命令行 `--api-id` → 环境变量 `TG_API_ID` → 配置 → 交互式提问**
///   （`login` / `run` 自己问，不让用户先去翻文档找配置项）；
/// - `api_hash`：环境变量 `TG_API_HASH` → 配置里已保存的 → 交互式提问。
///   **没有**对应命令行参数（硬约束 41）。
/// - `api_id` **不是**凭据（约束 41 只禁 Bot Token / 手机号 / 密码），环境变量与配置都认；
/// - `api_hash` 是凭据，但**每次冷启动都要随 `setTdlibParameters` 发给 TDLib**
///   （session 还有效也照样要发），不记住就得每次手输——所以环境变量与
///   配置里已保存的都认，**交互问出来的回写** `[telegram].api_hash` 明文一行。
///
/// ⚠️ 回写只发生在「这次真的问过」时：命令行 / 环境变量 / 配置来的值不动文件，
/// 「我没让它改配置它却改了」比多问一次更让人困惑。
fn resolve_parameters(
    config: &scaffold::Config,
    config_path: &Path,
    root: &Path,
    cli_api_id: Option<i64>,
) -> Result<fuyubot_tdlib::TdlibParameters, Box<dyn std::error::Error>> {
    let mut prompter = ConsolePrompter::default();
    // 这两个只有**交互问出来**才有值——它们是回写的唯一依据。
    let mut asked_id: Option<i64> = None;
    let mut asked_hash: Option<String> = None;

    let cli_api_id = checked_cli_api_id(cli_api_id)?;
    let id_from_env = std::env::var("TG_API_ID")
        .ok()
        .and_then(|value| value.parse::<i64>().ok());
    let (picked_id, id_source) =
        pick_api_id(cli_api_id, id_from_env, config.telegram.api_id(&config.bot));
    let api_id = if picked_id != 0 {
        picked_id
    } else {
        let answer = prompter.ask(
            "api_id（到 https://my.telegram.org 申请；它不是秘密，也可以填进 [telegram].api_id \
             或命令行 --api-id）",
        )?;
        let parsed = answer.parse::<i64>().map_err(|_| {
            format!("api_id 得是数字，给的是「{answer}」——到 https://my.telegram.org 申请一个")
        })?;
        asked_id = Some(parsed);
        parsed
    };
    let api_id = i32::try_from(api_id)
        .map_err(|_| format!("api_id {api_id} 超出 32 位范围，检查一下是不是填错了"))?;

    let hash_from_env = std::env::var("TG_API_HASH")
        .ok()
        .filter(|value| !value.trim().is_empty());
    let saved_hash = scaffold::load_api_hash(config_path);
    let api_hash = match (&hash_from_env, &saved_hash) {
        (Some(value), _) => value.clone(),
        (None, Some(saved)) => saved.clone(),
        (None, None) => {
            cli!(info, "没找到环境变量 TG_API_HASH，配置里也没存过。");
            let answer = prompter.ask("api_hash（从 https://my.telegram.org 拿，输入不回显）")?;
            asked_hash = Some(answer.clone());
            answer
        }
    };
    // 空值必须在**发请求之前**挡住：TDLib 会回 `Valid api_hash must be provided`，
    // 而状态机对这条错误会一直重试到预算耗尽（实测空转满 600s 才超时）。
    if api_hash.trim().is_empty() {
        return Err("api_hash 是空的——到 https://my.telegram.org 拿一个；\
                    它走环境变量 TG_API_HASH、配置里已保存的值，或交互式输入"
            .into());
    }

    // **配置与凭据都齐了就直接进下一步**：一次都没问过就说清凭据是从哪来的，
    // 免得用户以为「没提问 = 凭据没被用上」。
    if !prompter.asked {
        let hash_src = if hash_from_env.is_some() {
            "环境变量 TG_API_HASH"
        } else if saved_hash.is_some() {
            "配置 [telegram].api_hash（上次输入时保存的）"
        } else {
            "交互输入"
        };
        cli!(
            info,
            "api_id 与 api_hash 都已就位（{} + {hash_src}），跳过初始化与提问",
            id_source.describe()
        );
    }

    // 交互问出来的才落盘：下次冷启动不用再输一遍（**每次**都要随 `setTdlibParameters`
    // 发这两个值，session 有效也一样）。写盘失败只警告——记住失败不该让机器人起不来。
    if asked_id.is_some() || asked_hash.is_some() {
        match scaffold::remember_credentials(config_path, asked_id, asked_hash.as_deref()) {
            Ok(true) => {
                cli!(
                    info,
                    "已写回 {}（下次不用再输这两项）",
                    config_path.display()
                );
                if asked_hash.is_some() {
                    cli!(
                        warn,
                        "⚠️ api_hash 是凭据，现在以**明文**放在上面的文件里——\
                         别把它提交到版本库、也别整份发给别人"
                    );
                }
            }
            Ok(false) => {}
            Err(e) => cli!(warn, "写回 api_id / api_hash 失败（不影响本次运行）：{e}"),
        }
    }

    let data_dir = config.telegram.data_dir(root);
    std::fs::create_dir_all(&data_dir)?;
    Ok(fuyubot_tdlib::TdlibParameters::new(
        &data_dir, api_id, api_hash,
    ))
}

/// 运行期那条判据的**唯一**实现（变更 3-b）：这条更新是不是「被下线 / 封禁」的信号？
///
/// 返回「要用户再出一次凭据」的那个状态名；不是就返回 `None`。
/// 抽成纯函数是为了能离线测——`run` 里那个 `loop` 没法在单测里驱动，而这条判据错了
/// 会导致两种都很糟的后果：漏判（账号被踢了还在假装收更新）或误判（正常状态被当成掉线，
/// 反复弹登录）。
fn re_login_alert(payload: &serde_json::Value) -> Option<String> {
    let state = fuyubot_tdlib::AuthState::from_update(payload);
    state.needs_re_login().then(|| state.name().to_owned())
}

/// `updateConnectionState` → 面板 / `status` 要显示的那个连接状态。
///
/// 负载长这样：`{"@type":"updateConnectionState","state":{"@type":"connectionStateReady"}}`——
/// 状态在 **`state`** 里，不在顶层（与 `updateAuthorizationState` 的 `authorization_state` 不同）。
/// 认不出的状态（TDLib 以后新增的）返回 `None`：保留上一条已知状态，
/// 比把它当「已连接」安全。
fn connection_update_state(payload: &serde_json::Value) -> Option<ConnectionState> {
    payload.get("state").and_then(ConnectionState::from_json)
}

/// `updateAuthorizationState` → 要写进 status 的状态名（`authorizationStateReady` 等）。
///
/// 负载形如 `{"@type":"updateAuthorizationState","authorization_state":{"@type":"authorizationStateReady"}}`，
/// 与连接状态一样**嵌套**。取不出名字时返回 `None`：保留上一条已知状态，别把空串写进 status。
/// 认不出的新状态保留原始名字——面板显示「未知状态（名字）」比假装「已登录」安全。
fn authorization_update_name(payload: &serde_json::Value) -> Option<String> {
    let state = AuthState::from_update(payload);
    (!state.name().is_empty()).then(|| state.name().to_owned())
}

/// 登录成功之后补记 `authorizationStateReady`。
///
/// `log_in_with` 在登录窗口里**自己消费**了 `updateAuthorizationState`（只把非授权更新
/// 转给管线），所以事件总线收不到最后那条 Ready；而 `drive_login` 只在
/// `LoginOutcome::Ready` 时返回 `Ok`，因此「正常返回」本身就是权威判据。
fn mark_authorized(slot: &Mutex<Option<String>>) {
    *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(AuthState::Ready.name().to_owned());
}

/// 宿主手里那份「我是谁」→ status / 面板要显示的账户行。
///
/// 宿主在登录时写入 `getMe`，之后每条 `updateUser`（id 是自己那条）都会替换掉它，
/// 所以这里读到的是**最新的**名字 / 用户名，不是登录那一刻的死快照。
fn current_account(runtime: &fuyubot_loader::HostRuntime) -> Option<instance::Account> {
    runtime
        .self_user()
        .as_ref()
        .and_then(instance::Account::from_user)
}

/// `--replay` 的等待窗口要按「有没有脚本插件」选。
///
/// 判据基于 [`plugin_entries`]（契约 §4）：包看 `kind`，旧形态 dll 看同主干的
/// 同伴脚本；另外 `plugins/scripts/` 非空也算——脚本由 `fuyubot.script` 在**另一个进程**
/// 里跑，回放完要等它经管道回来。
fn has_script_plugins(plugin_dir: &Path) -> bool {
    let entries = fuyubot_loader::plugin_entries(plugin_dir).unwrap_or_default();
    if entries.iter().any(|entry| match entry {
        PluginEntry::Bundle(dir) => {
            BundleManifest::load(dir).is_ok_and(|manifest| manifest.kind.is_script())
        }
        PluginEntry::Legacy(path) => find_script(path).is_some(),
    }) {
        return true;
    }
    std::fs::read_dir(plugin_dir.join(fuyubot_loader::SCRIPTS_DIR)).is_ok_and(|mut entries| {
        entries.any(|entry| entry.is_ok_and(|entry| entry.path().is_dir()))
    })
}

/// 把一批更新灌进管线：不需要 Telegram 账号，也能验插件的判断与回投。
///
/// 文件里可以是一个 JSON 对象，也可以是一个对象数组。
fn replay_updates(
    pipeline: &Arc<UpdatePipeline>,
    file: &Path,
    runtime: &fuyubot_loader::HostRuntime,
    scripted: bool,
    quiet_override: Option<u64>,
) -> Result<(), Box<dyn std::error::Error>> {
    let text =
        std::fs::read_to_string(file).map_err(|e| format!("读不了 {}: {e}", file.display()))?;
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| format!("{} 不是合法 JSON: {e}", file.display()))?;
    let updates = match value {
        serde_json::Value::Array(items) => items,
        other => vec![other],
    };

    for update in &updates {
        let update_type = update.get("@type").and_then(|v| v.as_str()).unwrap_or("?");
        let outcome = pipeline.process_value(update.clone())?;
        cli!(info, "回放 {update_type} → {outcome:?}");
    }

    // 脚本插件是**另一个进程**：事件要经管道过去、反应要经管道回来，还可能捎带
    // 网络请求。回放完立刻打印，会把它们的日志与请求整批漏掉——所以先等它们安静下来。
    wait_until_quiet(
        runtime,
        quiet_override.unwrap_or(if scripted { 2000 } else { 150 }),
    );

    cli!(info, "插件打出来的日志：");
    for line in runtime.log_lines() {
        cli!(info, "  {line}");
    }
    cli!(info, "插件发出去的请求：");
    for request in runtime.sent_requests() {
        cli!(
            info,
            "  #{} {} {}",
            request.id,
            request.method,
            request.params
        );
    }
    Ok(())
}

/// 等到「连续一段时间没有新的日志/请求」为止（或到上限）。
///
/// 判据就是宿主自己那两份留档的长度：`HostRuntime` 没接 TDLib 时请求只留档，
/// 正好拿来当「插件有没有反应」的观察窗。
///
/// `scripted` 为真时窗口放宽：脚本插件可能要发网络请求，那种安静是**正常的**，
/// 等太短会把最终答复漏掉。
fn wait_until_quiet(runtime: &fuyubot_loader::HostRuntime, quiet_ms: u64) {
    let quiet = std::time::Duration::from_millis(quiet_ms);
    const CAP: std::time::Duration = std::time::Duration::from_secs(60);

    let started = std::time::Instant::now();
    let mut seen = (runtime.log_lines().len(), runtime.sent_requests().len());
    let mut last_change = std::time::Instant::now();
    while started.elapsed() < CAP {
        std::thread::sleep(std::time::Duration::from_millis(50));
        let now = (runtime.log_lines().len(), runtime.sent_requests().len());
        if now != seen {
            seen = now;
            last_change = std::time::Instant::now();
        } else if last_change.elapsed() >= quiet {
            return;
        }
    }
}

/// 测试用的小工具（只在 `cargo test` 里编译）。
#[cfg(test)]
pub(crate) mod test_support {
    use std::path::PathBuf;

    /// 测试用的临时目录。
    ///
    /// ⚠️ **不许用 `std::env::temp_dir()`**：MSYS（git bash）会把 `TEMP` 改成 `/tmp`，
    /// 这个值在 Windows 上解析成「**当前盘的根**」（`F:\tmp`），在盘根建目录会被拒绝
    /// （`OS error 5 PermissionDenied`）——于是一批测试**换个 shell 就红**（踩过一次）。
    ///
    /// 放 workspace 的 `target/` 下：一定可写、不依赖任何环境变量、也不污染系统临时目录。
    pub fn temp(name: &str) -> PathBuf {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-tmp")
            .join(env!("CARGO_PKG_NAME"))
            .join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建测试临时目录失败");
        dir
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    /// 测试里取值用：`command` 是 `Option`（省略 = 开面板），但这些用例都显式写了子命令。
    fn command_of(cli: Cli) -> Command {
        cli.command.expect("这次解析应当带子命令")
    }

    #[test]
    fn the_cli_definition_is_valid() {
        // clap 的 `debug_assert` 会检查短选项冲突之类的定义错误。
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }

    /// `./fuyubot`（不带子命令）= 直接开面板；子命令一个都不能因此被抢走。
    ///
    /// 这条同时钉住「省略子命令」这一种**合法**输入：以前它会因为 `command` 是必填的
    /// 而打成 usage 退出（双击 exe 得到一屏用法），现在等价于 `run --tui`。
    #[test]
    fn no_subcommand_means_the_panel() {
        let cli = Cli::try_parse_from(["fuyubot"]).expect("不带参数应当解析成功");
        assert!(
            cli.command.is_none(),
            "不带子命令不该落成某个具体的 Command"
        );

        // 真跑一遍分派入口：非 TTY 下要报「需要真终端」，而不是 usage。
        let error = dispatch(cli).expect_err("测试里没有真终端，面板应当拒绝启动");
        assert!(
            error.to_string().contains("需要真终端"),
            "应当报终端限制，实际是：{error}"
        );

        // 面板默认参数 = `run --tui` 的那一套（全默认值）。
        let args = RunArgs::default_for_tui();
        assert!(args.tui);
        assert!(!args.background && !args.no_tdlib && !args.no_login);
        assert_eq!(args.config, PathBuf::from(scaffold::CONFIG_FILE));
        assert_eq!(args.api_id, None);
        assert_eq!(args.replay, None);

        // 子命令照旧生效（不会被「省略」这条分支吃掉）。
        assert!(matches!(
            command_of(Cli::try_parse_from(["fuyubot", "run"]).unwrap()),
            Command::Run(_)
        ));
    }

    /// 面板开关：长写 `--tui`、短写 `-t`；与后台 / 回放互斥。
    #[test]
    fn run_accepts_the_tui_flag() {
        let parse = |args: &[&str]| Cli::try_parse_from(args.iter().map(OsString::from));

        for spelling in ["--tui", "-t"] {
            let cli = parse(&["fuyubot", "run", spelling]).expect("应当接受 {spelling}");
            match command_of(cli) {
                Command::Run(args) => assert!(args.tui, "{spelling} 没打开面板"),
                other => panic!("{spelling} 解析成了别的子命令：{other:?}"),
            }
        }

        // 面板要占终端：与后台 / 回放同时出现是用法错误。
        assert!(parse(&["fuyubot", "run", "--tui", "--background"]).is_err());
        assert!(parse(&["fuyubot", "run", "--tui", "--replay", "updates.json"]).is_err());

        // 无关参数原样保留。
        let cli = parse(&["fuyubot", "run", "--tui", "--api-id", "42"]).unwrap();
        match command_of(cli) {
            Command::Run(args) => {
                assert!(args.tui);
                assert_eq!(args.api_id, Some(42));
            }
            other => panic!("解析成了别的子命令：{other:?}"),
        }
    }

    /// 日志级别控制文件：只有白名单里的名字才生效，写坏了保持当前级别。
    #[test]
    fn log_level_control_file_is_only_applied_when_valid() {
        let root = test_support::temp("log-level-control");
        std::fs::create_dir_all(root.join(instance::STATE_DIR)).unwrap();

        assert_eq!(desired_log_level(&root, LogLevel::Info), LogLevel::Info);
        instance::request_log_level(&root, "trace").unwrap();
        assert_eq!(desired_log_level(&root, LogLevel::Info), LogLevel::Trace);
        // 已经是目标值：调用方用 `!=` 判断，不会每圈重复 set。
        assert_eq!(desired_log_level(&root, LogLevel::Trace), LogLevel::Trace);

        std::fs::write(root.join(instance::STATE_DIR).join("log-level"), "verbose").unwrap();
        assert_eq!(desired_log_level(&root, LogLevel::Debug), LogLevel::Debug);
    }

    /// 运行期判据（变更 3-b）：只有「要用户再出一次凭据」的状态才算被踢，
    /// `Ready` 与**我们自己退出的路上**那几个状态都不能算——否则正常停机会被当成掉线。
    #[test]
    fn only_credential_states_raise_the_re_login_alert() {
        let update = |state: &str| {
            serde_json::json!({
                "@type": "updateAuthorizationState",
                "authorization_state": { "@type": state }
            })
        };

        // 被下线 / 封禁会看到这几个：都要报警。
        for state in [
            "authorizationStateWaitPhoneNumber",
            "authorizationStateWaitCode",
            "authorizationStateWaitPassword",
            "authorizationStateWaitOtherDeviceConfirmation",
        ] {
            assert_eq!(
                re_login_alert(&update(state)).as_deref(),
                Some(state),
                "{state} 必须触发退回登录"
            );
        }

        // 正常状态与「主动退出」的那条路：都不能报警。
        for state in [
            "authorizationStateReady",
            "authorizationStateLoggingOut",
            "authorizationStateClosing",
            "authorizationStateClosed",
            "authorizationStateWaitTdlibParameters",
        ] {
            assert_eq!(
                re_login_alert(&update(state)),
                None,
                "{state} 不该被当成掉线"
            );
        }

        // 认不出的状态（TDLib 比我们新）也不能乱报警——`Unknown` 只进 Unknown 分支。
        assert_eq!(
            re_login_alert(&update("authorizationStateSomethingNew")),
            None
        );
        // 根本不是授权更新的负载（比如别的更新）要安全地返回 None。
        assert_eq!(
            re_login_alert(&serde_json::json!({"@type": "updateNewMessage"})),
            None
        );
    }

    /// `updateConnectionState` 的状态在 **`state`** 里；五种状态都要认得，
    /// 认不出 / 根本不是这条更新时返回 `None`（保留上一条已知状态，不默认 Ready）。
    #[test]
    fn connection_updates_are_read_from_the_state_field() {
        let update = |state: &str| {
            serde_json::json!({
                "@type": "updateConnectionState",
                "state": { "@type": state }
            })
        };

        for (name, expected) in [
            (
                "connectionStateWaitingForNetwork",
                ConnectionState::WaitingForNetwork,
            ),
            (
                "connectionStateConnectingToProxy",
                ConnectionState::ConnectingToProxy,
            ),
            ("connectionStateConnecting", ConnectionState::Connecting),
            ("connectionStateUpdating", ConnectionState::Updating),
            ("connectionStateReady", ConnectionState::Ready),
        ] {
            assert_eq!(
                connection_update_state(&update(name)),
                Some(expected),
                "{name} 没解析出来"
            );
        }

        assert_eq!(
            connection_update_state(&update("connectionStateSomethingNew")),
            None
        );
        // 状态是嵌套的：把 `@type` 放在顶层（别的更新的形状）不算数。
        assert_eq!(
            connection_update_state(
                &serde_json::json!({"@type": "updateConnectionState", "state": {}})
            ),
            None
        );
        assert_eq!(
            connection_update_state(&serde_json::json!({"@type": "updateConnectionState"})),
            None
        );
    }

    /// 授权状态也读嵌套的 `authorization_state`；空的 / 别的更新不能写出空串。
    #[test]
    fn authorization_updates_are_read_from_the_authorization_state_field() {
        let update = |state: &str| {
            serde_json::json!({
                "@type": "updateAuthorizationState",
                "authorization_state": { "@type": state }
            })
        };

        for name in [
            "authorizationStateWaitTdlibParameters",
            "authorizationStateWaitPhoneNumber",
            "authorizationStateWaitCode",
            "authorizationStateWaitPassword",
            "authorizationStateReady",
            "authorizationStateClosing",
        ] {
            assert_eq!(
                authorization_update_name(&update(name)).as_deref(),
                Some(name),
                "{name} 没解析出来"
            );
        }

        // 认不出的状态保留原始名字（面板会显示「未知状态（名字）」）。
        assert_eq!(
            authorization_update_name(&update("authorizationStateSomethingNew")).as_deref(),
            Some("authorizationStateSomethingNew")
        );
        // 缺 `authorization_state` / 根本不是这条更新：不写空串，保留上一条。
        assert_eq!(
            authorization_update_name(&serde_json::json!({"@type": "updateAuthorizationState"})),
            None
        );
        assert_eq!(
            authorization_update_name(&serde_json::json!({"@type": "updateNewMessage"})),
            None
        );
    }

    /// `log_in_with` 把登录期的授权更新吃掉了：登录成功后必须自己补记 Ready，
    /// 否则面板会一直显示「还没收到授权更新」。
    #[test]
    fn a_successful_login_records_the_ready_authorization_state() {
        let slot = Mutex::new(None);
        mark_authorized(&slot);
        assert_eq!(
            slot.into_inner().unwrap().as_deref(),
            Some("authorizationStateReady")
        );
    }

    #[test]
    fn subcommands_parse() {
        let cli = Cli::try_parse_from(["fuyubot", "plugin", "install", "x.dll"]).unwrap();
        assert!(matches!(
            command_of(cli),
            Command::Plugin(PluginArgs {
                command: PluginCommand::Install(_)
            })
        ));

        // `migrate` 是 **plugin 的子命令**（不是顶层），`--dry-run` 要能解析。
        let cli = Cli::try_parse_from(["fuyubot", "plugin", "migrate", "--dry-run"]).unwrap();
        match command_of(cli) {
            Command::Plugin(PluginArgs {
                command: PluginCommand::Migrate(args),
            }) => {
                assert!(args.dry_run);
                assert_eq!(args.config, PathBuf::from(scaffold::CONFIG_FILE));
                assert_eq!(args.plugin_dir, None);
            }
            other => panic!("解析成了 {other:?}"),
        }
        assert!(
            Cli::try_parse_from(["fuyubot", "migrate"]).is_err(),
            "migrate 应该在 plugin 下面（与 list / install / uninstall 一组）"
        );

        let cli = Cli::try_parse_from(["fuyubot", "run", "--replay", "u.json"]).unwrap();
        match command_of(cli) {
            Command::Run(args) => assert_eq!(args.replay.as_deref(), Some(Path::new("u.json"))),
            other => panic!("解析成了 {other:?}"),
        }

        // 开发跑的两个开关：`--log-level` 覆盖配置门槛，`--trace-updates` 打原始更新。
        let cli =
            Cli::try_parse_from(["fuyubot", "run", "--log-level", "trace", "--trace-updates"])
                .unwrap();
        match command_of(cli) {
            Command::Run(args) => {
                assert_eq!(args.log_level.as_deref(), Some("trace"));
                assert!(args.trace_updates);
            }
            other => panic!("解析成了 {other:?}"),
        }
        // 级别名写错（如 `verbose`）交给 `parse_log_level` 报错，不在 clap 这里。
        assert!(Cli::try_parse_from(["fuyubot", "run", "--log-level", "verbose"]).is_ok());

        // 来源过滤：可重复，顺序原样保留（打印「只看…」时按用户给的顺序）。
        let cli = Cli::try_parse_from([
            "fuyubot",
            "run",
            "--log-plugin",
            "fuyubot.demo",
            "--log-plugin",
            "fuyubot.start",
        ])
        .unwrap();
        match command_of(cli) {
            Command::Run(args) => assert_eq!(
                args.log_plugin,
                vec!["fuyubot.demo".to_owned(), "fuyubot.start".to_owned()]
            ),
            other => panic!("解析成了 {other:?}"),
        }

        // 后台运行：只是 `run` 的一个开关（`status` / `stop` 面对的是同一个实例）。
        let cli = Cli::try_parse_from(["fuyubot", "run", "--background"]).unwrap();
        match command_of(cli) {
            Command::Run(args) => assert!(args.background && !args.no_login),
            other => panic!("解析成了 {other:?}"),
        }
        assert!(matches!(
            command_of(Cli::try_parse_from(["fuyubot", "status"]).unwrap()),
            Command::Status(_)
        ));
        assert!(matches!(
            command_of(Cli::try_parse_from(["fuyubot", "stop"]).unwrap()),
            Command::Stop(_)
        ));
        assert!(matches!(
            command_of(Cli::try_parse_from(["fuyubot", "config"]).unwrap()),
            Command::Config(_)
        ));

        // 插件开发**不在**运行时 CLI 里（变更 12）——没有 fuyubot-sdk 了，示例在文档站。
        assert!(Cli::try_parse_from(["fuyubot", "new-plugin", "demo"]).is_err());
        // 插件也不归运行时 CLI 编（变更 5）：`build-plugin` 已经拆掉了。
        assert!(Cli::try_parse_from(["fuyubot", "build-plugin"]).is_err());
    }

    /// 缺口 22 的两个新子命令 + 缺口 24 的 `--api-id`：解析结果要是我们想的那样。
    ///
    /// 同时钉住「**没有**凭据参数」这条硬约束 41：`--api-hash` / `--token` 必须解析失败
    /// （谁想加它们，这条会先红）。
    #[test]
    fn login_logout_and_api_id_parse() {
        // login：默认配置文件 + 可选 --api-id（非秘密，可以写在命令行）。
        let cli = Cli::try_parse_from(["fuyubot", "login"]).unwrap();
        match command_of(cli) {
            Command::Login(args) => {
                assert_eq!(args.config, PathBuf::from(scaffold::CONFIG_FILE));
                assert_eq!(args.api_id, None);
            }
            other => panic!("解析成了 {other:?}"),
        }
        let cli = Cli::try_parse_from(["fuyubot", "login", "--api-id", "42"]).unwrap();
        match command_of(cli) {
            Command::Login(args) => assert_eq!(args.api_id, Some(42)),
            other => panic!("解析成了 {other:?}"),
        }

        // logout：默认是**预览**，`--yes` 才是真删。
        let cli = Cli::try_parse_from(["fuyubot", "logout"]).unwrap();
        match command_of(cli) {
            Command::Logout(args) => assert!(!args.yes, "默认绝不能是删除"),
            other => panic!("解析成了 {other:?}"),
        }
        let cli = Cli::try_parse_from(["fuyubot", "logout", "--yes"]).unwrap();
        match command_of(cli) {
            Command::Logout(args) => assert!(args.yes),
            other => panic!("解析成了 {other:?}"),
        }

        // run 也认 --api-id（缺口 24），并且和别的开关能一起给。
        let cli = Cli::try_parse_from(["fuyubot", "run", "--api-id", "7", "--no-login"]).unwrap();
        match command_of(cli) {
            Command::Run(args) => {
                assert_eq!(args.api_id, Some(7));
                assert!(args.no_login);
            }
            other => panic!("解析成了 {other:?}"),
        }

        // 硬约束 41：凭据**没有**命令行参数。
        assert!(
            Cli::try_parse_from(["fuyubot", "login", "--api-hash", "x"]).is_err(),
            "api_hash 是凭据，不许有命令行参数"
        );
        assert!(
            Cli::try_parse_from(["fuyubot", "run", "--token", "x"]).is_err(),
            "Bot Token 是凭据，不许有命令行参数"
        );
        assert!(
            Cli::try_parse_from(["fuyubot", "logout", "--api-id", "1"]).is_err(),
            "logout 不需要 api_id"
        );
    }

    /// 缺口 24 的优先级：**命令行 → 环境变量 → 配置 → 交互提问**。
    ///
    /// 这里测纯函数而不是 `resolve_parameters`：后者要读进程环境，而 `cargo test`
    /// 的测试线程共享环境，`set_var` 会让同批别的测试随机变红。
    #[test]
    fn api_id_priority_is_cli_then_env_then_config_then_ask() {
        assert_eq!(pick_api_id(Some(7), Some(8), 9), (7, ApiIdSource::Cli));
        assert_eq!(pick_api_id(None, Some(8), 9), (8, ApiIdSource::Env));
        assert_eq!(pick_api_id(None, None, 9), (9, ApiIdSource::Config));
        // 哪一路都没有（配置里是模板占位 0）：返回 0 = 得问用户。
        assert_eq!(pick_api_id(None, None, 0), (0, ApiIdSource::Asked));
        // 命令行给了就用它，哪怕环境变量 / 配置里也有别的值。
        assert_eq!(pick_api_id(Some(7), None, 0), (7, ApiIdSource::Cli));
        assert_eq!(pick_api_id(Some(7), Some(8), 0), (7, ApiIdSource::Cli));
        // `0` 一律当「没给」：`TG_API_ID=` 留空、模板里的 `api_id = 0` 都是常见写法。
        assert_eq!(pick_api_id(None, Some(0), 9), (9, ApiIdSource::Config));
        assert_eq!(pick_api_id(Some(0), None, 9), (9, ApiIdSource::Config));

        // 显式写错的 --api-id 要报错，不能静默退回配置（那会让人以为参数生效了）。
        assert!(checked_cli_api_id(Some(0)).is_err());
        assert!(checked_cli_api_id(Some(-5)).is_err());
        assert!(checked_cli_api_id(None).unwrap().is_none());
        assert_eq!(checked_cli_api_id(Some(42)).unwrap(), Some(42));
    }

    /// `status` / `stop` 只认**已存在**的项目：从深层目录也能向上找到根（变更 6）。
    ///
    /// 「找不到就报错」那条分支是**手工**验的：`status --config nope\fuyubot.toml` →
    /// `Error: "nope 及其上级目录里没有 fuyubot.toml——先 `fuyubot init` 建一个"`、退出码 1。
    ///
    /// 相对路径用「CWD → 临时目录」的真实相对路径来测，不依赖仓库根恰好有一份
    /// `fuyubot.toml`（那份被挪进 `.fuyubot/` 之后，旧写法会红）。
    #[test]
    fn status_walks_up_and_resolves_relative_paths() {
        let root = test_support::temp("status-root");
        scaffold::init(&root, "mybot", 1).unwrap();
        let nested = root.join("plugins").join("somewhere");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(
            existing_root(&nested.join(scaffold::CONFIG_FILE)).unwrap(),
            root
        );

        // 默认的 `--config fuyubot.toml` 是相对路径：`find_config` 必须先把起点变绝对，
        // 否则从子目录往上走时会因为 `""` 没有父目录而谎报「这里没有配置」。
        let relative = relative_to(&std::env::current_dir().unwrap(), &nested);
        let found = scaffold::find_config(&relative).expect("相对路径必须能向上找到 fuyubot.toml");
        assert!(found.is_absolute() && found.is_file(), "{found:?}");
        assert_eq!(
            std::fs::canonicalize(found.parent().unwrap()).unwrap(),
            std::fs::canonicalize(&root).unwrap()
        );
    }

    /// `target` 相对 `base` 的路径（两边都按 `Path::components` 比较）。
    fn relative_to(base: &Path, target: &Path) -> PathBuf {
        let base: Vec<_> = base.components().collect();
        let target: Vec<_> = target.components().collect();
        let common = base
            .iter()
            .zip(&target)
            .take_while(|(left, right)| left == right)
            .count();
        let mut relative = PathBuf::new();
        for _ in common..base.len() {
            relative.push("..");
        }
        for component in &target[common..] {
            relative.push(component.as_os_str());
        }
        relative
    }

    /// `config` 打的是**生效值**：默认值要出现、写错的 `account_type` 要报出来、
    /// 文件不在要说明「全是默认值」——三件事都是「我改的那行生效了吗」的答案。
    #[test]
    fn config_prints_effective_values_including_defaults() {
        let root = test_support::temp("config-effective");
        scaffold::init(&root, "mybot", 12345).unwrap();
        let path = root.join(scaffold::CONFIG_FILE);

        let text =
            scaffold::config_lines(&path, &root, &scaffold::load_config(&path).unwrap()).join("\n");
        assert!(text.contains("name         = mybot"), "{text}");
        assert!(text.contains("account_type = bot"), "{text}");
        assert!(text.contains("api_id       = 12345"), "{text}");
        // 默认值本身也要出现：用户看不出「这行是我写的还是填的」才是坑。
        assert!(text.contains("log_verbosity= 1"), "{text}");
        assert!(
            text.contains(&root.join("data").display().to_string()),
            "{text}"
        );
        assert!(
            text.contains(&root.join("plugins").display().to_string()),
            "{text}"
        );

        // 写错的账号类型**不许**静默当 bot（那会一直等不到验证码）。
        std::fs::write(&path, "[telegram]\naccount_type = \"robot\"\n").unwrap();
        let bad =
            scaffold::config_lines(&path, &root, &scaffold::load_config(&path).unwrap()).join("\n");
        assert!(bad.contains("认不出来"), "{bad}");

        // 一份都没有：如实说默认值，而不是假装读到了文件。
        let gone = root.join("nope.toml");
        let text = scaffold::config_lines(&gone, &root, &scaffold::Config::default()).join("\n");
        assert!(text.contains("不存在，下面全是默认值"), "{text}");
    }

    #[test]
    fn a_new_background_instance_starts_with_an_empty_log() {
        let root = test_support::temp("fresh-background-log");
        let log = instance::log_path(&root);
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        std::fs::write(&log, "old run\nmore old\n").unwrap();
        reset_log_file(&log).unwrap();
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "");
    }

    /// 面板按 `s` 后 `x` 失效的根因：daemon 继承了 launcher 的 stdout / stderr 管道，
    /// 面板的 `Command::output()` 永远等不到 EOF。这里测的是修复用的机制本身——
    /// 一个本来可继承的句柄，清掉继承位之后查到的标志必须变。
    #[cfg(windows)]
    #[test]
    fn clearing_the_inherit_flag_is_observable() {
        use std::os::windows::io::AsRawHandle;

        let root = test_support::temp("handle-inherit");
        let file = std::fs::File::create(root.join("probe.txt")).unwrap();
        let raw = file.as_raw_handle() as *mut std::ffi::c_void;

        assert!(
            windows_handle_inheritance::make_inheritable(raw),
            "先把测试句柄标成可继承"
        );
        assert_eq!(windows_handle_inheritance::is_inheritable(raw), Some(true));
        assert!(windows_handle_inheritance::clear_handle_inherit(raw));
        assert_eq!(
            windows_handle_inheritance::is_inheritable(raw),
            Some(false),
            "清掉继承位之后，子进程就不该再收到这个句柄"
        );
    }
}
