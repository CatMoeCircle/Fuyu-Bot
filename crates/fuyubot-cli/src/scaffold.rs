//! 运行时 CLI 的脚手架：`fuyubot init`（项目配置）与 `plugin install`。
//!
//! **插件开发不在这里**——没有开发 CLI 了，脚手架与示例都在文档站（`docs/decisions.md` 变更 12）。
//! 这里只剩「项目配置」与「把编好的插件装进插件目录」两件事。
//!
//! ⚠️ 配置 schema / 模板 / 文本级改写**已经搬到 `fuyubot_app::config`**：Android 的 JNI
//! 运行时读的是同一个 `fuyubot.toml`，schema 留在 CLI 里迟早跟 Android 那份漂移。这里只做
//! **转出口**，免得 CLI 与面板里几十个调用点全改成 `fuyubot_app::…`。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use fuyubot_core::Kernel;
use fuyubot_loader::{
    BundleManifest, DISABLED_DIR, DLL_EXTENSION, PluginEntry, PluginKind, PluginLoader,
    SCRIPTS_DIR, current_platform, is_bundle_dir, plugin_entries,
};

/// 配置那一套：单一实现在 `fuyubot-app`，这里原样转出。
pub use fuyubot_app::config::{
    CONFIG_FILE, Config, LOG_LEVELS, config_lines, load_api_hash, load_config, log_level_name,
    parse_log_level, quote_toml_string, remember_credentials, set_config_value,
};

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

// ── init ───────────────────────────────────────────────────────────

/// 生成 `fuyubot.toml` 与插件目录（不写 `api_hash`）。
pub fn init(root: &Path, name: &str, api_id: i64) -> Result<PathBuf, String> {
    init_with_api_hash(root, name, api_id, None)
}

/// 生成 `fuyubot.toml` 与插件目录，可选把 `api_hash` 一起明文写进 `[telegram]`。
pub fn init_with_api_hash(
    root: &Path,
    name: &str,
    api_id: i64,
    api_hash: Option<&str>,
) -> Result<PathBuf, String> {
    let config = root.join(CONFIG_FILE);
    if config.exists() {
        return Err(format!("{} 已经存在了，先挪走它", config.display()));
    }
    // 渲染 + 写盘在 `fuyubot-app`（Android 的 `config_init` 走同一个函数，模板只有一份）。
    let request = fuyubot_app::InitRequest {
        name: Some(name.to_owned()),
        api_id: Some(api_id),
        api_hash: api_hash.map(str::to_owned),
        ..fuyubot_app::InitRequest::default()
    };
    let config = fuyubot_app::init_config(root, &request)?;

    let plugins = root.join("plugins");
    std::fs::create_dir_all(&plugins).map_err(|e| format!("建不了 {}: {e}", plugins.display()))?;
    // 内置插件（/start、/help、/plugins）跟可执行文件一起打包，顺手复制进来：
    // 「构建/解包之后直接跑就有基础命令」，不用再 `plugin install` 三遍。
    match install_builtins(&plugins) {
        Ok(copied) if !copied.is_empty() => {
            cli!(
                info,
                "已安装 {} 个内置插件到 {}",
                copied.len(),
                plugins.display()
            );
        }
        Ok(_) => {}
        Err(error) => cli!(warn, "装内置插件失败（继续）: {error}"),
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
/// ⚠️ **被禁用（`….disable`）的也算「已存在」**：不然面板里关掉一个内置插件之后，
/// 下次 `run` 会重新拷一份启用的进来，开关等于没按——禁用是用户的决定，不该被
/// 「只装缺的」这一步悄悄撤销。
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
        if target.exists() || fuyubot_loader::disabled_path(&target).exists() {
            continue;
        }
        std::fs::copy(&source, &target)
            .map_err(|e| format!("拷贝 {} → {} 失败: {e}", source.display(), target.display()))?;
        copied.push(target);
    }
    Ok(copied)
}

// ── 脚本插件的静态元数据 ───────────────────────────────────────────

/// 从脚本工程的 manifest 里读出来的静态身份（不 dlopen、不执行脚本）。
///
/// 只覆盖分发格式 v1 需要的那几个字段：`id` / 展示名 / 版本 / 语言 / 入口。
/// 来源是 `package.json` 的 `fuyubot` 段或 `pyproject.toml` 的 `[tool.fuyubot]`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptMeta {
    /// 插件 id（`fuyubot.name`）。目录名不一致时以它为准。
    pub id: String,
    /// 展示名（清单里的可读名字；缺省与 `id` 相同）。
    pub name: String,
    /// 版本（`package.json.version` / `[project].version`），缺省 `0.0.0`。
    pub version: String,
    /// 谁来跑它：JS 或 Python。
    pub kind: PluginKind,
    /// 入口的相对路径（`plugin.json` 的 `entry`）。
    pub entry: String,
}

/// 读一个**脚本插件目录**的静态身份；没有 fuyubot 身份段时返回 `None`。
///
/// 先看 `package.json`（JS），再看 `pyproject.toml`（Python）——与
/// [`is_script_plugin_dir`] 同一把尺子，两个中间插件的 discover 也认这两处。
/// 只读文件，不跑 `bun install` / `uv sync`，更不执行任何脚本。
pub fn script_metadata(dir: &Path) -> Option<ScriptMeta> {
    if !dir.is_dir() {
        return None;
    }
    js_script_metadata(dir).or_else(|| py_script_metadata(dir))
}

/// `package.json` 的 `fuyubot` 段。
fn js_script_metadata(dir: &Path) -> Option<ScriptMeta> {
    let text = std::fs::read_to_string(dir.join("package.json")).ok()?;
    let json: serde_json::Value = serde_json::from_str(&text).ok()?;
    let fuyubot = json.get("fuyubot")?;
    let id = text_field(fuyubot, "name")?;
    // 展示名优先 fuyubot.display_name / title，其次 package.json 的 name，最后回退 id。
    let name = ["display_name", "title"]
        .iter()
        .find_map(|key| text_field(fuyubot, key))
        .or_else(|| text_field(&json, "name"))
        .unwrap_or_else(|| id.clone());
    let version = text_field(&json, "version").unwrap_or_else(|| "0.0.0".to_owned());
    let entry = text_field(fuyubot, "entry").unwrap_or_else(|| default_js_entry(dir));
    Some(ScriptMeta {
        id,
        name,
        version,
        kind: PluginKind::Js,
        entry,
    })
}

/// `pyproject.toml` 的 `[tool.fuyubot]`。
fn py_script_metadata(dir: &Path) -> Option<ScriptMeta> {
    let text = std::fs::read_to_string(dir.join("pyproject.toml")).ok()?;
    let value: toml::Value = toml::from_str(&text).ok()?;
    let fuyubot = value.get("tool")?.get("fuyubot")?;
    let id = toml_text_field(fuyubot, "name")?;
    let project = value.get("project");
    let name = ["display_name", "title"]
        .iter()
        .find_map(|key| toml_text_field(fuyubot, key))
        .or_else(|| project.and_then(|project| toml_text_field(project, "name")))
        .unwrap_or_else(|| id.clone());
    let version = project
        .and_then(|project| toml_text_field(project, "version"))
        .unwrap_or_else(|| "0.0.0".to_owned());
    let entry = toml_text_field(fuyubot, "entry").unwrap_or_else(|| "main.py".to_owned());
    Some(ScriptMeta {
        id,
        name,
        version,
        kind: PluginKind::Python,
        entry,
    })
}

