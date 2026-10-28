//! 插件加载器：dlopen + `plugin_init` / `plugin_start` / `plugin_stop`。

use std::ffi::{CStr, CString};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use core::ffi::c_char;

use fuyubot_abi::{
    ABI_VERSION, PluginFreeHandleFn, PluginHandle, PluginInitFn, PluginStartFn, PluginStopFn,
    SYM_PLUGIN_FREE_HANDLE, SYM_PLUGIN_FREE_STRING, SYM_PLUGIN_INIT, SYM_PLUGIN_MANIFEST,
    SYM_PLUGIN_START, SYM_PLUGIN_STOP,
};
use fuyubot_core::{Context, Effect};
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
        let (init, start, stop, free_handle) = {
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
            // v4 的出口，同样可选：v3 插件没有它（那就只能像以前一样把状态留在那儿）。
            let free_handle = unsafe { lib.get::<PluginFreeHandleFn>(SYM_PLUGIN_FREE_HANDLE) }
                .ok()
                .map(|symbol| *symbol);
            (init, start, stop, free_handle)
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
            // 但 v4 留了一个专门的出口：plugin_free_handle 只接 handle 本身、不碰
            // VTable，签名在契约里是**永久冻结**的（AGENTS 约束 57），所以在这一刻
            // 调用它是安全的——插件在 init 里分配的状态就靠它归还。
            // 没导出这个符号的（v3 插件）只能像以前一样把状态留在那儿。
            if let Some(free) = free_handle {
                // SAFETY: handle 来自同一轮 init；该符号只读 handle 里的 context。
                unsafe { free(handle) };
            }
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

    /// 扫一个动态库的元数据：名字、ABI 版本。**不启动它。**
    ///
    /// 之所以要走这一步：依赖是按**插件名**声明的（点分 ID），而 dll 的文件名
    /// 常常和它不是一回事，宿主没先验手段知道「这个文件是谁」。所以先礼貌地
    /// 问一句名字，排好队再正式加载。
    ///
    /// 代价是这个库被映射了两次（探测一次、加载一次）。Windows / Linux 都有
    /// 文件映射缓存，第二次几乎免费；换来的是**正式加载路径完全不用改**，
    /// 也不必把 `LoadedPlugin` 拆成「已 init / 已 start」两种状态。
    pub fn probe(&self, path: impl AsRef<Path>) -> Result<Probe, LoadError> {
        let path = path.as_ref();
        let location = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());
        // SAFETY: 加载动态库等于执行其中的代码，只加载可信文件。
        let lib = unsafe { fuyubot_tdlib::loader::open_library(&location) }.map_err(|source| {
            LoadError::Library {
                path: path.to_owned(),
                source,
            }
        })?;

        // 探测期内不能让宿主任就把东西注册了：这个 scope **不 commit**，
        // drop 时整批回滚（见 `HostRuntime::plugin_scope`）。
        let scope = self.runtime.plugin_scope();

        let init: Symbol<PluginInitFn> =
            unsafe { lib.get(SYM_PLUGIN_INIT) }.map_err(|source| LoadError::MissingSymbol {
                path: path.to_owned(),
                symbol: "plugin_init",
                source,
            })?;
        let init = *init;
        let stop = unsafe { lib.get::<PluginStopFn>(SYM_PLUGIN_STOP) }
            .ok()
            .map(|symbol| *symbol);
        let free_handle = unsafe { lib.get::<PluginFreeHandleFn>(SYM_PLUGIN_FREE_HANDLE) }
            .ok()
            .map(|symbol| *symbol);

        // SAFETY: init / stop 来自本库；vtable 由本运行时持有，地址稳定。
        let handle = unsafe { init(self.runtime.vtable_ptr()) };
        if !handle.is_valid() {
            return Err(LoadError::InitFailed {
                path: path.to_owned(),
            });
        }

        let name = plugin_name(&handle, path);

        // 礼貌地把 init 期间分配的东西还给插件：一次探测不该在插件侧漏状态。
        if handle.abi_version == ABI_VERSION {
            if let Some(stop) = stop {
                // SAFETY: ABI 已校验，stop 是它的 plugin_stop。
                unsafe { stop(handle) };
            }
        } else if let Some(free) = free_handle {
            // 版本对不上时只能走 v4 的 plugin_free_handle（签名永久冻结，
            // 不碰 VTable），理由同 load_with_config。
            // SAFETY: 同上，只读 handle 里的 context。
            unsafe { free(handle) };
        }
        // scope 在这里自然 drop：init 注册过的一切整批撤销。

        // 显式收掉它：`init` 期间注册过的一切在这一刻整批撤销——
        // 一次探测不该在宿主上留下任何痕迹。
        drop(scope);

        // 自描述要在同一轮 dlopen 里读：库还热着，省一次开合。
        // 名字对不上只告警——`plugin_init` 那个名字是内核用来寻址的，以它为准。
        let manifest = unsafe { read_manifest(&lib) };
        if let Some(manifest) = &manifest {
            if !manifest.name.is_empty() && manifest.name != name {
                self.runtime.warn(&format!(
                    "插件自报名与 plugin_init 不一致：manifest={} init={name}（以 init 为准）",
                    manifest.name
                ));
            }
        }

        Ok(Probe {
            path: path.to_owned(),
            name,
            abi_version: handle.abi_version,
            manifest,
        })
    }

    /// 扫描目录逐个加载；单个失败不中断其余的。
    ///
    /// 顺序是文件名字典序——**没有依赖语义**，见 [`Self::load_dir_ordered`]。
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

    /// 扫描目录，为每个动态库造一个 [`DylibPlugin`]（**只探测，不启动**）。
    ///
    /// 依赖排序**不在这里做**：拿到适配器之后交给
    /// [`PluginManager::load_all`](fuyubot_core::PluginManager::load_all)，
    /// 由内核的 fiber 状态机决定谁先起来、谁先等着。这是本版唯一一套依赖语义。
    ///
    /// 探测失败的不进队列，但原因要留住——文件在那儿却悄悄消失是最难查的那类事。
    pub fn scan_dir(
        &self,
        dir: impl AsRef<Path>,
        config_for: impl Fn(&Probe) -> String,
    ) -> std::io::Result<Vec<Result<DylibPlugin, LoadError>>> {
        let mut adapters = Vec::new();
        for path in plugin_files(dir)? {
            // 先探一次拿到配置需要的元数据（脚本路径那类东西由调用方按 path 决定）。
            match self.probe(&path) {
                Ok(probe) => {
                    let config = config_for(&probe);
                    adapters.push(Ok(DylibPlugin::new(
                        probe,
                        config,
                        Arc::clone(&self.runtime),
                    )));
                }
                Err(error) => adapters.push(Err(error)),
            }
        }
        Ok(adapters)
    }
}

