//! 运行时 CLI 的脚手架：`fuyubot init`（项目配置）与 `plugin install`。
//!
//! **插件的脚手架不在这里**——那是开发 CLI `fuyubot-sdk init` 的事（`docs/decisions.md` 变更 4）。
//! 这里只剩「项目配置」与「把编好的插件装进插件目录」两件事。
//!
//! 项目模板用 `include_str!` 嵌进二进制，所以 `fuyubot init` 在任何目录都能跑，
//! 不需要用户装一份模板文件。

use std::path::{Path, PathBuf};

use fuyubot_loader::DLL_EXTENSION;

/// 项目配置文件名。
pub const CONFIG_FILE: &str = "fuyubot.toml";

const TEMPLATE_CONFIG: &str = include_str!("../templates/fuyubot.toml");

// ── 项目配置 ───────────────────────────────────────────────────────

#[derive(Debug, Default, serde::Deserialize)]
pub struct Config {
    #[serde(default)]
    pub bot: Bot,
    #[serde(default)]
    pub telegram: Telegram,
    #[serde(default)]
    pub plugins: Plugins,
    #[serde(default)]
    pub tdlib: Tdlib,
}

#[derive(Debug, Default, serde::Deserialize)]
pub struct Bot {
    pub name: Option<String>,
    /// TDLib api_id 的**旧位置**（[`Telegram::api_id`] 优先）。新配置写进 `[telegram]`。
    ///
    /// `api_hash` 这里**故意没有**字段：它是凭据，只允许从环境变量 `TG_API_HASH`
    /// 或交互式输入来（硬约束 41）。写了也会被 serde 忽略。
    pub api_id: Option<i64>,
}

/// `[telegram]`：TDLib 参数与**账号类型**。
///
/// ⚠️ 凭据（`api_hash` / token / 手机号）**不在这里**（硬约束 41）——只落**类型**。
#[derive(Debug, Default, serde::Deserialize)]
pub struct Telegram {
    /// `"bot"`（默认）或 `"user"`——决定登录走哪条路（Bot Token / 手机号 / 扫码）。
    pub account_type: Option<String>,
    /// TDLib 的 `database_directory` 与 `files_directory` **都**指它（decisions 变更 1：
    /// 只建 `data/`，不拆 `data/db`）。相对项目根目录。
    pub data_dir: Option<String>,
    /// TDLib api_id（应用凭据，不是秘密）。两处都没写就用 0。
    pub api_id: Option<i64>,
}

impl Telegram {
    /// 认账号类型。**写错就报错指路**，不静默当 bot——否则用户会拿着错的登录方式
    /// 一直等不到验证码，而错误信息完全不指向配置。
    ///
    /// 大小写与首尾空格都容忍（`" User "` 也认）：手写配置里这两种很常见，
    /// 而 `Bot` / `User` 本来就无歧义——trim 了却不忽略大小写是自相矛盾的。
    pub fn account_type(&self) -> Result<fuyubot_tdlib::AccountType, String> {
        let raw = self
            .account_type
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("bot");
        match raw.to_ascii_lowercase().as_str() {
            "bot" => Ok(fuyubot_tdlib::AccountType::Bot),
            "user" => Ok(fuyubot_tdlib::AccountType::User),
            _ => Err(format!(
                "[telegram].account_type = \"{raw}\" 认不出来，只能写 \"bot\" 或 \"user\""
            )),
        }
    }

    /// `[telegram].api_id` 优先，退回旧的 `[bot].api_id`。
    pub fn api_id(&self, bot: &Bot) -> i64 {
        self.api_id.or(bot.api_id).unwrap_or(0)
    }

    /// TDLib 的数据目录（配置里没写就 `<root>/data`）。
    pub fn data_dir(&self, root: &Path) -> PathBuf {
        root.join(self.data_dir.as_deref().unwrap_or("data"))
    }
}

#[derive(Debug, Default, serde::Deserialize)]
pub struct Plugins {
    pub dir: Option<String>,
}