/// JS 入口的约定：`index.ts` 在就用它，否则退回编译后的 `index.js`。
fn default_js_entry(dir: &Path) -> String {
    if dir.join("index.ts").is_file() {
        "index.ts".to_owned()
    } else {
        "index.js".to_owned()
    }
}

/// JSON 里一个非空字符串字段（首尾空格去掉）。
fn text_field(value: &serde_json::Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

/// TOML 里一个非空字符串字段（首尾空格去掉）。
fn toml_text_field(value: &toml::Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(toml::Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

/// 版本号的落盘形态：只留清单允许的字符；空 / 全是非法字符时退回 `0.0.0`。
///
/// `plugin.json` 的 `version` 只认 `[0-9A-Za-z.+-]`（契约 §3），而脚本工程的
/// 版本号可能带空格之类的写法——迁移是**帮用户转形态**，不该因为一个空格就拒绝。
fn safe_version(raw: &str) -> String {
    let version: String = raw
        .trim()
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '+' | '-'))
        .collect();
    if version.is_empty() {
        "0.0.0".to_owned()
    } else {
        version
    }
}

/// 把脚本身份合成一份 `plugin.json`。
fn script_manifest(meta: &ScriptMeta) -> BundleManifest {
    BundleManifest {
        schema: 1,
        id: meta.id.clone(),
        // `name` 是必填且不能为空：展示名缺失时用 id 顶上。
        name: meta.name.trim().to_owned(),
        version: safe_version(&meta.version),
        kind: meta.kind,
        entry: meta.entry.clone(),
        abi: None,
        // 脚本跨平台：空表 = 全平台（契约 §3）。
        platforms: Vec::new(),
        depends: Vec::new(),
        optional_depends: Vec::new(),
        provides: Vec::new(),
        config: None,
        description: String::new(),
        license: String::new(),
        homepage: String::new(),
    }
}

/// 用「写出去再读回来」的方式校验一份要落盘的清单。
///
/// `BundleManifest` 的字段是公开的，构造时不走解析；`id` / `version` / `entry`
/// 的语法校验只在 [`BundleManifest::parse`] 里。写盘前过一遍它，免得生成一份
/// 连自己都读不回来的 `plugin.json`。
fn validated_manifest(mut manifest: BundleManifest) -> Result<BundleManifest, String> {
    if manifest.name.trim().is_empty() {
        manifest.name = manifest.id.clone();
    }
    BundleManifest::parse(&manifest.to_json()).map_err(|error| error.to_string())
}

// ── plugin install ─────────────────────────────────────────────────

/// 把一个插件装进插件目录。**三种形态，按来源自动分派**：
///
/// 1. **插件包目录**（v1）：目录根部有 `plugin.json`。`kind = native` → 整目录复制到
///    `plugins/<id>/`；`python` / `js` → `plugins/scripts/<id>/`（契约 §6）。
/// 2. **老式脚本插件目录**（v2）：没有 `plugin.json`，但 `package.json` /
///    `pyproject.toml` 里带 fuyubot 身份段 → 复制到 `plugins/scripts/<目录名>/`，
///    顺手合成一份 `plugin.json`（下一步迁移就不用再做了）。
/// 3. **原生插件**（旧形态，兼容期继续支持）：一个编好的动态库，连同伴脚本一起
///    平铺进插件目录。
///
/// zip 不在这里解：安装归档是 TUI 插件商店的事（AGENTS 约束 61）。
pub fn install(source: &Path, plugin_dir: &Path) -> Result<Vec<PathBuf>, String> {
    if source.is_dir() {
        if is_bundle_dir(source) {
            return install_bundle(source, plugin_dir);
        }
        if is_script_plugin_dir(source) {
            return install_script_dir(source, plugin_dir);
        }
    } else {
        // 文件形态：只认动态库（含 `foo.dll.disable`），其余明确指路。
        let enabled = fuyubot_loader::enabled_path(source);
        let is_library = enabled
            .extension()
            .is_some_and(|extension| extension == DLL_EXTENSION);
        if !is_library {
            let extension = source
                .extension()
                .map(|extension| extension.to_string_lossy().into_owned())
                .unwrap_or_default();
            if extension.eq_ignore_ascii_case("zip") {
                return Err(format!(
                    "{} 是 zip：请用 TUI 插件商店安装，或手动解压到 plugins/<id>/（目录里要有 plugin.json）",
                    source.display()
                ));
            }
            return Err(format!(
                "{} 不是插件动态库（.{}）：目录请给带 plugin.json / package.json 的插件工程，\
                 文件请给 .{DLL_EXTENSION}",
                source.display(),
                if extension.is_empty() {
                    "无扩展名"
                } else {
                    &extension
                }
            ));
        }
    }
    let artifact = find_artifact(source)?;
    install_from(&artifact, &companion_dirs(source, &artifact), plugin_dir)
}

/// 装一个**插件包目录**：按 `kind` 路由，整目录复制。
///
/// 只校验 `entry` 存在，不校验平台：用户可能在为另一台机器备货，提前拒绝反而添乱
/// （真正加载时 `probe_bundle` 会按平台拒绝，那条错误更准确）。
fn install_bundle(source: &Path, plugin_dir: &Path) -> Result<Vec<PathBuf>, String> {
    let manifest = BundleManifest::load(source)
        .map_err(|error| format!("{} 不是可用的插件包：{error}", source.display()))?;
    manifest
        .entry_path(source)
        .map_err(|error| format!("{} 的入口不可用：{error}", source.display()))?;
    let container = if manifest.kind == PluginKind::Native {
        plugin_dir.to_path_buf()
    } else {
        plugin_dir.join(SCRIPTS_DIR)
    };
    let target = container.join(&manifest.id);
    if target.exists() {
        return Err(format!(
            "{} 已经存在了；要覆盖先 `fuyubot plugin uninstall {}`",
            target.display(),
            manifest.id
        ));
    }
    copy_dir_recursive(source, &target)?;
    Ok(vec![target])
}

/// 复制时**跳过的目录名**：依赖与构建产物。
///
/// 到了目标机器该由中间插件跑 `bun install` / `uv sync` 现装现建：拷贝一份
/// `node_modules` / `.venv` 既慢又容易和新 lock 文件打架，`target` / `.git`
/// 更是纯粹的死重。
const SKIP_ON_COPY: &[&str] = &[
    "node_modules",
    ".venv",
    "target",
    ".git",
    "__pycache__",
    ".pytest_cache",
    "dist",
];

/// `source` 是不是一个**脚本插件目录**：带着 manifest，且 manifest 里有 fuyubot 身份段。
///
/// 判据刻意与 `fuyubot.script` 的 `discover` 一致（格式是公开契约），但这里不引它——
/// CLI 不该依赖插件作者那一侧的 crate。两边各自轻量解析，格式漂移由端到端测试兜。
pub fn is_script_plugin_dir(source: &Path) -> bool {
    if !source.is_dir() {
        return false;
    }
    // JS：package.json 的 "fuyubot": { "name": "..." }
    if let Ok(text) = std::fs::read_to_string(source.join("package.json")) {
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
            let named = json
                .get("fuyubot")
                .and_then(|f| f.get("name"))
                .and_then(serde_json::Value::as_str)
                .is_some();
            if named {
                return true;
            }
        }
    }
    // Python：pyproject.toml 的 [tool.fuyubot] name = "..."
    if let Ok(text) = std::fs::read_to_string(source.join("pyproject.toml")) {
        if let Ok(value) = toml::from_str::<toml::Value>(&text) {
            let named = value
                .get("tool")
                .and_then(|t| t.get("fuyubot"))
                .and_then(|f| f.get("name"))
                .and_then(toml::Value::as_str)
                .is_some();
            if named {
                return true;
            }
        }
    }
    false
}

