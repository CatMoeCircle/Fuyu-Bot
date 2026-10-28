//! fuyubot 命令行入口（**运行时** CLI）。
//!
//! 这里不实现任何宿主函数：`HostApiVTable` 的实现全在 `fuyubot-loader` 里，
//! 本文件只负责把内核、宿主运行时、插件目录接起来。
//!
//! **插件开发的事不在这里**：脚手架由 `fuyubot-sdk init` 生成（见 `docs/decisions.md` 变更 4）。
//!
//! ```text
//! fuyubot init                              生成 fuyubot.toml + plugins/
//! fuyubot run [--background]                起框架、加载插件（后台运行 = 日志进 logs/fuyubot.log）
//! fuyubot status                            看这个目录上的实例在不在跑
//! fuyubot config                            打印**生效的**配置值（含默认值）
//! fuyubot stop                              停掉它
//! fuyubot plugin list                       列出插件目录里的插件
//! fuyubot plugin install <路径>             把一个插件装进插件目录
//! ```
//!
//! **本 CLI 不编译、不打包插件**（`docs/decisions.md` 变更 4/5）：插件由作者自己的语言工具链编，
//! 编好之后 `fuyubot plugin install <产物>` 只是**复制**进插件目录。以前的 `build-plugin`
//! 已按新规格拆掉。

mod instance;
mod scaffold;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clap::{Args, Parser, Subcommand};
use fuyubot_core::{Kernel, Plugin as _};
use fuyubot_loader::{DylibPlugin, PluginLoader, UpdatePipeline, plugin_files};
// 扩展 trait 要在作用域里才能对 `ConsolePrompter` 调 `ask`（固有方法不覆盖它）。
use fuyubot_tdlib::Prompter as _;

/// `fuyubot`：基于 TDLib 的插件化 bot 框架。
#[derive(Parser, Debug)]
#[command(name = "fuyubot", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// 初始化一个 fuyubot 项目（生成 fuyubot.toml 与插件目录）
    Init(InitArgs),
    /// 起框架、加载插件
    Run(RunArgs),
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
    /// api_hash 这个命令**不收**：`fuyubot run` 首次会问，问完写回配置
    /// （或事先设环境变量 `TG_API_HASH`，那就一次都不用输）。
    #[arg(long, default_value_t = 0)]
    api_id: i64,
}

#[derive(Args, Debug)]
struct RunArgs {
    /// 配置文件
    #[arg(long, default_value = scaffold::CONFIG_FILE)]
    config: PathBuf,
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
    /// 把一批更新的 JSON 灌进管线后退出：**不接 TDLib、不用账号**，纯离线验插件
    #[arg(long)]
    replay: Option<PathBuf>,
    /// 回放后等插件的静默窗口（毫秒）。不填就按有没有脚本插件自动选
    /// （脚本插件 2000，原生插件 150）。插件要发慢速网络请求时把它调大。
    #[arg(long, value_name = "MS")]
    replay_wait: Option<u64>,
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
    /// 列出插件目录里能被加载的插件（脚本插件展平到每个脚本）
    List(PluginDirArgs),
    /// 把一个插件装进插件目录（目录 = 脚本插件，文件 = 原生插件）
    Install(InstallArgs),
    /// 卸载一个插件（脚本插件按目录名，原生插件按动态库名）
    Uninstall(UninstallArgs),
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
    /// 插件动态库，或插件工程目录（目录里会去找 target/ 下的产物）；
    /// **脚本插件**传目录（带 package.json / pyproject.toml 的那个）
    path: PathBuf,
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

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    match cli.command {
        Command::Init(args) => {
            let config = scaffold::init(&args.dir, &args.name, args.api_id)?;
            println!("已生成 {}", config.display());
            println!("插件目录：{}", args.dir.join("plugins").display());
            println!("下一步：fuyubot-sdk init demo --lang rust（插件脚手架由开发 CLI 生成）");
        }
        Command::Run(args) if args.background => run_background(args)?,
        Command::Run(args) => run(args)?,
        Command::Status(args) => status(&args)?,
        Command::Stop(args) => stop(&args)?,
        Command::Config(args) => config_list(&args)?,
        Command::Plugin(args) => match args.command {
            PluginCommand::List(args) => plugin_list(args)?,
            PluginCommand::Install(args) => plugin_install(args)?,
            PluginCommand::Uninstall(args) => plugin_uninstall(args)?,
        },
    }
    Ok(())
}

/// `fuyubot status`：只读 `.fuyubot/`，**不加载插件、不接 TDLib**。
fn status(args: &StatusArgs) -> Result<(), Box<dyn std::error::Error>> {
    let root = existing_root(&args.config)?;
    for line in instance::status_lines(&root) {
        println!("{line}");
    }
    println!("  日志       {}", instance::log_path(&root).display());
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
        println!("{line}");
    }
    Ok(())
}

/// `fuyubot stop`。
fn stop(args: &StatusArgs) -> Result<(), Box<dyn std::error::Error>> {
    let root = existing_root(&args.config)?;
    println!("{}", instance::stop(&root)?);
    Ok(())
}

