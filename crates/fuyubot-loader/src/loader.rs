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

use crate::bundle::{BundleError, BundleManifest, PluginEntry, PluginKind, plugin_entries};
use crate::pipeline::UpdatePipeline;
use crate::runtime::{HostRuntime, PluginHost};

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
    #[error("插件包 {dir} 不可用: {source}")]
    Bundle {
        dir: PathBuf,
        #[source]
        source: BundleError,
    },
    #[error(
        "Go 插件 {path} 没有 plugin.json：它无法卸载，必须先有静态元数据才能安全加载\
         （跑 `fuyubot plugin migrate` 建包，或手工补一个 plugin.json）"
    )]
    GoRequiresManifest { path: PathBuf },
    #[error(
        "插件包 {path} 的 plugin_init 自报名是 {actual}，与清单 id {declared} 不一致：\
         依赖与配置都按清单 id 寻址，请把 plugin.json 的 id 改成 {actual}，或改插件里的名字"
    )]
    BundleNameMismatch {
        path: PathBuf,
        declared: String,
        actual: String,
    },
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
    /// 这个插件那份 `host_ctx`（含 vtable 副本）：插件在任意线程调宿主时都靠它认归属。
    /// `Option` 只为 Go 的 c-shared 库：它不能卸载，插件里残留的 goroutine 可能还会
    /// 调宿主，所以那份上下文要跟着库一起泄漏到进程结束。
    _host: Option<Box<PluginHost>>,
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
        // `plugin_stop` 里的日志 / `set_config` 仍要认得出是哪个插件：加载作用域
        // 早在 `load` 返回时就弹掉了，这里临时把 owner 压回栈顶。
        let _owner = self._runtime.enter_owner(self.owner);
        if let Some(stop) = self.stop.take() {
            // SAFETY: handle 来自本插件的 plugin_init，且 ABI 版本已经校验过。
            unsafe { stop(self.handle) };
        }
        // 插件停掉之后，把它注册的订阅/工具/服务/中间件一次性撤掉。
        self._runtime.rollback_owner(self.owner);
        // owner → 名字的映射也一并摘掉（插件卸载后不能再以它之名写配置）。
        self._runtime.unregister_owner_name(self.owner);
        // 动态库这时才真正卸载；Go 的 c-shared 除外——卸载它会让进程崩（见类型文档）。
        // 上下文也要一起留给它：goroutine 可能还活着，`host_ctx` 不能悬空。
        if self.go_runtime {
            if let Some(lib) = self._lib.take() {
                std::mem::forget(lib);
            }
            if let Some(host) = self._host.take() {
                std::mem::forget(host);
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
        // init 期间也要认得出是谁：manifest 的名字先登记，init 返回后再用 handle 的名字覆盖。
        self.runtime
            .register_owner_name(owner, &declared_name(&lib, path));

        // 每份插件自己的 `host_ctx` + vtable 副本：插件在自己起的线程里调宿主也认得出是谁。
        let host = self.runtime.plugin_host(owner);
        // SAFETY: init 是本库的 plugin_init；vtable 在 Box 里，地址稳定。
        let handle = unsafe { init(host.vtable_ptr()) };
        if !handle.is_valid() {
            self.runtime.unregister_owner_name(owner);
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
            self.runtime.unregister_owner_name(owner);
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
            _host: Some(host),
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
    ///
    /// ⚠️ **探测打开的那一份永远不 `dlclose`**（见 [`ProbeLibrary`]）：探测会真的跑
    /// 插件代码（`plugin_init` / `plugin_stop`），而 Rust std 会为 `thread_local!` 的
    /// 析构注册一个 pthread key；bionic 在 `dlclose` 时不摘那个 key 的 destructor，
    /// 之后线程退出就会跳进已解除映射的地址（2026 设备侧实测）。
    ///
    /// ⚠️ **Go 的 c-shared 会被拒绝**（[`LoadError::GoRequiresManifest`]）：它一旦映射
    /// 就永远不能卸载，所以没有 `plugin.json` 就不许碰。要探测它就显式用
    /// [`Self::probe_for_migration`]。
    pub fn probe(&self, path: impl AsRef<Path>) -> Result<Probe, LoadError> {
        self.probe_impl(path.as_ref(), false)
    }

    /// 只给 `plugin migrate` 用：允许探测 Go 的 c-shared。
    ///
    /// 会 dlopen（进程即将退出），调用方负责提示「已永久映射，仅本次迁移进程」。
    /// 其余语义与 [`Self::probe`] 完全一致。
    pub fn probe_for_migration(&self, path: &Path) -> Result<Probe, LoadError> {
        self.probe_impl(path, true)
    }

    /// [`Self::probe`] 的本体：`allow_go` 决定「没有清单的 Go 库」是拒绝还是照探。
    fn probe_impl(&self, path: &Path, allow_go: bool) -> Result<Probe, LoadError> {
        let location = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());

        // Go 的 c-shared 卸载即崩（运行时限制），所以「没有静态元数据」时连探测都不做：
        // 探测本身就会把它永久映射进本进程。这一步只读文件魔数，**不 dlopen**。
        let go_runtime = looks_like_go_binary(&location);
        if go_runtime && !allow_go {
            return Err(LoadError::GoRequiresManifest {
                path: path.to_owned(),
            });
        }

        // SAFETY: 加载动态库等于执行其中的代码，只加载可信文件。
        let lib = unsafe { fuyubot_tdlib::loader::open_library(&location) }.map_err(|source| {
            LoadError::Library {
                path: path.to_owned(),
                source,
            }
        })?;
        // ⚠️ 探测结束**一律不卸载**（不只是 Go）：这一步已经在这个映射里跑过插件代码
        // （`plugin_init` / `plugin_stop`）。dlclose 一个「跑过代码」的库在 Android 上
        // 不止一种翻车方式：Rust std 的 TLS 析构列表（老系统 / 没解析到
        // `__cxa_thread_atexit_impl` 时走 pthread key 兜底）、库自己
        // `pthread_key_create` 注册的 destructor、`atexit`、残留线程——bionic 都
        // **不会**因为 dlclose 帮你撤销。之后线程退出时就会跳到已解除映射的地址
        // （2026 设备侧实测：`pc == fault addr`，帧在 `pthread_key_clean_all`）。
        // 多留一份映射，换掉一整类「启动 / 收摊后随机崩」。
        let lib = ProbeLibrary { lib: Some(lib) };

        // 探测期内不能让宿主任就把东西注册了：这个 scope **不 commit**，
        // drop 时整批回滚（见 `HostRuntime::plugin_scope`）。
        let scope = self.runtime.plugin_scope();
        let owner = scope.owner();
        // init 期间也要认得出是谁（探测也会跑一遍插件代码，日志同样要带来源）。
        self.runtime
            .register_owner_name(owner, &declared_name(&lib, path));

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

        // 探测也会跑一遍插件代码（日志要带来源），所以同样给它一份自己的上下文。
        let host = self.runtime.plugin_host(owner);
        // SAFETY: init / stop 来自本库；vtable 在 Box 里，地址稳定。
        let handle = unsafe { init(host.vtable_ptr()) };
        if !handle.is_valid() {
            self.runtime.unregister_owner_name(owner);
            return Err(LoadError::InitFailed {
                path: path.to_owned(),
            });
        }

        let name = plugin_name(&handle, path);
        // 用 handle 里的名字覆盖探测前登记的 manifest 名（两者通常一致）。
        self.runtime.register_owner_name(owner, &name);

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
        self.runtime.unregister_owner_name(owner);
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
            // 旧形态：入口就是那个文件本身。
            entry: path.to_owned(),
            kind: PluginKind::Native,
            bundle: None,
            go_runtime,
        })
    }

    /// 探测一个**目录包**（`<dir>/plugin.json`）。**绝不 dlopen。**
    ///
    /// 这条路径上的信息全部来自文件系统：读清单、校验平台、解析 `entry`、读入口文件
    /// 的魔数。所以「这包装不装得上」在插件代码跑起来之前就有答案——这正是分发格式
    /// v1 存在的理由（见 `docs/plugin-format-v1.md`）。
    ///
    /// `abi_version` 取清单里的 `abi`（没写就是 0 = 「装完才知道」）；依赖取
    /// `depends` / `optional_depends` 合成的 [`Manifest`]，与 ABI v4 的自描述同构。
    pub fn probe_bundle(&self, dir: &Path) -> Result<Probe, LoadError> {
        let (manifest, entry) = bundle_parts(dir)?;

        // 契约 §2：目录名与 id 不一致时以 id 为准，但要留一条能查的日志。
        let dir_name = dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        if dir_name != manifest.id {
            self.runtime.warn(&format!(
                "插件包目录名 {dir_name} 与 plugin.json 的 id {} 不一致（以 id 为准）",
                manifest.id
            ));
        }

        // 只读魔数：Go 的 c-shared 在 bundle 里是允许的（它有静态元数据了），
        // 但调用方要知道「这个装了就卸不掉」，好把它排在队列最后。
        let go_runtime = looks_like_go_binary(&entry);
        let probe_manifest = Manifest {
            name: manifest.id.clone(),
            version: manifest.version.clone(),
            inject: manifest.depends.clone(),
            soft_inject: manifest.optional_depends.clone(),
        };

        Ok(Probe {
            path: dir.to_owned(),
            name: manifest.id.clone(),
            abi_version: manifest.abi.unwrap_or(0),
            manifest: Some(probe_manifest),
            entry,
            kind: manifest.kind,
            bundle: Some(manifest),
            go_runtime,
        })
    }

    /// 探测一个扫描结果：目录包走 [`Self::probe_bundle`]，平铺库走 [`Self::probe`]。
    pub fn probe_entry(&self, entry: &PluginEntry) -> Result<Probe, LoadError> {
        match entry {
            PluginEntry::Bundle(dir) => self.probe_bundle(dir),
            PluginEntry::Legacy(path) => self.probe(path),
        }
    }

    /// 扫描目录逐个加载；单个失败不中断其余的。
    ///
    /// 顺序由 [`plugin_entries`] 决定（bundle 按 id 在前，legacy 按文件名在后）——
    /// **没有依赖语义**，依赖排序交给内核的 fiber 状态机。
    pub fn load_dir(
        &self,
        dir: impl AsRef<Path>,
    ) -> std::io::Result<Vec<(PathBuf, Result<LoadedPlugin, LoadError>)>> {
        let mut results = Vec::new();
        for entry in plugin_entries(dir.as_ref())? {
            // 展示路径用包目录 / 平铺文件本身，不是包内的入口文件。
            let shown = match &entry {
                PluginEntry::Bundle(path) | PluginEntry::Legacy(path) => path.clone(),
            };
            let outcome = self.load_entry(&entry);
            results.push((shown, outcome));
        }
        Ok(results)
    }

    /// 扫描目录，为每个候选造一个 [`DylibPlugin`]（**只探测，不启动**）。
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
        for entry in plugin_entries(dir.as_ref())? {
            // 先探一次拿到配置需要的元数据（脚本路径那类东西由调用方按 path 决定）。
            match self.probe_entry(&entry) {
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

    /// 直接加载一个扫描结果（不经过 `Probe`：旧形态照旧只开一次库）。
    fn load_entry(&self, entry: &PluginEntry) -> Result<LoadedPlugin, LoadError> {
        match entry {
            PluginEntry::Legacy(path) => self.load(path),
            // 包要先把 `entry` 解析出来——`load` 只认文件。
            PluginEntry::Bundle(dir) => {
                let (manifest, entry) = bundle_parts(dir)?;
                let loaded = self.load(&entry)?;
                // 与内核那条路径（`DylibPlugin::apply`）同一套身份校验：
                // 装上了但自报名不是清单 id = 没装上（`drop` 会 plugin_stop 并回滚注册）。
                if loaded.name != manifest.id {
                    let actual = loaded.name.clone();
                    drop(loaded);
                    return Err(LoadError::BundleNameMismatch {
                        path: entry,
                        declared: manifest.id,
                        actual,
                    });
                }
                Ok(loaded)
            }
        }
    }
}

/// 探测期间持有的动态库；**任何出口都不卸载**。
///
/// 最早只有 Go 的 c-shared 需要这样（它的运行时线程一旦 dlclose 就崩）；现在这条规则
/// 适用于**所有跑过代码的库**：探测已经执行了 `plugin_init` / `plugin_stop`，库里的
/// TLS 析构、pthread key destructor、`atexit`、残留线程都不受 dlclose 管理，
/// 之后线程退出时可能跳到已解除映射的代码上。多留一份映射，换掉一整类随机崩。
struct ProbeLibrary {
    lib: Option<Library>,
}

impl std::ops::Deref for ProbeLibrary {
    type Target = Library;

    fn deref(&self) -> &Self::Target {
        self.lib.as_ref().expect("探测期间库一直在")
    }
}

impl Drop for ProbeLibrary {
    fn drop(&mut self) {
        if let Some(lib) = self.lib.take() {
            // 与 `LoadedPlugin` 的软卸载、`TdClient` 对 tdjson 的处理同一条纪律：
            // 跑过代码的库不 dlclose（理由见类型文档）。
            std::mem::forget(lib);
        }
    }
}

/// 读清单、校验平台与入口，返回（清单, 入口路径）。
///
/// [`PluginLoader::probe_bundle`] 与 [`PluginLoader::load_entry`] 共用同一条校验链：
/// 免得「探测放行、加载拒绝」这类两边不一致。
fn bundle_parts(dir: &Path) -> Result<(BundleManifest, PathBuf), LoadError> {
    let map = |source: BundleError| LoadError::Bundle {
        dir: dir.to_owned(),
        source,
    };
    let manifest = BundleManifest::load(dir).map_err(map)?;
    manifest.validate(dir).map_err(map)?;
    let entry = manifest.entry_path(dir).map_err(map)?;
    Ok((manifest, entry))
}

/// 一个插件的**探测结果**：它是谁，还不知道要不要它。
///
/// 旧形态的探测只做「把元数据问出来」：`dlopen` → `plugin_init` → 读名字 →
/// 交给 `plugin_stop` 收拾 → 释放。全程**不碰 `plugin_start`**，插件的业务
/// 逻辑一次也没跑过。真正要加载时，正式那条路径会自己再开一遍。
///
/// 目录包（[`Self::bundle`] 非空）的探测**根本不 dlopen**：清单、平台、入口
/// 全从文件系统读出来（见 [`PluginLoader::probe_bundle`]）。
#[derive(Debug, Clone)]
pub struct Probe {
    /// 展示路径：目录包是那个目录，旧形态是那个动态库。
    pub path: PathBuf,
    /// 插件名（bundle = 清单里的 `id`；legacy = `plugin_init` 自己报的名字）。
    pub name: String,
    /// 静态包：清单里的 `abi`，没写就是 0（未知，装完才知道）；legacy：真实值。
    pub abi_version: u32,
    /// 依赖自描述。bundle 用 `depends` / `optional_depends` 合成；
    /// legacy 读 ABI v4 的 `plugin_manifest`（老插件没有这个导出，就是 `None`）。
    pub manifest: Option<Manifest>,
    /// 真正要 `dlopen` 的那个文件（bundle = 包内入口；legacy = 文件自己）。
    pub entry: PathBuf,
    /// 谁来跑这个入口（原生库 / Python / JS）。
    pub kind: PluginKind,
    /// 目录包的原样清单；旧形态是 `None`。
    pub bundle: Option<BundleManifest>,
    /// 入口文件是不是 Go 编出来的 c-shared（**只读魔数**判定的，没加载）。
    /// 是的话它一旦映射就永远不能卸载，调用方应当把它排在加载队列最后。
    pub go_runtime: bool,
}

/// 禁用插件用的后缀：`foo.dll` → `foo.dll.disable`。
///
/// **禁用就是改名**：加载器只认后缀正好是 [`DLL_EXTENSION`] 的文件，`.dll.disable`
/// 的扩展名是 `disable`，于是连候选都进不了。没有第二份「禁用清单」要跟插件目录
/// 同步，卸掉壳重命名回来就恢复——面板的开关（`Space`）关到底就是这一次改名。
///
/// 加在**最后**而不是替换扩展名：`foo.disable` 会让人以为插件名变了，
/// `foo.dll.disable` 一眼能看出「本来是哪个文件被停用了」。
pub const DISABLED_SUFFIX: &str = ".disable";

/// 这个路径上的插件被禁用过没有（`foo.dll.disable` → `true`）。
pub fn is_disabled(path: &Path) -> bool {
    path.file_name()
        .map(|name| name.to_string_lossy().ends_with(DISABLED_SUFFIX))
        .unwrap_or(false)
}

/// 启用状态下的路径：`foo.dll.disable` → `foo.dll`（本来就没禁用就原样返回）。
pub fn enabled_path(path: &Path) -> PathBuf {
    let Some(name) = path.file_name().map(|name| name.to_string_lossy()) else {
        return path.to_path_buf();
    };
    match name.strip_suffix(DISABLED_SUFFIX) {
        Some(stem) => path.with_file_name(stem),
        None => path.to_path_buf(),
    }
}

/// 禁用状态下的路径：`foo.dll` → `foo.dll.disable`（已经禁用就原样返回）。
pub fn disabled_path(path: &Path) -> PathBuf {
    if is_disabled(path) {
        return path.to_path_buf();
    }
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(DISABLED_SUFFIX);
    path.with_file_name(name)
}

/// 目录下的候选插件文件（按扩展名筛），已排序。
///
/// 被禁用的（[`DISABLED_SUFFIX`]）**不在**结果里：扩展名已经不是
/// [`DLL_EXTENSION`] 了，这一层判断天然把它们滤掉，不需要额外的豁免清单。
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

/// 插件自报的名字：`plugin_init` 给的静态字符串（插件自己持有，框架不释放）。
/// 没给（NULL / 空串）就是 `None`。
fn declared_plugin_name(handle: &PluginHandle) -> Option<String> {
    if handle.name.is_null() {
        return None;
    }
    // SAFETY: ABI 契约要求 name 是插件持有的 NUL 结尾字符串，库还活着。
    let name = unsafe { CStr::from_ptr(handle.name) }.to_string_lossy();
    if name.is_empty() {
        None
    } else {
        Some(name.into_owned())
    }
}

/// 插件名：优先用 `plugin_init` 给的那个，没有就退回文件 stem（诊断用）。
fn plugin_name(handle: &PluginHandle, path: &Path) -> String {
    declared_plugin_name(handle).unwrap_or_else(|| {
        path.file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_else(|| "unknown".to_owned())
    })
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

/// init 之前能确定的名字：`plugin_manifest` 优先，没有就用文件 stem。
///
/// 为什么要提前拿：`plugin_init` / `plugin_stop` 里插件也可能打日志，而日志来源
/// 靠 owner → 名字的映射；等 init 返回再登记的话，启动期的日志只能标成 `plugin`，
/// `--log-plugin` 也就过滤不到它们。
fn declared_name(lib: &Library, path: &Path) -> String {
    // SAFETY: 只调用插件的 `plugin_manifest` 导出，契约与 `probe` 里那次相同。
    unsafe { read_manifest(lib) }
        .map(|manifest| manifest.name)
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| {
            path.file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or("plugin")
                .to_owned()
        })
}

/// 从文件里把库打开、init、start 全走一遍，成功后交回活着的那些东西。
///
/// 和 [`PluginLoader::load_with_config`] 的分工：那边「加载完就把生命周期交出去」，
/// 返回一个自己管收摊的 [`LoadedPlugin`]；这里把四件东西交回来，由
/// [`DylibPlugin`] 决定什么时候、以什么顺序收。
///
/// `expected_bundle` 只对目录包有值：契约要求**插件自报的名字必须与清单 id 一致**，
/// 不一致就拒绝——否则依赖是按 id 声明的、内核按 id 寻址，装上的却叫另一个名字。
/// 版本不一致只告警（`plugin_manifest` 的 version 来自插件的 `CARGO_PKG_VERSION`，
/// 作者可能故意让它和发行包版本不同）。
fn start_library(
    runtime: &Arc<HostRuntime>,
    path: &Path,
    config: &str,
    expected_bundle: Option<&BundleManifest>,
) -> Result<
    (
        PluginHandle,
        Option<PluginStopFn>,
        Library,
        u64,
        Box<PluginHost>,
    ),
    LoadError,
> {
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

    let (init, start, stop, free_handle) = {
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
        let free_handle = unsafe { lib.get::<PluginFreeHandleFn>(SYM_PLUGIN_FREE_HANDLE) }
            .ok()
            .map(|symbol| *symbol);
        (init, start, stop, free_handle)
    };

    // 从这里开始，任何提前返回都会让 `scope` 在 drop 时把这批注册整批回滚。
    let scope = runtime.plugin_scope();
    let owner = scope.owner();
    // init 期间也要认得出是谁（日志来源 / `set_config`）。
    runtime.register_owner_name(owner, &declared_name(&lib, path));

    // 每份插件自己的 `host_ctx` + vtable 副本：插件在自己起的线程里调宿主也认得出是谁。
    let host = runtime.plugin_host(owner);
    // SAFETY: init 是本库的 plugin_init；vtable 在 Box 里，地址稳定。
    let handle = unsafe { init(host.vtable_ptr()) };
    if !handle.is_valid() {
        runtime.unregister_owner_name(owner);
        return Err(LoadError::InitFailed {
            path: path.to_owned(),
        });
    }

    if handle.abi_version != ABI_VERSION {
        // 版本不兼容时**不能**调 plugin_stop：那是对一个我们不认识的 ABI 下手。
        runtime.unregister_owner_name(owner);
        return Err(LoadError::AbiMismatch {
            path: path.to_owned(),
            plugin: handle.abi_version,
            host: ABI_VERSION,
        });
    }

    // 名字必须在 start 之前登记：插件在 `start` 里就可能调 `set_config`，
    // 宿主靠 owner → 名字的映射判定「这是谁的段」。
    let name = plugin_name(&handle, path);

    // 目录包的身份校验（AGENTS 约束 59）：自报名非空且不等于清单 id → 硬失败。
    // 还没 `plugin_start`，所以只走 v4 的 `plugin_free_handle` 归还 init 里分配的状态。
    if let Some(bundle) = expected_bundle {
        if let Some(reported) = declared_plugin_name(&handle) {
            if reported != bundle.id {
                if let Some(free) = free_handle {
                    // SAFETY: handle 来自同一轮 init；该符号只读 handle 里的 context。
                    unsafe { free(handle) };
                }
                runtime.unregister_owner_name(owner);
                return Err(LoadError::BundleNameMismatch {
                    path: path.to_owned(),
                    declared: bundle.id.clone(),
                    actual: reported,
                });
            }
        }
        // 版本只告警：发行包版本与插件 crate 版本本来就可以不同。
        // SAFETY: lib 还活着，契约同 `read_manifest`。
        if let Some(described) = unsafe { read_manifest(&lib) } {
            if !described.version.is_empty() && described.version != bundle.version {
                runtime.warn(&format!(
                    "插件包 {} 的清单版本 {} 与 plugin_manifest 的 {} 不一致（以清单为准）",
                    bundle.id, bundle.version, described.version
                ));
            }
        }
    }
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
    Ok((handle, stop, lib, owner, host))
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
/// ⚠️ **卸载是软的（不 `dlclose`）**：`Effect` 跑完只停插件、撤注册，动态库与那份
/// `host_ctx` **直接 `forget`**，进程退出才由系统回收。原因是安全边界——事件总线的
/// 订阅快照持着指向插件代码的回调 `Arc`，插件自己也可能留下没 join 干净的线程，
/// `dlclose` 之后它们再跑一步就是访问违例（Android 上表现为「stop 之后整个 App 崩」）。
/// Cordis 那边靠 GC 托管根本没有这个问题；我们只能选择不卸载。
/// 代价是每个卸过的插件留几 MB 映射，换掉的是一整类崩溃。
/// （Go 的 c-shared 本来就不能卸载，见约束 50——现在两种插件口径一致了。）
pub struct DylibPlugin {
    name: String,
    /// 展示路径：目录包是那个目录，旧形态是那个动态库。
    path: PathBuf,
    /// 真正 `dlopen` 的文件。目录包是包内入口，旧形态与 `path` 相同。
    entry: PathBuf,
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
    /// 目录包可能是 0——清单没写 `abi`，装完才知道。
    abi_version: u32,
    /// ABI v4 的自描述：依赖与版本以插件自己说的为准。
    manifest: Option<Manifest>,
    /// 谁来跑这个入口（原生库 / Python / JS）。
    kind: PluginKind,
    /// 目录包的原样清单（旧形态是 `None`）。
    bundle: Option<BundleManifest>,
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
        // ⚠️ 用 `entry` 而不是 `path`：目录包要打开的是包内那个动态库。
        // 目录包还要求自报名等于清单 id（约束 59），旧形态没有这条约束。
        let expected_bundle = self.bundle.as_ref();
        let started = start_library(&self.runtime, &self.entry, &self.config, expected_bundle);
        let (handle, stop, lib, owner, host) = started.map_err(|error| {
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
            _host: host,
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
        /// 这个插件那份 `host_ctx`（含 vtable 副本）；`plugin_stop` 期间还活着。
        _host: Box<PluginHost>,
    },
}

impl DylibPlugin {
    /// 包装一个已经探测过的插件，但**不启动它**。
    pub fn new(probe: Probe, config: impl Into<String>, runtime: Arc<HostRuntime>) -> Self {
        Self {
            name: probe.name,
            path: probe.path,
            entry: probe.entry,
            config: config.into(),
            slot: Arc::new(Mutex::new(Slot::Idle)),
            runtime,
            abi_version: probe.abi_version,
            manifest: probe.manifest,
            kind: probe.kind,
            bundle: probe.bundle,
        }
    }

    /// 插件自描述（ABI v4）。`None` = 老插件，没有这个导出。
    pub fn manifest(&self) -> Option<&Manifest> {
        self.manifest.as_ref()
    }

    /// 目录包的原样清单；旧形态（平铺动态库）是 `None`。
    pub fn bundle(&self) -> Option<&BundleManifest> {
        self.bundle.as_ref()
    }

    /// 谁来跑这个入口（原生库 / Python / JS）。
    pub fn kind(&self) -> PluginKind {
        self.kind
    }

    /// 探测 + 包装一步到位。
    ///
    /// 传目录就是目录包（读 `plugin.json`，**不 dlopen**），传文件就是旧形态的动态库。
    pub fn probe(
        loader: &PluginLoader,
        path: impl AsRef<Path>,
        config: impl Into<String>,
    ) -> Result<Self, LoadError> {
        let path = path.as_ref();
        let probe = if path.is_dir() {
            loader.probe_bundle(path)?
        } else {
            loader.probe(path)?
        };
        Ok(Self::new(probe, config, Arc::clone(loader.runtime())))
    }

    /// 展示路径：目录包是那个目录，旧形态是那个动态库。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 真正 `dlopen` 的文件（目录包是包内入口）。
    pub fn entry(&self) -> &Path {
        &self.entry
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

    /// 停掉并撤掉它注册的一切；**动态库与 `host_ctx` 都留到进程结束**。
    ///
    /// `lib` 是这次激活打开的动态库。它**不会被 drop**——直接 `forget`，这就是
    /// 「软卸载」的全部实现（理由见类型文档）：收摊之后事件总线里可能还有在途的广播
    /// 快照，插件自己也可能留下了没 join 干净的线程；`dlclose` 之后它们再跑一步就是
    /// SIGSEGV（Android 上表现为「stop 之后整个 App 崩」）。`host_ctx` 同理——残留线程
    /// 还可能调宿主，指针不能悬空。
    ///
    /// 收成**自由函数**而不是 `&self` 上的方法：回滚闭包要 `'static`，只拿得到
    /// `Arc<Mutex<Slot>>` 与 `Arc<HostRuntime>`，拿不到 `&self`。
    fn teardown(slot: &Arc<Mutex<Slot>>, runtime: &HostRuntime, lib: Library) {
        let mut guard = slot.lock().unwrap_or_else(|e| e.into_inner());
        // 先取出运行中的那套，收摊后再决定「库去哪」。
        let previous = std::mem::replace(&mut *guard, Slot::Idle);
        match previous {
            Slot::Running {
                handle,
                stop,
                owner,
                _host: host,
            } => {
                // `plugin_stop` 里的日志 / `set_config` 仍要认得出是哪个插件。
                let _owner = runtime.enter_owner(owner);
                // 先让插件自己收摊，再撤宿主这侧的注册。
                if let Some(stop) = stop {
                    // SAFETY: handle 来自本插件的 plugin_init，ABI 版本已校验。
                    unsafe { stop(handle) };
                }
                runtime.rollback_owner(owner);
                runtime.unregister_owner_name(owner);
                // ⚠️ 故意不卸载：见函数文档与 AGENTS 约束 50（Go c-shared 同一条理由）。
                std::mem::forget(lib);
                std::mem::forget(host);
            }
            // 理论上到不了：teardown 只由 Running 状态的 Effect 调。真到了也别把库丢了。
            other => {
                *guard = other;
                std::mem::forget(lib);
            }
        }
    }
}

#[cfg(test)]
mod disabled_tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-tmp/fuyubot-loader")
            .join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn renaming_is_the_only_bookkeeping() {
        let dll = Path::new("plugins").join(format!("foo.{DLL_EXTENSION}"));
        let off = disabled_path(&dll);
        assert_eq!(
            off.file_name().unwrap().to_string_lossy(),
            format!("foo.{DLL_EXTENSION}.disable"),
            "后缀加在最后，扩展名不变形"
        );
        assert!(is_disabled(&off));
        assert!(!is_disabled(&dll));
        assert_eq!(enabled_path(&off), dll);
        // 幂等：已经是那个方向就原样返回，别叠出 `.disable.disable`。
        assert_eq!(disabled_path(&off), off);
        assert_eq!(enabled_path(&dll), dll);
    }

    #[test]
    fn a_disabled_plugin_drops_out_of_the_candidate_list() {
        let dir = temp("disabled-scan");
        let live = dir.join(format!("live.{DLL_EXTENSION}"));
        let off = dir.join(format!("off.{DLL_EXTENSION}.disable"));
        std::fs::write(&live, b"x").unwrap();
        std::fs::write(&off, b"x").unwrap();

        let found = plugin_files(&dir).unwrap();
        assert_eq!(found, vec![live], "禁用的不进候选表：{found:?}");
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

/// 目录包（`plugin.json`）的探测：静态、不加载、Go 的两种口径。
#[cfg(test)]
mod bundle_probe_tests {
    use super::*;
    use crate::bundle::BUNDLE_MANIFEST;
    // `DylibPlugin::name()` 是内核插件 trait 的方法，调用点在测试里要显式带进来。
    use fuyubot_core::Plugin as _;

    /// 测试临时目录：仓库约定放 `target/test-tmp`（不用系统 TEMP）。
    fn temp(name: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-tmp/fuyubot-loader")
            .join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn loader() -> PluginLoader {
        PluginLoader::new(Context::new()).expect("宿主运行时该装得起来")
    }

    /// 落一个完整的包。入口写**纯文本**：真去 dlopen 一定失败，探测成功即证明没加载。
    fn package(root: &Path, dir_name: &str, id: &str) -> PathBuf {
        let dir = root.join(dir_name);
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::write(dir.join("bin").join("plugin.dll"), b"not a real library").unwrap();
        let manifest = serde_json::json!({
            "schema": 1,
            "id": id,
            "name": "Weather",
            "version": "1.2.0",
            "kind": "native",
            "entry": "bin/plugin.dll",
            "abi": 5,
            "depends": ["fuyubot.help.commands"],
            "optional_depends": ["media.ffmpeg"],
        });
        std::fs::write(dir.join(BUNDLE_MANIFEST), manifest.to_string()).unwrap();
        dir
    }

    /// **静态探测不加载**：入口是纯文本，`probe_bundle` 仍然成功。
    #[test]
    fn a_bundle_probe_never_loads_the_entry() {
        let root = temp("probe-bundle-static");
        let dir = package(&root, "com.example.weather", "com.example.weather");

        let probe = loader().probe_bundle(&dir).expect("静态探测不该失败");
        assert_eq!(probe.name, "com.example.weather");
        assert_eq!(probe.path, dir);
        assert_eq!(probe.entry, dir.join("bin").join("plugin.dll"));
        assert_eq!(probe.kind, PluginKind::Native);
        assert_eq!(probe.abi_version, 5, "清单写了 abi=5");
        assert!(!probe.go_runtime);
        let bundle = probe.bundle.as_ref().expect("原样清单要带上");
        assert_eq!(bundle.version, "1.2.0");
        let manifest = probe.manifest.as_ref().expect("bundle 一定有依赖自描述");
        assert_eq!(manifest.name, "com.example.weather");
        assert_eq!(manifest.version, "1.2.0");
        assert_eq!(manifest.inject, ["fuyubot.help.commands"]);
        assert_eq!(manifest.soft_inject, ["media.ffmpeg"]);
    }

    /// 适配器存的是「展示路径 = 包目录 / 入口 = 包内文件」这两条路径。
    #[test]
    fn the_adapter_keeps_the_display_path_and_the_real_entry_apart() {
        let root = temp("adapter-entry");
        let dir = package(&root, "com.example.weather", "com.example.weather");

        let adapter = DylibPlugin::probe(&loader(), &dir, "").expect("目录该走 probe_bundle");
        assert_eq!(adapter.path(), dir);
        assert_eq!(adapter.entry(), dir.join("bin").join("plugin.dll"));
        assert_eq!(adapter.kind(), PluginKind::Native);
        assert!(adapter.bundle().is_some());
        assert_eq!(adapter.abi_version(), 5);
    }

    /// 清单坏掉时错误要指到包上，而不是「打不开动态库」。
    #[test]
    fn a_broken_manifest_surfaces_as_a_bundle_error() {
        let root = temp("probe-bundle-broken");
        let dir = package(&root, "com.example.weather", "com.example.weather");
        std::fs::remove_file(dir.join("bin").join("plugin.dll")).unwrap();

        match loader().probe_bundle(&dir) {
            Err(LoadError::Bundle {
                dir: reported,
                source,
            }) => {
                assert_eq!(reported, dir);
                assert!(
                    matches!(source, BundleError::EntryMissing(_)),
                    "入口没了该报 EntryMissing，实际 {source:?}"
                );
            }
            other => panic!("坏包该报 Bundle，实际 {other:?}"),
        }

        // 平台不匹配同样走 Bundle（先判平台，不拿「入口不存在」误导人）。
        let manifest = serde_json::json!({
            "schema": 1,
            "id": "com.example.weather",
            "name": "Weather",
            "version": "1.2.0",
            "kind": "native",
            "entry": "bin/plugin.dll",
            "platforms": ["plan9-mips"],
        });
        std::fs::write(dir.join(BUNDLE_MANIFEST), manifest.to_string()).unwrap();
        assert!(matches!(
            loader().probe_bundle(&dir),
            Err(LoadError::Bundle {
                source: BundleError::UnsupportedPlatform { .. },
                ..
            })
        ));
    }

    /// Go 的平铺库**没有清单就拒绝**，而且拒绝发生在 dlopen 之前（不崩、不映射）。
    #[test]
    fn a_go_library_without_a_manifest_is_refused() {
        let dir = temp("go-legacy");
        let dll = dir.join(format!("goplugin.{DLL_EXTENSION}"));
        let mut bytes = vec![0_u8; 4096];
        bytes.extend_from_slice(b"\xff Go buildinf:");
        bytes.extend_from_slice(&[0_u8; 64]);
        std::fs::write(&dll, &bytes).unwrap();

        let loader = loader();
        match loader.probe(&dll) {
            Err(LoadError::GoRequiresManifest { path }) => assert_eq!(path, dll),
            other => panic!("Go 平铺库该被拒绝，实际 {other:?}"),
        }

        // 迁移入口允许探测 Go：它绕过了「没清单」那道闸，直接走到 dlopen，
        // 所以这里拿到的是「打不开」（假文件），而不是「没清单」。
        assert!(
            matches!(
                loader.probe_for_migration(&dll),
                Err(LoadError::Library { .. })
            ),
            "probe_for_migration 该放行到 dlopen"
        );
    }

    /// `scan_dir` 走 `plugin_entries`：bundle 在前（按 id）、legacy 在后，
    /// 单个失败不中断其余的。
    #[test]
    fn scan_dir_walks_entries_in_contract_order() {
        let root = temp("scan-entries");
        package(&root, "com.example.weather", "com.example.weather");
        package(&root, "some-dir-name", "com.example.alpha");
        // 假平铺库：探测会在 dlopen 处失败，但它必须出现在结果里（不能悄悄消失）。
        let legacy = root.join(format!("legacy.{DLL_EXTENSION}"));
        std::fs::write(&legacy, b"not a real library").unwrap();

        let adapters = loader()
            .scan_dir(&root, |probe| format!("{{\"name\":\"{}\"}}", probe.name))
            .expect("扫目录不该失败");
        assert_eq!(adapters.len(), 3, "两个包 + 一个平铺库");

        let names: Vec<&str> = adapters
            .iter()
            .filter_map(|adapter| adapter.as_ref().ok())
            .map(|adapter| adapter.name())
            .collect();
        assert_eq!(
            names,
            ["com.example.alpha", "com.example.weather"],
            "bundle 按 id 字典序"
        );
        assert!(
            matches!(adapters[2], Err(LoadError::Library { .. })),
            "平铺库的失败要留住原因"
        );
    }

    /// `load_dir` 对包要打开**包内入口**：入口是纯文本时，错误来自 dlopen 而不是
    /// 「路径是个目录」。
    #[test]
    fn load_dir_resolves_the_bundle_entry() {
        let root = temp("load-dir-bundle");
        let dir = package(&root, "com.example.weather", "com.example.weather");

        let results = loader().load_dir(&root).expect("扫目录不该失败");
        assert_eq!(results.len(), 1);
        let (shown, outcome) = &results[0];
        assert_eq!(shown, &dir, "展示路径是包目录");
        match outcome {
            Err(LoadError::Library { path, .. }) => {
                assert_eq!(path, &dir.join("bin").join("plugin.dll"), "真去开的是入口")
            }
            other => panic!("纯文本入口该在 dlopen 处失败，实际 {other:?}"),
        }
    }

    /// 编一个真 cdylib 出来（demo 插件的 `plugin_init` 自报名是 `fuyubot.demo`）。
    ///
    /// 和 `tests/plugins.rs` 的做法一致：测试自己建 fixture，不依赖「先跑过 cargo build」。
    fn build_demo_fixture() -> PathBuf {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        // MSVC 的 link.exe 要在 TEMP 里建临时文件，受限环境里指向系统 TEMP 会编不出来。
        let cargo_temp = root.join("target/test-tmp/cargo-temp");
        std::fs::create_dir_all(&cargo_temp).unwrap();
        let status = std::process::Command::new(env!("CARGO"))
            .args(["build", "--offline", "-p", "fuyubot-plugin-demo"])
            .current_dir(&root)
            .env("TEMP", &cargo_temp)
            .env("TMP", &cargo_temp)
            .status()
            .expect("启动 cargo 失败");
        assert!(status.success(), "构建 demo fixture 失败");

        let file = if cfg!(target_os = "windows") {
            "fuyubot_plugin_demo.dll".to_owned()
        } else if cfg!(target_os = "macos") {
            "libfuyubot_plugin_demo.dylib".to_owned()
        } else {
            "libfuyubot_plugin_demo.so".to_owned()
        };
        let path = root.join("target").join("debug").join(file);
        assert!(path.is_file(), "没找到产物 {}", path.display());
        path
    }

    /// 目录包的自报名必须等于清单 id（AGENTS 约束 59）：不一致就拒绝加载，并说清改哪边。
    #[test]
    fn a_bundle_rejects_a_plugin_whose_name_differs_from_the_manifest_id() {
        let demo = build_demo_fixture();
        let root = temp("bundle-name-mismatch");
        let dir = root.join("com.example.wrong");
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::copy(&demo, dir.join("bin").join("demo.dll")).unwrap();
        let manifest = serde_json::json!({
            "schema": 1,
            "id": "com.example.wrong",
            "name": "Wrong",
            "version": "1.0.0",
            "kind": "native",
            "entry": "bin/demo.dll",
        });
        std::fs::write(dir.join(BUNDLE_MANIFEST), manifest.to_string()).unwrap();

        let loader = loader();
        let adapter = DylibPlugin::probe(&loader, &dir, "").expect("静态探测照旧成功");
        match adapter.apply(&Context::new()) {
            Err(fuyubot_core::KernelError::PluginApplyFailed { reason, .. }) => {
                assert!(
                    reason.contains("com.example.wrong") && reason.contains("fuyubot.demo"),
                    "错误要说清两边各是什么：{reason}"
                );
            }
            Ok(_) => panic!("自报名与清单 id 不一致时不该加载成功"),
            Err(other) => panic!("该报「名字不一致」，实际 {other}"),
        }

        // 便捷 API（`load_dir`）也必须拦下：装上了但名字不对 = 没装上。
        let results = loader.load_dir(&root).expect("扫目录不该失败");
        assert_eq!(results.len(), 1);
        assert!(
            matches!(results[0].1, Err(LoadError::BundleNameMismatch { .. })),
            "load_dir 也要做同一套身份校验：{:?}",
            results[0].1
        );
    }

    /// 迁移探测必须把 Go 的库**留到进程结束**：`ProbeLibrary` 的 guard 管所有出口，
    /// 不只是成功那条（失败分支同样会 drop 局部 `Library`）。
    ///
    /// 判定手法：Windows 上**已映射**的 DLL 删不掉（共享冲突），删得掉就说明它被 dlclose 了
    /// ——对 Go 的 c-shared 那正是会让进程崩的那一步。
    #[test]
    fn a_migration_probe_keeps_a_go_library_mapped() {
        // 把一个真 cdylib 伪装成 Go 的 c-shared：Go 的构建信息魔数在 PE 尾部不影响加载。
        let demo = build_demo_fixture();
        let root = temp("go-keep-mapped");
        let path = root.join(format!("go-like.{DLL_EXTENSION}"));
        let mut bytes = std::fs::read(&demo).unwrap();
        bytes.extend_from_slice(b"\xff Go buildinf:");
        bytes.extend_from_slice(&[0_u8; 32]);
        std::fs::write(&path, &bytes).unwrap();

        let loader = loader();
        // ① 普通探测：读到魔数就拒，连 dlopen 都不做。
        assert!(matches!(
            loader.probe(&path),
            Err(LoadError::GoRequiresManifest { .. })
        ));

        // ② 迁移探测：允许加载，并且不许在返回时卸载它。
        let probe = loader.probe_for_migration(&path).expect("迁移探测该成功");
        assert!(probe.go_runtime, "魔数判定为 Go");
        assert_eq!(probe.name, "fuyubot.demo");

        #[cfg(windows)]
        {
            // 对照：同一目录里没被加载的文件删得掉——所以下面的失败只能是「还在映射」。
            let control = root.join("control.dll");
            std::fs::write(&control, b"x").unwrap();
            assert!(
                std::fs::remove_file(&control).is_ok(),
                "对照组：没被加载的文件该删得掉"
            );
            assert!(
                std::fs::remove_file(&path).is_err(),
                "迁移探测之后 Go 的库必须还映射着（删得掉 = 已经 dlclose 了）"
            );
        }
    }
}