/// 把一个脚本插件**目录**装进 `<plugin_dir>/scripts/<目录名>/`。
///
/// 目标已存在就报错、不覆盖：覆盖会把作者在机器人那边改过的东西冲掉，
/// 而「先卸再装」是两个明确的动作。
pub fn install_script_dir(source: &Path, plugin_dir: &Path) -> Result<Vec<PathBuf>, String> {
    let name = source
        .file_name()
        .ok_or_else(|| format!("{} 没有目录名", source.display()))?;
    let scripts_dir = plugin_dir.join(SCRIPTS_DIR);
    let target = scripts_dir.join(name);
    if target.exists() {
        return Err(format!(
            "{} 已经存在了；要覆盖先 `fuyubot plugin uninstall {}`",
            target.display(),
            name.to_string_lossy()
        ));
    }
    copy_dir_recursive(source, &target)?;
    // 顺手写一份静态清单：宿主不就 dlopen 也能列举它（契约 §2 / §6）。
    // **失败只告警**——脚本本身已经装好了，少一份 sidecar 不影响它跑。
    if let Some(meta) = script_metadata(source) {
        match validated_manifest(script_manifest(&meta)) {
            Ok(manifest) => {
                if let Err(error) = manifest.write(&target) {
                    cli!(
                        warn,
                        "已安装脚本插件，但生成 plugin.json 失败（不影响运行）: {error}"
                    );
                }
            }
            Err(error) => cli!(
                warn,
                "已安装脚本插件，但它的 manifest 生成不了 plugin.json（不影响运行）: {error}"
            ),
        }
    }
    Ok(vec![target])
}

fn copy_dir_recursive(from: &Path, into: &Path) -> Result<(), String> {
    std::fs::create_dir_all(into).map_err(|e| format!("建不了 {}: {e}", into.display()))?;
    let entries = std::fs::read_dir(from).map_err(|e| format!("读不了 {}: {e}", from.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("读不了 {}: {e}", from.display()))?;
        let file_name = entry.file_name();
        if SKIP_ON_COPY
            .iter()
            .any(|skip| *skip == file_name.to_string_lossy())
        {
            continue;
        }
        let path = entry.path();
        let target = into.join(&file_name);
        if path.is_dir() {
            copy_dir_recursive(&path, &target)?;
        } else if path.is_file() {
            std::fs::copy(&path, &target)
                .map_err(|e| format!("拷贝 {} → {} 失败: {e}", path.display(), target.display()))?;
        }
    }
    Ok(())
}

// ── plugin uninstall ───────────────────────────────────────────────

/// 卸载。按**新形态优先**的顺序找，找到就整个删：
///
/// 1. `plugins/<id>/`（原生插件包）；
/// 2. `plugins/scripts/<id>/`（脚本插件包 / 老式脚本目录）；
/// 3. `plugins/.disabled/<id>/`（被禁用的包）；
/// 4. `plugins/<id>.disable/`（旧面板「目录改名禁用」的残留）。
///
/// 都没有才退回**旧形态动态库**（带不带 `lib` 前缀、带不带 `.disable` 后缀都试），
/// 连同伴文件（`.py` / `.js` / `.ts` / `.pyi`）一起删：单留一个 dll 在目录里，
/// 下次 `run` 还会试着加载它，然后因为找不到脚本而失败。
///
/// ⚠️ **禁用过的也要能卸**（`foo.dll.disable`）：不认这个后缀的话，面板里关掉一个插件
/// 之后就再也删不掉它了——只能先启用、再卸载。
pub fn uninstall(name: &str, plugin_dir: &Path) -> Result<Vec<PathBuf>, String> {
    let directories = [
        plugin_dir.join(name),
        plugin_dir.join(SCRIPTS_DIR).join(name),
        plugin_dir.join(DISABLED_DIR).join(name),
        plugin_dir.join(format!("{name}{}", fuyubot_loader::DISABLED_SUFFIX)),
    ];
    for directory in directories {
        // `plugin.json` 坏掉的包也得能卸：只要求目录在，不要求清单能解析。
        let looks_like_plugin =
            directory.is_dir() && (is_bundle_dir(&directory) || is_script_plugin_dir(&directory));
        if looks_like_plugin {
            std::fs::remove_dir_all(&directory)
                .map_err(|e| format!("删不了 {}: {e}", directory.display()))?;
            return Ok(vec![directory]);
        }
    }

    let script_dir = plugin_dir.join(SCRIPTS_DIR).join(name);
    if script_dir.is_dir() {
        std::fs::remove_dir_all(&script_dir)
            .map_err(|e| format!("删不了 {}: {e}", script_dir.display()))?;
        return Ok(vec![script_dir]);
    }

    for stem in [name.to_owned(), format!("lib{name}")] {
        let dll = plugin_dir.join(format!("{stem}.{DLL_EXTENSION}"));
        // 启用着的是 `foo.dll`，禁用过的是 `foo.dll.disable`：两个位置都算「装过」。
        let dll = if dll.is_file() {
            Some(dll)
        } else {
            let disabled = fuyubot_loader::disabled_path(&dll);
            disabled.is_file().then_some(disabled)
        };
        let Some(dll) = dll else {
            continue;
        };
        let mut removed = vec![dll.clone()];
        std::fs::remove_file(&dll).map_err(|e| format!("删不了 {}: {e}", dll.display()))?;
        // 同伴文件：同目录、同主干、脚本类扩展名。
        let entries = std::fs::read_dir(plugin_dir)
            .map_err(|e| format!("读不了 {}: {e}", plugin_dir.display()))?;
        for entry in entries.flatten() {
            let path = entry.path();
            let same_stem = path
                .file_stem()
                .is_some_and(|s| s.to_string_lossy() == stem);
            let is_script = path
                .extension()
                .is_some_and(|e| matches!(e.to_str(), Some("py" | "js" | "ts" | "pyi")));
            if same_stem && is_script && path.is_file() {
                std::fs::remove_file(&path)
                    .map_err(|e| format!("删不了 {}: {e}", path.display()))?;
                removed.push(path);
            }
        }
        return Ok(removed);
    }

    Err(format!(
        "插件目录里既没有 {name}/、scripts/{name}/、.{DISABLED_DIR}/{name}/，\
         也没有 {name}.{DLL_EXTENSION}：先 `fuyubot plugin list` 看看现在有什么"
    ))
}

// ── plugin migrate ─────────────────────────────────────────────────

/// Go 的 c-shared 一旦映射就卸不掉：迁移必须把这件事说清楚（AGENTS 约束 60）。
const GO_MIGRATION_NOTE: &str =
    "提示：Go 的 c-shared 已永久映射到本次迁移进程，进程退出即释放；下次 run 会正常加载。";