/// `run --background` 的父进程：**派生一个没有终端的自己**，等它真起来再返回。
///
/// 不做成「后台线程 + 父进程不退出」：用户要的是「敲完命令就回到提示符」，
/// 也就是真进程分离（`run --background` 之后 `fuyubot status` 应该报「运行中」）。
///
/// ⚠️ 子进程的 stdin 是 `null`：**后台跑不能交互**。要登录（没有 session、也没有
/// `TG_BOT_TOKEN`）就会直接失败——父进程把日志尾巴打出来指路，而不是挂在那里等人输入。
fn run_background(args: RunArgs) -> Result<(), Box<dyn std::error::Error>> {
    let root = parent_or_dot(&args.config);
    // 先在本进程里把项目建好：两个进程同时 init 会撞车（一个成功一个报「已经存在」）。
    ensure_project(&root, &args.config)?;

    let log = instance::log_path(&root);
    if let Some(parent) = log.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("建不了 {}: {e}", parent.display()))?;
    }
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
    if let Some(replay) = &args.replay {
        command.arg("--replay").arg(replay);
    }
    if let Some(wait) = args.replay_wait {
        command.arg("--replay-wait").arg(wait.to_string());
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
            println!("已在后台运行：pid {pid}，日志 {}", log.display());
            println!("`fuyubot status` 看状态，`fuyubot stop` 停掉它");
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            println!("后台进程（pid {pid}）还在启动中（登录可能要一会儿）");
            println!("日志：{}；状态：`fuyubot status`", log.display());
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

// ── run / plugin list ──────────────────────────────────────────────

fn run(args: RunArgs) -> Result<(), Box<dyn std::error::Error>> {
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(run_async(args))
}

async fn run_async(args: RunArgs) -> Result<(), Box<dyn std::error::Error>> {
    // 启动时刻：写进状态文件给 `status` 算「已运行多久」。
    let started_at = instance::unix_now();
    // 项目根目录：`fuyubot.toml` 所在目录（找不到就是当前目录）。
    let root = parent_or_dot(&args.config);

    // **自动 init**：缺配置文件就照模板生成一份，用户不用先手工跑一次 `fuyubot init`。
    //
    // ⚠️ **`--replay` 不自动 init**：回放是「离线验插件」的开发路径，不该因为跑一次就在
    //    当前目录里留下 `fuyubot.toml`（踩过一次：在仓库根跑回放，仓库里凭空多了个配置文件）。
    if args.replay.is_none() {
        ensure_project(&root, &args.config)?;
    }

    let config_path = match scaffold::find_config(&root) {
        Some(found) if args.config == Path::new(scaffold::CONFIG_FILE) => found,
        _ => args.config.clone(),
    };
    let root = parent_or_dot(&config_path);
    let config = scaffold::load_config(&config_path)?;
    let plugin_dir = args
        .plugin_dir
        .clone()
        .unwrap_or_else(|| config.plugin_dir(&root));
    // 插件目录不在就建一个空的：`plugin_files` 对**不存在的目录**是 io 错误，
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
                    println!("已安装内置插件 {}", path.display());
                }
            }
            Err(error) => eprintln!("装内置插件失败（继续）: {error}"),
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

    let mut kernel = Kernel::new();
    kernel.start().await?;

    // 宿主运行时把事件 / 服务 / 工具 / 中间件装进 kernel 的 Context，
    // 并生成给插件用的 HostApiVTable。
    let loader = PluginLoader::new(kernel.context().clone())?;
    let pipeline = Arc::new(loader.pipeline());

    // 把全局配置装进宿主运行时：插件经 ABI 的 `get_config` / `set_config` 读写它。
    // 必须在插件加载之前——插件 `start` 里就可能读自己的段。
    let config_json = config.to_json()?;
    loader
        .runtime()
        .install_config(Some(config_path.clone()), config_json);

    // 插件日志门槛：`[plugins].log_level`。**必须在插件加载之前**——
    // 插件 `init` / `start` 里打的日志也受它管。
    pipeline
        .runtime()
        .set_log_verbosity(config.plugins.log_level()?);

    // ── 启动顺序 ────────────────────────────────────────────────
    //
    //   ① 自动 init（上面刚做）
    //   ② **交互式问 api_id / api_hash**（缺了才问；环境变量/配置都有就不问）
    //   ③ 把 client 挂进宿主运行时 —— 插件 `start` 期发的请求才会真的进 TDLib
    //   ④ **加载全部插件**
    //   ⑤ 才等 TDLib 到「等参数」状态并 `setTdlibParameters` → 登录
    //   ⑥ `getMe` 打印账户信息
    //   ⑦ 最后才开始分发更新
    //
    // ③④ 必须在 ⑤ **之前**：插件要在 TDLib 开始产出之前订阅好，否则登录窗口里的
    // 更新就漏了（登录窗口收到的东西也不会被吞，见 `log_in_with` 的 `bypass`）。
    //
    // 回放模式**不接 TDLib**：它是「离线验插件」用的，接了反而有两宗罪——
    // 需要账号/网络，而且接收循环停不下来（见 docs/gaps.md 缺口 8），
    // tokio 运行时 drop 时会一直等它，进程退不出去。
    let replaying = args.replay.is_some();
    let tdlib_client = if args.no_tdlib || replaying {
        if replaying {
            println!("--replay：本次不接 TDLib（回放是离线的，请求只留档）");
        } else {
            println!("--no-tdlib：跳过 TDLib，插件照常加载");
        }
        None
    } else {
        attach_tdlib(
            &pipeline,
            config.tdlib.path.as_deref(),
            config.tdlib.log_verbosity(),
        )
    };

    // ② 凭据先问清楚。`--no-login` 不问也不发参数——TDLib 会停在
    //    `authorizationStateWaitTdlibParameters`，**已有的 session 也不会被打开**
    //    （数据库目录正是在 `setTdlibParameters` 里给的）。它只适合「只想把插件跑起来」。
    let parameters = match (tdlib_client.as_ref(), args.no_login) {
        (Some(_), false) => Some(resolve_parameters(&config, &config_path, &root)?),
        _ => None,
    };

    // ④ 插件全部加载完（**在发 `setTdlibParameters` 之前**）。
    // `spawn = true`：`run` 要的是真跑起来——装依赖、起进程、watch 一整套。
    let mut loaded = load_plugins(&mut kernel, &loader, &plugin_dir, true)?;

    // ⑤⑥ 现在才驱动登录：等「等参数」状态 → 设置 → 等授权状态 →（要登录就登录）→ Ready。
    if let (Some(client), Some(parameters)) = (tdlib_client.as_ref(), parameters.as_ref()) {
        let account = config.telegram.account_type()?;
        drive_login(client, parameters, account, &pipeline)?;
    }

    // ⑦ 插件与登录都就位，现在才开接收循环（gaps 缺口 8：句柄要一直留着，否则停不掉）。
    let receive = tdlib_client
        .as_ref()
        .map(|client| pipeline.spawn_dispatch(client));

    let bot_name = config
        .bot
        .name
        .clone()
        .unwrap_or_else(|| "fuyubot".to_owned());
    // 插件速览：`status` 是**另一个进程**，只能从这份文件读，所以要写下来。
    // （插件自己那条路走 `fuyubot.host.plugins` 服务，`load_plugins` 里已经装了活来源。）
    let detail = instance::PluginDetail::from_report(&kernel.report());
    println!(
        "{bot_name}（fuyubot {}）已启动：插件 {} 个，工具 {} 个",
        env!("CARGO_PKG_VERSION"),
        detail.active.len(),
        loader.runtime().tools().len()
    );
    // 发布 / 重发状态用同一份计算：`--background` 的父进程等的是**第一次**发布，
    // 而运行期插件上下线之后要让 `status` 跟上——两处算的必须是同一个东西。
    let publish = |detail: instance::PluginDetail| -> Result<(), Box<dyn std::error::Error>> {
        let Some(lock) = instance_lock.as_ref() else {
            return Ok(());
        };
        lock.publish(&instance::Info {
            pid: std::process::id(),
            started: started_at,
            version: env!("CARGO_PKG_VERSION").to_owned(),
            plugins: detail.active.len(),
            name: bot_name.clone(),
            detail,
        })?;
        Ok(())
    };
    // 状态**这时候才发布**：`run --background` 的父进程等的就是它（拿上锁只说明进程活着）。
    publish(detail.clone())?;
    // 运行循环用它判断「要不要重发」——不记下来的话，每圈都算得出差异、
    // 或者干脆每圈都写一遍。
    let mut last_detail = detail;

    if let Some(replay) = args.replay {
        // 脚本插件在**另一个进程**里跑，反应要经管道回来，还可能捎带网络请求；
        // 纯 Rust/C 插件是同步的，回放完就已经尘埃落定。按这个差别选等待窗口。
        let scripted = plugin_files(&plugin_dir)?
            .iter()
            .any(|path| scaffold::find_script(path).is_some());
        replay_updates(
            &pipeline,
            &replay,
            loader.runtime(),
            scripted,
            args.replay_wait,
        )?;
        loaded.clear();
        kernel.stop().await?;
        return Ok(());
    }

    // 没有 TDLib 就到此为止：接收循环没起来，本进程只是个「装好插件的空壳」。
    let Some(mut receive) = receive else {
        loaded.clear();
        kernel.stop().await?;
        return Ok(());
    };
    // 有接收循环就一定有 client（`receive` 只可能由上面的 `tdlib_client` 派生出来）。
    let Some(client) = tdlib_client.as_ref() else {
        unreachable!("有接收循环就一定有 TDLib 客户端")
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
        loader
            .runtime()
            .events()
            .on("updateAuthorizationState", None, move |payload| {
                if let Some(state) = re_login_alert(payload) {
                    *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(state);
                }
                Ok(())
            })
    };

    println!("开始接收 TDLib 更新（Ctrl-C 退出，或在别处 `fuyubot stop`）");
    //
    // ⚠️ Ctrl-C 目前仍是**硬杀**（进程被直接终止，插件的 `stop` 走不到）：要捕获它得开
    //    tokio 的 `signal` feature，那会带进 mio + signal-hook-registry + errno——AGENTS 的
    //    「不新增依赖」只放行 clap / object，所以这件事得用户点头（见 docs/decisions.md 变更 3-c）。
    //    要优雅退出就走 `fuyubot stop`（它写停止标记，我们在下面这一圈里看到就自己收摊）。
    loop {
        tokio::time::sleep(Duration::from_millis(500)).await;

        // 插件在运行期会自行上下线（依赖消失就退回 Pending，回来再自动激活），
        // 而 `fuyubot status` 是**另一个进程**、只能读这份文件。所以这一圈顺带把
        // 变了的状态重新发布一次。
        //
        // ⚠️ **只在真的变了才写**：`publish` 会整份重写文件，每 500ms 都写一遍
        // 是白费 I/O，还会让 `status` 的读和写撞上（Windows 上偶尔读到半份）。
        {
            let current = instance::PluginDetail::from_report(&kernel.report());
            if current != last_detail {
                publish(current.clone())?;
                last_detail = current;
            }
        }

        // `fuyubot stop` 请我们停下：**优雅收摊**——停接收循环 → 卸载插件（跑 `plugin_stop`）
        // → 停内核 → 放锁（`InstanceLock` 随作用域 drop）。
        if instance_lock
            .as_ref()
            .is_some_and(instance::InstanceLock::stop_requested)
        {
            println!("收到停止请求：停接收循环、卸载插件");
            receive.stop();
            receive.join().await;
            loaded.clear();
            if let Some(lock) = instance_lock.as_ref() {
                lock.clear_stop();
            }
            kernel.stop().await?;
            return Ok(());
        }

        let Some(state) = alert.lock().unwrap_or_else(|e| e.into_inner()).take() else {
            continue;
        };
        // 退回登录要用启动时那套参数。`--no-login` 下我们根本没发过参数，也就没有可重登的
        // 东西——**这条必须在停循环之前判**：`join()` 会按值消费 `receive`，从这里 `continue`
        // 就会跳过下面那次重新赋值，编译器直接报 use-after-move（也确实会漏掉循环）。
        let Some(parameters) = parameters.as_ref() else {
            eprintln!(
                "⚠️ 授权状态变成 {state}，但本次是 --no-login（没发过 setTdlibParameters），\
                 没有可用的重登参数——只能继续等"
            );
            continue;
        };
        let account = config.telegram.account_type()?;

        eprintln!("⚠️ 授权状态变成 {state}：账号可能被下线或封禁 —— 停掉接收循环，退回登录流程");
        receive.stop();
        receive.join().await;
        drive_login(client, parameters, account, &pipeline)?;
        // 登录完再开分发。**插件不重新加载**：它们还订阅着，重载会重复注册。
        receive = pipeline.spawn_dispatch(client);
        println!("已重新进入运行状态");
    }
}

