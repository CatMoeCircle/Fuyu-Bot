//! fuyubot 命令行入口（**运行时** CLI）。
//!
//! 这里不实现任何宿主函数：`HostApiVTable` 的实现全在 `fuyubot-loader` 里，
//! 本文件只负责把内核、宿主运行时、插件目录接起来。
//!
//! **插件开发的事不在这里**：脚手架由 `fuyubot-sdk init` 生成（见 `docs/ROADMAP.md` 变更 4）。
//!
//! ```text
//! fuyubot init                              生成 fuyubot.toml + plugins/
//! fuyubot build-plugin                      编成 cdylib 并装进插件目录（过渡期命令，见下）
//! fuyubot run                               起框架、加载插件
//! fuyubot plugin list                       列出插件目录里的插件
//! fuyubot plugin install <路径>             把一个插件装进插件目录
//! ```
//!
//! `build-plugin` 按新规格**不该存在于运行时 CLI**（构建/打包归作者自己的工具链，变更 5），
//! 但现在还撑着 `examples/` 的流程；等 `fuyubot plugin add` + 文档到位再拆。

mod builder;
mod scaffold;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::{Args, Parser, Subcommand, ValueEnum};
use fuyubot_core::Kernel;
use fuyubot_loader::{LoadedPlugin, PluginLoader, UpdatePipeline, plugin_files};

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
    /// 把当前插件编译成 cdylib，并装进插件目录
    ///
    /// **这是过渡期的命令**：新规格里「构建/打包」不归两个 CLI 管（见
    /// `docs/ROADMAP.md` 变更 5），但它现在撑着 `examples/` 的流程，
    /// 等 `fuyubot plugin add` + 作者自己的工具链就位后再拆掉。
    BuildPlugin(BuildArgs),
    /// 起框架、加载插件
    Run(RunArgs),
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
    /// TDLib api_id（到 https://my.telegram.org 申请）
    #[arg(long, default_value_t = 0)]
    api_id: i64,
    /// TDLib api_hash
    #[arg(long, default_value = "")]
    api_hash: String,
}

#[derive(Args, Debug)]
struct BuildArgs {
    /// 插件目录；不给就自动找（当前目录，或插件目录下唯一的一个插件工程）
    #[arg(long)]
    dir: Option<PathBuf>,
    /// 用 release 配置
    #[arg(long)]
    release: bool,
    /// 强制指定语言（默认按目录内容判断）
    #[arg(long, value_enum)]
    lang: Option<LangArg>,
    /// 只编译，不装进插件目录
    #[arg(long)]
    no_install: bool,
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
    /// 把一批更新的 JSON 灌进管线后退出：**不接 TDLib、不用账号**，纯离线验插件
    #[arg(long)]
    replay: Option<PathBuf>,
    /// 回放后等插件的静默窗口（毫秒）。不填就按有没有脚本插件自动选
    /// （脚本插件 2000，原生插件 150）。插件要发慢速网络请求时把它调大。
    #[arg(long, value_name = "MS")]
    replay_wait: Option<u64>,
}

#[derive(Args, Debug)]
struct PluginArgs {
    #[command(subcommand)]
    command: PluginCommand,
}