/// 把旧形态插件迁成插件包（`plugins/<id>/plugin.json`）。
///
/// 两种来源：
/// - **平铺动态库**：只在这里允许 `dlopen` 一次（用户显式触发、进程即将退出），
///   拿 name / version / 依赖 / ABI → 建 `plugins/<id>/bin/` → 移动 dll 与同主干
///   同伴脚本 → 写 `plugin.json`。`id` 非法或目标已存在时**跳过并报告**，不合并。
///   内置插件（`fuyubot_plugin_start` 等）一律跳过：它们由 `run` 自己装，
///   迁成包会让下一次 `run` 再拷一份。
/// - **`scripts/<dir>` 没有 `plugin.json`**：用 [`script_metadata`] 合成一份写进去，
///   **不移动目录**。
///
/// `dry_run` 只打印计划、不落盘。已有 `plugin.json` 的一律跳过（幂等）。
/// 返回人话报告，每行一条。
pub fn migrate(plugin_dir: &Path, dry_run: bool) -> Result<Vec<String>, String> {
    if !plugin_dir.is_dir() {
        return Err(format!(
            "插件目录 {} 不存在：先 `fuyubot init`，或 `fuyubot plugin install` 装一个",
            plugin_dir.display()
        ));
    }
    let mut report = Vec::new();
    let entries = plugin_entries(plugin_dir)
        .map_err(|error| format!("扫不了 {}: {error}", plugin_dir.display()))?;

    // 先把平铺库挑出来（内置插件在这一步就跳过，绝不 dlopen 它们）。
    let mut flat: Vec<PathBuf> = Vec::new();
    for entry in &entries {
        let PluginEntry::Legacy(path) = entry else {
            continue;
        };
        let stem = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default();
        let bare = stem.strip_prefix("lib").unwrap_or(&stem);
        if BUILTIN_PLUGINS.contains(&bare) {
            report.push(format!(
                "跳过内置插件 {}（run 会自己装；迁成包会导致重复加载）",
                path.display()
            ));
            continue;
        }
        flat.push(path.clone());
    }
    if !flat.is_empty() {
        // **只有确实有平铺库才起内核**：纯脚本迁移不该为此建一个宿主运行时。
        migrate_flat_libraries(plugin_dir, &flat, dry_run, &mut report)?;
    }
    migrate_script_dirs(plugin_dir, dry_run, &mut report);

    if report.is_empty() {
        report.push("没有需要迁移的东西：所有插件都已经是包形态".to_owned());
    }
    Ok(report)
}

/// 平铺动态库的迁移：探测、建包、移动、写清单。
fn migrate_flat_libraries(
    plugin_dir: &Path,
    libraries: &[PathBuf],
    dry_run: bool,
    report: &mut Vec<String>,
) -> Result<(), String> {
    // 迁移 native 需要一个 `PluginLoader`（HostRuntime）；作用域就到这里，
    // 文件操作在拿到探测结果之后做，不需要一直握着运行时。
    let runtime =
        tokio::runtime::Runtime::new().map_err(|error| format!("起不了 tokio 运行时: {error}"))?;
    let probes = runtime.block_on(async {
        let mut kernel = Kernel::new();
        kernel
            .start()
            .await
            .map_err(|error| format!("建不了宿主运行时: {error}"))?;
        let loader = PluginLoader::new(kernel.context().clone())
            .map_err(|error| format!("建不了插件加载器: {error}"))?;
        let probes: Vec<(PathBuf, Result<fuyubot_loader::Probe, String>)> = libraries
            .iter()
            .map(|path| {
                // 迁移入口允许探测 Go：代价是它被永久映射（调用方已经打印提示）。
                (
                    path.clone(),
                    loader
                        .probe_for_migration(path)
                        .map_err(|error| error.to_string()),
                )
            })
            .collect();
        Ok::<_, String>(probes)
    })?;
    drop(runtime);

    for (path, probe) in probes {
        let probe = match probe {
            Ok(probe) => probe,
            Err(error) => {
                report.push(format!("跳过 {}: {error}", path.display()));
                continue;
            }
        };
        let Some(file_name) = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
        else {
            report.push(format!("跳过 {}：没有文件名", path.display()));
            continue;
        };
        let manifest = match native_manifest(&probe, &file_name) {
            Ok(manifest) => manifest,
            Err(error) => {
                report.push(format!("跳过 {}: {error}", path.display()));
                continue;
            }
        };
        let target = plugin_dir.join(&manifest.id);
        if target.exists() {
            report.push(format!(
                "跳过 {}：目标 {} 已经存在（不合并，先 `fuyubot plugin uninstall {}`）",
                path.display(),
                target.display(),
                manifest.id
            ));
            continue;
        }
        let bin = target.join("bin");
        let destination = bin.join(&file_name);
        let depends = if manifest.depends.is_empty() {
            "无".to_owned()
        } else {
            manifest.depends.join(", ")
        };

        if dry_run {
            report.push(format!(
                "[dry-run] 会把 {} 迁到 {}/（kind=native，ABI {}，依赖：{depends}）",
                path.display(),
                target.display(),
                probe.abi_version
            ));
            if probe.go_runtime {
                report.push(format!("[dry-run] {}", GO_MIGRATION_NOTE));
            }
            continue;
        }

        if let Err(error) = std::fs::create_dir_all(&bin) {
            report.push(format!(
                "跳过 {}：建不了 {}（{error}）",
                path.display(),
                bin.display()
            ));
            continue;
        }
        if let Err(error) = std::fs::rename(&path, &destination) {
            report.push(format!(
                "跳过 {}：移不进 {}（{error}；文件是否正被运行中的实例占用？）",
                path.display(),
                destination.display()
            ));
            // 空目录不留给用户一个「半成品包」。
            let _ = std::fs::remove_dir(&bin);
            let _ = std::fs::remove_dir(&target);
            continue;
        }

        let mut moved_companions = Vec::new();
        for companion in companion_scripts(plugin_dir, &path) {
            let Some(name) = companion.file_name() else {
                continue;
            };
            let into = bin.join(name);
            match std::fs::rename(&companion, &into) {
                Ok(()) => moved_companions.push((companion, into)),
                Err(error) => report.push(format!(
                    "警告：{} 没搬进 {}（{error}）——它会留在插件目录根上，加载时可能找不到",
                    companion.display(),
                    bin.display()
                )),
            }
        }

        match manifest.write(&target) {
            Ok(manifest_path) => {
                report.push(format!(
                    "已迁移 {} → {}（kind=native，ABI {}，依赖：{depends}）",
                    path.display(),
                    manifest_path.display(),
                    probe.abi_version
                ));
            }
            Err(error) => {
                // 清单写不下去就把 dll 原样搬回去，别留下一个加载不了、也认不出的死目录。
                let rollback = std::fs::rename(&destination, &path);
                for (from, into) in moved_companions {
                    let _ = std::fs::rename(&into, &from);
                }
                let _ = std::fs::remove_dir(&bin);
                let _ = std::fs::remove_dir(&target);
                report.push(match rollback {
                    Ok(()) => format!("迁移 {} 失败：{error}（dll 已移回原处）", path.display()),
                    Err(rollback) => format!(
                        "迁移 {} 失败：{error}；把 dll 移回原处也失败了（{rollback}），\
                         它现在在 {}",
                        path.display(),
                        destination.display()
                    ),
                });
                continue;
            }
        }
        if probe.go_runtime {
            report.push(GO_MIGRATION_NOTE.to_owned());
        }
    }
    Ok(())
}