/// 一个动态库的**探测结果**：它是谁，还不知道要不要它。
///
/// 探测只做「把元数据问出来」：`dlopen` → `plugin_init` → 读名字 →
/// 交给 `plugin_stop` 收拾 → 释放。全程**不碰 `plugin_start`**，插件的业务
/// 逻辑一次也没跑过。真正要加载时，正式那条路径会自己再开一遍。
#[derive(Debug, Clone)]
pub struct Probe {
    pub path: PathBuf,
    /// `plugin_init` 自己报出来的名字（插件名是点分的，和 dll 文件名常常不同）。
    pub name: String,
    pub abi_version: u32,
    /// ABI v4 的自描述（名字 / 版本 / 依赖）。老插件没有这个导出，就是 `None`。
    pub manifest: Option<Manifest>,
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

/// 插件自描述：ABI v4 的 `plugin_manifest` 导出。
///
/// 宿主在 `plugin_init` **之前**读它，所以依赖声明不依赖「插件先被启动」——
/// 这正是内核要的：`inject` 决定要不要激活，激活了才轮到 `start`。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Manifest {
    /// 插件自报的名字（应当与 `plugin_init` 给的一致，不一致时只告警，以 init 为准）。
    pub name: String,
    pub version: String,
    /// 硬依赖：缺了不激活。
    pub inject: Vec<String>,
    /// 软依赖：缺了不阻塞，有了优先排前面。
    pub soft_inject: Vec<String>,
}

