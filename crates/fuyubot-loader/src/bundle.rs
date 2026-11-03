//! 插件分发格式 v1：`plugin.json` 静态元数据与目录发现。
//!
//! 一个插件 = 一个目录 + 一份 `plugin.json`。这一层**不 dlopen**：列举、校验、
//! 排序、平台过滤全在文件系统上做完，所以「装了什么、能不能装」在进程被插件代码
//! 污染之前就有答案（契约见[文档站](https://catmoecircle.github.io/FuyuBot-docs/docs/plugin/bundle/) §2 / §3）。
//!
//! 旧形态（平铺 `*.dll`）继续认，两种形态由 [`plugin_entries`] 一起给出：
//! bundle 在前（按 id 字典序），legacy 在后（按文件名）。
//!
//! 解析是**手写**的 `serde_json::Value` 读取，不给 `BundleManifest` 加 derive：
//! 未知字段必须被忽略（向前兼容），而且错误要能指到具体字段——derive 的
//! `deny_unknown_fields` / `missing field` 两头都做不到。

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use crate::loader::{DISABLED_SUFFIX, DLL_EXTENSION};

/// 包内清单的文件名。
pub const BUNDLE_MANIFEST: &str = "plugin.json";

/// 被禁用的包挪进去的目录名（`plugins/.disabled/<id>/`）。
pub const DISABLED_DIR: &str = ".disabled";

/// 脚本插件根（`plugins/scripts/`），P0 只列举、不加载。
pub const SCRIPTS_DIR: &str = "scripts";

/// 本宿主认识的清单版本。
const SCHEMA_VERSION: u32 = 1;

/// 插件 id 的长度上限（字节）。
const ID_MAX_LEN: usize = 128;

/// 插件种类。决定入口是什么、由谁来真正跑它。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginKind {
    /// 原生动态库：宿主自己 dlopen。
    Native,
    /// Python 脚本：由 SDK 的 `fuyubot_python_host` 中间插件跑。
    Python,
    /// JavaScript / TypeScript 脚本：由 `fuyubot_js_host` 跑。
    Js,
}

impl PluginKind {
    /// 清单里写的那个串（也用于 CLI 输出）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Python => "python",
            Self::Js => "js",
        }
    }

    /// 解析清单里的 `kind`。不认识就返回 `None`（由调用方报「字段不合法」）。
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "native" => Some(Self::Native),
            "python" => Some(Self::Python),
            "js" => Some(Self::Js),
            _ => None,
        }
    }

    /// 是不是脚本插件（python / js）。脚本的入口是脚本文件，不是动态库。
    pub fn is_script(self) -> bool {
        matches!(self, Self::Python | Self::Js)
    }
}

impl std::fmt::Display for PluginKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 一份 `plugin.json` 解析出来的静态元数据。
///
/// 字段与契约 §3 一一对应；可选项缺省时是空串 / 空表，`abi` / `config` 用 `Option`
/// 区分「没写」与「写了 0」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleManifest {
    pub schema: u32,
    pub id: String,
    pub name: String,
    pub version: String,
    pub kind: PluginKind,
    /// 入口（相对包根）。语法在 [`Self::parse`] 里就校验过，落盘检查在 [`Self::entry_path`]。
    pub entry: String,
    /// 静态预检用的建议值；真值以 `plugin_init` 返回的 handle 为准。
    pub abi: Option<u32>,
    /// 空 = 全平台；`any` 也等于全平台；否则必须含 [`current_platform`]。
    pub platforms: Vec<String>,
    /// 硬依赖（服务名，进内核 `inject`）。
    pub depends: Vec<String>,
    /// 软依赖（进 `soft_inject`）。
    pub optional_depends: Vec<String>,
    /// 本插件提供的服务名（静态预检 / 商店展示用）。
    pub provides: Vec<String>,
    /// 默认配置的相对路径。P0 只解析、不合并。
    pub config: Option<String>,
    pub description: String,
    pub license: String,
    pub homepage: String,
}

/// 清单不可用的原因。每一条都要能告诉用户下一步该改哪里。
#[derive(Debug, thiserror::Error)]
pub enum BundleError {
    #[error("读不了插件清单 {path}: {source}（确认包里有 plugin.json，且有读权限）")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path} 不是合法的 JSON: {source}（plugin.json 要 UTF-8、且只有一个顶层对象）")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("plugin.json 缺少必填字段 {0}，或它的取值不合法（见文档站「插件分发格式 v1」§3）")]
    Missing(&'static str),
    #[error(
        "插件 id {0:?} 不合法：要匹配 [A-Za-z0-9][A-Za-z0-9._-]*、不超过 128 字节、\
         不以 . 开头或结尾，且第一段不能是 Windows 保留名（CON/PRN/AUX/NUL/COM1-9/LPT1-9）"
    )]
    BadId(String),
    #[error("版本号 {0:?} 不合法：非空，且只允许 [0-9A-Za-z.+-]")]
    BadVersion(String),
    #[error("plugin.json 的 schema 是 {0}，本宿主只认 1：升级宿主，或换一份兼容的插件包")]
    UnsupportedSchema(u32),
    #[error("entry {0:?} 不合法：必须是包内相对路径，禁止绝对路径、`..`、盘符与开头的分隔符")]
    BadEntry(String),
    #[error("entry 指向的文件不存在：{0}（核对 plugin.json 的 entry 与包内实际布局）")]
    EntryMissing(PathBuf),
    #[error("entry {0} 解析后跑到包外了：插件包不许引用自己目录之外的文件")]
    EntryEscapes(PathBuf),
    #[error(
        "插件只支持 {want:?}，当前平台是 {host}：换对应平台的包，或把 platforms 写成 [\"any\"]"
    )]
    UnsupportedPlatform {
        want: Vec<String>,
        host: &'static str,
    },
}

