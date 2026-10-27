//! 插件加载器：dlopen + `plugin_init` / `plugin_start` / `plugin_stop`。

use std::ffi::{CStr, CString};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use fuyubot_abi::{
    ABI_VERSION, PluginHandle, PluginInitFn, PluginStartFn, PluginStopFn, SYM_PLUGIN_INIT,
    SYM_PLUGIN_START, SYM_PLUGIN_STOP,
};
use fuyubot_core::Context;
use libloading::{Library, Symbol};

use crate::pipeline::UpdatePipeline;
use crate::runtime::HostRuntime;

/// 动态库后缀（按平台）。
pub const DLL_EXTENSION: &str = if cfg!(target_os = "windows") {
    "dll"
} else if cfg!(target_os = "macos") {
    "dylib"
} else {
    "so"
};

/// 加载失败的原因。每一种都不会 panic，也不会留下半初始化的资源。
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("打开动态库失败 {path}: {source}")]
    Library {
        path: PathBuf,
        #[source]
        source: libloading::Error,
    },
    #[error("{path} 不是 fuyubot 插件：缺少符号 {symbol}: {source}")]
    MissingSymbol {
        path: PathBuf,
        symbol: &'static str,
        #[source]
        source: libloading::Error,
    },
    #[error("宿主运行时装载失败: {0}")]
    Runtime(String),
    #[error("插件 {path} 的 plugin_init 返回了失败句柄（它拒绝加载，或宿主 ABI 与它不符）")]
    InitFailed { path: PathBuf },
    #[error("插件 {path} 的 ABI 版本是 {plugin}，宿主是 {host}")]
    AbiMismatch {
        path: PathBuf,
        plugin: u32,
        host: u32,
    },
    #[error("插件 {path} 的 plugin_start 返回 {code}")]
    StartFailed { path: PathBuf, code: i32 },
    #[error("配置字符串里有 NUL 字节")]
    BadConfig,
}

/// 一个已加载并已启动的插件。
///
/// **drop 即停用**：先 `plugin_stop`，再把该插件注册到宿主的资源整批回滚，
/// 最后释放动态库（字段声明顺序保证了这一点）。
///
/// 例外：**Go 编出来的 c-shared 库不能卸载**——它的运行时有线程/信号处理器，
/// `FreeLibrary` / `dlclose` 之后进程立刻 access violation（0xC0000005）。
/// 所以对这类库故意泄漏句柄，让它在进程生命周期内一直映射着（见 `go_runtime`）。
pub struct LoadedPlugin {
    pub name: String,
    pub path: PathBuf,
    pub abi_version: u32,
    handle: PluginHandle,
    stop: Option<PluginStopFn>,
    /// 这次加载在宿主侧注册的所有东西的归属 id。
    owner: u64,
    /// 保证动态库活到最后：插件代码在 drop 之前一直可用。
    _lib: Option<Library>,
    /// 这个库是不是 Go 的 c-shared（`crosscall2` 是 Go 运行时符号）。
    /// 是的话 drop 时只 `forget`、不卸载。
    go_runtime: bool,
    /// 保证宿主运行时比插件活得久。
    _runtime: Arc<HostRuntime>,
}

impl std::fmt::Debug for LoadedPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoadedPlugin")
            .field("name", &self.name)
            .field("path", &self.path)
            .field("abi_version", &self.abi_version)
            .finish_non_exhaustive()
    }
}

impl LoadedPlugin {
    /// 插件自己的实例数据指针，仅供诊断。
    pub fn context(&self) -> *mut core::ffi::c_void {
        self.handle.context
    }
}

impl Drop for LoadedPlugin {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            // SAFETY: handle 来自本插件的 plugin_init，且 ABI 版本已经校验过。
            unsafe { stop(self.handle) };
        }
        // 插件停掉之后，把它注册的订阅/工具/服务/中间件一次性撤掉。
        self._runtime.rollback_owner(self.owner);
        // owner → 名字的映射也一并摘掉（插件卸载后不能再以它之名写配置）。
        self._runtime.unregister_owner_name(self.owner);
        // 动态库这时才真正卸载；Go 的 c-shared 除外——卸载它会让进程崩（见类型文档）。
        if self.go_runtime {
            if let Some(lib) = self._lib.take() {
                std::mem::forget(lib);
            }
        }
    }
}

/// 插件加载器。持有宿主运行时，`load` 出来的插件与它共享同一份运行时。
pub struct PluginLoader {
    runtime: Arc<HostRuntime>,
}