type PluginManifestFn = unsafe extern "C" fn() -> *const c_char;
type PluginFreeStringFn = unsafe extern "C" fn(*mut c_char);

/// 读插件自描述。老插件（ABI v3 及以前）没有这个符号，返回 `None`。
///
/// `plugin_manifest` 每次调用都新分配一条字符串，契约是**调用方用 `plugin_free_string`
/// 归还**——不还就是每次加载漏一小段（v4 之前没有这个符号，所以只有新插件会走到）。
///
/// # Safety
/// `lib` 必须是已经加载好的、由本进程持有的插件库。
unsafe fn read_manifest(lib: &Library) -> Option<Manifest> {
    // SAFETY: 只从刚加载的库里取符号，名字与类型由 ABI 约定。
    let manifest: Symbol<PluginManifestFn> = unsafe { lib.get(SYM_PLUGIN_MANIFEST) }.ok()?;
    // SAFETY: 契约要求它返回 NUL 结尾的 JSON（可能为 NULL）。
    let raw = unsafe { manifest() };
    if raw.is_null() {
        return None;
    }
    // SAFETY: 同上，而且这份字符串在 free 之前一直有效。
    let text = unsafe { CStr::from_ptr(raw) }
        .to_string_lossy()
        .into_owned();
    // SAFETY: 指针来自本库的 `CString::into_raw`，只还给它一次。没有这个符号就让它漏
    // 一次（加载期一次性，且说明这是个不合规的 v4 插件）。
    if let Ok(free) = unsafe { lib.get::<PluginFreeStringFn>(SYM_PLUGIN_FREE_STRING) } {
        unsafe { free(raw as *mut c_char) };
    }

    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let strings = |key: &str| -> Vec<String> {
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
    };
    let field = |key: &str| -> String {
        value
            .get(key)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    Some(Manifest {
        name: field("name"),
        version: field("version"),
        inject: strings("inject"),
        soft_inject: strings("soft_inject"),
    })
}

/// 从文件里把库打开、init、start 全走一遍，成功后交回活着的那些东西。
///
/// 和 [`PluginLoader::load_with_config`] 的分工：那边「加载完就把生命周期交出去」，
/// 返回一个自己管收摊的 [`LoadedPlugin`]；这里把四件东西交回来，由
/// [`DylibPlugin`] 决定什么时候、以什么顺序收。
fn start_library(
    runtime: &Arc<HostRuntime>,
    path: &Path,
    config: &str,
) -> Result<(PluginHandle, Option<PluginStopFn>, Library, u64), LoadError> {
    let config = CString::new(config).map_err(|_| LoadError::BadConfig)?;
    // 规范化成绝对路径：`open_library` 只对绝对路径开 Windows 的
    // `LOAD_WITH_ALTERED_SEARCH_PATH`，插件旁边的兄弟 DLL 靠这个才找得到（约束 30 / 坑 43）。
    let location = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());
    // SAFETY: 加载动态库就等于执行其中的代码，只加载可信文件。
    let lib = unsafe { fuyubot_tdlib::loader::open_library(&location) }.map_err(|source| {
        LoadError::Library {
            path: path.to_owned(),
            source,
        }
    })?;

    let (init, start, stop) = {
        // SAFETY: 只从刚加载的库里取符号，名字与类型由 ABI 约定。
        let init: Symbol<PluginInitFn> =
            unsafe { lib.get(SYM_PLUGIN_INIT) }.map_err(|source| LoadError::MissingSymbol {
                path: path.to_owned(),
                symbol: "plugin_init",
                source,
            })?;
        let init = *init;
        let start = unsafe { lib.get::<PluginStartFn>(SYM_PLUGIN_START) }
            .ok()
            .map(|symbol| *symbol);
        let stop = unsafe { lib.get::<PluginStopFn>(SYM_PLUGIN_STOP) }
            .ok()
            .map(|symbol| *symbol);
        (init, start, stop)
    };

    // 从这里开始，任何提前返回都会让 `scope` 在 drop 时把这批注册整批回滚。
    let scope = runtime.plugin_scope();
    let owner = scope.owner();

    // SAFETY: init 是本库的 plugin_init；vtable 由本运行时持有，地址稳定。
    let handle = unsafe { init(runtime.vtable_ptr()) };
    if !handle.is_valid() {
        return Err(LoadError::InitFailed {
            path: path.to_owned(),
        });
    }

    if handle.abi_version != ABI_VERSION {
        // 版本不兼容时**不能**调 plugin_stop：那是对一个我们不认识的 ABI 下手。
        return Err(LoadError::AbiMismatch {
            path: path.to_owned(),
            plugin: handle.abi_version,
            host: ABI_VERSION,
        });
    }

    // 名字必须在 start 之前登记：插件在 `start` 里就可能调 `set_config`，
    // 宿主靠 owner → 名字的映射判定「这是谁的段」。
    let name = plugin_name(&handle, path);
    runtime.register_owner_name(owner, &name);

    if let Some(start) = start {
        // SAFETY: handle 来自本插件的 init；config 在调用期间存活。
        let code = unsafe { start(handle, config.as_ptr()) };
        if code != 0 {
            if let Some(stop) = stop {
                // SAFETY: ABI 已经校验过，这里停掉是安全的。
                unsafe { stop(handle) };
            }
            runtime.unregister_owner_name(owner);
            return Err(LoadError::StartFailed {
                path: path.to_owned(),
                code,
            });
        }
    }

    scope.commit();
    Ok((handle, stop, lib, owner))
}