#[derive(Debug, Default, serde::Deserialize)]
pub struct Tdlib {
    pub path: Option<String>,
    /// TDLib 的日志详细度。**配置里没写就用 1**（框架的默认值，见
    /// [`fuyubot_tdlib::DEFAULT_LOG_VERBOSITY`]）——TDLib 自己的默认太啰嗦。
    pub log_verbosity: Option<i32>,
}

impl Tdlib {
    /// 日志详细度：配置优先，否则默认 1。**负数钳到 0**（TDLib 只认非负）。
    pub fn log_verbosity(&self) -> i32 {
        self.log_verbosity
            .unwrap_or(fuyubot_tdlib::DEFAULT_LOG_VERBOSITY)
            .max(0)
    }
}

impl Config {
    /// 插件目录（配置里没写就用 `plugins`）。
    pub fn plugin_dir(&self, root: &Path) -> PathBuf {
        root.join(self.plugins.dir.as_deref().unwrap_or("plugins"))
    }
}

/// 读配置；文件不存在就给一份默认值（**不是错误**：不配也能起）。
pub fn load_config(path: &Path) -> Result<Config, String> {
    if !path.is_file() {
        return Ok(Config::default());
    }
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("读不了 {}: {e}", path.display()))?;
    toml::from_str(&text).map_err(|e| format!("{} 不是合法配置: {e}", path.display()))
}

/// 从 `start` 往上找 `fuyubot.toml`。
///
/// ⚠️ **相对路径必须先变绝对**：`Path::new(".").parent()` 是 `Some("")`，`""` 又已经没有父目录，
/// 于是从相对路径出发爬一步就到底了——在子目录里跑 `fuyubot status` 会谎报「这里没有配置」。
/// 而默认的 `--config fuyubot.toml` 走的正是相对路径（踩过一次：`config` 在子目录里
/// 报「文件不存在，全是默认值」，其实上层就有）。
pub fn find_config(start: &Path) -> Option<PathBuf> {
    let start: PathBuf = if start.is_absolute() {
        start.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(start)
    };
    let mut dir = Some(start);
    while let Some(current) = dir {
        let candidate = current.join(CONFIG_FILE);
        if candidate.is_file() {
            return Some(candidate);
        }
        dir = current.parent().map(Path::to_path_buf);
    }
    None
}

// ── config list ────────────────────────────────────────────────────

/// `fuyubot config` 的几行输出：**生效值**（缺省已填好），外加这份文件到底在哪。
///
/// 纯函数，不碰环境。文件不存在时如实说「全是默认值」——`run` 本来就是按默认值起的。
pub fn config_lines(config_path: &Path, root: &Path, config: &Config) -> Vec<String> {
    let missing = if config_path.is_file() {
        ""
    } else {
        "（不存在，下面全是默认值）"
    };
    let account = match config.telegram.account_type() {
        Ok(kind) => kind.as_str().to_owned(),
        // 写错就**把原因原样报出来**：这正是「我改的那行生效了吗」要回答的东西。
        Err(e) => format!("⚠️ {e}"),
    };
    let tdlib_path = config
        .tdlib
        .path
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("自动（TDLIB_TDJSON_PATH → 自带 → exe 同级 → 系统库 → 裸库名）");
    vec![
        format!("配置文件  {}{missing}", config_path.display()),
        format!("项目根    {}", root.display()),
        format!(
            "[bot]       name         = {}",
            config.bot.name.as_deref().unwrap_or("(未设置)")
        ),
        format!("[telegram]  account_type = {account}"),
        format!(
            "[telegram]  api_id       = {}",
            config.telegram.api_id(&config.bot)
        ),
        format!(
            "[telegram]  data_dir     = {}",
            config.telegram.data_dir(root).display()
        ),
        format!(
            "[plugins]   dir          = {}",
            config.plugin_dir(root).display()
        ),
        format!("[tdlib]     path         = {tdlib_path}"),
        format!(
            "[tdlib]     log_verbosity= {}",
            config.tdlib.log_verbosity()
        ),
        "凭据        api_hash / token / 手机号不读配置（TG_API_HASH 等环境变量或交互输入）"
            .to_owned(),
    ]
}