impl std::fmt::Debug for PluginLoader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginLoader")
            .field("runtime", &self.runtime)
            .finish()
    }
}

impl PluginLoader {
    /// 在给定的内核 [`Context`] 上装好宿主运行时。
    pub fn new(ctx: Context) -> Result<Self, LoadError> {
        let runtime = HostRuntime::new(ctx).map_err(LoadError::Runtime)?;
        Ok(Self { runtime })
    }

    /// 复用已有的运行时（多个加载器共享一份宿主状态）。
    pub fn from_runtime(runtime: Arc<HostRuntime>) -> Self {
        Self { runtime }
    }

    pub fn runtime(&self) -> &Arc<HostRuntime> {
        &self.runtime
    }

    /// 更新分发管线：`td_receive` 拿到的原始更新交给它处理。
    pub fn pipeline(&self) -> UpdatePipeline {
        UpdatePipeline::new(Arc::clone(&self.runtime))
    }

    /// 加载并启动一个插件。
    pub fn load(&self, path: impl AsRef<Path>) -> Result<LoadedPlugin, LoadError> {
        self.load_with_config(path, "")
    }

    /// 加载并启动，带上给插件的配置 JSON。
    pub fn load_with_config(
        &self,
        path: impl AsRef<Path>,
        config: &str,
    ) -> Result<LoadedPlugin, LoadError> {
        let path = path.as_ref();
        let config = CString::new(config).map_err(|_| LoadError::BadConfig)?;

        // 先规范化成绝对路径：`open_library` 只对绝对路径开 Windows 的
        // `LOAD_WITH_ALTERED_SEARCH_PATH`，**插件旁边的兄弟 DLL 就靠这个才找得到**。
        // 用 `Library::new`（flags = 0）时系统只查 exe 目录 + System32 + PATH，
        // 会报 `ERROR_MOD_NOT_FOUND`，而那个错误完全不指向真正的原因（见 AGENTS 约束 30 / pitfalls 坑 43）。
        // 规范化失败（文件不存在之类）就退回原路径，让下面的加载报出正常的 `Library` 错误。
        let location = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());
        // SAFETY: 加载动态库就等于执行其中的代码，只加载可信文件。
        let lib = unsafe { fuyubot_tdlib::loader::open_library(&location) }.map_err(|source| {
            LoadError::Library {
                path: path.to_owned(),
                source,
            }
        })?;

        // Go 的 c-shared 库卸载即崩（运行时限制），所以要先认出它。
        // 不能靠导出符号：Go 只导出 `//export` 出来的那几个 + `_cgo_dummy_export`，
        // `crosscall2` 之类并不在导出表里。可靠的标记是文件末尾的 `Go buildinf:` 头。
        let go_runtime = looks_like_go_binary(&location);

        // 先把符号取成裸函数指针，再让库可以被移动进 LoadedPlugin。
        let (init, start, stop) = {
            // SAFETY: 只从刚加载的库里取符号，名字与类型由 ABI 约定。
            let init: Symbol<PluginInitFn> =
                unsafe { lib.get(SYM_PLUGIN_INIT) }.map_err(|source| LoadError::MissingSymbol {
                    path: path.to_owned(),
                    symbol: "plugin_init",
                    source,
                })?;
            let init = *init;
            // start / stop 是可选的：只做元数据登记的插件不需要它们。
            let start = unsafe { lib.get::<PluginStartFn>(SYM_PLUGIN_START) }
                .ok()
                .map(|symbol| *symbol);
            let stop = unsafe { lib.get::<PluginStopFn>(SYM_PLUGIN_STOP) }
                .ok()
                .map(|symbol| *symbol);
            (init, start, stop)
        };

        // 从这里开始，任何提前返回都会让 `scope` 在 drop 时把这批注册整批回滚。
        let scope = self.runtime.plugin_scope();
        let owner = scope.owner();

        // SAFETY: init 是本库的 plugin_init；vtable 由本运行时持有，地址稳定。
        let handle = unsafe { init(self.runtime.vtable_ptr()) };
        if !handle.is_valid() {
            return Err(LoadError::InitFailed {
                path: path.to_owned(),
            });
        }

        if handle.abi_version != ABI_VERSION {
            // 版本不兼容时**不能**调 plugin_stop：那是对一个我们不认识的 ABI 下手。
            // 只能回滚宿主侧资源；插件在 init 里自己分配的状态这一版无法回收（见 docs/gaps.md 缺口 7）。
            return Err(LoadError::AbiMismatch {
                path: path.to_owned(),
                plugin: handle.abi_version,
                host: ABI_VERSION,
            });
        }