#[derive(Subcommand, Debug)]
enum PluginCommand {
    /// 列出插件目录里能被加载的插件
    List(PluginDirArgs),
    /// 把一个编好的插件（文件或工程目录）装进插件目录
    Install(InstallArgs),
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
    /// 插件动态库，或插件工程目录（目录里会去找 target/ 下的产物）
    path: PathBuf,
    #[arg(long, default_value = scaffold::CONFIG_FILE)]
    config: PathBuf,
    #[arg(long)]
    plugin_dir: Option<PathBuf>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum LangArg {
    Rust,
    C,
    Cpp,
    Python,
    Js,
}

impl From<LangArg> for builder::Kind {
    /// `--lang` 只影响走哪条编译路径：C/C++ 走 C 编译器，其余走 cargo。
    ///
    /// 插件的**脚手架**不在这里生成——那是 `fuyubot-sdk init` 的事（见 `ROADMAP.md` 变更 4）。
    fn from(value: LangArg) -> Self {
        match value {
            LangArg::C | LangArg::Cpp => Self::C,
            LangArg::Rust | LangArg::Python | LangArg::Js => Self::Cargo,
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    match cli.command {
        Command::Init(args) => {
            let config = scaffold::init(&args.dir, &args.name, args.api_id, &args.api_hash)?;
            println!("已生成 {}", config.display());
            println!("插件目录：{}", args.dir.join("plugins").display());
            println!("下一步：fuyubot-sdk init demo --lang rust（插件脚手架由开发 CLI 生成）");
        }
        Command::BuildPlugin(args) => build_plugin(args)?,
        Command::Run(args) => run(args)?,
        Command::Plugin(args) => match args.command {
            PluginCommand::List(args) => plugin_list(args)?,
            PluginCommand::Install(args) => plugin_install(args)?,
        },
    }
    Ok(())
}

// ── build-plugin ───────────────────────────────────────────────────

fn build_plugin(args: BuildArgs) -> Result<(), Box<dyn std::error::Error>> {
    let dir = match &args.dir {
        Some(dir) => dir.clone(),
        None => locate_plugin_project()?,
    };
    let kind = args.lang.map(builder::Kind::from);

    let artifact = builder::build(&dir, args.release, kind)?;
    println!("编好了：{}", artifact.display());

    if args.no_install {
        return Ok(());
    }
    // 顺手装进插件目录：编完直接 `run` 就能看到插件，
    // 不用先记住 `plugin install`（Python / JS 插件的脚本也会一起拷过去）。
    match project_root(&dir) {
        Some(root) => {
            let config = scaffold::load_config(&root.join(scaffold::CONFIG_FILE))?;
            let plugin_dir = config.plugin_dir(&root);
            let copied = scaffold::install_from(&artifact, &[dir.clone()], &plugin_dir)?;
            println!("已装进 {}：", plugin_dir.display());
            for path in copied {
                println!("  {}", path.display());
            }
        }
        None => println!("（没在上级目录找到 fuyubot.toml，跳过安装；`fuyubot init` 可以建一个）"),
    }
    Ok(())
}

/// 当前目录就是插件工程？不是的话，看插件目录下有没有唯一的一个插件工程。
fn locate_plugin_project() -> Result<PathBuf, String> {
    let cwd = std::env::current_dir().map_err(|e| format!("拿不到当前目录: {e}"))?;
    if cwd.join("Cargo.toml").is_file() || !list_sources(&cwd).is_empty() {
        return Ok(cwd);
    }
    let root = project_root(&cwd).unwrap_or(cwd);
    let config = scaffold::load_config(&root.join(scaffold::CONFIG_FILE)).unwrap_or_default();
    let plugins = config.plugin_dir(&root);
    let mut candidates = Vec::new();
    for entry in std::fs::read_dir(&plugins).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() && (path.join("Cargo.toml").is_file() || !list_sources(&path).is_empty()) {
            candidates.push(path);
        }
    }
    match candidates.len() {
        0 => Err(format!(
            "{} 下没有插件工程，用 --dir 指定一个",
            plugins.display()
        )),
        1 => Ok(candidates.remove(0)),
        _ => Err(format!(
            "有多个插件工程，用 --dir 指定：{}",
            candidates
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

fn list_sources(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "c" || extension == "cpp")
        })
        .collect()
}

/// 从 `start` 往上找带 `fuyubot.toml` 的目录。
fn project_root(start: &Path) -> Option<PathBuf> {
    scaffold::find_config(start).and_then(|path| path.parent().map(Path::to_path_buf))
}

// ── run / plugin list ──────────────────────────────────────────────

fn run(args: RunArgs) -> Result<(), Box<dyn std::error::Error>> {
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(run_async(args))
}

async fn run_async(args: RunArgs) -> Result<(), Box<dyn std::error::Error>> {
    let config_path = match scaffold::find_config(
        args.config
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new(".")),
    ) {
        Some(found) if args.config == Path::new(scaffold::CONFIG_FILE) => found,
        _ => args.config.clone(),
    };
    let root = config_path.parent().unwrap_or(Path::new(".")).to_path_buf();
    let config = scaffold::load_config(&config_path)?;
    let plugin_dir = args
        .plugin_dir
        .clone()
        .unwrap_or_else(|| config.plugin_dir(&root));

    let mut kernel = Kernel::new();
    kernel.start().await?;

    // 宿主运行时把事件 / 服务 / 工具 / 中间件装进 kernel 的 Context，
    // 并生成给插件用的 HostApiVTable。
    let loader = PluginLoader::new(kernel.context().clone())?;
    let pipeline = Arc::new(loader.pipeline());

    // **先接 TDLib，再加载插件**：插件在 `start` 里发的请求才会真的进 TDLib，
    // 而不是拿到一个合成请求号。接不上也照常启动，只是 `td_invoke*` 只留档。
    //
    // 回放模式**不接 TDLib**：它是「离线验插件」用的，接了反而有两宗罪——
    // 需要账号/网络，而且接收循环停不下来（见 HANDOFF 缺口 8），
    // tokio 运行时 drop 时会一直等它，进程退不出去。
    let replaying = args.replay.is_some();
    let tdlib_attached = if args.no_tdlib || replaying {
        if replaying {
            println!("--replay：本次不接 TDLib（回放是离线的，请求只留档）");
        } else {
            println!("--no-tdlib：跳过 TDLib，插件照常加载");
        }
        false
    } else {
        attach_tdlib(&pipeline, config.tdlib.path.as_deref())
    };

    let mut loaded = load_plugins(&loader, &plugin_dir)?;
    let bot_name = config
        .bot
        .name
        .clone()
        .unwrap_or_else(|| "fuyubot".to_owned());
    if let Some(note) = login_note(&config) {
        println!("{note}");
    }
    println!(
        "{bot_name}（fuyubot {}）已启动：插件 {} 个，工具 {} 个",
        env!("CARGO_PKG_VERSION"),
        loaded.len(),
        loader.runtime().tools().len()
    );

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
    if !tdlib_attached {
        loaded.clear();
        kernel.stop().await?;
        return Ok(());
    }

    // 接收循环在后台把每条更新送进管线；主任务挂在这里陪它。
    // **进程靠 Ctrl-C 结束**（优雅退出还没做，见 HANDOFF）。
    println!("开始接收 TDLib 更新（Ctrl-C 退出）");
    std::future::pending::<()>().await;
    Ok(())
}

fn plugin_list(args: PluginDirArgs) -> Result<(), Box<dyn std::error::Error>> {
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async {
        let root = project_root(
            args.config
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new(".")),
        )
        .unwrap_or_else(|| PathBuf::from("."));
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
                builder::DLL_EXTENSION
            );
        }
        // 插件名只有 dlopen 之后才知道，所以这里是「真的加载一次再看」。
        // 用与 `run` 同一个加载函数：带脚本的插件（Python/JS）同样需要拿到
        // 脚本的绝对路径，否则会按当前目录去找、然后加载失败。
        for plugin in load_plugins(&loader, &plugin_dir)? {
            println!(
                "{}\tABI {}\t{}",
                plugin.name,
                plugin.abi_version,
                plugin.path.display()
            );
        }
        Ok::<(), Box<dyn std::error::Error>>(())
    })
}