/// `fuyubot plugin list`：列出插件目录里的插件。
///
/// 两层输出：**动态库插件**一行一个（dlopen 之后才知道名字，所以真的加载一次），
/// 外加**脚本插件展平**——两个中间插件把自己托管的清单暴露成
/// `fuyubot.host.scripts.<lang>` service，这里按前缀枚举后逐个 `list`。
/// 不展平的话用户只能看到两个 dll，看不到它们究竟管着哪些脚本。
///
/// 给插件传 `"spawn": false`：列清单不该替用户跑 `bun install` / `uv sync`，
/// 更不该把作者的脚本进程拉起来（中间插件据此进入只发现模式）。
fn plugin_list(args: PluginDirArgs) -> Result<(), Box<dyn std::error::Error>> {
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async {
        let root = project_root(&parent_or_dot(&args.config)).unwrap_or_else(|| PathBuf::from("."));
        let config = scaffold::load_config(&args.config)?;
        let plugin_dir = args.plugin_dir.unwrap_or_else(|| config.plugin_dir(&root));

        let mut kernel = Kernel::new();
        kernel.start().await?;
        let loader = PluginLoader::new(kernel.context().clone())?;

        let files = plugin_files(&plugin_dir).unwrap_or_default();
        if files.is_empty() {
            println!(
                "{} 里没有插件（.{}）",
                plugin_dir.display(),
                fuyubot_loader::DLL_EXTENSION
            );
        }
        // 插件名只有 dlopen 之后才知道，所以这里是「真的加载一次再看」。
        // 用与 `run` 同一个加载函数：带脚本的插件（Python/JS）同样需要拿到
        // 脚本的绝对路径，否则会按当前目录去找、然后加载失败。
        // `spawn = false`：只发现不启动（见函数文档）。
        //
        // ⚠️ `loaded` 必须**绑到变量上**：`LoadedPlugin` drop 即 `plugin_stop` +
        // owner 回滚，它一死，中间插件注册的清单 service 跟着注销——后面
        // `print_managed_scripts` 就什么都查不到。写在 `for` 的临时位置里
        // 正是踩过这个坑。
        let loaded = load_plugins(&mut kernel, &loader, &plugin_dir, false)?;
        for plugin in &loaded {
            println!(
                "{}\tABI {}\t{}",
                plugin.name(),
                plugin.abi_version(),
                plugin.path().display()
            );
        }

        print_managed_scripts(&loader);
        Ok::<(), Box<dyn std::error::Error>>(())
    })
}