// ── init ───────────────────────────────────────────────────────────

/// 生成 `fuyubot.toml` 与插件目录。
///
/// ⚠️ 只写 `api_id`：`api_hash` 是凭据，**不许**落进配置文件（硬约束 41）。
pub fn init(root: &Path, name: &str, api_id: i64) -> Result<PathBuf, String> {
    let config = root.join(CONFIG_FILE);
    if config.exists() {
        return Err(format!("{} 已经存在了，先挪走它", config.display()));
    }
    std::fs::create_dir_all(root).map_err(|e| format!("建不了 {}: {e}", root.display()))?;
    let rendered = TEMPLATE_CONFIG
        .replace("__NAME__", name)
        .replace("__API_ID__", &api_id.to_string());
    write(&config, &rendered)?;

    let plugins = root.join("plugins");
    std::fs::create_dir_all(&plugins).map_err(|e| format!("建不了 {}: {e}", plugins.display()))?;
    // 内置插件（/start、/help、/plugins）跟可执行文件一起打包，顺手复制进来：
    // 「构建/解包之后直接跑就有基础命令」，不用再 `plugin install` 三遍。
    match install_builtins(&plugins) {
        Ok(copied) if !copied.is_empty() => {
            println!("已安装 {} 个内置插件到 {}", copied.len(), plugins.display());
        }
        Ok(_) => {}
        Err(error) => eprintln!("装内置插件失败（继续）: {error}"),
    }
    Ok(config)
}

/// 内置插件的文件名主干：`fuyubot_plugin_start` → `fuyubot_plugin_start.{dll,so,dylib}`。
///
/// 这三个由 `cargo package-bot`（以及 release 流程）编出来，跟可执行文件放在一起。
/// ⚠️ 是**打包后**的名字：cargo 在 Linux / macOS 上给 cdylib 加 `lib` 前缀，
/// 发布包与 `cargo package-bot` 的 `target/release/` 里两种形态都可能出现，
/// 所以查找走 [`builtin_artifact`]，别在这里拼死文件名。
pub const BUILTIN_PLUGINS: &[&str] = &[
    "fuyubot_plugin_start",
    "fuyubot_plugin_help",
    "fuyubot_plugin_manage",
];

/// 在 `dir` 里找内置插件的产物：无前缀名优先，其次才是 cargo 加的 `lib` 前缀名。
///
/// Windows 上 `cargo build -p fuyubot-plugin-start` 出的是 `fuyubot_plugin_start.dll`，
/// Linux / macOS 上却是 `libfuyubot_plugin_start.{so,dylib}`——同一件事两种名字。
fn builtin_artifact(dir: &Path, stem: &str) -> Option<PathBuf> {
    [
        format!("{stem}.{DLL_EXTENSION}"),
        format!("lib{stem}.{DLL_EXTENSION}"),
    ]
    .into_iter()
    .map(|file| dir.join(file))
    .find(|path| path.is_file())
}

