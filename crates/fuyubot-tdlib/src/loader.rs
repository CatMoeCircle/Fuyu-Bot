//! 原生动态库的定位、加载与版本校验。
//!
//! 优先级从高到低，第一个命中者胜出：
//! 1. `TDLIB_TDJSON_PATH` 指向的文件或目录；
//! 2. vendored 目录（需要 `vendored` feature）；
//! 3. Linux 系统目录里带版本号的 `libtdjson.so.*`，取与目标主版本兼容的最高版本；
//! 4. 裸库名，交给操作系统动态链接器搜索。

pub mod vendored;

use std::fmt;
use std::path::{Path, PathBuf};

use libloading::Library;

use crate::ffi::{SYM_CREATE, SYM_DESTROY, SYM_EXECUTE, SYM_RECEIVE, SYM_SEND, TdApi};

/// 编译期绑定的 TDLib 版本，来自 `[package.metadata.fuyubot] tdlib-version`。
pub const TDLIB_VERSION: &str = env!("FUYUBOT_TDLIB_VERSION");

/// 加载失败原因。
#[derive(Debug, thiserror::Error)]
pub enum TdlibLoadError {
    /// 四个优先级都没找到。`attempts` 里逐条列出试过什么、为什么不行。
    #[error("找不到可用的 tdjson 动态库。已尝试：\n{attempts}")]
    NotFound { attempts: String },
    /// vendored 目录里的版本和编译期声明的版本对不上。
    #[error("vendored 库版本不匹配：期望 {expected}，实际 {found}")]
    VersionMismatch { expected: String, found: String },
    #[error("加载 {location} 失败: {source}")]
    Dlopen {
        location: String,
        #[source]
        source: libloading::Error,
    },
    #[error("库里没有符号 {name}: {source}")]
    Symbol {
        name: String,
        #[source]
        source: libloading::Error,
    },
    #[error("td_json_client_create 返回了空指针")]
    NullClientHandle,
}

/// 库是从哪一层找到的。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LibrarySource {
    /// 优先级 1：`TDLIB_TDJSON_PATH` 指向的文件。
    EnvFile,
    /// 优先级 1：`TDLIB_TDJSON_PATH` 指向的目录。
    EnvDir,
    /// 优先级 2：vendored 目录。
    Vendored,
    /// 优先级 3：系统目录里带版本号的库。
    SystemVersioned,
    /// 优先级 4：裸库名，交给系统链接器。
    BareName,
}

impl fmt::Display for LibrarySource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::EnvFile => "TDLIB_TDJSON_PATH 文件",
            Self::EnvDir => "TDLIB_TDJSON_PATH 目录",
            Self::Vendored => "vendored",
            Self::SystemVersioned => "系统已安装（带版本号）",
            Self::BareName => "裸库名（系统链接器）",
        };
        f.write_str(text)
    }
}

/// 命中的库。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedLibrary {
    /// 传给 `Library::new` 的值：绝对路径，或交给系统链接器搜索的裸库名。
    pub target: PathBuf,
    pub source: LibrarySource,
    /// 一路上的说明，含优先级 3 的候选清单与优先级 4 的“不做版本校验”提醒。
    pub notes: Vec<String>,
}

/// 解析时用到的环境快照。
///
/// 测试直接构造它，避免改进程环境变量（Edition 2024 里 `set_var` 是 `unsafe`，
/// 而且测试并行跑会互相打架）。
#[derive(Debug, Clone, Default)]
pub struct LoadEnv {
    /// `TDLIB_TDJSON_PATH`。
    pub tdlib_path: Option<PathBuf>,
    /// 编译期注入的 vendored 目录，只在 `vendored` feature 下存在。
    pub vendored_dir: Option<PathBuf>,
    /// 编译期注入的 vendored 版本。
    pub vendored_version: Option<String>,
    /// 优先级 3 的候选目录。
    pub system_dirs: Vec<PathBuf>,
    /// 目标平台，取值同 `std::env::consts::OS`。
    pub os: String,
}

impl LoadEnv {
    /// 从真实进程环境与编译期常量构造。
    pub fn from_process() -> Self {
        Self {
            tdlib_path: std::env::var_os("TDLIB_TDJSON_PATH")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from),
            vendored_dir: compiled_vendored_dir(),
            vendored_version: compiled_vendored_version(),
            system_dirs: default_system_dirs(),
            os: std::env::consts::OS.to_owned(),
        }
    }
}

