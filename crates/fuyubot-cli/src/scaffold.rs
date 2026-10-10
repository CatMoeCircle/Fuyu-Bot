//! 运行时 CLI 的脚手架：`fuyubot init`（项目配置）与 `plugin install`。
//!
//! **插件的脚手架不在这里**——那是开发 CLI `fuyubot-sdk init` 的事（`ROADMAP.md` 变更 4）。
//! 这里只剩「项目配置」与「把编好的插件装进插件目录」两件事。
//!
//! 项目模板用 `include_str!` 嵌进二进制，所以 `fuyubot init` 在任何目录都能跑，
//! 不需要用户装一份模板文件。

use std::path::{Path, PathBuf};

use crate::builder::DLL_EXTENSION;

/// 项目配置文件名。
pub const CONFIG_FILE: &str = "fuyubot.toml";

const TEMPLATE_CONFIG: &str = include_str!("../templates/fuyubot.toml");

// ── 项目配置 ───────────────────────────────────────────────────────

#[derive(Debug, Default, serde::Deserialize)]
pub struct Config {
    #[serde(default)]
    pub bot: Bot,
    #[serde(default)]
    pub plugins: Plugins,
    #[serde(default)]
    pub tdlib: Tdlib,
}

#[derive(Debug, Default, serde::Deserialize)]
pub struct Bot {
    pub name: Option<String>,
    /// TDLib api_id（**不是**秘密，放配置里可以）。
    ///
    /// `api_hash` 这里**故意没有**字段：它是凭据，只允许从环境变量 `TG_API_HASH`
    /// 或交互式输入来（硬约束 41）。写了也会被 serde 忽略。
    pub api_id: Option<i64>,
}

#[derive(Debug, Default, serde::Deserialize)]
pub struct Plugins {
    pub dir: Option<String>,
}

#[derive(Debug, Default, serde::Deserialize)]
pub struct Tdlib {
    pub path: Option<String>,
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
pub fn find_config(start: &Path) -> Option<PathBuf> {
    let mut dir = Some(start.to_path_buf());
    while let Some(current) = dir {
        let candidate = current.join(CONFIG_FILE);
        if candidate.is_file() {
            return Some(candidate);
        }
        dir = current.parent().map(Path::to_path_buf);
    }
    None
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
    Ok(config)
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
/// - `build-plugin` 直接把自己知道的工程目录传进来（它已经拿到产物的真实路径了）。
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
             在插件工程目录里跑 `fuyubot build-plugin`（它会编译并装进插件目录）；\
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
        assert_eq!(config.bot.api_id, Some(12345));
        assert!(root.join("plugins").is_dir());
        // 第二次要被挡住，不能把用户改过的配置冲掉。
        assert!(init(&root, "mybot", 1).is_err());
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
}