/// 把中间插件托管的脚本清单摊开打印。
///
/// 约定：名字以 `fuyubot.host.scripts` 开头的 service 都有一个 `list` 方法，
/// 返回 `{"lang":…,"scripts":[…]}`。按前缀枚举而不是写死两个名字：
/// 将来多一门语言（`fuyubot.host.scripts.go` 之类）不用改这里。
fn print_managed_scripts(loader: &PluginLoader) {
    let runtime = loader.runtime();
    let mut lines: Vec<String> = Vec::new();
    for service in runtime.service_names() {
        if !service.starts_with("fuyubot.host.scripts") {
            continue;
        }
        let lang = service.rsplit('.').next().unwrap_or("?").to_owned();
        match runtime.call_service(&service, "list", "{}") {
            Ok(json) => match serde_json::from_str::<serde_json::Value>(&json) {
                Ok(parsed) => {
                    for script in parsed["scripts"].as_array().into_iter().flatten() {
                        let name = script["name"].as_str().unwrap_or("?");
                        let entry = [script["dir"].as_str(), script["entry"].as_str()]
                            .into_iter()
                            .flatten()
                            .collect::<PathBuf>();
                        lines.push(format!("  {name}\t{lang}\t{}", entry.display()));
                    }
                }
                Err(e) => eprintln!("{service} 返回的不是合法 JSON: {e}"),
            },
            Err(e) => eprintln!("查不了 {service}: {e}"),
        }
    }
    if !lines.is_empty() {
        println!("脚本插件（{} 个，由中间插件托管）：", lines.len());
        for line in lines {
            println!("{line}");
        }
    }
}