/// 从探测结果合成 native 的 `plugin.json`。
fn native_manifest(
    probe: &fuyubot_loader::Probe,
    file_name: &str,
) -> Result<BundleManifest, String> {
    let manifest = probe.manifest.as_ref();
    let manifest = BundleManifest {
        schema: 1,
        id: probe.name.clone(),
        // 展示名优先用插件自报的 manifest.name，没有就用 id。
        name: manifest
            .map(|manifest| manifest.name.trim().to_owned())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| probe.name.clone()),
        version: safe_version(
            manifest
                .map(|manifest| manifest.version.as_str())
                .unwrap_or(""),
        ),
        kind: PluginKind::Native,
        // 包内入口：dll 就放在 `bin/` 下，文件名不变。
        entry: format!("bin/{file_name}"),
        abi: Some(probe.abi_version),
        // 迁出来的包只对**当前平台**负责：dll 是平台相关的产物。
        platforms: vec![current_platform().to_owned()],
        depends: manifest
            .map(|manifest| manifest.inject.clone())
            .unwrap_or_default(),
        optional_depends: manifest
            .map(|manifest| manifest.soft_inject.clone())
            .unwrap_or_default(),
        provides: Vec::new(),
        config: None,
        description: String::new(),
        license: String::new(),
        homepage: String::new(),
    };
    validated_manifest(manifest)
}

/// 平铺插件旁边同主干的脚本（`.py` / `.js` / `.ts` / `.pyi`）。
///
/// 宿主按「cdylib 同目录」找脚本（`find_script`），迁进 `bin/` 时要一起带走，
/// 否则包是完整的、脚本却留在插件目录根上，加载时找不到。
fn companion_scripts(plugin_dir: &Path, library: &Path) -> Vec<PathBuf> {
    let Some(stem) = library.file_stem().map(|stem| stem.to_string_lossy()) else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(plugin_dir) else {
        return Vec::new();
    };
    let mut companions: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_file() && path != library)
        .filter(|path| {
            path.file_stem()
                .is_some_and(|candidate| candidate.to_string_lossy() == stem)
                && path.extension().is_some_and(|extension| {
                    matches!(extension.to_str(), Some("py" | "js" | "ts" | "pyi"))
                })
        })
        .collect();
    companions.sort();
    companions
}

/// `scripts/<dir>` 的迁移：生成 `plugin.json`，目录本身不动。
fn migrate_script_dirs(plugin_dir: &Path, dry_run: bool, report: &mut Vec<String>) {
    let scripts_dir = plugin_dir.join(SCRIPTS_DIR);
    let Ok(entries) = std::fs::read_dir(&scripts_dir) else {
        return;
    };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_dir()
                && path
                    .file_name()
                    .is_some_and(|name| !name.to_string_lossy().starts_with('.'))
        })
        .collect();
    dirs.sort();

    for dir in dirs {
        if is_bundle_dir(&dir) {
            report.push(format!("跳过 {}：已经有 plugin.json 了", dir.display()));
            continue;
        }
        let Some(meta) = script_metadata(&dir) else {
            report.push(format!(
                "跳过 {}：没有 package.json 的 fuyubot 段，也没有 pyproject.toml 的 [tool.fuyubot]",
                dir.display()
            ));
            continue;
        };
        let manifest = match validated_manifest(script_manifest(&meta)) {
            Ok(manifest) => manifest,
            Err(error) => {
                report.push(format!(
                    "跳过 {}：合成不出合法的 plugin.json（{error}）",
                    dir.display()
                ));
                continue;
            }
        };
        // 入口得真的在：清单写一个不存在的文件，宿主加载时才报错就太晚了。
        if let Err(error) = manifest.entry_path(&dir) {
            report.push(format!("跳过 {}：入口不可用（{error}）", dir.display()));
            continue;
        }
        if dry_run {
            report.push(format!(
                "[dry-run] 会给 {} 写 plugin.json（id={}，kind={}，entry={}）",
                dir.display(),
                manifest.id,
                manifest.kind,
                manifest.entry
            ));
            continue;
        }
        match manifest.write(&dir) {
            Ok(path) => report.push(format!(
                "已为 {} 生成 {}（id={}，kind={}）",
                dir.display(),
                path.display(),
                manifest.id,
                manifest.kind
            )),
            Err(error) => report.push(format!("写不了 {} 的 plugin.json：{error}", dir.display())),
        }
    }
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

    // 禁用着的旧版本不算「已装」：装新版本 = 想让它跑起来，先把禁用的那个清掉。
    // 删掉的那个**不进返回值**：返回值是「这次装了什么」，把删掉的文件混进去
    // 会让 `plugin install` 打出一行「已安装 …dll.disable」——听着像装了个禁用件。
    if let Some(staged) = copied.first() {
        let stale = fuyubot_loader::disabled_path(staged);
        if stale.exists() {
            std::fs::remove_file(&stale)
                .map_err(|e| format!("删不了旧的禁用文件 {}: {e}", stale.display()))?;
        }
    }

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
///
/// 查找顺序（缺口 19）：
/// 1. `source/target/{release,debug}` —— 普通工程的产物（同一插件两边都有时取**较新**的，
///    见 [`newest_by_stem`]：`target/release` 里躺着半年前的旧 ABI 产物是常事）；
/// 2. `source` 本身 —— C/C++ 手编、或产物已经拷到工程目录；
/// 3. 沿父链找**最近的** `target/{release,debug}` —— workspace 成员的产物在**仓库根**，
///    工程自己那份 `target/` 根本不存在。
///
/// 第 3 步扫到的是整个 workspace 的公共产物目录（一屏都是别人的 dll），所以先按
/// 工程目录名收窄（`weather-bot` / `weather_bot` 两种写法都认）；收窄不到才算没找到，
/// 不会随手抓一个别人的插件装上去。
pub fn find_artifact(source: &Path) -> Result<PathBuf, String> {
    if source.is_file() {
        return Ok(source.to_path_buf());
    }
    // 相对路径要先落成绝对：父链是「按文件系统」走的，`crates/foo` 的父链在
    // 不同工作目录下会指向不同的 `target/`。已经是绝对的路径原样保留（不做 `\\?\` 前缀）。
    let source = if source.is_absolute() {
        source.to_path_buf()
    } else {
        std::path::absolute(source).unwrap_or_else(|_| source.to_path_buf())
    };
    let expected = source.file_name().map(|name| {
        // cargo 的目录名用 `-`，产物名用 `_`（`plugin-demo` → `plugin_demo`）。
        name.to_string_lossy().replace('-', "_")
    });

    let own = newest_by_stem(target_dlls(&source));
    if !own.is_empty() {
        return pick_artifact(own, &source);
    }

    let direct = dlls_in(&source);
    if !direct.is_empty() {
        return pick_artifact(direct, &source);
    }

    for ancestor in source.ancestors().skip(1) {
        let candidates = newest_by_stem(target_dlls(ancestor));
        if candidates.is_empty() {
            continue;
        }
        let narrowed: Vec<PathBuf> = match &expected {
            Some(name) => candidates
                .into_iter()
                .filter(|path| artifact_stem_matches(path, name))
                .collect(),
            None => candidates,
        };
        if !narrowed.is_empty() {
            return pick_artifact(narrowed, &source);
        }
        // 这个 target/ 里没有本工程的产物：可能是更大的 workspace，继续往上。
    }

    Err(format!(
        "{} 里没找到 .{DLL_EXTENSION}（工程自己的 target/、工程目录、以及沿父链的 target/ 都查过了）。\
         本 CLI **不编译插件**（打包归作者自己的语言工具链）：先用 `cargo build`（Rust / \
         Python / JS 插件的 cdylib 都是 Rust 编的）或 `cl /LD`（C/C++）编出来，\
         再把产物或工程目录交给本命令。\
         workspace 成员的产物在仓库根的 target/ 下——按工程目录名匹配；\
         目录名和 crate 名对不上时，直接把那个 .{DLL_EXTENSION} 的路径交给本命令。",
        source.display()
    ))
}