#[cfg(feature = "vendored")]
fn compiled_vendored_dir() -> Option<PathBuf> {
    option_env!("FUYUBOT_TDLIB_VENDORED_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

#[cfg(not(feature = "vendored"))]
fn compiled_vendored_dir() -> Option<PathBuf> {
    None
}

#[cfg(feature = "vendored")]
fn compiled_vendored_version() -> Option<String> {
    option_env!("FUYUBOT_TDLIB_VENDORED_VERSION").map(str::to_owned)
}

#[cfg(not(feature = "vendored"))]
fn compiled_vendored_version() -> Option<String> {
    None
}

/// Linux 上优先找的目录；其它平台不参与优先级 3。
fn default_system_dirs() -> Vec<PathBuf> {
    if cfg!(target_os = "linux") {
        [
            "/usr/lib/x86_64-linux-gnu",
            "/usr/local/lib",
            "/usr/lib",
            "/lib",
        ]
        .iter()
        .map(PathBuf::from)
        .collect()
    } else {
        Vec::new()
    }
}

/// 用真实环境解析。
pub fn resolve() -> Result<ResolvedLibrary, TdlibLoadError> {
    resolve_with(&LoadEnv::from_process(), &bare_name_loads)
}

/// 裸库名是否真的能被系统链接器找到。`resolve_with` 用它判定优先级 4 是否命中。
fn bare_name_loads(name: &Path) -> bool {
    // SAFETY: 只是探测能不能加载；加载成功的话句柄立即 drop，不执行任何代码。
    unsafe { Library::new(name) }.is_ok()
}

/// 用给定的环境与探测函数解析，便于测试。
pub fn resolve_with(
    env: &LoadEnv,
    bare_name_probe: &dyn Fn(&Path) -> bool,
) -> Result<ResolvedLibrary, TdlibLoadError> {
    let mut attempts: Vec<String> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    let wanted_name = vendored::library_filename(&env.os);

    // ── 优先级 1：TDLIB_TDJSON_PATH ────────────────────────────────
    match &env.tdlib_path {
        Some(path) if path.is_dir() => {
            match wanted_name.and_then(|name| existing(path.join(name))) {
                Some(found) => return Ok(resolved(found, LibrarySource::EnvDir, notes)),
                None => attempts.push(format!(
                    "[1] TDLIB_TDJSON_PATH={} 是目录，但里面没有 {}",
                    path.display(),
                    wanted_name.unwrap_or("<未知平台无对应库名>")
                )),
            }
        }
        Some(path) if path.is_file() => {
            notes.push("TDLIB_TDJSON_PATH 直接指定了文件，跳过版本校验".to_owned());
            return Ok(resolved(path.clone(), LibrarySource::EnvFile, notes));
        }
        Some(path) => attempts.push(format!(
            "[1] TDLIB_TDJSON_PATH={} 既不是文件也不是目录",
            path.display()
        )),
        None => attempts.push("[1] 未设置 TDLIB_TDJSON_PATH".to_owned()),
    }

    // ── 优先级 2：vendored ────────────────────────────────────────
    match &env.vendored_dir {
        Some(dir) => {
            if let Some(found) = &env.vendored_version
                && found != TDLIB_VERSION
            {
                return Err(TdlibLoadError::VersionMismatch {
                    expected: TDLIB_VERSION.to_owned(),
                    found: found.clone(),
                });
            }
            match wanted_name.and_then(|name| existing(dir.join(name))) {
                Some(found) => return Ok(resolved(found, LibrarySource::Vendored, notes)),
                None => attempts.push(format!(
                    "[2] vendored 目录 {} 里没有 {}",
                    dir.display(),
                    wanted_name.unwrap_or("<未知平台无对应库名>")
                )),
            }
        }
        None => attempts.push(
            "[2] 没有 vendored 目录：未启用 vendored feature，或构建期没能把库取回来".to_owned(),
        ),
    }

    // ── 优先级 3：Linux 系统目录里带版本号的库 ──────────────────────
    if env.os == "linux" {
        let candidates = versioned_candidates(&env.system_dirs);
        if candidates.is_empty() {
            attempts.push(format!(
                "[3] 这些目录里没有 libtdjson.so.<版本>：{}",
                env.system_dirs
                    .iter()
                    .map(|d| d.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        } else {
            let listed = candidates
                .iter()
                .map(|(path, version)| format!("{} ({version:?})", path.display()))
                .collect::<Vec<_>>()
                .join(", ");
            notes.push(format!("[3] 候选库（已按版本降序）：{listed}"));
            if let Some((path, version)) = candidates.first() {
                notes.push(format!("[3] 选中版本 {version:?}"));
                return Ok(resolved(
                    path.clone(),
                    LibrarySource::SystemVersioned,
                    notes,
                ));
            }
        }
    } else {
        attempts.push(format!(
            "[3] 仅 Linux 会扫描带版本号的系统库（当前 {}）",
            env.os
        ));
    }

    // ── 优先级 4：裸库名 ─────────────────────────────────────────
    if let Some(name) = wanted_name {
        let bare = PathBuf::from(name);
        if bare_name_probe(&bare) {
            notes.push(format!(
                "[4] 用裸库名 {name} 让系统链接器搜索，未做版本校验"
            ));
            return Ok(resolved(bare, LibrarySource::BareName, notes));
        }
        attempts.push(format!("[4] 裸库名 {name} 系统链接器也找不到"));
    } else {
        attempts.push(format!("[4] 未知平台 {}，没有约定的库文件名", env.os));
    }

    Err(TdlibLoadError::NotFound {
        attempts: attempts.join("\n"),
    })
}

fn resolved(target: PathBuf, source: LibrarySource, notes: Vec<String>) -> ResolvedLibrary {
    ResolvedLibrary {
        target,
        source,
        notes,
    }
}

/// 存在且是普通文件才算命中。
fn existing(path: PathBuf) -> Option<PathBuf> {
    path.is_file().then_some(path)
}

/// 扫目录，挑出 `libtdjson.so.<major>[.<minor>[.<patch>]]`，
/// 只要与目标主版本兼容的，按版本从高到低排序。
pub fn versioned_candidates(dirs: &[PathBuf]) -> Vec<(PathBuf, (u32, u32, u32))> {
    let expected_major = vendored::parse_version(TDLIB_VERSION).map(|v| v.0);
    let mut found: Vec<(PathBuf, (u32, u32, u32))> = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(version) = vendored::parse_versioned_library_name(&name) else {
                continue;
            };
            if expected_major.is_some_and(|major| major != version.0) {
                continue;
            }
            if entry.path().is_file() {
                found.push((entry.path(), version));
            }
        }
    }
    found.sort_by(|a, b| b.1.cmp(&a.1));
    found.dedup_by(|a, b| a.0 == b.0);
    found
}

/// 加载动态库，让系统**先查这个库自己所在的目录**。
///
/// Windows 上必须用 `LOAD_WITH_ALTERED_SEARCH_PATH`：`Library::new` 传的是 `flags = 0`，
/// 系统只查「exe 目录 + System32 + PATH」，**不查被加载的那个 DLL 自己所在的目录**。
/// 于是「tdjson 和它自己的依赖（libssl / libcrypto / zlib）并排放在一起」这种部署形态
/// 会直接失败，报 `LoadLibraryExW { source: 126 }`（ERROR_MOD_NOT_FOUND）——
/// 错误信息完全不指向真正的原因（真库在目录里，缺的是它的依赖）。
/// 只有绝对路径才加这个 flag：裸库名（优先级 4）没有「自己所在的目录」，照旧交给系统搜索。
///
/// **同一个坑对插件同样成立**（插件旁边放它的兄弟 DLL），所以 `fuyubot-loader` 也调它
/// ——传进来的路径得先规范化成绝对路径，否则这个 flag 加不上。
///
/// # Safety
/// 调用方保证加载这个库是安全的（会执行它的初始化代码），并让返回的句柄活到
/// 从它身上取出的符号不再被使用为止。
pub unsafe fn open_library(target: &Path) -> Result<Library, libloading::Error> {
    #[cfg(windows)]
    if target.is_absolute() {
        use libloading::os::windows::{LOAD_WITH_ALTERED_SEARCH_PATH, Library as WinLibrary};

        // SAFETY: 由调用方的契约保证。
        let lib = unsafe { WinLibrary::load_with_flags(target, LOAD_WITH_ALTERED_SEARCH_PATH) }?;
        return Ok(Library::from(lib));
    }
    // SAFETY: 由调用方的契约保证。
    unsafe { Library::new(target) }
}

/// 解析 5 个符号。
///
/// # Safety
/// `lib` 必须是已加载的 TDLib，且活到返回的 [`TdApi`] 不再被使用为止。
pub unsafe fn resolve_symbols(lib: &Library) -> Result<TdApi, TdlibLoadError> {
    unsafe fn symbol<T: Copy>(lib: &Library, name: &str) -> Result<T, TdlibLoadError> {
        // SAFETY: 调用方保证库正确；类型由名字约定。
        let raw = unsafe { lib.get::<T>(name) }.map_err(|source| TdlibLoadError::Symbol {
            name: name.to_owned(),
            source,
        })?;
        Ok(*raw)
    }

    // SAFETY: 同函数契约。
    unsafe {
        Ok(TdApi {
            create: symbol(lib, SYM_CREATE)?,
            destroy: symbol(lib, SYM_DESTROY)?,
            send: symbol(lib, SYM_SEND)?,
            receive: symbol(lib, SYM_RECEIVE)?,
            execute: symbol(lib, SYM_EXECUTE)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// 假库文件：优先级 1/2/3 只看文件名与存在性，不会真的 dlopen。
    fn touch(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, b"not a real library").unwrap();
        path
    }

    fn env_with(os: &str) -> LoadEnv {
        LoadEnv {
            os: os.to_owned(),
            ..LoadEnv::default()
        }
    }

    #[test]
    fn priority_1_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = touch(dir.path(), "tdjson.dll");
        let env = LoadEnv {
            tdlib_path: Some(path.clone()),
            os: "windows".into(),
            ..LoadEnv::default()
        };
        let got = resolve_with(&env, &|_| false).unwrap();
        assert_eq!(got.source, LibrarySource::EnvFile);
        assert_eq!(got.target, path);
    }

    #[test]
    fn priority_1_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = touch(dir.path(), "libtdjson.so");
        let env = LoadEnv {
            tdlib_path: Some(dir.path().to_owned()),
            os: "linux".into(),
            ..LoadEnv::default()
        };
        let got = resolve_with(&env, &|_| false).unwrap();
        assert_eq!(got.source, LibrarySource::EnvDir);
        assert_eq!(got.target, path);
    }

    #[cfg(feature = "vendored")]
    #[test]
    fn priority_2_vendored() {
        let dir = tempfile::tempdir().unwrap();
        let path = touch(dir.path(), "tdjson.dll");
        let env = LoadEnv {
            vendored_dir: Some(dir.path().to_owned()),
            vendored_version: Some(TDLIB_VERSION.to_owned()),
            os: "windows".into(),
            ..LoadEnv::default()
        };
        let got = resolve_with(&env, &|_| false).unwrap();
        assert_eq!(got.source, LibrarySource::Vendored);
        assert_eq!(got.target, path);

        // 版本对不上必须直接报错，而不是悄悄用错版本。
        let env = LoadEnv {
            vendored_dir: Some(dir.path().to_owned()),
            vendored_version: Some("0.0.1".into()),
            os: "windows".into(),
            ..LoadEnv::default()
        };
        assert!(matches!(
            resolve_with(&env, &|_| false),
            Err(TdlibLoadError::VersionMismatch { .. })
        ));
    }

    #[test]
    fn priority_3_picks_highest_compatible_version() {
        let dir = tempfile::tempdir().unwrap();
        let expected = vendored::parse_version(TDLIB_VERSION).unwrap();
        let highest = format!("libtdjson.so.{}.{}.{}", expected.0, expected.1, expected.2);
        touch(dir.path(), &highest);
        touch(dir.path(), &format!("libtdjson.so.{}.0.0", expected.0));
        // 主版本不兼容，必须被排除，哪怕版本号更大。
        touch(dir.path(), &format!("libtdjson.so.{}.0.0", expected.0 + 1));

        let env = LoadEnv {
            system_dirs: vec![dir.path().to_owned()],
            os: "linux".into(),
            ..LoadEnv::default()
        };
        let got = resolve_with(&env, &|_| false).unwrap();
        assert_eq!(got.source, LibrarySource::SystemVersioned);
        assert_eq!(got.target.file_name().unwrap().to_string_lossy(), highest);
    }

    #[test]
    fn all_missing_reports_every_attempt() {
        let dir = tempfile::tempdir().unwrap();
        let empty = tempfile::tempdir().unwrap();
        let env = LoadEnv {
            tdlib_path: Some(dir.path().join("nope")),
            system_dirs: vec![empty.path().to_owned()],
            os: "linux".into(),
            ..LoadEnv::default()
        };
        let err = resolve_with(&env, &|_| false).unwrap_err();
        let text = err.to_string();
        for marker in ["[1]", "[2]", "[3]", "[4]"] {
            assert!(text.contains(marker), "错误信息漏了 {marker}: {text}");
        }
        assert!(text.contains("nope"), "错误信息漏了尝试过的路径: {text}");
    }

    #[test]
    fn priority_4_bare_name() {
        // 探测函数说“系统链接器找得到”，就该走裸库名。
        let got = resolve_with(&env_with("linux"), &|_| true).unwrap();
        assert_eq!(got.source, LibrarySource::BareName);
        assert_eq!(got.target, PathBuf::from("libtdjson.so"));
    }
}