/// 这个动态库是不是 Go 编出来的（Go 的库不能卸载，见 [`LoadedPlugin`]）。///
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

/// 一个动态库插件在**内核 Fiber 状态机**里的形态。
///
/// 这是「把 dylib 接进 Cordis」的桥：Cordis 那边的插件是一个实现了
/// [`fuyubot_core::Plugin`] 的 Rust 值，依赖用 `inject()` 声明、生命周期由
/// [`PluginManager`](fuyubot_core::PluginManager) 的 Pending/Active 状态机驱赶；
/// 而 dylib 那侧的「插件」是四个导出符号 + 一张 vtable。适配器把后者伪装成前者，
/// 于是两边只剩**一套**依赖语义，不再各写一份。
///
/// 它**不**自己做依赖排序，也不自己判断能不能起来——那是 `PluginManager` 的活。
/// 它只负责：`apply` = dlopen + `plugin_init` + `plugin_start`，
/// 返回的 `Effect` = `plugin_stop` + 宿主侧注册整批回滚。
///
/// ⚠️ **卸载是软的（不 `dlclose`）**：`Effect` 跑完只停插件、撤注册，动态库本身
/// 留在 `Slot::Stopped` 里，进程退出才释放。原因是安全边界——事件总线的订阅快照
/// 持着指向插件代码的回调 `Arc`，`dlclose` 之后那条订阅万一被调到就是访问违例。
/// Cordis 那边靠 GC 托管根本没有这个问题；我们只能选择不卸载。
/// 代价是每个卸过的插件留几 MB 映射，换掉的是一整类崩溃。
/// （Go 的 c-shared 本来就不能卸载，见约束 50——现在两种插件口径一致了。）
pub struct DylibPlugin {
    name: String,
    path: PathBuf,
    config: String,
    /// 承载 `plugin_init` / `plugin_start` / `plugin_stop` 与全部 ABI 状态。
    ///
    /// `Mutex` 不是为了并发调用——ABI 回调是同步的——而是因为 `apply` 走
    /// `&self`（[`fuyubot_core::Plugin`] 的签名），可变状态只能靠内部可变性。
    ///
    /// 外面再套一层 `Arc` 是为了 `Effect`：回滚闭包要求 `'static`，而它由内核
    /// 在**卸载时**才跑，那时 `&self` 早就不在了。闭包自己持一份 `Arc` 才安全。
    slot: Arc<Mutex<Slot>>,
    runtime: Arc<HostRuntime>,
    /// 探测阶段就知道的 ABI 版本（只为诊断；真加载时会再校验一次）。
    abi_version: u32,
    /// ABI v4 的自描述：依赖与版本以插件自己说的为准。
    manifest: Option<Manifest>,
}