impl BundleManifest {
    /// 从内存里的 JSON 文本解析。**不碰文件系统**，所以路径类错误里的 `path` 是 `<memory>`。
    pub fn parse(text: &str) -> Result<Self, BundleError> {
        Self::parse_at(text, Path::new("<memory>"))
    }

    /// 读 `dir/plugin.json` 并解析。只读清单本身，不检查 `entry` 是否存在。
    pub fn load(dir: &Path) -> Result<Self, BundleError> {
        let path = dir.join(BUNDLE_MANIFEST);
        let text = std::fs::read_to_string(&path).map_err(|source| BundleError::Io {
            path: path.clone(),
            source,
        })?;
        Self::parse_at(&text, &path)
    }

    /// 这份清单能不能在当前平台跑（空 `platforms` 与 `any` 都算全平台）。
    pub fn supports_host_platform(&self) -> bool {
        if self.platforms.is_empty() {
            return true;
        }
        let host = current_platform();
        self.platforms
            .iter()
            .any(|platform| platform == "any" || platform == host)
    }

    /// 把 `entry` 解析成包内路径，并确认它确实是一个文件。
    ///
    /// 三道关：**语法**（绝对路径 / `..` / 盘符 / 开头分隔符）、**包内**
    /// （`canonicalize` 之后比前缀，符号链接带不出去）、**存在且是文件**。
    /// 返回的是词法路径（`dir` + 各段），不是 `canonicalize` 的结果——Windows 的
    /// `\\?\` 前缀会跟着进日志和配置，没必要。
    pub fn entry_path(&self, dir: &Path) -> Result<PathBuf, BundleError> {
        // `entry` 是 pub 字段，调用方可能改过，所以这里重新校验一遍语法。
        let parts = entry_parts(&self.entry)?;
        let path = parts.iter().fold(dir.to_path_buf(), |mut path, part| {
            path.push(part);
            path
        });

        // 包根不在了，入口自然也不在——先给一条「文件不存在」，别报成「跑出包外」。
        let Ok(root) = std::fs::canonicalize(dir) else {
            return Err(BundleError::EntryMissing(path));
        };
        let Ok(resolved) = std::fs::canonicalize(&path) else {
            return Err(BundleError::EntryMissing(path));
        };
        if !resolved.starts_with(&root) {
            return Err(BundleError::EntryEscapes(path));
        }
        if !resolved.is_file() {
            return Err(BundleError::EntryMissing(path));
        }
        Ok(path)
    }

    /// 装之前该确认的一切：平台、入口存在且是文件。
    ///
    /// **平台先判**：一个 linux-only 的包在 Windows 上，`entry` 里的 `.dll` 当然不存在，
    /// 但真正该说的是「这包不是给你这个平台的」——先报 `entry` 只会把人引去改错地方。
    pub fn validate(&self, dir: &Path) -> Result<(), BundleError> {
        if !self.supports_host_platform() {
            return Err(BundleError::UnsupportedPlatform {
                want: self.platforms.clone(),
                host: current_platform(),
            });
        }
        self.entry_path(dir)?;
        Ok(())
    }

    /// 序列化成 `plugin.json` 的形状：两空格缩进、键顺序固定、末尾带换行。
    ///
    /// 只写契约 §3 里那几个字段（未知字段本来就被忽略，写回去也没意义）。
    /// `abi` 为 `None` 时**不输出**该键；`platforms` 等数组为空时写 `[]`。
    /// 保证 `parse(&to_json()) == self`——CLI 的 `install` / `migrate` 靠它生成 sidecar。
    pub fn to_json(&self) -> String {
        // 顺序就是契约表格里的顺序：人读、diff 都稳定。
        let mut fields: Vec<(&str, String)> = vec![
            ("schema", self.schema.to_string()),
            ("id", quoted(&self.id)),
            ("name", quoted(&self.name)),
            ("version", quoted(&self.version)),
            ("kind", quoted(self.kind.as_str())),
            ("entry", quoted(&self.entry)),
        ];
        if let Some(abi) = self.abi {
            fields.push(("abi", abi.to_string()));
        }
        fields.push(("platforms", json_array(&self.platforms)));
        fields.push(("depends", json_array(&self.depends)));
        fields.push(("optional_depends", json_array(&self.optional_depends)));
        fields.push(("provides", json_array(&self.provides)));
        if let Some(config) = &self.config {
            fields.push(("config", quoted(config)));
        }
        fields.push(("description", quoted(&self.description)));
        fields.push(("license", quoted(&self.license)));
        fields.push(("homepage", quoted(&self.homepage)));

        let body: Vec<String> = fields
            .iter()
            .map(|(key, value)| format!("  \"{key}\": {value}"))
            .collect();
        format!("{{\n{}\n}}\n", body.join(",\n"))
    }

    /// 把清单写到 `<dir>/plugin.json`，返回写出的路径。
    ///
    /// 目录本身不建：`install` / `migrate` 都会先准备好目标目录，这里替它们建目录
    /// 只会把「路径给错了」变成「在错的地方建了一棵空树」。
    pub fn write(&self, dir: &Path) -> Result<PathBuf, BundleError> {
        let path = dir.join(BUNDLE_MANIFEST);
        std::fs::write(&path, self.to_json()).map_err(|source| BundleError::Io {
            path: path.clone(),
            source,
        })?;
        Ok(path)
    }