/// 某个目录里所有看起来是插件动态库的文件（不递归）。
fn dlls_in(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file() && path.extension().is_some_and(|e| e == DLL_EXTENSION))
        .collect()
}

/// 一个工程根（或 workspace 根）下 `target/release` 与 `target/debug` 的产物合集。
fn target_dlls(base: &Path) -> Vec<PathBuf> {
    let mut found = dlls_in(&base.join("target").join("release"));
    found.extend(dlls_in(&base.join("target").join("debug")));
    found
}

/// 同名（主干相同，`lib` 前缀不计）的产物只留**最新**的那份。
///
/// release / debug 是两次独立的构建，谁新谁代表「刚编的那一版」。以前固定先看 release，
/// 结果把半年前的旧 ABI 产物装进了插件目录（真踩过：`target/release` 里的 demo 插件
/// 还是 ABI v1，`plugin_init` 直接拒绝加载）。不同主干仍然算多个候选，交给调用方指定。
fn newest_by_stem(candidates: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut by_stem: BTreeMap<String, PathBuf> = BTreeMap::new();
    for path in candidates {
        let Some(stem) = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
        else {
            continue;
        };
        let stem = stem.strip_prefix("lib").unwrap_or(&stem).to_owned();
        match by_stem.get(&stem) {
            Some(existing) if modified(existing) >= modified(&path) => {}
            _ => {
                by_stem.insert(stem, path);
            }
        }
    }
    by_stem.into_values().collect()
}

/// 文件的修改时间；读不到就当最旧（永远不会赢过读得到的那个）。
fn modified(path: &Path) -> std::time::SystemTime {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .unwrap_or(std::time::UNIX_EPOCH)
}

/// 产物主干是否就是工程（`lib` 前缀与平台大小写都容忍）。
fn artifact_stem_matches(path: &Path, expected: &str) -> bool {
    let Some(stem) = path.file_stem().map(|stem| stem.to_string_lossy()) else {
        return false;
    };
    let bare = stem.strip_prefix("lib").unwrap_or(&stem);
    bare.eq_ignore_ascii_case(expected)
}