/// 把 dylib 插件伪装成内核插件。
///
/// `apply` 做完 `plugin_init` + `plugin_start`，把「怎么收摊」包成 [`Effect`]：
/// 内核只在两处跑它——这个 fiber 退回 Pending（依赖消失），或整个卸载。
/// 那正是 Cordis 的 `Effect` 语义，所以不需要 `dispose()`（它留给「服务注销之外
/// 的清理」，dylib 插件没有这种东西）。
/// 依赖声明的种类。
#[derive(Debug, Clone, Copy)]
enum Dependency {
    /// 硬依赖：缺了不激活。
    Hard,
    /// 软依赖：缺了不阻塞，有了优先排前面。
    Soft,
}

/// 依赖声明从哪来：**插件自描述优先**（ABI v4 的 `plugin_manifest`），
/// 没声明才退回配置段。
///
/// 为什么反过来了：插件自己最清楚自己调哪个服务，写死在插件代码里就不会出现
/// 「换了台机器忘了抄 inject」这类配置漂移；配置只给老插件（v3，没有 manifest）
/// 与临时覆盖兜底。
fn declared(
    manifest: Option<&Manifest>,
    which: Dependency,
    from_config: Vec<String>,
) -> Vec<String> {
    if let Some(manifest) = manifest {
        let declared = match which {
            Dependency::Hard => &manifest.inject,
            Dependency::Soft => &manifest.soft_inject,
        };
        if !declared.is_empty() {
            return declared.clone();
        }
    }
    from_config
}

impl fuyubot_core::Plugin for DylibPlugin {
    fn name(&self) -> &str {
        &self.name
    }

    /// 硬依赖：manifest 优先，配置段 `[plugin.<插件名>].inject` 兜底。
    fn inject(&self) -> Vec<String> {
        let from_config = self
            .runtime
            .plugin_config_value(&self.name, "inject")
            .and_then(|value| value.as_array().cloned())
            .into_iter()
            .flatten()
            .filter_map(|item| item.as_str().map(str::to_owned))
            .collect();
        declared(self.manifest.as_ref(), Dependency::Hard, from_config)
    }

    /// 软依赖：同上，manifest 优先；配置段 `[plugin.<名>].soft_inject` 兜底。
    ///
    /// 缺了不阻塞、有了优先排前面——「基础设施先起来」靠的就是它。
    fn soft_inject(&self) -> Vec<String> {
        let from_config = self
            .runtime
            .plugin_config_value(&self.name, "soft_inject")
            .and_then(|value| value.as_array().cloned())
            .into_iter()
            .flatten()
            .filter_map(|item| item.as_str().map(str::to_owned))
            .collect();
        declared(self.manifest.as_ref(), Dependency::Soft, from_config)
    }

    fn apply(&self, _ctx: &Context) -> Result<Effect, fuyubot_core::KernelError> {
        let (handle, stop, lib, owner) = start_library(&self.runtime, &self.path, &self.config)
            .map_err(|error| {
                // 内核的 `KernelError` 不认 `LoadError`（那是本 crate 的类型，
                // 且 core 不许依赖 loader）。带上原因文本，日志里还能看懂。
                fuyubot_core::KernelError::PluginApplyFailed {
                    plugin: self.name.clone(),
                    reason: error.to_string(),
                }
            })?;

        // `manifest.version` 以前读进来就躺在结构里没人用。它的用途是诊断：
        // 激活时把「插件自己说它是哪个版本、要什么」记一笔——插件名可以撞车
        // （dll 文件名、目录名、配置都能影响），版本 + 依赖才是这个二进制的事实。
        if let Some(manifest) = &self.manifest {
            self.runtime.info(&format!(
                "激活 {} v{}（硬依赖 {}、软依赖 {}）",
                self.name,
                manifest.version,
                manifest.inject.len(),
                manifest.soft_inject.len()
            ));
        }

        *self.lock() = Slot::Running {
            handle,
            stop,
            owner,
        };

        // 闭包自己持一份 `Arc`：内核会在**卸载时**才跑它，`&self` 那时已经没了。
        let slot = Arc::clone(&self.slot);
        let runtime = Arc::clone(&self.runtime);
        Ok(Effect::new(move || Self::teardown(&slot, &runtime, lib)))
    }
}