fn plugin_install(args: InstallArgs) -> Result<(), Box<dyn std::error::Error>> {
    let root = project_root(&parent_or_dot(&args.config)).unwrap_or_else(|| PathBuf::from("."));
    let config = scaffold::load_config(&args.config)?;
    let plugin_dir = args.plugin_dir.unwrap_or_else(|| config.plugin_dir(&root));

    for path in scaffold::install(&args.path, &plugin_dir)? {
        println!("已安装 {}", path.display());
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
        println!("已卸载 {}", path.display());
    }
    Ok(())
}

/// `run` 的自动初始化：缺配置文件就照模板写一份，插件目录不在就建。
///
/// **刻意不复用 `init()`**：那个命令在文件已存在时报错（对显式 `init` 是对的），
/// 而 `run` 要的是幂等——「有就直接跑，没有就建好再跑」，这样用户少一步。
///
/// 只处理**默认文件名**：`--config` 指了别的名字就是显式意图，不替用户凭空造一个。
fn ensure_project(root: &Path, configured: &Path) -> Result<(), Box<dyn std::error::Error>> {
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
    let api_id = std::env::var("TG_API_ID")
        .ok()
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(0);
    scaffold::init(root, &name, api_id)?;
    println!(
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
                    println!("{question}：用环境变量 TG_BOT_TOKEN（不回显）");
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
        println!("这个账号是 user 类型，选登录方式：");
        println!("  1) 手机号（收验证码）");
        println!("  2) 二维码（手机 Telegram 扫码）");
        match self.ask("输入 1 或 2")?.trim() {
            "1" => Ok(fuyubot_tdlib::LoginMethod::PhoneNumber),
            "2" => Ok(fuyubot_tdlib::LoginMethod::QrCode),
            other => Err(fuyubot_tdlib::LoginError::NoInput {
                question: format!("登录方式只能选 1 或 2，给的是「{other}」"),
            }),
        }
    }

    fn notify(&mut self, message: &str) {
        println!("{message}");
    }
}