    /// [`Self::parse`] 的带路径版本：`load` 用它把真实文件名塞进 JSON 错误里。
    fn parse_at(text: &str, path: &Path) -> Result<Self, BundleError> {
        let value: serde_json::Value =
            serde_json::from_str(text).map_err(|source| BundleError::Json {
                path: path.to_path_buf(),
                source,
            })?;

        let schema = value
            .get("schema")
            .and_then(serde_json::Value::as_u64)
            .and_then(|schema| u32::try_from(schema).ok())
            .ok_or(BundleError::Missing("schema"))?;
        if schema != SCHEMA_VERSION {
            return Err(BundleError::UnsupportedSchema(schema));
        }

        let id = required_str(&value, "id")?;
        check_id(id)?;

        let name = required_str(&value, "name")?;

        let version = required_str(&value, "version")?;
        check_version(version)?;

        // 不认识的 kind 归到 `Missing("kind")`：契约里没有 `BadKind`，而错误文案
        // 已经说明「取值不合法」，够用户改对了。
        let kind = value
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .and_then(PluginKind::parse)
            .ok_or(BundleError::Missing("kind"))?;

        let entry = required_str(&value, "entry")?;
        entry_parts(entry)?;

        Ok(Self {
            schema,
            id: id.to_owned(),
            name: name.to_owned(),
            version: version.to_owned(),
            kind,
            entry: entry.to_owned(),
            // 写了但不是数字：当作没写（可选字段，不值得为此拒绝整个包）。
            abi: value
                .get("abi")
                .and_then(serde_json::Value::as_u64)
                .and_then(|abi| u32::try_from(abi).ok()),
            platforms: string_array(&value, "platforms"),
            depends: string_array(&value, "depends"),
            optional_depends: string_array(&value, "optional_depends"),
            provides: string_array(&value, "provides"),
            config: value
                .get("config")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            description: optional_str(&value, "description"),
            license: optional_str(&value, "license"),
            homepage: optional_str(&value, "homepage"),
        })
    }
}

/// 当前平台的规范串，与发布资产命名一致（`windows-x86_64` / `linux-aarch64` / …）。
///
/// 只认发布矩阵里的组合；其余（不常见的 OS / 架构）返回 `"unknown"`——
/// 清单里写 `platforms: []` 或 `["any"]` 的插件照样能用，写死了具体平台的则不会误装。
pub fn current_platform() -> &'static str {
    if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        "windows-x86_64"
    } else if cfg!(all(target_os = "windows", target_arch = "aarch64")) {
        "windows-aarch64"
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        "linux-x86_64"
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        "linux-aarch64"
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        "macos-x86_64"
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "macos-aarch64"
    } else {
        "unknown"
    }
}

/// 扫描出来的一个候选插件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginEntry {
    /// 目录包（里面有 `plugin.json`），值是包目录。
    Bundle(PathBuf),
    /// 旧形态：平铺在插件目录里的动态库，值是那个文件。
    Legacy(PathBuf),
}

/// 列出插件目录里的候选：bundle 在前（按 id 字典序），legacy 在后（按文件名）。
///
/// 只扫**一层**：`bin/` / `node_modules/` / `.venv/` 里的东西永远不进结果。
/// 以 `.` 开头的目录（`.disabled` / `.staging` / `.cache`）与 `<id>.disable`
/// （旧面板「改名禁用」的残留）一律跳过——它们已经被停用过，不能再当活插件装一次。
///
/// 目录不存在返回 `Err`（与 [`plugin_files`](crate::plugin_files) 一致：调用方本来
/// 就按「插件目录有问题」处理）；目录存在但没有候选时返回 `Ok(vec![])`。
pub fn plugin_entries(dir: &Path) -> std::io::Result<Vec<PluginEntry>> {
    let mut bundles: Vec<(String, PathBuf)> = Vec::new();
    let mut legacy: Vec<PathBuf> = Vec::new();

    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        let name = file_name(&path);
        if path.is_dir() {
            if name.starts_with('.') || name.ends_with(DISABLED_SUFFIX) {
                continue;
            }
            if !is_bundle_dir(&path) {
                continue;
            }
            // 排序键是 id 而不是目录名：契约 §2 规定目录名与 id 不一致时以 id 为准，
            // 那么顺序也该跟着 id 走。清单坏掉的包按目录名排（它照样会被列出来，
            // 让 `probe` 去报那条真正的错误）。
            let id = BundleManifest::load(&path)
                .map(|manifest| manifest.id)
                .unwrap_or_else(|_| name.clone());
            bundles.push((id, path));
        } else if path.extension().is_some_and(|ext| ext == DLL_EXTENSION) {
            legacy.push(path);
        }
    }

    bundles.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    legacy.sort();

    let mut out = Vec::with_capacity(bundles.len() + legacy.len());
    out.extend(
        bundles
            .into_iter()
            .map(|(_, path)| PluginEntry::Bundle(path)),
    );
    out.extend(legacy.into_iter().map(PluginEntry::Legacy));
    Ok(out)
}