/// 一次激活产出的全部「活的东西」。没激活时是 [`Slot::Idle`]。
///
/// ⚠️ 手写 `Send + Sync`：`PluginHandle` 里有 `*mut c_void` / `*const c_char`
/// （插件自己的不透明数据），编译器不会自动判定它可跨线程。但 ABI 契约要求插件
/// **必须**是线程安全的（回调会从接收循环线程进来，见约束 22 与 `Plugin: Send + Sync`），
/// 所以这个断言与 `define_plugin!` 对插件的那个要求是同一个前提。
unsafe impl Send for Slot {}
unsafe impl Sync for Slot {}

enum Slot {
    Idle,
    Running {
        handle: PluginHandle,
        /// `plugin_stop` 是可选导出（只做元数据登记的插件没有它）。
        stop: Option<PluginStopFn>,
        owner: u64,
    },
    /// 已经断了，动态库留着不让卸载（见 [`DylibPlugin`] 的文档）。
    /// 装着那个 `Library` 只为让它活到进程结束，字段本身不读。
    #[allow(dead_code)]
    Stopped {
        lib: Library,
    },
}

impl DylibPlugin {
    /// 包装一个已经探测过的动态库，但**不启动它**。
    pub fn new(probe: Probe, config: impl Into<String>, runtime: Arc<HostRuntime>) -> Self {
        Self {
            name: probe.name,
            path: probe.path,
            config: config.into(),
            slot: Arc::new(Mutex::new(Slot::Idle)),
            runtime,
            abi_version: probe.abi_version,
            manifest: probe.manifest,
        }
    }

    /// 插件自描述（ABI v4）。`None` = 老插件，没有这个导出。
    pub fn manifest(&self) -> Option<&Manifest> {
        self.manifest.as_ref()
    }