fn plugin_install(args: InstallArgs) -> Result<(), Box<dyn std::error::Error>> {
    let root = project_root(
        args.config
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new(".")),
    )
    .unwrap_or_else(|| PathBuf::from("."));
    let config = scaffold::load_config(&args.config)?;
    let plugin_dir = args.plugin_dir.unwrap_or_else(|| config.plugin_dir(&root));

    for path in scaffold::install(&args.path, &plugin_dir)? {
        println!("已安装 {}", path.display());
    }
    Ok(())
}

/// 配置里给了凭据就提醒一句：**真实登录流程还没接**（见 HANDOFF 缺口 4）。
///
/// 别把 `api_hash` 打出来——它是凭据。这里只读它来判断「配没配」。
fn login_note(config: &scaffold::Config) -> Option<String> {
    let api_id = config.bot.api_id?;
    let api_hash = config.bot.api_hash.as_deref()?;
    if api_id == 0 || api_hash.is_empty() {
        return None;
    }
    Some(format!(
        "配置里有 api_id={api_id}，但真实登录流程还没接（见 HANDOFF 缺口 4）；\
         现在只在接入 TDLib 后收发更新"
    ))
}

/// 逐个加载插件目录里的动态库。
///
/// 带脚本的插件（Python / JS）要把脚本的**绝对路径**当配置传进去——cdylib 自己
/// 不知道自己在哪个目录，宿主才知道（见 `scaffold::find_script`）。
fn load_plugins(
    loader: &PluginLoader,
    plugin_dir: &Path,
) -> Result<Vec<LoadedPlugin>, Box<dyn std::error::Error>> {
    let mut loaded = Vec::new();
    for path in plugin_files(plugin_dir)? {
        let config = match scaffold::find_script(&path) {
            Some(script) => serde_json::json!({ "script": script }).to_string(),
            None => String::new(),
        };
        match loader.load_with_config(&path, &config) {
            Ok(plugin) => {
                println!("已加载插件 {} ({})", plugin.name, path.display());
                loaded.push(plugin);
            }
            Err(e) => eprintln!("跳过 {}: {e}", path.display()),
        }
    }
    Ok(loaded)
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
/// 返回 `false` 表示「没有可用的 TDLib」——**这不是启动失败**：插件照样加载，
/// `td_invoke*` 只留档 + 发合成请求号（整条链路在没有真库时也能被跑通）。
fn attach_tdlib(pipeline: &Arc<UpdatePipeline>, configured: Option<&str>) -> bool {
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
            return false;
        }
    }

    let client = match fuyubot_tdlib::TdClient::new() {
        Ok(client) => Arc::new(client),
        Err(e) => {
            eprintln!("创建 TDLib 客户端失败（不接更新）：{e}");
            return false;
        }
    };

    match pipeline.attach_tdlib(Arc::clone(&client)) {
        Ok(()) => true,
        Err(_) => {
            eprintln!("宿主已经接过一个 TDLib 客户端了");
            false
        }
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

        // 插件的脚手架**不在**运行时 CLI 里（变更 4）——它只在 `fuyubot-sdk`。
        assert!(Cli::try_parse_from(["fuyubot", "new-plugin", "demo"]).is_err());
    }

    /// `--lang` 现在只用来选编译路径（C 编译器 / cargo），不再映射到某个脚手架语言。
    #[test]
    fn language_flag_only_picks_a_build_path() {
        assert!(matches!(builder::Kind::from(LangArg::C), builder::Kind::C));
        assert!(matches!(
            builder::Kind::from(LangArg::Cpp),
            builder::Kind::C
        ));
        for arg in [LangArg::Rust, LangArg::Python, LangArg::Js] {
            assert!(matches!(builder::Kind::from(arg), builder::Kind::Cargo));
        }
    }
}