/// 把内置插件装进插件目录：先看**可执行文件旁边**，再看它下面的 `plugins/`
/// （发布包里是后者，`cargo package-bot` 出来的是前者）。
///
/// **只装缺的**：目标已存在就跳过，不覆盖用户换过的版本。返回这次真的拷过去的文件。
/// 装进去的名字一律**无前缀**（`fuyubot_plugin_start.so`，不带 `lib`）：与发布包一致，
/// 也让「已存在就跳过」的判断对得上——否则 Linux 上每次 `init` 都会再拷一份 `lib…so`。
///
/// 这只是「复制一个已经编好的产物」，不碰编译——和 `plugin install` 同一件事（约束 40）。
pub fn install_builtins(plugin_dir: &Path) -> Result<Vec<PathBuf>, String> {
    let exe = std::env::current_exe().map_err(|e| format!("拿不到可执行文件路径: {e}"))?;
    let Some(exe_dir) = exe.parent() else {
        return Ok(Vec::new());
    };
    let sources = [exe_dir.to_path_buf(), exe_dir.join("plugins")];
    std::fs::create_dir_all(plugin_dir)
        .map_err(|e| format!("建不了 {}: {e}", plugin_dir.display()))?;

    let mut copied = Vec::new();
    for stem in BUILTIN_PLUGINS {
        let Some(source) = sources.iter().find_map(|dir| builtin_artifact(dir, stem)) else {
            continue;
        };
        let target = plugin_dir.join(format!("{stem}.{DLL_EXTENSION}"));
        if target.exists() {
            continue;
        }
        std::fs::copy(&source, &target)
            .map_err(|e| format!("拷贝 {} → {} 失败: {e}", source.display(), target.display()))?;
        copied.push(target);
    }
    Ok(copied)
}

// ── plugin install ─────────────────────────────────────────────────

/// 把一个已编好的插件（文件或插件工程目录）装进插件目录。
///
/// 会连**旁边的脚本文件**一起拷：Python / JS / TS 插件的 `.py` / `.js` / `.ts` 必须和 cdylib
/// 待在同一目录，宿主才找得到（见 `find_script`）。
pub fn install(source: &Path, plugin_dir: &Path) -> Result<Vec<PathBuf>, String> {
    let artifact = find_artifact(source)?;
    install_from(&artifact, &companion_dirs(source, &artifact), plugin_dir)
}

/// 把某个具体产物连同它的同伴文件装进插件目录。
///
/// `extra_dirs` 是**同伴文件的额外搜索目录**：
/// - `plugin install <工程目录>` 走 [`companion_dirs`]（工程目录 + 产物所在目录）；
/// - 别处要装一个已知路径的产物时，把它的工程目录传进来即可。
///
/// 只搬「像插件同伴」的文件（`.py` / `.js` / `.ts` / `.pyi`），别的（README、`target/`……）不动。
///
/// ⚠️ 用 `extension()` 判断时**没有 `"d.ts"` 这一项**：`Path::extension("fuyubot_plugin.d.ts")`
/// 给的是 `"ts"`。以前这里写了 `"d.ts"`，那个分支**永远走不到**——也就是 `.d.ts` 从来没被拷过。
/// 现在靠 `"ts"` 一并盖住 `.ts` 与 `.d.ts`。
pub fn install_from(
    artifact: &Path,
    extra_dirs: &[PathBuf],
    plugin_dir: &Path,
) -> Result<Vec<PathBuf>, String> {
    std::fs::create_dir_all(plugin_dir)
        .map_err(|e| format!("建不了 {}: {e}", plugin_dir.display()))?;
    let mut copied = vec![copy(artifact, plugin_dir)?];

    for dir in extra_dirs {
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            let is_companion = path.extension().is_some_and(|extension| {
                matches!(extension.to_str(), Some("py" | "js" | "ts" | "pyi"))
            });
            if is_companion && path.is_file() {
                copied.push(copy(&path, plugin_dir)?);
            }
        }
    }
    Ok(copied)
}