/// 列出被禁用的目录包：`<dir>/.disabled/<id>/` 与旧面板留下的 `<dir>/<id>.disable/`。
///
/// 两种都只认**里面有 `plugin.json`** 的目录：禁用清单不另存一份，目录本身就是事实。
/// 目录不存在（连插件目录都没有、或从没禁用过任何东西）时返回 `Ok(vec![])`——
/// 「没有禁用项」是常态，不是错误。
pub fn disabled_bundles(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();

    // ① 契约里的形态：整个包挪进 `.disabled/`。
    let off = dir.join(DISABLED_DIR);
    if let Some(entries) = read_dir_if_present(&off)? {
        for entry in entries {
            let path = entry?.path();
            if path.is_dir() && !file_name(&path).starts_with('.') && is_bundle_dir(&path) {
                out.push(path);
            }
        }
    }

    // ② 旧面板的形态：目录改名为 `<id>.disable`，文件全留在原地。
    if let Some(entries) = read_dir_if_present(dir)? {
        for entry in entries {
            let path = entry?.path();
            let name = file_name(&path);
            if path.is_dir()
                && name.ends_with(DISABLED_SUFFIX)
                && !name.starts_with('.')
                && is_bundle_dir(&path)
            {
                out.push(path);
            }
        }
    }

    out.sort();
    Ok(out)
}

/// 列出脚本插件根（`plugins/scripts/`）下带 `plugin.json` 的子目录。
///
/// 只用于**列举**：真正的发现 / 启停仍归 SDK 的 `fuyubot_python_host` /
/// `fuyubot_js_host`。没有清单的老脚本目录不会出现在这里（调用方可以自己看
/// `package.json` / `pyproject.toml`）。目录不存在时返回 `Ok(vec![])`。
pub fn script_bundles(scripts_dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let Some(entries) = read_dir_if_present(scripts_dir)? else {
        return Ok(out);
    };
    for entry in entries {
        let path = entry?.path();
        let name = file_name(&path);
        if path.is_dir()
            && !name.starts_with('.')
            && !name.ends_with(DISABLED_SUFFIX)
            && is_bundle_dir(&path)
        {
            out.push(path);
        }
    }
    out.sort();
    Ok(out)
}

/// 这个目录是不是一个目录包（有 `plugin.json` 就是）。
pub fn is_bundle_dir(path: &Path) -> bool {
    path.join(BUNDLE_MANIFEST).is_file()
}