    /// 探测 + 包装一步到位。
    pub fn probe(
        loader: &PluginLoader,
        path: impl AsRef<Path>,
        config: impl Into<String>,
    ) -> Result<Self, LoadError> {
        let probe = loader.probe(path)?;
        Ok(Self::new(probe, config, Arc::clone(loader.runtime())))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn abi_version(&self) -> u32 {
        self.abi_version
    }

    /// 现在是不是活着（`plugin_start` 成功过且没被停）。
    pub fn is_running(&self) -> bool {
        matches!(*self.lock(), Slot::Running { .. })
    }

    fn lock(&self) -> MutexGuard<'_, Slot> {
        self.slot.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 停掉并撤掉它注册的一切，把库留在 [`Slot::Stopped`] 里。
    ///
    /// `lib` 是这次激活打开的动态库。它**不会被 drop**——收进 `Stopped` 让它活到
    /// 进程结束，这就是「软卸载」的全部实现（理由见类型文档）。
    ///
    /// 收成**自由函数**而不是 `&self` 上的方法：回滚闭包要 `'static`，只拿得到
    /// `Arc<Mutex<Slot>>` 与 `Arc<HostRuntime>`，拿不到 `&self`。
    fn teardown(slot: &Arc<Mutex<Slot>>, runtime: &HostRuntime, lib: Library) {
        let mut guard = slot.lock().unwrap_or_else(|e| e.into_inner());
        let previous = std::mem::replace(&mut *guard, Slot::Stopped { lib });
        if let Slot::Running {
            handle,
            stop,
            owner,
        } = previous
        {
            // 先让插件自己收摊，再撤宿主这侧的注册。
            if let Some(stop) = stop {
                // SAFETY: handle 来自本插件的 plugin_init，ABI 版本已校验。
                unsafe { stop(handle) };
            }
            runtime.rollback_owner(owner);
            runtime.unregister_owner_name(owner);
        }
    }
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

/// 依赖声明的取值顺序：manifest 优先、配置兜底。
#[cfg(test)]
mod declared_tests {
    use super::{Dependency, Manifest, declared};

    fn manifest(inject: &[&str], soft: &[&str]) -> Manifest {
        Manifest {
            name: "xq.anime".to_owned(),
            version: "0.1.0".to_owned(),
            inject: inject.iter().map(|s| (*s).to_owned()).collect(),
            soft_inject: soft.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    #[test]
    fn the_manifest_wins_over_the_config_section() {
        // 插件自描述写了依赖，配置里那套（可能是从别的机器抄来的）就不该再起作用。
        let described = manifest(&["storage.mongo"], &["media.ffmpeg"]);
        assert_eq!(
            declared(
                Some(&described),
                Dependency::Hard,
                vec!["旧的.配置".to_owned()]
            ),
            vec!["storage.mongo".to_owned()]
        );
        assert_eq!(
            declared(
                Some(&described),
                Dependency::Soft,
                vec!["旧的.配置".to_owned()]
            ),
            vec!["media.ffmpeg".to_owned()]
        );
    }

    #[test]
    fn an_empty_manifest_falls_back_to_the_config_section() {
        // manifest 里那一侧是空数组 = 插件没声明这类依赖，不能把配置也一起否掉
        // （老插件升级上来时，配置里那份 inject 还得继续生效）。
        let empty = manifest(&[], &[]);
        assert_eq!(
            declared(
                Some(&empty),
                Dependency::Hard,
                vec!["from.config".to_owned()]
            ),
            vec!["from.config".to_owned()]
        );
        assert_eq!(
            declared(
                Some(&empty),
                Dependency::Soft,
                vec!["from.config".to_owned()]
            ),
            vec!["from.config".to_owned()]
        );
    }

    #[test]
    fn a_plugin_without_a_manifest_keeps_reading_the_config_section() {
        // ABI v3 的老插件没有 `plugin_manifest`：行为必须和以前一模一样。
        assert_eq!(
            declared(None, Dependency::Hard, vec!["from.config".to_owned()]),
            vec!["from.config".to_owned()]
        );
        assert_eq!(
            declared(None, Dependency::Soft, vec!["from.config".to_owned()]),
            vec!["from.config".to_owned()]
        );
    }

    #[test]
    fn hard_and_soft_fall_back_independently() {
        // 回落是**逐侧**判定，不是「manifest 里有东西就整个采信」：
        // 插件只声明硬依赖时，软依赖那半边仍然该读配置（它没说过自己不软依赖谁）。
        let only_hard = manifest(&["storage.mongo"], &[]);
        assert_eq!(
            declared(
                Some(&only_hard),
                Dependency::Hard,
                vec!["旧的.inject".to_owned()]
            ),
            vec!["storage.mongo"]
        );
        assert_eq!(
            declared(
                Some(&only_hard),
                Dependency::Soft,
                vec!["配置里的.soft".to_owned()]
            ),
            vec!["配置里的.soft"]
        );

        // 反过来也一样：只声明软依赖时硬依赖读配置。
        let only_soft = manifest(&[], &["media.ffmpeg"]);
        assert_eq!(
            declared(
                Some(&only_soft),
                Dependency::Hard,
                vec!["配置里的.inject".to_owned()]
            ),
            vec!["配置里的.inject"]
        );
        assert_eq!(
            declared(
                Some(&only_soft),
                Dependency::Soft,
                vec!["旧的.soft".to_owned()]
            ),
            vec!["media.ffmpeg"]
        );
    }
}