        // 现在就能定下插件名（`handle.name` 在 init 时就有），并在 start 之前登记
        // owner → 名字的映射：插件在 `start` 里（以及之后的回调里）调 `set_config`
        // 时，宿主靠它判定「这是谁的段」。名字在 start 失败时也要能算（见下面）。
        let name = plugin_name(&handle, path);
        self.runtime.register_owner_name(owner, &name);

        if let Some(start) = start {
            // SAFETY: handle 来自本插件的 init；config 在调用期间存活。
            let code = unsafe { start(handle, config.as_ptr()) };
            if code != 0 {
                // ABI 已经校验过，这里停掉是安全的。
                if let Some(stop) = stop {
                    // SAFETY: 同一个 handle。
                    unsafe { stop(handle) };
                }
                // start 失败：这次的 owner 映射也要摘掉（加载没成功，不该留名字）。
                self.runtime.unregister_owner_name(owner);
                return Err(LoadError::StartFailed {
                    path: path.to_owned(),
                    code,
                });
            }
        }

        scope.commit();

        Ok(LoadedPlugin {
            name,
            path: path.to_owned(),
            abi_version: handle.abi_version,
            handle,
            stop,
            owner,
            _lib: Some(lib),
            go_runtime,
            _runtime: Arc::clone(&self.runtime),
        })
    }

    /// 卸载。等同于 `drop(plugin)`，留着是为了让调用点意图更明确。
    pub fn unload(&self, plugin: LoadedPlugin) {
        drop(plugin);
    }

    /// 扫描目录逐个加载；单个失败不中断其余的。
    pub fn load_dir(
        &self,
        dir: impl AsRef<Path>,
    ) -> std::io::Result<Vec<(PathBuf, Result<LoadedPlugin, LoadError>)>> {
        let mut results = Vec::new();
        for path in plugin_files(dir)? {
            let outcome = self.load(&path);
            results.push((path, outcome));
        }
        Ok(results)
    }
}

/// 目录下的候选插件文件（按扩展名筛），已排序。
pub fn plugin_files(dir: impl AsRef<Path>) -> std::io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e == DLL_EXTENSION) {
            out.push(path);
        }
    }
    out.sort();
    Ok(out)
}

/// 插件名：优先用 `plugin_init` 给的那个静态字符串（插件自己持有，框架不释放）。
fn plugin_name(handle: &PluginHandle, path: &Path) -> String {
    if !handle.name.is_null() {
        // SAFETY: ABI 契约要求 name 是插件持有的 NUL 结尾字符串，库还活着。
        let name = unsafe { CStr::from_ptr(handle.name) }.to_string_lossy();
        if !name.is_empty() {
            return name.into_owned();
        }
    }
    path.file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| "unknown".to_owned())
}

/// 这个动态库是不是 Go 编出来的（Go 的库不能卸载，见 [`LoadedPlugin`]）。
///
/// 判据是 Go 的构建信息魔数 `\xff Go buildinf:`。**不能靠导出符号**：Go 的 c-shared
/// 只导出 `//export` 的那几个函数，`crosscall2` 之类并不在导出表里（踩过）；
/// 也不能只看文件末尾——PE 里这段在中间的数据段（也踩过）。
///
/// 整份读进内存找魔数，插件就几 MB，加载一次的开销可以忽略；
/// 真到「插件动辄上百 MB」再改成带重叠加窗的分块扫描。
fn looks_like_go_binary(path: &Path) -> bool {
    const MAGIC: &[u8] = b"\xff Go buildinf:";

    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    bytes.windows(MAGIC.len()).any(|window| window == MAGIC)
}

#[cfg(test)]
mod go_runtime_tests {
    use super::*;

    #[test]
    fn detects_go_build_info_magic() {
        // 造一个中间带 Go buildinfo 魔数的文件，和一份普通文件做对照。
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-tmp/fuyubot-loader");
        std::fs::create_dir_all(&dir).unwrap();

        let go_like = dir.join("go-like.bin");
        let mut bytes = vec![0_u8; 4096];
        bytes.extend_from_slice(b"\xff Go buildinf:");
        bytes.extend_from_slice(&[0_u8; 64]);
        std::fs::write(&go_like, &bytes).unwrap();
        assert!(looks_like_go_binary(&go_like));

        let plain = dir.join("plain.bin");
        std::fs::write(&plain, vec![7_u8; 4096]).unwrap();
        assert!(!looks_like_go_binary(&plain));
    }
}