/// `read_dir` 的「不存在算空」版本：可选目录（`.disabled/` / `scripts/`）用。
fn read_dir_if_present(dir: &Path) -> std::io::Result<Option<std::fs::ReadDir>> {
    match std::fs::read_dir(dir) {
        Ok(entries) => Ok(Some(entries)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// 文件名（非 UTF-8 的部分用替换字符，够排序与展示）。
fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// 必填字符串字段：缺了、或类型不对，都报 `Missing`（错误文案已涵盖两种）。
fn required_str<'a>(
    value: &'a serde_json::Value,
    key: &'static str,
) -> Result<&'a str, BundleError> {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .ok_or(BundleError::Missing(key))
}

/// 可选字符串字段：缺了、或类型不对，都当没写。
fn optional_str(value: &serde_json::Value, key: &str) -> String {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// 可选的字符串数组：只取元素里是字符串的那些（其余忽略，不整包拒绝）。
fn string_array(value: &serde_json::Value, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(serde_json::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// 校验 id：`[A-Za-z0-9][A-Za-z0-9._-]*`、≤128 字节、不以 `.` 开头或结尾、不含路径分隔符、
/// 第一段不是 Windows 保留设备名。
///
/// 后两条是 Windows 的坑，不是洁癖：目录名以 `.` 结尾会被系统**静默吃掉**，
/// 而 `CON` / `NUL` / `COM1` 这些名字建不出目录（`plugins/con/` 会失败或指向设备）。
fn check_id(id: &str) -> Result<(), BundleError> {
    let bad = || BundleError::BadId(id.to_owned());
    if id.is_empty() || id.len() > ID_MAX_LEN || id.starts_with('.') || id.ends_with('.') {
        return Err(bad());
    }
    let mut chars = id.chars();
    let first = chars.next().unwrap_or_default();
    if !first.is_ascii_alphanumeric() {
        return Err(bad());
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        return Err(bad());
    }
    // `con.foo` 同样非法：Windows 只按**第一段**（第一个点之前）判设备名。
    if is_windows_reserved_name(id.split('.').next().unwrap_or_default()) {
        return Err(bad());
    }
    Ok(())
}

/// 这一段是不是 Windows 的保留设备名（大小写不敏感）：`CON` / `PRN` / `AUX` / `NUL` /
/// `COM1`–`COM9` / `LPT1`–`LPT9`。
fn is_windows_reserved_name(segment: &str) -> bool {
    let upper = segment.to_ascii_uppercase();
    if matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL") {
        return true;
    }
    // id 的字符集是 ASCII，这里按字节比不会切坏多字节边界。
    if upper.len() != 4 {
        return false;
    }
    let (prefix, digit) = (&upper[..3], upper.as_bytes()[3]);
    (prefix == "COM" || prefix == "LPT") && matches!(digit, b'1'..=b'9')
}

/// 校验版本号：非空，只允许 `[0-9A-Za-z.+-]`（不强制 semver，商店阶段再严）。
fn check_version(version: &str) -> Result<(), BundleError> {
    if version.is_empty()
        || !version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '+' | '-'))
    {
        return Err(BundleError::BadVersion(version.to_owned()));
    }
    Ok(())
}

/// 把 `entry` 拆成路径段，顺手挡掉一切能跑出包外的写法。
///
/// 反斜杠按分隔符处理（Windows 作者常这么写），但**开头**的反斜杠是绝对路径，
/// 必须拒绝；含 `:` 的段一并拒绝（`C:\x`、`file:…` 这类在两种平台上都不该出现）。
/// `.` 与空段直接跳过（`./bin/x.dll` 是常见写法，没必要因此拒绝）。
fn entry_parts(entry: &str) -> Result<Vec<&str>, BundleError> {
    let bad = || BundleError::BadEntry(entry.to_owned());
    if entry.is_empty() || entry.starts_with('/') || entry.starts_with('\\') || entry.contains(':')
    {
        return Err(bad());
    }
    let mut parts = Vec::new();
    for part in entry.split(['/', '\\']) {
        match part {
            "" | "." => continue,
            ".." => return Err(bad()),
            part => parts.push(part),
        }
    }
    if parts.is_empty() {
        return Err(bad());
    }
    Ok(parts)
}

/// JSON 字符串字面量：转义交给 `serde_json`（中文原样写，控制字符按规范转义）。
fn quoted(text: &str) -> String {
    serde_json::Value::String(text.to_owned()).to_string()
}

/// JSON 字符串数组，空表写 `[]`。
fn json_array(items: &[String]) -> String {
    let rendered: Vec<String> = items.iter().map(|item| quoted(item)).collect();
    format!("[{}]", rendered.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试临时目录：仓库约定放 `target/test-tmp`（不用系统 TEMP，见契约 §7）。
    fn temp(name: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-tmp/fuyubot-loader/bundle")
            .join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建测试临时目录失败");
        dir
    }

    /// 一份合法清单，测试按需改其中一两个字段。
    fn valid() -> serde_json::Value {
        serde_json::json!({
            "schema": 1,
            "id": "com.example.weather",
            "name": "Weather",
            "version": "1.2.0",
            "kind": "native",
            "entry": "bin/weather.dll",
            "abi": 5,
            "platforms": ["windows-x86_64", "linux-x86_64"],
            "depends": ["fuyubot.help.commands"],
            "optional_depends": ["media.ffmpeg"],
            "provides": ["weather.api"],
            "config": "config.default.toml",
            "description": "天气",
            "license": "MIT",
            "homepage": "https://example.com/weather"
        })
    }

    fn parse(value: &serde_json::Value) -> Result<BundleManifest, BundleError> {
        BundleManifest::parse(&value.to_string())
    }

    /// 落一个「清单 + 入口文件」的完整包，返回包目录。
    fn package(root: &Path, dir_name: &str, id: &str) -> PathBuf {
        let dir = root.join(dir_name);
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::write(dir.join("bin").join("weather.dll"), b"not a real dll").unwrap();
        let mut value = valid();
        value["id"] = serde_json::json!(id);
        std::fs::write(dir.join(BUNDLE_MANIFEST), value.to_string()).unwrap();
        dir
    }

    #[test]
    fn a_valid_manifest_round_trips_every_field() {
        let manifest = parse(&valid()).expect("合法清单该解析成功");
        assert_eq!(manifest.schema, 1);
        assert_eq!(manifest.id, "com.example.weather");
        assert_eq!(manifest.name, "Weather");
        assert_eq!(manifest.version, "1.2.0");
        assert_eq!(manifest.kind, PluginKind::Native);
        assert_eq!(manifest.entry, "bin/weather.dll");
        assert_eq!(manifest.abi, Some(5));
        assert_eq!(manifest.platforms, ["windows-x86_64", "linux-x86_64"]);
        assert_eq!(manifest.depends, ["fuyubot.help.commands"]);
        assert_eq!(manifest.optional_depends, ["media.ffmpeg"]);
        assert_eq!(manifest.provides, ["weather.api"]);
        assert_eq!(manifest.config.as_deref(), Some("config.default.toml"));
        assert_eq!(manifest.description, "天气");
        assert_eq!(manifest.license, "MIT");
        assert_eq!(manifest.homepage, "https://example.com/weather");
    }

    #[test]
    fn optional_fields_may_be_absent() {
        let mut value = valid();
        for key in [
            "abi",
            "platforms",
            "depends",
            "optional_depends",
            "provides",
            "config",
            "description",
            "license",
            "homepage",
        ] {
            value.as_object_mut().unwrap().remove(key);
        }
        let manifest = parse(&value).expect("可选项全缺也该解析成功");
        assert_eq!(manifest.abi, None);
        assert!(manifest.platforms.is_empty());
        assert!(manifest.depends.is_empty());
        assert_eq!(manifest.config, None);
        assert_eq!(manifest.description, "");
        assert!(manifest.supports_host_platform(), "空 platforms = 全平台");
    }

    #[test]
    fn missing_required_fields_name_themselves() {
        for (key, expected) in [
            ("schema", "schema"),
            ("id", "id"),
            ("name", "name"),
            ("version", "version"),
            ("kind", "kind"),
            ("entry", "entry"),
        ] {
            let mut value = valid();
            value.as_object_mut().unwrap().remove(key);
            match parse(&value) {
                Err(BundleError::Missing(field)) => assert_eq!(field, expected, "缺 {key}"),
                other => panic!("缺 {key} 该报 Missing({expected})，实际 {other:?}"),
            }
        }
    }

    #[test]
    fn bad_ids_are_rejected() {
        // 路径分隔符、以 `.` 开头、超长、空串、含空格——都不是 id。
        for id in [
            "com/example",
            "com\\example",
            ".hidden",
            "",
            "has space",
            "中文插件",
            &"a".repeat(ID_MAX_LEN + 1),
        ] {
            let mut value = valid();
            value["id"] = serde_json::json!(id);
            assert!(
                matches!(parse(&value), Err(BundleError::BadId(_))),
                "id {id:?} 该被拒绝"
            );
        }
        // Windows 的两个坑：尾点会被系统静默吃掉；第一段是保留设备名就建不出目录。
        for id in [
            "com.example.",
            "CON",
            "con.foo",
            "NUL.plugin",
            "LPT1",
            "com3.plugin",
            "AUX",
            "prn.x",
        ] {
            let mut value = valid();
            value["id"] = serde_json::json!(id);
            assert!(
                matches!(parse(&value), Err(BundleError::BadId(_))),
                "id {id:?} 该被拒绝（Windows 尾点 / 保留名）"
            );
        }
        // 边界：正好 128 字节、以数字开头、含 `.` `_` `-` 都合法；
        // 保留名只按第一段判，`console` / `com10` 不是设备名。
        for id in [
            "9fuyubot.manage-weather_v2",
            "console.plugin",
            "com10.plugin",
            "auxiliary",
        ] {
            let mut value = valid();
            value["id"] = serde_json::json!(id);
            assert!(parse(&value).is_ok(), "id {id:?} 该合法");
        }
    }

    #[test]
    fn bad_versions_are_rejected() {
        for version in ["", "1.2 beta", "v1.2.0!", "1.2.0/../x"] {
            let mut value = valid();
            value["version"] = serde_json::json!(version);
            assert!(
                matches!(parse(&value), Err(BundleError::BadVersion(_))),
                "version {version:?} 该被拒绝"
            );
        }
        for version in ["1.2.0", "1.2.0-rc.1+build.7", "0.0.0"] {
            let mut value = valid();
            value["version"] = serde_json::json!(version);
            assert!(parse(&value).is_ok(), "version {version:?} 该合法");
        }
    }

    #[test]
    fn an_unknown_schema_is_reported_with_its_number() {
        let mut value = valid();
        value["schema"] = serde_json::json!(2);
        match parse(&value) {
            Err(BundleError::UnsupportedSchema(schema)) => assert_eq!(schema, 2),
            other => panic!("schema=2 该报 UnsupportedSchema，实际 {other:?}"),
        }
    }

    #[test]
    fn entry_cannot_escape_the_package() {
        // `..`、绝对路径（POSIX 与 Windows 两种写法）、盘符、开头的反斜杠。
        for entry in [
            "../x.dll",
            "bin/../../x.dll",
            "bin\\..\\..\\x.dll",
            "/usr/lib/x.so",
            "\\\\server\\share\\x.dll",
            "C:\\plugins\\x.dll",
            "C:/plugins/x.dll",
            "",
        ] {
            let mut value = valid();
            value["entry"] = serde_json::json!(entry);
            assert!(
                matches!(parse(&value), Err(BundleError::BadEntry(_))),
                "entry {entry:?} 该被拒绝"
            );
        }
        // `./` 前缀是常见写法，归一化后照样合法。
        let mut value = valid();
        value["entry"] = serde_json::json!("./bin/weather.dll");
        let manifest = parse(&value).expect("./ 前缀不该被拒绝");
        assert_eq!(manifest.entry, "./bin/weather.dll");
    }

    #[test]
    fn an_entry_that_is_not_on_disk_is_reported_as_missing() {
        let root = temp("entry-missing");
        let dir = root.join("com.example.weather");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(BUNDLE_MANIFEST), valid().to_string()).unwrap();

        let manifest = BundleManifest::load(&dir).expect("清单本身是好的");
        match manifest.entry_path(&dir) {
            Err(BundleError::EntryMissing(path)) => {
                assert!(
                    path.ends_with(Path::new("bin").join("weather.dll")),
                    "{path:?}"
                )
            }
            other => panic!("入口不存在该报 EntryMissing，实际 {other:?}"),
        }
        // 目录也不存在时同样是 EntryMissing，而不是「跑出包外」。
        assert!(matches!(
            manifest.entry_path(&root.join("no-such-package")),
            Err(BundleError::EntryMissing(_))
        ));
    }

    #[test]
    fn an_entry_that_exists_is_resolved_relative_to_the_package() {
        let root = temp("entry-ok");
        let dir = package(&root, "com.example.weather", "com.example.weather");
        let manifest = BundleManifest::load(&dir).unwrap();
        let entry = manifest.entry_path(&dir).expect("入口该解析成功");
        assert_eq!(entry, dir.join("bin").join("weather.dll"));
        manifest.validate(&dir).expect("合法包该通过校验");
    }

    #[test]
    fn a_directory_entry_is_not_a_file() {
        let root = temp("entry-is-dir");
        let dir = root.join("com.example.weather");
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        let mut value = valid();
        value["entry"] = serde_json::json!("bin");
        std::fs::write(dir.join(BUNDLE_MANIFEST), value.to_string()).unwrap();
        let manifest = BundleManifest::load(&dir).unwrap();
        assert!(matches!(
            manifest.entry_path(&dir),
            Err(BundleError::EntryMissing(_))
        ));
    }

    #[test]
    fn platform_filtering_matches_the_contract() {
        let mut value = valid();
        // 一个一定不等于当前平台的串。
        value["platforms"] = serde_json::json!(["plan9-mips"]);
        let manifest = parse(&value).unwrap();
        assert!(!manifest.supports_host_platform());
        match manifest.validate(&temp("platform-mismatch")) {
            Err(BundleError::UnsupportedPlatform { want, host }) => {
                assert_eq!(want, ["plan9-mips"]);
                assert_eq!(host, current_platform());
            }
            other => panic!("平台不匹配该报 UnsupportedPlatform，实际 {other:?}"),
        }

        // `any` 与空表都是全平台。
        let mut value = valid();
        value["platforms"] = serde_json::json!(["any"]);
        assert!(parse(&value).unwrap().supports_host_platform());
        let mut value = valid();
        value["platforms"] = serde_json::json!([]);
        assert!(parse(&value).unwrap().supports_host_platform());
    }

    #[test]
    fn unknown_fields_are_ignored_for_forward_compatibility() {
        let mut value = valid();
        value["store"] = serde_json::json!({ "price": 0, "tags": ["weather"] });
        value["future_flag"] = serde_json::json!(true);
        let manifest = parse(&value).expect("未知字段不该让清单失效");
        assert_eq!(manifest.id, "com.example.weather");
    }

    /// `to_json` 写出来的东西必须能被自己读回去（CLI 的 install / migrate 靠它生成 sidecar）。
    #[test]
    fn to_json_round_trips() {
        let manifest = parse(&valid()).unwrap();
        let text = manifest.to_json();
        assert!(text.ends_with('\n'), "末尾带换行，diff 才干净");
        assert_eq!(
            BundleManifest::parse(&text).unwrap(),
            manifest,
            "to_json → parse 该原样回来：\n{text}"
        );
        // 键顺序固定（schema / id / name / version / kind / entry …），人读与 diff 都稳定。
        let order: Vec<usize> = ["schema", "id", "name", "version", "kind", "entry"]
            .iter()
            .map(|key| text.find(&format!("\"{key}\"")).expect("键都该在"))
            .collect();
        assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "{order:?}");

        // 可选项：`abi` 为 None 时不输出该键；空数组写 `[]`。
        let mut value = valid();
        value.as_object_mut().unwrap().remove("abi");
        value["platforms"] = serde_json::json!([]);
        value.as_object_mut().unwrap().remove("config");
        let minimal = parse(&value).unwrap();
        let text = minimal.to_json();
        assert!(
            !text.contains("\"abi\""),
            "没写 abi 就不该输出该键：\n{text}"
        );
        assert!(!text.contains("\"config\""), "没写 config 就不该输出该键");
        assert!(text.contains("\"platforms\": []"), "{text}");
        assert_eq!(BundleManifest::parse(&text).unwrap(), minimal);
    }

    #[test]
    fn write_lands_the_manifest_in_the_package() {
        let root = temp("write");
        let dir = root.join("com.example.weather");
        std::fs::create_dir_all(&dir).unwrap();
        let manifest = parse(&valid()).unwrap();

        let path = manifest.write(&dir).expect("写清单该成功");
        assert_eq!(path, dir.join(BUNDLE_MANIFEST));
        assert_eq!(BundleManifest::load(&dir).unwrap(), manifest);
        assert_eq!(
            BundleManifest::load(&dir).unwrap().to_json(),
            manifest.to_json()
        );

        // 目录不存在 = Io（不替调用方建目录）。
        assert!(matches!(
            manifest.write(&root.join("no-such-dir")),
            Err(BundleError::Io { .. })
        ));
    }

    #[test]
    fn broken_json_names_the_file() {
        let root = temp("broken-json");
        let dir = root.join("com.example.weather");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(BUNDLE_MANIFEST), b"{ not json").unwrap();
        match BundleManifest::load(&dir) {
            Err(BundleError::Json { path, .. }) => {
                assert!(path.ends_with(BUNDLE_MANIFEST), "{path:?}")
            }
            other => panic!("坏 JSON 该报 Json，实际 {other:?}"),
        }
        // 没有清单 = Io。
        assert!(matches!(
            BundleManifest::load(&root.join("no-such-dir")),
            Err(BundleError::Io { .. })
        ));
    }

    #[test]
    fn the_kind_decides_who_runs_the_entry() {
        assert_eq!(PluginKind::parse("native"), Some(PluginKind::Native));
        assert_eq!(PluginKind::parse("python"), Some(PluginKind::Python));
        assert_eq!(PluginKind::parse("js"), Some(PluginKind::Js));
        assert_eq!(PluginKind::parse("rust"), None, "只认契约里那三种");
        assert!(!PluginKind::Native.is_script());
        assert!(PluginKind::Python.is_script());
        assert!(PluginKind::Js.is_script());
        assert_eq!(PluginKind::Python.as_str(), "python");
        assert_eq!(PluginKind::Python.to_string(), "python");
    }

    #[test]
    fn current_platform_looks_like_a_release_asset_name() {
        let platform = current_platform();
        assert!(!platform.is_empty());
        assert!(
            platform.contains('-'),
            "平台串是 <os>-<arch> 形态：{platform}"
        );
    }

    #[test]
    fn discovery_puts_bundles_first_then_legacy() {
        let root = temp("discovery");
        // 两个包：目录名与 id 故意一个一致、一个不一致（以 id 排序）。
        package(&root, "com.example.weather", "com.example.weather");
        package(&root, "some-dir-name", "com.example.alpha");
        // 旧形态平铺库、被禁用的平铺库。
        std::fs::write(root.join(format!("weather.{DLL_EXTENSION}")), b"x").unwrap();
        std::fs::write(
            root.join(format!("old.{DLL_EXTENSION}{DISABLED_SUFFIX}")),
            b"x",
        )
        .unwrap();
        // 隐藏目录（`.disabled` / `.staging`）与没有清单的普通目录都不该进候选。
        std::fs::create_dir_all(root.join(DISABLED_DIR).join("gone")).unwrap();
        std::fs::write(
            root.join(DISABLED_DIR).join("gone").join(BUNDLE_MANIFEST),
            valid().to_string(),
        )
        .unwrap();
        std::fs::create_dir_all(root.join(".staging").join("half")).unwrap();
        std::fs::write(
            root.join(".staging").join("half").join(BUNDLE_MANIFEST),
            valid().to_string(),
        )
        .unwrap();
        std::fs::create_dir_all(root.join(SCRIPTS_DIR).join("py-plugin")).unwrap();
        std::fs::write(root.join("notes.txt"), b"hi").unwrap();
        // 旧面板的残留：目录被改名成 `<id>.disable`。
        let renamed = package(&root, "com.example.legacy-off", "com.example.legacy-off");
        let renamed = renamed.with_file_name(format!("com.example.legacy-off{DISABLED_SUFFIX}"));
        std::fs::rename(root.join("com.example.legacy-off"), &renamed).unwrap();

        let entries = plugin_entries(&root).unwrap();
        let bundles: Vec<&Path> = entries
            .iter()
            .filter_map(|entry| match entry {
                PluginEntry::Bundle(path) => Some(path.as_path()),
                PluginEntry::Legacy(_) => None,
            })
            .collect();
        assert_eq!(
            bundles,
            vec![
                root.join("some-dir-name").as_path(),
                root.join("com.example.weather").as_path(),
            ],
            "bundle 按 id 排：alpha < weather"
        );
        let legacy: Vec<&Path> = entries
            .iter()
            .filter_map(|entry| match entry {
                PluginEntry::Legacy(path) => Some(path.as_path()),
                PluginEntry::Bundle(_) => None,
            })
            .collect();
        assert_eq!(
            legacy,
            vec![root.join(format!("weather.{DLL_EXTENSION}")).as_path()],
            "legacy 按文件名排，禁用的不进候选"
        );
        assert_eq!(
            entries.len(),
            3,
            "隐藏目录 / 无清单目录 / `<id>.disable` 都不该进候选：{entries:?}"
        );
    }

    #[test]
    fn a_missing_plugin_dir_is_an_error_but_an_empty_one_is_not() {
        let root = temp("empty-dir");
        assert!(plugin_entries(&root).unwrap().is_empty());
        assert!(
            plugin_entries(&root.join("no-such-dir")).is_err(),
            "插件目录不存在要报错（与 plugin_files 一致）"
        );
    }

    #[test]
    fn disabled_bundles_lists_both_disabling_styles() {
        let root = temp("disabled");
        let off = root.join(DISABLED_DIR).join("com.example.gone");
        std::fs::create_dir_all(&off).unwrap();
        std::fs::write(off.join(BUNDLE_MANIFEST), valid().to_string()).unwrap();
        // `.disabled/` 下没有清单的目录不算（可能是别的东西）。
        std::fs::create_dir_all(root.join(DISABLED_DIR).join("junk")).unwrap();
        // 旧面板的改名残留。
        let renamed = package(&root, "com.example.renamed", "com.example.renamed");
        std::fs::rename(
            root.join("com.example.renamed"),
            renamed.with_file_name(format!("com.example.renamed{DISABLED_SUFFIX}")),
        )
        .unwrap();

        let found = disabled_bundles(&root).unwrap();
        assert_eq!(
            found,
            vec![
                off.clone(),
                root.join(format!("com.example.renamed{DISABLED_SUFFIX}")),
            ]
        );

        // 没有 `.disabled/` 是常态：空表而不是错误。
        let bare = temp("disabled-none");
        assert!(disabled_bundles(&bare).unwrap().is_empty());
        assert!(
            disabled_bundles(&bare.join("no-such-dir"))
                .unwrap()
                .is_empty(),
            "插件目录都没有时也算「没有禁用项」"
        );
    }

    #[test]
    fn script_bundles_only_lists_directories_with_a_manifest() {
        let root = temp("scripts");
        let scripts = root.join(SCRIPTS_DIR);
        let with_manifest = scripts.join("py-plugin");
        std::fs::create_dir_all(&with_manifest).unwrap();
        std::fs::write(with_manifest.join(BUNDLE_MANIFEST), valid().to_string()).unwrap();
        // 老脚本目录：只有 package.json，不在列举结果里。
        let legacy = scripts.join("js-plugin");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("package.json"), b"{}").unwrap();
        // 隐藏目录 / 禁用目录同样跳过。
        std::fs::create_dir_all(scripts.join(".staging").join("half")).unwrap();
        std::fs::write(
            scripts.join(".staging").join("half").join(BUNDLE_MANIFEST),
            valid().to_string(),
        )
        .unwrap();

        assert_eq!(script_bundles(&scripts).unwrap(), vec![with_manifest]);
        // 没有 scripts 目录：空表，不是错误。
        assert!(script_bundles(&root.join("no-scripts")).unwrap().is_empty());
        assert!(is_bundle_dir(&scripts.join("py-plugin")));
        assert!(!is_bundle_dir(&legacy));
    }
}