/// 把登录要用的 TDLib 参数凑齐：**缺什么就交互式问用户**，问到的**回写配置**。
///
/// 优先级：环境变量 → 配置 → **交互式提问**（`run` 自己问，
/// 不让用户先去翻文档找配置项）。
/// - `api_id` **不是**凭据（约束 41 只禁 Bot Token / 手机号 / 密码），环境变量与配置都认；
/// - `api_hash` 是凭据，但**每次冷启动都要随 `setTdlibParameters` 发给 TDLib**
///   （session 还有效也照样要发），不记住就得每次手输——所以环境变量与
///   配置里已保存的都认，**交互问出来的回写** `[telegram].api_hash` 明文一行。
///
/// ⚠️ 回写只发生在「这次真的问过」时：环境变量/配置来的值不动文件，
/// 「我没让它改配置它却改了」比多问一次更让人困惑。
fn resolve_parameters(
    config: &scaffold::Config,
    config_path: &Path,
    root: &Path,
) -> Result<fuyubot_tdlib::TdlibParameters, Box<dyn std::error::Error>> {
    let mut prompter = ConsolePrompter::default();
    // 这两个只有**交互问出来**才有值——它们是回写的唯一依据。
    let mut asked_id: Option<i64> = None;
    let mut asked_hash: Option<String> = None;

    let id_from_env = std::env::var("TG_API_ID")
        .ok()
        .and_then(|value| value.parse::<i64>().ok())
        .filter(|id| *id != 0);
    let configured_id = id_from_env.unwrap_or_else(|| config.telegram.api_id(&config.bot));
    let api_id = if configured_id != 0 {
        configured_id
    } else {
        let answer = prompter.ask(
            "api_id（到 https://my.telegram.org 申请；它不是秘密，也可以填进 [telegram].api_id）",
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
            println!("没找到环境变量 TG_API_HASH，配置里也没存过。");
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
        let id_src = if id_from_env.is_some() {
            "环境变量 TG_API_ID"
        } else {
            "配置 [telegram].api_id"
        };
        let hash_src = if hash_from_env.is_some() {
            "环境变量 TG_API_HASH"
        } else if saved_hash.is_some() {
            "配置 [telegram].api_hash（上次输入时保存的）"
        } else {
            "交互输入"
        };
        println!("api_id 与 api_hash 都已就位（{id_src} + {hash_src}），跳过初始化与提问");
    }

    // 交互问出来的才落盘：下次冷启动不用再输一遍（**每次**都要随 `setTdlibParameters`
    // 发这两个值，session 有效也一样）。写盘失败只警告——记住失败不该让机器人起不来。
    if asked_id.is_some() || asked_hash.is_some() {
        match scaffold::remember_credentials(config_path, asked_id, asked_hash.as_deref()) {
            Ok(true) => {
                println!("已写回 {}（下次不用再输这两项）", config_path.display());
                if asked_hash.is_some() {
                    println!(
                        "⚠️ api_hash 是凭据，现在以**明文**放在上面的文件里——\
                         别把它提交到版本库、也别整份发给别人"
                    );
                }
            }
            Ok(false) => {}
            Err(e) => eprintln!("写回 api_id / api_hash 失败（不影响本次运行）：{e}"),
        }
    }

    let data_dir = config.telegram.data_dir(root);
    std::fs::create_dir_all(&data_dir)?;
    Ok(fuyubot_tdlib::TdlibParameters::new(
        &data_dir, api_id, api_hash,
    ))
}

/// 驱动登录到 `Ready`，成功后用 `getMe` 把账户信息打到控制台。
///
/// ⚠️ `log_in_with` 是**同步阻塞**的（自己 `receive` 轮询）。调用它的两个时机都要保证
/// 接收循环**没在跑**：启动时（还没起）与运行期退回登录时（先 `stop()` + `join()`）。
///
/// 登录窗口里与授权无关的东西（别的更新、插件请求的响应）经 `bypass` **直接进管线**——
/// 插件这时候已经加载好了（`run` 刻意让插件先于 `setTdlibParameters`），吞掉就是漏更新。
fn drive_login(
    client: &Arc<fuyubot_tdlib::TdClient>,
    parameters: &fuyubot_tdlib::TdlibParameters,
    account: fuyubot_tdlib::AccountType,
    pipeline: &Arc<UpdatePipeline>,
) -> Result<(), Box<dyn std::error::Error>> {
    println!(
        "等待 TDLib 到「等参数」授权状态并设置（账号类型 {account:?}，数据目录 {}）",
        parameters.data_dir.display()
    );
    let mut prompter = ConsolePrompter::default();
    let mut bypass = |routed| pipeline.dispatch(routed);
    match fuyubot_tdlib::log_in_with(
        client,
        parameters,
        account,
        &mut prompter,
        Duration::from_secs(600),
        &mut bypass,
    )? {
        fuyubot_tdlib::LoginOutcome::Ready => {}
        fuyubot_tdlib::LoginOutcome::Closed => {
            return Err("TDLib 把会话关掉了（authorizationStateClosed）".into());
        }
    }
    // 登录期判据（变更 3-b）：一次都没问过就进了 Ready，说明 `data_dir` 里的 session
    // 还有效——要明说，否则用户会以为自己刚输入的凭据生效了（其实根本没用到）。
    if !prompter.asked {
        println!(
            "已有有效 session，TDLib 直接进了 authorizationStateReady（本次没有走任何登录交互）"
        );
    }

    // 登录完成，问一次账户信息（变更 3-a 第 3 步）。
    match call_and_wait(
        client,
        serde_json::json!({ "@type": "getMe" }),
        Duration::from_secs(15),
        pipeline,
    ) {
        Ok(me) => println!("开始运行，账户：{}", describe_account(&me)),
        Err(error) => eprintln!("已登录，但 getMe 没拿到（{error}）——不影响运行"),
    }
    Ok(())
}

/// 发一条请求并等它的响应（`log_in_with` 那种同步循环的简化版）。
///
/// ⚠️ 期间收到**别的**东西一律交给 `pipeline.dispatch`，不吞：调用点在「插件已经加载好」
/// 之后（`getMe` 是在 `drive_login` 里、插件早就位），丢掉就是漏更新。
/// 前提同样是**接收循环没在跑**。
fn call_and_wait(
    client: &fuyubot_tdlib::TdClient,
    request: serde_json::Value,
    budget: Duration,
    pipeline: &Arc<UpdatePipeline>,
) -> Result<serde_json::Value, String> {
    let id = client
        .send(request)
        .map_err(|error| format!("发请求失败：{error}"))?;
    let deadline = std::time::Instant::now() + budget;
    while std::time::Instant::now() < deadline {
        match client.receive(Duration::from_millis(500)) {
            Some(fuyubot_tdlib::Routed::Response(response)) if response.id == id => {
                return Ok(response.payload);
            }
            Some(other) => pipeline.dispatch(other),
            None => continue,
        }
    }
    Err(format!("等 {id:?} 的响应超时（{budget:?}）"))
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

/// 把 `getMe` 的结果描述成一行。
///
/// 字段路径按 AGENTS 约束 42 拿真库核过：bot 判定在 **`type.@type == "userTypeBot"`**、
/// 用户名在 **`usernames.active_usernames[0]`**（`user` **没有**顶层 `is_bot` / `username`）。
fn describe_account(me: &serde_json::Value) -> String {
    let id = me
        .get("id")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(0);
    let name = ["first_name", "last_name"]
        .iter()
        .filter_map(|key| me.get(key).and_then(serde_json::Value::as_str))
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let username = me
        .get("usernames")
        .and_then(|u| u.get("active_usernames"))
        .and_then(serde_json::Value::as_array)
        .and_then(|list| list.first())
        .and_then(serde_json::Value::as_str);
    let is_bot = me
        .get("type")
        .and_then(|t| t.get("@type"))
        .and_then(serde_json::Value::as_str)
        == Some("userTypeBot");

    let mut line = if name.is_empty() {
        format!("id={id}")
    } else {
        format!("{name}（id={id}）")
    };
    if let Some(username) = username {
        line.push_str(&format!(" @{username}"));
    }
    line.push_str(if is_bot { " [bot]" } else { " [user]" });
    line
}

/// 去掉 Windows `canonicalize` 给的 `\\?\` 长路径前缀。
///
/// 语义上两种写法等价（fs 调用、拼接都兼容），但带前缀的路径打到终端、
/// 混进 `plugin list` 输出里就是一串看不懂的 `\\?\F:\…`，纯属噪声。
fn strip_unc_prefix(path: PathBuf) -> PathBuf {
    const UNC: &str = r"\\?\";
    match path.to_string_lossy().strip_prefix(UNC) {
        Some(rest) => PathBuf::from(rest),
        None => path,
    }
}

fn load_plugins(
    kernel: &mut Kernel,
    loader: &PluginLoader,
    plugin_dir: &Path,
    spawn: bool,
) -> Result<Vec<Arc<DylibPlugin>>, Box<dyn std::error::Error>> {
    // 插件自己算不出「插件目录」在哪（它跑在宿主进程里），只能由宿主告诉它。
    // 内置的 `/plugins` 命令就靠这个配置；绝对路径，免得随启动目录漂。
    // `canonicalize` 在 Windows 上带 `\\?\` 长路径前缀——去掉，配置与输出都干净。
    let absolute = strip_unc_prefix(
        plugin_dir
            .canonicalize()
            .unwrap_or_else(|_| plugin_dir.into()),
    );
    // 脚本插件的家是 `<plugin_dir>/scripts/`（`plugin install` 的落点）：
    // 传绝对路径给中间插件（它读 `scripts_dir`），别让它按 cwd 猜 `plugins/scripts`
    // ——`--config` / `--plugin-dir` 指到别处、或不在项目根目录起进程时那样必错。
    let scripts_dir = absolute.join("scripts");

    // 先把目录扫成探针：插件名只有 dlopen 之后才知道，而依赖是按**名字**声明的。
    // 探测只问名字、不 start（见 `PluginLoader::probe`）。
    let mut adapters: Vec<Arc<DylibPlugin>> = Vec::new();
    for path in plugin_files(plugin_dir)? {
        let config = match scaffold::find_script(&path) {
            Some(script) => serde_json::json!({ "script": script, "plugins_dir": absolute, "scripts_dir": scripts_dir, "spawn": spawn }),
            None => serde_json::json!({ "plugins_dir": absolute, "scripts_dir": scripts_dir, "spawn": spawn }),
        }
        .to_string();
        // 探测失败就跳过这一个，原因打出来——文件在那儿却悄悄消失是最难查的那类事。
        match DylibPlugin::probe(loader, &path, config) {
            Ok(adapter) => adapters.push(Arc::new(adapter)),
            Err(error) => eprintln!("跳过 {}: {error}", path.display()),
        }
    }

    // 交给内核：依赖齐的当场激活，不齐的停在 Pending**等着**（不是失败），
    // 能力出现/消失时内核自己的钩子会重新求值——这就是「依赖持续跟踪」。
    let plugins: Vec<Arc<dyn fuyubot_core::Plugin>> = adapters
        .iter()
        .map(|adapter| Arc::clone(adapter) as Arc<dyn fuyubot_core::Plugin>)
        .collect();
    kernel.plugins().load_all(plugins)?;

    // 让插件经 ABI 问得到「进程里加载了哪些」：装的是**活来源**而不是快照，
    // 所以之后插件怎么上下线都不用手动刷新（旧接口要求每次变化后刷，实际没人刷得到）。
    loader.runtime().install_plugin_reporter(kernel.reporter());

    // Pending 的要说清楚：不说的话，人就会以为它在正常跑。
    for (name, missing) in kernel.plugins().pending() {
        eprintln!(
            "插件 {name} 的依赖还没齐，先不启动：缺 {}",
            missing.join(", ")
        );
    }
    for (name, reason) in kernel.plugins().failed() {
        eprintln!("插件 {name} 启动失败：{reason}");
    }

    Ok(adapters)
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
        println!("回放 {update_type} → {outcome:?}");
    }

    // 脚本插件是**另一个进程**：事件要经管道过去、反应要经管道回来，还可能捎带
    // 网络请求。回放完立刻打印，会把它们的日志与请求整批漏掉——所以先等它们安静下来。
    wait_until_quiet(
        runtime,
        quiet_override.unwrap_or(if scripted { 2000 } else { 150 }),
    );

    println!("插件打出来的日志：");
    for line in runtime.log_lines() {
        println!("  {line}");
    }
    println!("插件发出去的请求：");
    for request in runtime.sent_requests() {
        println!("  #{} {} {}", request.id, request.method, request.params);
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

/// 解析 TDLib、建客户端，并起接收循环把它接上更新管线。
///
/// 把 TDLib 挂进宿主运行时，返回那个客户端（**不起接收循环**）。
///
/// 返回 `None` 表示「没有可用的 TDLib」——**这不是启动失败**：插件照样加载，
/// `td_invoke*` 只留档 + 发合成请求号（整条链路在没有真库时也能被跑通）。
///
/// ⚠️ 接收循环由调用方在**插件全部加载完之后**用
/// [`UpdatePipeline::spawn_dispatch`] 起（否则插件会漏订阅）。
fn attach_tdlib(
    pipeline: &Arc<UpdatePipeline>,
    configured: Option<&str>,
    log_verbosity: i32,
) -> Option<Arc<fuyubot_tdlib::TdClient>> {
    let resolved = match configured.filter(|path| !path.trim().is_empty()) {
        Some(path) => {
            let path = PathBuf::from(path);
            if !path.exists() {
                eprintln!(
                    "配置里的 tdlib.path 不存在：{}，回退到自动查找",
                    path.display()
                );
                fuyubot_tdlib::resolve()
            } else {
                // 配置点名了库的位置，就用它当优先级 1（不再去猜裸库名）。
                let mut env = fuyubot_tdlib::LoadEnv::from_process();
                env.tdlib_path = Some(path);
                fuyubot_tdlib::resolve_with(&env, &|_| false)
            }
        }
        None => fuyubot_tdlib::resolve(),
    };

    match resolved {
        Ok(resolved) => {
            println!(
                "TDLib {} 命中「{}」：{}",
                fuyubot_tdlib::TDLIB_VERSION,
                resolved.source,
                resolved.target.display()
            );
            for note in &resolved.notes {
                println!("  {note}");
            }
        }
        Err(e) => {
            eprintln!("TDLib 未就绪（不接更新）：{e}");
            return None;
        }
    }

    let client = match fuyubot_tdlib::TdClient::new() {
        Ok(client) => Arc::new(client),
        Err(e) => {
            eprintln!("创建 TDLib 客户端失败（不接更新）：{e}");
            return None;
        }
    };
    // `new()` 已经把日志压到默认值（1）；这里让 `[tdlib] log_verbosity` 覆盖它。
    // 全局设置，调一次对所有客户端生效。
    if log_verbosity != fuyubot_tdlib::DEFAULT_LOG_VERBOSITY {
        client.set_log_verbosity(log_verbosity);
    }
    println!("TDLib 日志详细度：{log_verbosity}");

    match pipeline.attach_tdlib(Arc::clone(&client)) {
        Ok(()) => Some(client),
        Err(_) => {
            eprintln!("宿主已经接过一个 TDLib 客户端了");
            None
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

    #[test]
    fn the_cli_definition_is_valid() {
        // clap 的 `debug_assert` 会检查短选项冲突之类的定义错误。
        use clap::CommandFactory;
        Cli::command().debug_assert();
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

    #[test]
    fn subcommands_parse() {
        let cli = Cli::try_parse_from(["fuyubot", "plugin", "install", "x.dll"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Plugin(PluginArgs {
                command: PluginCommand::Install(_)
            })
        ));

        let cli = Cli::try_parse_from(["fuyubot", "run", "--replay", "u.json"]).unwrap();
        match cli.command {
            Command::Run(args) => assert_eq!(args.replay.as_deref(), Some(Path::new("u.json"))),
            other => panic!("解析成了 {other:?}"),
        }

        // 后台运行：只是 `run` 的一个开关（`status` / `stop` 面对的是同一个实例）。
        let cli = Cli::try_parse_from(["fuyubot", "run", "--background"]).unwrap();
        match cli.command {
            Command::Run(args) => assert!(args.background && !args.no_login),
            other => panic!("解析成了 {other:?}"),
        }
        assert!(matches!(
            Cli::try_parse_from(["fuyubot", "status"]).unwrap().command,
            Command::Status(_)
        ));
        assert!(matches!(
            Cli::try_parse_from(["fuyubot", "stop"]).unwrap().command,
            Command::Stop(_)
        ));
        assert!(matches!(
            Cli::try_parse_from(["fuyubot", "config"]).unwrap().command,
            Command::Config(_)
        ));

        // 插件的脚手架**不在**运行时 CLI 里（变更 4）——它只在 `fuyubot-sdk`。
        assert!(Cli::try_parse_from(["fuyubot", "new-plugin", "demo"]).is_err());
        // 插件也不归运行时 CLI 编（变更 5）：`build-plugin` 已经拆掉了。
        assert!(Cli::try_parse_from(["fuyubot", "build-plugin"]).is_err());
    }

    /// `status` / `stop` 只认**已存在**的项目：从深层目录也能向上找到根（变更 6）。
    ///
    /// ⚠️ 「找不到就报错」那条分支在仓库里测不出来：测试临时目录在 `target/test-tmp/` 下，
    /// 往上一定能撞见仓库根那份 `fuyubot.toml`。所以这里只测找得到的那一半；
    /// 报错那半是**手工**验的：`status --config nope\fuyubot.toml` →
    /// `Error: "nope 及其上级目录里没有 fuyubot.toml——先 `fuyubot init` 建一个"`、退出码 1。
    #[test]
    fn status_walks_up_to_the_project_root() {
        let root = test_support::temp("status-root");
        scaffold::init(&root, "mybot", 1).unwrap();
        let nested = root.join("plugins").join("somewhere");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(
            existing_root(&nested.join(scaffold::CONFIG_FILE)).unwrap(),
            root
        );

        // 相对路径也得能往上走：默认的 `--config fuyubot.toml` 就是相对路径，
        // 而 `Path::new(".").parent()` 是 `Some("")`、`""` 没有父目录——不把起点变绝对，
        // 在子目录里跑 `status` / `config` 会谎报「这里没有配置」。
        // 测试 CWD 是 crate 根，往上两级就是仓库根那份 `fuyubot.toml`。
        let found = scaffold::find_config(Path::new(scaffold::CONFIG_FILE))
            .expect("相对路径必须能向上找到仓库根的 fuyubot.toml");
        assert!(found.is_absolute() && found.is_file(), "{found:?}");
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
}