/// 零 / 一 / 多个候选的收尾：多个就让调用方自己指定，不猜。
fn pick_artifact(mut candidates: Vec<PathBuf>, source: &Path) -> Result<PathBuf, String> {
    match candidates.len() {
        0 => Err(format!("{} 里没找到 .{DLL_EXTENSION}", source.display())),
        1 => Ok(candidates.remove(0)),
        _ => {
            candidates.sort();
            Err(format!(
                "{} 里有多个候选，指定其中一个：{}",
                source.display(),
                candidates
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        }
    }
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
    // 装进去的总是**启用**的名字：从 `foo.dll.disable` 装回来会得到一个禁用的插件，
    // 「装」这个动作本身就该让它能用。
    let enabled = fuyubot_loader::enabled_path(from);
    let name = enabled
        .file_name()
        .ok_or_else(|| format!("{} 没有文件名", from.display()))?;
    let target = into.join(name);
    std::fs::copy(from, &target)
        .map_err(|e| format!("拷贝 {} → {} 失败: {e}", from.display(), target.display()))?;
    Ok(target)
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

    #[test]
    fn init_can_write_api_hash_in_plain_text() {
        let root = temp("init-hash");
        let path = init_with_api_hash(&root, "mybot", 12345, Some("deadbeef")).unwrap();
        assert_eq!(load_api_hash(&path).as_deref(), Some("deadbeef"));
        assert_eq!(load_config(&path).unwrap().telegram.api_id, Some(12345));
    }

    /// 装插件时**脚本要跟着走**（宿主按 cdylib 同目录找脚本），无关文件不搬。
    /// 「去哪找脚本」的规则本身在 `fuyubot-loader::find_script`（缺口 28），这里只测拷贝。
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

    /// workspace 成员的产物在**仓库根**的 `target/` 下，工程自己那份不存在（缺口 19）。
    #[test]
    fn workspace_member_artifact_is_found_in_the_repo_target() {
        let repo = temp("find-artifact-workspace");
        let member = repo.join("crates").join("weather-bot");
        std::fs::create_dir_all(&member).unwrap();
        let out = repo.join("target").join("debug");
        std::fs::create_dir_all(&out).unwrap();
        let wanted = out.join(format!("weather_bot.{DLL_EXTENSION}"));
        std::fs::write(&wanted, b"lib").unwrap();
        // 公共 target/ 里一定有别人的产物：不能随手抓一个装上去。
        std::fs::write(out.join(format!("other_plugin.{DLL_EXTENSION}")), b"lib").unwrap();

        assert_eq!(find_artifact(&member).unwrap(), wanted);
    }

    /// release / debug 各有一份时取**较新**的，而不是盲选 release。
    ///
    /// 真踩过：仓库根 `target/release` 里躺着一个 ABI v1 的 demo 插件（半年前编的），
    /// 而刚 `cargo build` 出来的是 debug——按「先 release」装进去的那个，宿主直接拒绝加载。
    #[test]
    fn the_newer_of_release_and_debug_wins() {
        let project = temp("find-artifact-newest");
        let release = project.join("target").join("release");
        let debug = project.join("target").join("debug");
        std::fs::create_dir_all(&release).unwrap();
        std::fs::create_dir_all(&debug).unwrap();
        std::fs::write(release.join(format!("weather.{DLL_EXTENSION}")), b"old").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let fresh = debug.join(format!("weather.{DLL_EXTENSION}"));
        std::fs::write(&fresh, b"new").unwrap();

        assert_eq!(find_artifact(&project).unwrap(), fresh);
    }

    /// 仓库根的 target/ 里没有本工程的产物时如实报「没找到」，不猜别人的。
    #[test]
    fn a_workspace_target_without_this_crate_is_reported() {
        let repo = temp("find-artifact-nomatch");
        let member = repo.join("crates").join("ghost-plugin");
        std::fs::create_dir_all(&member).unwrap();
        let out = repo.join("target").join("debug");
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join(format!("other_plugin.{DLL_EXTENSION}")), b"lib").unwrap();

        let error = find_artifact(&member).unwrap_err();
        assert!(error.contains("没找到"), "{error}");
        assert!(
            !error.contains("other_plugin"),
            "不该把别人列成候选：{error}"
        );
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

    // ── v2：脚本插件目录形态 ──────────────────────────────────────

    /// 造一个脚本插件目录（JS 形态），返回目录路径。
    fn script_plugin_dir(name: &str) -> PathBuf {
        let dir = temp(name);
        std::fs::write(
            dir.join("package.json"),
            r#"{"name":"weather-bot","type":"module","fuyubot":{"name":"fuyubot.example.weather","entry":"index.ts"}}"#,
        )
        .unwrap();
        std::fs::write(dir.join("index.ts"), b"export {}").unwrap();
        std::fs::write(dir.join("bun.lock"), b"{}").unwrap();
        dir
    }

    /// 「是不是脚本插件目录」按 manifest 里的 fuyubot 身份段判，
    /// 与两个中间插件的 discover 是同一把尺子。
    #[test]
    fn script_plugin_detection_follows_the_manifest() {
        let js = script_plugin_dir("detect-js");
        assert!(is_script_plugin_dir(&js));

        let py = temp("detect-py");
        std::fs::write(
            py.join("pyproject.toml"),
            "[project]\nname = \"x\"\n\n[tool.fuyubot]\nname = \"fuyubot.example.x\"\nentry = \"main.py\"\n",
        )
        .unwrap();
        assert!(is_script_plugin_dir(&py));

        // 有 manifest 没身份段：普通 Node / Python 工程，不是插件。
        let plain = temp("detect-plain");
        std::fs::write(plain.join("package.json"), r#"{"name":"x"}"#).unwrap();
        assert!(!is_script_plugin_dir(&plain));

        // 空目录 / 文件：都不是。
        assert!(!is_script_plugin_dir(&temp("detect-empty")));
    }

    /// 目录形态 install：整目录进 `plugins/scripts/<名字>/`，
    /// 依赖与构建产物**不拷**（那边由中间插件现装现建）。
    #[test]
    fn install_a_script_plugin_dir_copies_everything_but_deps() {
        let source = script_plugin_dir("scriptdir-src");
        // 这些不该出现在安装结果里。
        std::fs::create_dir_all(source.join("node_modules").join("is-odd")).unwrap();
        std::fs::write(source.join("node_modules/is-odd/index.js"), b"1").unwrap();
        std::fs::create_dir_all(source.join(".venv")).unwrap();
        std::fs::write(source.join(".venv/pyvenv.cfg"), b"").unwrap();
        std::fs::create_dir_all(source.join("target")).unwrap();
        std::fs::write(source.join("target/out.dll"), b"").unwrap();
        std::fs::write(source.join("README.md"), b"hi").unwrap();
        std::fs::create_dir_all(source.join("src")).unwrap();
        std::fs::write(source.join("src/util.ts"), b"export {}").unwrap();

        let plugin_dir = temp("scriptdir-dst");
        let copied = install(&source, &plugin_dir).unwrap();
        assert_eq!(copied.len(), 1, "{copied:?}");
        let target = &copied[0];
        assert_eq!(target, &plugin_dir.join("scripts").join("scriptdir-src"));

        // 源码与 manifest 都过去了；依赖与构建产物没过去。
        assert!(target.join("index.ts").is_file());
        assert!(target.join("package.json").is_file());
        assert!(target.join("bun.lock").is_file());
        assert!(target.join("README.md").is_file());
        assert!(target.join("src/util.ts").is_file(), "子目录要递归拷");
        assert!(!target.join("node_modules").exists());
        assert!(!target.join(".venv").exists());
        assert!(!target.join("target").exists());
    }

    /// 目标已存在就报错、不覆盖——覆盖会冲掉用户在机器人那边改过的东西。
    #[test]
    fn installing_over_an_existing_script_dir_is_refused() {
        let source = script_plugin_dir("scriptdir-dup-src");
        let plugin_dir = temp("scriptdir-dup-dst");
        install(&source, &plugin_dir).unwrap();
        let error = install(&source, &plugin_dir).unwrap_err();
        assert!(error.contains("已经存在"), "{error}");
    }

    /// 卸载脚本插件 = 删整个目录。
    #[test]
    fn uninstall_removes_a_script_dir() {
        let source = script_plugin_dir("uninst-src");
        let plugin_dir = temp("uninst-dst");
        install(&source, &plugin_dir).unwrap();

        let removed = uninstall("uninst-src", &plugin_dir).unwrap();
        assert_eq!(removed.len(), 1);
        assert!(!plugin_dir.join("scripts/uninst-src").exists());
    }

    /// 卸载原生插件 = dll + 同主干的同伴脚本，`lib` 前缀形态也认。
    #[test]
    fn uninstall_removes_a_native_plugin_with_its_companions() {
        let plugin_dir = temp("uninst-native");
        let stem = "weather";
        std::fs::write(plugin_dir.join(format!("{stem}.{DLL_EXTENSION}")), b"lib").unwrap();
        std::fs::write(plugin_dir.join(format!("{stem}.py")), b"print()").unwrap();
        std::fs::write(plugin_dir.join("other.py"), b"keep").unwrap();

        let removed = uninstall(stem, &plugin_dir).unwrap();
        assert_eq!(removed.len(), 2, "{removed:?}");
        assert!(!plugin_dir.join(format!("{stem}.{DLL_EXTENSION}")).exists());
        assert!(!plugin_dir.join(format!("{stem}.py")).exists());
        assert!(
            plugin_dir.join("other.py").is_file(),
            "别人的同伴文件不能顺手删"
        );
    }

    /// 两样都没有时报错指路，不静默成功。
    #[test]
    fn uninstall_of_nothing_is_an_error() {
        let plugin_dir = temp("uninst-nothing");
        let error = uninstall("ghost", &plugin_dir).unwrap_err();
        assert!(error.contains("ghost"), "{error}");
        assert!(error.contains("plugin list"), "{error}");
    }

    /// 面板里禁用过的插件（`foo.dll.disable`）照样能卸：否则「关了之后再也删不掉」。
    #[test]
    fn uninstall_also_removes_a_disabled_library() {
        let plugin_dir = temp("uninst-disabled");
        let dll = plugin_dir.join(format!("weather.{DLL_EXTENSION}"));
        let off = fuyubot_loader::disabled_path(&dll);
        std::fs::write(&off, b"lib").unwrap();

        let removed = uninstall("weather", &plugin_dir).unwrap();
        assert_eq!(removed, vec![off.clone()], "{removed:?}");
        assert!(!off.exists());
    }

    /// 从禁用文件装回来要得到**启用**的名字：装完不等重启还是禁用的，会让人以为装坏了。
    #[test]
    fn installing_a_disabled_file_lands_on_the_enabled_name() {
        let plugin_dir = temp("install-disabled");
        let source = temp("install-disabled-src");
        let off = fuyubot_loader::disabled_path(&source.join(format!("weather.{DLL_EXTENSION}")));
        std::fs::write(&off, b"lib").unwrap();

        let copied = install_from(&off, &[], &plugin_dir).unwrap();
        assert_eq!(
            copied,
            vec![plugin_dir.join(format!("weather.{DLL_EXTENSION}"))]
        );
        assert!(!fuyubot_loader::disabled_path(&copied[0]).exists());
    }

    // ── v1：插件包（plugin.json）───────────────────────────────────

    /// 造一个插件包目录：`entry` 指向的文件真的存在。
    fn bundle_dir(name: &str, id: &str, kind: &str, entry: &str) -> PathBuf {
        let dir = temp(name);
        if let Some((parent, _)) = entry.rsplit_once('/') {
            std::fs::create_dir_all(dir.join(parent)).unwrap();
        }
        std::fs::write(dir.join(entry), b"x").unwrap();
        let manifest = serde_json::json!({
            "schema": 1,
            "id": id,
            "name": "Demo",
            "version": "1.0.0",
            "kind": kind,
            "entry": entry,
        });
        std::fs::write(dir.join("plugin.json"), manifest.to_string()).unwrap();
        dir
    }

    /// 目录 + plugin.json：native 进 `plugins/<id>/`，脚本进 `plugins/scripts/<id>/`。
    #[test]
    fn install_routes_a_bundle_by_kind() {
        let plugin_dir = temp("bundle-install-dst");

        let native = bundle_dir(
            "bundle-native-src",
            "com.example.native",
            "native",
            "bin/plugin.dll",
        );
        let copied = install(&native, &plugin_dir).unwrap();
        assert_eq!(copied, vec![plugin_dir.join("com.example.native")]);
        assert!(
            plugin_dir
                .join("com.example.native/bin/plugin.dll")
                .is_file()
        );
        assert!(plugin_dir.join("com.example.native/plugin.json").is_file());

        let script = bundle_dir(
            "bundle-script-src",
            "com.example.script",
            "python",
            "main.py",
        );
        let copied = install(&script, &plugin_dir).unwrap();
        assert_eq!(copied, vec![plugin_dir.join("scripts/com.example.script")]);
        assert!(
            plugin_dir
                .join("scripts/com.example.script/main.py")
                .is_file()
        );

        // 目标已存在：报错、不覆盖（覆盖会冲掉用户在机器人那边改过的东西）。
        let error = install(&native, &plugin_dir).unwrap_err();
        assert!(error.contains("已经存在"), "{error}");
    }

    /// 老式脚本目录装完要顺手带一份 `plugin.json`（宿主不 dlopen 也能列出来）。
    #[test]
    fn install_a_script_dir_writes_the_plugin_json_sidecar() {
        let source = script_plugin_dir("scriptdir-sidecar-src");
        let plugin_dir = temp("scriptdir-sidecar-dst");
        install(&source, &plugin_dir).unwrap();

        let target = plugin_dir.join("scripts/scriptdir-sidecar-src");
        let manifest = BundleManifest::load(&target).expect("装完该有 plugin.json");
        assert_eq!(manifest.id, "fuyubot.example.weather");
        assert_eq!(manifest.kind, PluginKind::Js);
        assert_eq!(manifest.entry, "index.ts");
        assert_eq!(manifest.version, "0.0.0", "manifest 没版本时用占位");
        manifest
            .validate(&target)
            .expect("生成的清单要能过宿主那关");
    }

    /// zip 不在 CLI 解：明确指向 TUI 商店（AGENTS 约束 61）。
    #[test]
    fn installing_a_zip_is_refused_with_a_store_hint() {
        let source = temp("install-zip-src");
        let zip = source.join("plugin.zip");
        std::fs::write(&zip, b"PK").unwrap();
        let plugin_dir = temp("install-zip-dst");

        let error = install(&zip, &plugin_dir).unwrap_err();
        assert!(error.contains("zip"), "{error}");
        assert!(error.contains("TUI"), "{error}");
    }

    /// 卸载按新形态优先：`plugins/<id>/`、`plugins/scripts/<id>/`、`.disabled/<id>/`。
    #[test]
    fn uninstall_removes_bundles_in_every_location() {
        let plugin_dir = temp("uninst-bundles");

        let native = bundle_dir(
            "uninst-bundle-native-src",
            "com.example.native",
            "native",
            "bin/plugin.dll",
        );
        std::fs::create_dir_all(&plugin_dir).unwrap();
        copy_dir_recursive(&native, &plugin_dir.join("com.example.native")).unwrap();

        let script = bundle_dir(
            "uninst-bundle-script-src",
            "com.example.script",
            "js",
            "index.ts",
        );
        copy_dir_recursive(&script, &plugin_dir.join("scripts/com.example.script")).unwrap();

        let off = bundle_dir(
            "uninst-bundle-off-src",
            "com.example.off",
            "native",
            "bin/plugin.dll",
        );
        copy_dir_recursive(&off, &plugin_dir.join(".disabled/com.example.off")).unwrap();

        for (name, path) in [
            ("com.example.native", plugin_dir.join("com.example.native")),
            (
                "com.example.script",
                plugin_dir.join("scripts/com.example.script"),
            ),
            (
                "com.example.off",
                plugin_dir.join(".disabled/com.example.off"),
            ),
        ] {
            let removed = uninstall(name, &plugin_dir).unwrap();
            assert_eq!(removed, vec![path.clone()], "{name}");
            assert!(!path.exists(), "{name} 该被整个删掉");
        }
    }

    // ── v1：plugin migrate ─────────────────────────────────────────

    /// `scripts/<dir>` 没有清单：生成 `plugin.json`，目录不动。
    #[test]
    fn migrate_generates_plugin_json_for_a_script_dir() {
        let plugin_dir = temp("migrate-scripts");
        let dir = plugin_dir.join("scripts/py-demo");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("main.py"), b"print()").unwrap();
        std::fs::write(
            dir.join("pyproject.toml"),
            "[project]\nname = \"demo\"\nversion = \"0.2.0+build.1\"\n\n\
             [tool.fuyubot]\nname = \"fuyubot.example.py\"\n",
        )
        .unwrap();

        let report = migrate(&plugin_dir, false).unwrap().join("\n");
        assert!(report.contains("已为"), "{report}");
        let manifest = BundleManifest::load(&dir).unwrap();
        assert_eq!(manifest.id, "fuyubot.example.py");
        assert_eq!(manifest.kind, PluginKind::Python);
        assert_eq!(manifest.version, "0.2.0+build.1");
        assert_eq!(manifest.entry, "main.py");
        assert_eq!(manifest.name, "demo");
        manifest.validate(&dir).expect("生成的清单要能过宿主那关");
        assert!(dir.is_dir(), "脚本目录不许被移动");

        // 幂等：第二次只报「已经有 plugin.json」。
        let report = migrate(&plugin_dir, false).unwrap().join("\n");
        assert!(report.contains("已经有 plugin.json"), "{report}");
    }

    /// `--dry-run` 只打印计划：不写 sidecar、不移动目录。
    #[test]
    fn migrate_dry_run_changes_nothing() {
        let plugin_dir = temp("migrate-dry-run");
        let dir = plugin_dir.join("scripts/js-demo");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("index.js"), b"export {}").unwrap();
        std::fs::write(
            dir.join("package.json"),
            r#"{"name":"demo","version":"1.0.0","fuyubot":{"name":"fuyubot.example.js"}}"#,
        )
        .unwrap();

        let report = migrate(&plugin_dir, true).unwrap().join("\n");
        assert!(report.contains("[dry-run]"), "{report}");
        assert!(!dir.join("plugin.json").exists(), "dry-run 不许落盘");
        assert!(dir.join("index.js").is_file(), "目录也不许动");
    }

    /// 内置插件不迁移：`run` 会自己装，迁成包会让它再拷一份。
    #[test]
    fn migrate_skips_builtin_libraries() {
        let plugin_dir = temp("migrate-builtin");
        let dll = plugin_dir.join(format!("fuyubot_plugin_start.{DLL_EXTENSION}"));
        std::fs::write(&dll, b"not a library").unwrap();

        let report = migrate(&plugin_dir, true).unwrap().join("\n");
        assert!(report.contains("内置插件"), "{report}");
        assert!(dll.is_file(), "内置插件不许动");
        assert!(!plugin_dir.join("fuyubot.start").exists());
    }

    /// 装新版本时顺手清掉旧的禁用文件：留着的话新版是启用的、旧版也躺在旁边。
    #[test]
    fn installing_over_a_disabled_copy_clears_the_stale_one() {
        let plugin_dir = temp("install-over-disabled");
        let source = temp("install-over-disabled-src");
        let artifact = source.join(format!("weather.{DLL_EXTENSION}"));
        std::fs::write(&artifact, b"new").unwrap();
        let stale =
            fuyubot_loader::disabled_path(&plugin_dir.join(format!("weather.{DLL_EXTENSION}")));
        std::fs::write(&stale, b"old").unwrap();

        install_from(&artifact, &[], &plugin_dir).unwrap();
        assert!(!stale.exists(), "旧的禁用文件该被清掉");
        assert!(
            plugin_dir
                .join(format!("weather.{DLL_EXTENSION}"))
                .is_file()
        );
    }
}