/// 在目录里找那个唯一的插件动态库。
pub fn find_artifact(source: &Path) -> Result<PathBuf, String> {
    if source.is_file() {
        return Ok(source.to_path_buf());
    }
    let mut candidates = Vec::new();
    for dir in [
        source.join("target").join("release"),
        source.join("target").join("debug"),
        source.to_path_buf(),
    ] {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_file() && path.extension().is_some_and(|e| e == DLL_EXTENSION) {
                candidates.push(path);
            }
        }
        if !candidates.is_empty() {
            break;
        }
    }
    match candidates.len() {
        0 => Err(format!(
            "{} 里没找到 .{DLL_EXTENSION}。\
             本 CLI **不编译插件**（打包归作者自己的语言工具链）：先用 `cargo build`（Rust / \
             Python / JS 插件的 cdylib 都是 Rust 编的）或 `cl /LD`（C/C++）编出来，\
             再把产物或工程目录交给本命令。\
             注意 workspace 成员的产物落在**仓库根**的 target/ 下，本命令只查工程自己的 target/。",
            source.display()
        )),
        1 => Ok(candidates.remove(0)),
        _ => Err(format!(
            "{} 里有多个候选，指定其中一个：{}",
            source.display(),
            candidates
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// 插件自己的脚本（`.py` / `.js` / `.ts`）——和 cdylib 同目录同名的那个。
///
/// 宿主加载 cdylib 时把它的绝对路径当配置传进去（见 `run`），所以这里要能找对。
/// cargo 在类 Unix 上会给 cdylib 加 `lib` 前缀，两种都试。
///
/// `.ts` 也在列：TypeScript 插件走 Node 自带的**类型剥离**直接跑（Node ≥ 23.6），
/// 不需要先 `tsc`（见 `examples/ts-counter`）。Node 太老会由桥报「启动失败」，
/// 不会变成「加载成功但什么都不干」。
pub fn find_script(dll: &Path) -> Option<PathBuf> {
    let dir = dll.parent()?;
    let stem = dll.file_stem()?.to_string_lossy().into_owned();
    let bare = stem.strip_prefix("lib").unwrap_or(&stem).to_owned();
    for candidate in [stem.clone(), bare] {
        for extension in ["py", "js", "ts"] {
            let path = dir.join(format!("{candidate}.{extension}"));
            if path.is_file() {
                return Some(path);
            }
        }
    }
    None
}

/// 同伴文件可能待在哪些目录里。
///
/// 工程目录（源里就放着脚本）与产物所在目录（构建期拷过去的）都要看，
/// 顺序即拷贝顺序，重复的由调用方自己保证不成问题（`copy` 是覆盖写）。
fn companion_dirs(source: &Path, artifact: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if source.is_dir() {
        dirs.push(source.to_path_buf());
    }
    if let Some(parent) = artifact.parent() {
        dirs.push(parent.to_path_buf());
    }
    dirs
}

fn copy(from: &Path, into: &Path) -> Result<PathBuf, String> {
    let name = from
        .file_name()
        .ok_or_else(|| format!("{} 没有文件名", from.display()))?;
    let target = into.join(name);
    std::fs::copy(from, &target)
        .map_err(|e| format!("拷贝 {} → {} 失败: {e}", from.display(), target.display()))?;
    Ok(target)
}

fn write(path: &Path, content: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("建不了 {}: {e}", parent.display()))?;
    }
    std::fs::write(path, content).map_err(|e| format!("写不了 {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::temp;

    #[test]
    fn init_writes_a_config_that_parses_back() {
        let root = temp("init");
        let path = init(&root, "mybot", 12345).unwrap();
        let config = load_config(&path).unwrap();
        assert_eq!(config.bot.name.as_deref(), Some("mybot"));
        // api_id 现在写在 `[telegram]`；`[bot].api_id` 仍认（兼容位置），两者都能取到。
        assert_eq!(config.telegram.api_id, Some(12345));
        assert_eq!(config.telegram.api_id(&config.bot), 12345);
        // 模板默认 bot + `data/`；账号类型与数据目录都要能从生成的配置里读回来。
        assert_eq!(
            config.telegram.account_type().unwrap(),
            fuyubot_tdlib::AccountType::Bot
        );
        assert_eq!(config.telegram.data_dir(&root), root.join("data"));
        assert!(root.join("plugins").is_dir());
        // 第二次要被挡住，不能把用户改过的配置冲掉。
        assert!(init(&root, "mybot", 1).is_err());
    }

    /// 账号类型写错必须**报错指路**，不能静默当 bot——否则用户会拿着错的登录方式
    /// 一直等不到验证码，而错误信息完全不指向配置。
    #[test]
    fn a_bogus_account_type_is_rejected_with_a_hint() {
        let telegram = Telegram {
            account_type: Some("robot".to_owned()),
            ..Telegram::default()
        };
        let error = telegram.account_type().unwrap_err();
        assert!(error.contains("robot"), "报错要带上写错的值：{error}");
        assert!(
            error.contains("bot") && error.contains("user"),
            "报错要说清只能写哪两个：{error}"
        );
        // 大写/带空格不算错（手写配置很常见），`user` 要真的认成 user。
        let telegram = Telegram {
            account_type: Some(" User ".to_owned()),
            ..Telegram::default()
        };
        assert_eq!(
            telegram.account_type().unwrap(),
            fuyubot_tdlib::AccountType::User
        );
    }

    /// 日志详细度的默认值与钳位：**没配就是 1**（TDLib 自己的默认太啰嗦），
    /// 负数当 0（TDLib 只认非负，别把负值原样丢过去）。
    #[test]
    fn log_verbosity_defaults_to_one_and_clamps_negatives() {
        assert_eq!(Tdlib::default().log_verbosity(), 1);
        assert_eq!(
            Tdlib {
                path: None,
                log_verbosity: Some(5)
            }
            .log_verbosity(),
            5
        );
        assert_eq!(
            Tdlib {
                path: None,
                log_verbosity: Some(-3)
            }
            .log_verbosity(),
            0
        );
        // 模板里写出来的那个值也要等于默认值，否则「默认 1」只存在于代码注释里。
        let rendered = TEMPLATE_CONFIG;
        assert!(
            rendered.contains("log_verbosity = 1"),
            "模板里的 log_verbosity 必须与默认值一致：\n{rendered}"
        );
    }

    #[test]
    fn init_never_writes_the_api_hash_into_the_config() {
        // 硬约束 41：api_hash 是凭据，不许落盘。
        let root = temp("init-no-secret");
        let path = init(&root, "mybot", 12345).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        // 只查**键**：注释里提到 `api_hash` 是故意的（要提醒用户别往这儿写）。
        assert!(
            !text
                .lines()
                .any(|line| line.trim_start().starts_with("api_hash")),
            "配置里出现了 api_hash 键：\n{text}"
        );
        assert!(
            !text.contains("__API_HASH__"),
            "模板占位符没被换掉：\n{text}"
        );
    }

    #[test]
    fn missing_config_is_not_an_error() {
        let config = load_config(Path::new("does-not-exist.toml")).unwrap();
        assert_eq!(config.bot.api_id, None);
    }

    #[test]
    fn script_is_found_next_to_the_library() {
        let dir = temp("find-script");
        let dll = dir.join(format!("libweather.{DLL_EXTENSION}"));
        std::fs::write(&dll, b"x").unwrap();
        assert_eq!(find_script(&dll), None);
        std::fs::write(dir.join("weather.py"), b"print()").unwrap();
        assert_eq!(
            find_script(&dll).unwrap().file_name().unwrap(),
            "weather.py",
            "要去掉 cargo 加的 lib 前缀"
        );
    }

    /// `.ts` 也在查找列表里（TypeScript 插件直接靠 Node 剥类型跑）。
    /// 同一个名字同时存在时按 `py` → `js` → `ts` 的顺序取第一个。
    #[test]
    fn script_lookup_also_accepts_typescript() {
        let dir = temp("find-script-ts");
        let dll = dir.join(format!("counter.{DLL_EXTENSION}"));
        std::fs::write(&dll, b"x").unwrap();
        std::fs::write(dir.join("counter.ts"), b"export {}").unwrap();
        assert_eq!(
            find_script(&dll).unwrap().file_name().unwrap(),
            "counter.ts"
        );

        // .js 优先于 .ts（两个都在时不该挑 TS）
        std::fs::write(dir.join("counter.js"), b"// js").unwrap();
        assert_eq!(
            find_script(&dll).unwrap().file_name().unwrap(),
            "counter.js"
        );
    }

    /// 装插件时**脚本要跟着走**（宿主按 cdylib 同目录找脚本），无关文件不搬。
    #[test]
    fn install_copies_the_library_and_its_companions() {
        let source = temp("install-src");
        let plugin_dir = temp("install-dst");
        let dll = source.join(format!("weather.{DLL_EXTENSION}"));
        std::fs::write(&dll, b"lib").unwrap();
        std::fs::write(source.join("weather.py"), b"print()").unwrap();
        std::fs::write(source.join("fuyubot_plugin.pyi"), b"stub").unwrap();
        // `.d.ts` 的 extension() 是 `ts`——这条以前**拷不到**（旧代码里那个分支是死代码）。
        std::fs::write(source.join("fuyubot_plugin.d.ts"), b"declare").unwrap();
        std::fs::write(source.join("counter.ts"), b"export {}").unwrap();
        std::fs::write(source.join("readme.md"), b"nope").unwrap();

        let copied = install(&dll, &plugin_dir).unwrap();
        let names: Vec<String> = copied
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();

        assert!(
            names.contains(&format!("weather.{DLL_EXTENSION}")),
            "{names:?}"
        );
        assert!(names.contains(&"weather.py".to_owned()), "{names:?}");
        assert!(
            names.contains(&"fuyubot_plugin.pyi".to_owned()),
            "{names:?}"
        );
        assert!(
            names.contains(&"fuyubot_plugin.d.ts".to_owned()),
            "{names:?}"
        );
        assert!(names.contains(&"counter.ts".to_owned()), "{names:?}");
        assert!(
            !names.iter().any(|n| n == "readme.md"),
            "无关文件不该搬：{names:?}"
        );
        assert!(plugin_dir.join("weather.py").is_file());
    }

    /// 工程目录形态：产物在 `target/debug/` 下，脚本在工程根——两边都要扫到。
    #[test]
    fn install_from_scans_the_project_dir_where_the_scripts_live() {
        let project = temp("install-project");
        let plugin_dir = temp("install-from-dst");
        let out = project.join("target").join("debug");
        std::fs::create_dir_all(&out).unwrap();
        let dll = out.join(format!("weather.{DLL_EXTENSION}"));
        std::fs::write(&dll, b"lib").unwrap();
        std::fs::write(project.join("weather.py"), b"print()").unwrap();

        let copied = install_from(&dll, &[project.clone()], &plugin_dir).unwrap();
        assert_eq!(copied.len(), 2, "{copied:?}");
        assert!(plugin_dir.join("weather.py").is_file());
    }

    /// Linux / macOS 上 cargo 给 cdylib 加 `lib` 前缀，Windows 上不加。
    ///
    /// 以前 `install_builtins` 只拼无前缀名，于是非 Windows 上 `cargo package-bot` 之后
    /// `init` 会**静默**装不上内置插件（发布脚本里同样的拼法还把 release 打挂了）。
    #[test]
    fn builtin_artifact_accepts_the_cargo_lib_prefix() {
        let dir = temp("builtin-artifact");
        let stem = "fuyubot_plugin_start";
        let plain = dir.join(format!("{stem}.{DLL_EXTENSION}"));
        let prefixed = dir.join(format!("lib{stem}.{DLL_EXTENSION}"));

        assert_eq!(builtin_artifact(&dir, stem), None, "两个都没有时应该找不到");

        std::fs::write(&prefixed, b"lib").unwrap();
        assert_eq!(
            builtin_artifact(&dir, stem).as_deref(),
            Some(prefixed.as_path()),
            "带 lib 前缀的产物也要认"
        );

        // 两个都在时优先无前缀（Windows / 发布包的形态），别被 cargo 的中间产物盖掉。
        std::fs::write(&plain, b"plain").unwrap();
        assert_eq!(
            builtin_artifact(&dir, stem).as_deref(),
            Some(plain.as_path())
        );
    }
}
