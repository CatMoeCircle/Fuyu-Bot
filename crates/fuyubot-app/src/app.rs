//! 编排：把「内核 → 宿主运行时 → 接 TDLib → 装插件 → 登录 → 分发 → 收摊」写在一处。
//!
//! 这一层只做**接线**，不定义规则：配置 schema、默认值、登录语义、收摊步骤全在下层
//! （`loader` / `tdlib`）。两个宿主（CLI、Android JNI）的差别只有 [`AppOptions`] 怎么造、
//! [`AppHost`] 怎么实现。
//!
//! ## 顺序就是语义（别改）
//!
//! ```text
//! ① 指 tdjson 路径（可选）        ⑤ 装插件（先于登录：登录窗口里的更新不能漏）
//! ② 内核 Kernel::start            ⑥ 登录（阻塞；期间没有接收循环）
//! ③ 宿主运行时 + 管线              ⑦ 开分发 spawn_dispatch
//! ④ 配置 / 日志门槛 / 生命周期桥
//! ```
//!
//! ⑤ 必须在 ⑥ 之前：插件要在 TDLib 开始产出之前订阅好；登录窗口里的更新靠
//! `Session::login` 的 `bypass` 转给管线，不能吞。⑥ 期间也不能有接收循环：
//! `Session::login` 自己 `receive`，并发 `td_receive` 会串号。
//!
//! ## 不需要 TDLib 的场合
//!
//! `tdlib: None`（= CLI 的 `--no-tdlib` / `--replay`）：只起内核、宿主与插件，
//! [`App::dispatch`] 返回 `None`，[`App::login`] 直接报 [`AppError::NoSession`]。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use fuyubot_core::{Effect, Kernel, KernelError, Plugin};
use fuyubot_loader::{
    DylibPlugin, HostRuntime, LoadError, LogLevel, PluginEntry, PluginLoader, UpdatePipeline,
    find_script, plugin_entries,
};
use fuyubot_tdlib::{AccountType, ReceiveLoop, Session, SessionError, TdClient, TdlibParameters};
use serde_json::Value;

use crate::host::{AppHost, AppState};
use crate::live::{LiveView, PluginInfo, PluginLoadReport};
use crate::prompt::HostPrompter;
use crate::tdstate::{TdState, TdStateWatch};

/// 编排这一路可能出的错。
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("内核：{0}")]
    Kernel(#[from] KernelError),
    #[error("宿主运行时：{0}")]
    Loader(#[from] LoadError),
    #[error("TDLib：{0}")]
    Session(#[from] SessionError),
    #[error("TDLib 客户端：{0}")]
    Tdlib(#[from] fuyubot_tdlib::TdlibError),
    #[error("TDLib 库没就绪：{0}")]
    TdlibUnavailable(String),
    #[error("宿主已经挂过 TDLib 客户端")]
    AlreadyAttached,
    #[error("这次启动没有接 TDLib（`--no-tdlib` / 回放），不能登录")]
    NoSession,
    #[error("插件目录 {0}：{1}")]
    PluginDir(PathBuf, std::io::Error),
}

/// 一次 `boot` 需要的全部输入。
pub struct AppOptions {
    /// TDLib 的数据库 / 文件目录（`fuyubot.toml` 的 `[telegram].data_dir`）。
    pub data_dir: PathBuf,
    /// 插件目录。
    pub plugin_dir: PathBuf,
    /// 插件日志门槛。
    pub log_level: LogLevel,
    /// 只看这些插件的日志（空 = 全收）。
    pub log_plugins: Vec<String>,
    /// 装进宿主运行时的配置树（插件经 `get_config` 读它）。
    ///
    /// ⚠️ **别放凭据**：`api_hash` / `bot_token` 不该出现在这里（约束 41 的精神）。
    pub config: Value,
    /// 用户配置文件（启动时读；插件配置落盘到它旁边的 `plugin-config/`，不回写它）。
    /// `None` = 这次启动没有配置根，**`set_config` 会报错**
    /// （不再静默写内存：插件会以为已经保存，重启后又变回旧值）。
    pub config_path: Option<PathBuf>,
    /// `TDLIB_TDJSON_PATH` 的取值（在起客户端之前写一次）；`None` = 交给 loader 的优先级。
    pub tdjson_path: Option<PathBuf>,
    /// 接 TDLib 的设置；`None` = 本次不接（`--no-tdlib` / `--replay`）。
    pub tdlib: Option<TdlibSettings>,
    /// 交互出口。
    pub host: Arc<dyn AppHost>,
}

/// TDLib 那一半的输入。
pub struct TdlibSettings {
    pub parameters: TdlibParameters,
    pub account: AccountType,
    /// 宿主**已经给过**的 bot token：登录时这一类问题不问 UI。
    pub bot_token: Option<String>,
    /// TDLib 自己的日志详细度；`None` = 默认（1）。
    pub log_verbosity: Option<i32>,
    /// 登录总预算。
    pub login_budget: Duration,
    /// `false` = 只接客户端、不走登录（CLI 的 `--no-login`）。
    pub login: bool,
}

/// 一个跑起来的宿主：内核 + 宿主运行时 + 管线 +（可选）TDLib 会话 + 已装插件。
pub struct App {
    kernel: Kernel,
    loader: PluginLoader,
    pipeline: Arc<UpdatePipeline>,
    session: Option<Session>,
    plugins: Vec<Arc<DylibPlugin>>,
    /// 这次扫描的结果（扫到几个 / 哪些没进内核）。`live_view` 与上报共用同一份。
    load: PluginLoadReport,
    /// 生命周期桥的守卫：drop 就退订（约束 11 同一条道理）。
    _lifecycle: Effect,
    /// TDLib 的授权 / 连接状态（登录期走 `Prompter::auth_state`，运行期走事件总线）。
    td_state: TdState,
    /// 两条状态订阅的守卫：drop 就退订。
    _td_state_watch: TdStateWatch,
    host: Arc<dyn AppHost>,
    settings: Option<TdlibSettings>,
    plugin_dir: PathBuf,
}

impl App {
    /// 标准起法：`boot` + 装插件 +（该登录时）登录。两个宿主都用它。
    pub async fn start(options: AppOptions) -> Result<Self, AppError> {
        let mut app = Self::boot(options).await?;
        app.load_plugins()?;
        if app.should_login() {
            app.login()?;
        }
        Ok(app)
    }

    /// ①～④ + 建 TDLib 客户端（**不登录**）：`--no-login` / 面板登录这类场合用。
    pub async fn boot(options: AppOptions) -> Result<Self, AppError> {
        let AppOptions {
            data_dir: _,
            plugin_dir,
            log_level,
            log_plugins,
            config,
            config_path,
            tdjson_path,
            tdlib,
            host,
        } = options;

        // ① `TdClient::new()` 只认进程环境（loader 的优先级 1），没有注入点。
        // 在**起客户端之前**写一次、之后不再改：这是唯一能让它生效的杠杆。
        //
        // SAFETY: edition 2024 里 `set_var` 是 unsafe——确实可能与别的线程的 `getenv` 竞争。
        // 只写一次自有变量、bionic 的 `setenv` 自带锁，风险可接受（要彻底去掉得给 loader
        // 加一个注入点，那要改现有代码）。
        if let Some(dir) = &tdjson_path {
            unsafe { std::env::set_var("TDLIB_TDJSON_PATH", dir) };
        }

        // 提前探一次库：错在这里报，比在 `Session::create` 里报清楚；命中时也把「从哪找到的」留下来
        // （探不到 `TDLIB_TDJSON_PATH` / exe 同级 / 系统库时，用户就靠这一行排查）。
        if tdlib.is_some() {
            let found = fuyubot_tdlib::resolve()
                .map_err(|error| AppError::TdlibUnavailable(error.to_string()))?;
            tracing::info!(
                target: "fuyubot::app",
                "TDLib {} 命中「{}」：{}",
                fuyubot_tdlib::TDLIB_VERSION,
                found.source,
                found.target.display()
            );
            for note in &found.notes {
                tracing::info!(target: "fuyubot::app", "  {note}");
            }
        }

        // ② 内核
        let mut kernel = Kernel::new();
        kernel.start().await?;

        // ③ 宿主运行时 + 管线
        let loader = PluginLoader::new(kernel.context().clone())?;
        let pipeline = Arc::new(loader.pipeline());

        // ③′ 授权 / 连接状态：**必须早于登录**——连接状态只在变化时来一条，
        // 等登录完再订阅就漏掉 `connectionStateReady`（与 CLI 同一条理由）。
        let td_state = TdState::new();
        let td_state_watch = td_state.watch(loader.runtime());

        // ④ 配置 / 日志门槛 / 生命周期桥
        let lifecycle = loader.runtime().install_lifecycle_bridge(kernel.plugins());
        loader.runtime().install_config(config_path, config);
        loader.runtime().set_log_verbosity(log_level);
        loader.runtime().set_log_plugins(log_plugins);

        // ⑤ 接 TDLib（只建客户端；登录在 `login`）
        let session = match &tdlib {
            Some(settings) => {
                let session = Session::create(settings.log_verbosity)?;
                pipeline
                    .attach_tdlib(Arc::clone(session.client()))
                    .map_err(|_| AppError::AlreadyAttached)?;
                Some(session)
            }
            None => None,
        };

        Ok(Self {
            kernel,
            loader,
            pipeline,
            session,
            plugins: Vec::new(),
            load: PluginLoadReport::default(),
            _lifecycle: lifecycle,
            td_state,
            _td_state_watch: td_state_watch,
            host,
            settings: tdlib,
            plugin_dir,
        })
    }

    /// ⑤ 装插件：扫目录 → 探测 → 交给内核的 Fiber 状态机。返回装上的个数。
    ///
    /// 扫描结果（扫到几个、哪些没进内核、为什么）记进 [`Self::load_report`]，并在
    /// 交给内核之后经 [`AppHost::plugin_report`] 一次性上报——**登录之前**，
    /// 所以登录窗口里 / 登录失败时，宿主也已经拿到了清单与失败原因。
    pub fn load_plugins(&mut self) -> Result<usize, AppError> {
        let dir = self.plugin_dir().to_path_buf();
        let scripts_dir = dir.join("scripts");
        // 目录包与旧形态平铺动态库都在这里；`plugin.json` 存在时**不 dlopen**。
        let entries =
            plugin_entries(&dir).map_err(|error| AppError::PluginDir(dir.clone(), error))?;
        let mut report = PluginLoadReport {
            scanned: entries.len(),
            ..PluginLoadReport::default()
        };

        let mut adapters: Vec<Arc<DylibPlugin>> = Vec::new();
        // Go 的 c-shared 一旦映射就永远不能卸载：放到队列最后，让能卸载的先 settle。
        // （依赖顺序仍由内核的 fiber 决定；这里只保证「不可逆的那一步」晚发生。）
        let mut unloadable: Vec<Arc<DylibPlugin>> = Vec::new();
        for entry in entries {
            let shown = match &entry {
                PluginEntry::Bundle(path) | PluginEntry::Legacy(path) => path.clone(),
            };
            // 探测失败只跳过这一个，原因打出来——文件在那儿却悄悄消失是最难查的那类事。
            // 同时记进报告：日志在 Android 上只落文件，宿主要能经 `onPlugins` 看到它。
            let probe = match self.loader.probe_entry(&entry) {
                Ok(probe) => probe,
                Err(error) => {
                    tracing::warn!(
                        target: "fuyubot::app",
                        "跳过插件 {}：{error}",
                        shown.display()
                    );
                    report.skip(shown, error.to_string());
                    continue;
                }
            };
            // 脚本包由 `fuyubot.script` 插件从 `plugins/scripts/` 发现，宿主**不直接 dlopen**
            // 它的入口（那是一个 .py/.ts）。放错位置就明确报出来，别等 dlopen 报「不是动态库」。
            if probe.kind.is_script() {
                tracing::warn!(
                    target: "fuyubot::app",
                    "跳过脚本插件 {}：脚本插件应放在 plugins/scripts/ 下（kind={}）",
                    shown.display(),
                    probe.kind
                );
                report.skip(
                    shown,
                    format!("脚本插件应放在 plugins/scripts/ 下（kind={}）", probe.kind),
                );
                continue;
            }
            // 旧形态（dll + 同名脚本）继续按同伴文件找。
            let script = find_script(&probe.entry);
            let config = match script {
                Some(script) => serde_json::json!({
                    "script": script,
                    "plugins_dir": dir,
                    "scripts_dir": scripts_dir,
                    "spawn": true,
                }),
                None => serde_json::json!({
                    "plugins_dir": dir,
                    "scripts_dir": scripts_dir,
                    "spawn": true,
                }),
            }
            .to_string();

            let go_runtime = probe.go_runtime;
            let adapter = Arc::new(DylibPlugin::new(
                probe,
                config,
                Arc::clone(self.loader.runtime()),
            ));
            if go_runtime {
                unloadable.push(adapter);
            } else {
                adapters.push(adapter);
            }
        }
        adapters.extend(unloadable);

        let plugins: Vec<Arc<dyn fuyubot_core::Plugin>> = adapters
            .iter()
            .map(|adapter| Arc::clone(adapter) as Arc<dyn fuyubot_core::Plugin>)
            .collect();
        self.kernel.plugins().load_all(plugins)?;
        self.loader
            .runtime()
            .install_plugin_reporter(self.kernel.reporter());

        let count = adapters.len();
        self.plugins = adapters;
        self.load = report;
        self.report_plugins();
        // 登录之前就把装载结果交给宿主：登录要等用户输入，甚至可能失败，
        // 那之后才「能查」等于最需要它的时候没有。
        self.host.plugin_report(&self.plugin_report_json());
        tracing::info!(target: "fuyubot::app", "插件已交给内核：{count} 个");
        Ok(count)
    }

    /// ⑥ 登录 + `getMe`。`login: false` 或没接 TDLib 时是 [`AppError::NoSession`]。
    pub fn login(&mut self) -> Result<(), AppError> {
        let settings = self.settings.as_ref().ok_or(AppError::NoSession)?;
        let parameters = settings.parameters.clone();
        let (account, budget) = (settings.account, settings.login_budget);
        let bot_token = settings.bot_token.clone();

        let pipeline = Arc::clone(&self.pipeline);
        let host = Arc::clone(&self.host);
        // 登录窗口里的授权状态经 `Prompter::auth_state` 落到这里（事件总线看不到那一条）。
        let td_state = self.td_state.clone();
        let session = self.session.as_mut().ok_or(AppError::NoSession)?;

        self.host.state(AppState::Starting, "插件已就位，开始登录");
        let mut prompter =
            HostPrompter::new(host.as_ref(), bot_token.as_deref()).with_state(td_state);
        // 登录窗口里 TDLib 已经在发东西（还有插件 start 期的响应）：一律转给管线，别吞。
        let mut bypass = |routed| pipeline.dispatch(routed);
        session.login(&parameters, account, budget, &mut prompter, &mut bypass)?;

        // 账户信息交给运行时：插件的 `on_command` 靠 `fuyubot.host.identity` 判断点名的是谁。
        match session.me() {
            Some(me) => {
                let me = me.clone();
                self.loader.runtime().set_self_user(me);
            }
            None => tracing::warn!(target: "fuyubot::app", "getMe 没拿到（不影响运行）"),
        }
        self.host.state(AppState::Ready, "已登录，开始分发更新");
        Ok(())
    }

    /// ⑦ 开分发。没接 TDLib = `None`（回放 / `--no-tdlib`）。
    pub fn dispatch(&self) -> Option<ReceiveLoop> {
        self.session
            .as_ref()
            .map(|session| self.pipeline.spawn_dispatch(session.client()))
    }

    /// 收摊：停接收循环 → 卸插件（`plugin_stop`）→ TDLib 关会话刷盘。返回「TDLib 是否确认关闭」。
    ///
    /// ## 顺序是刻意的：插件先走，TDLib 最后关
    ///
    /// `plugin_stop` 里插件仍然会调宿主——写日志、`set_config`、给用户留一句「下线了」。
    /// 那些调用全都要落在**还活着**的客户端上；先关会话再卸插件，等于把插件最后一段
    /// 收尾动作丢在一个已关闭的句柄上（Android 上这一步同时是崩溃高发区）。
    /// 所以顺序是「停接收 → 卸插件 → 关会话」，CLI 与 Android 宿主共用这一条。
    ///
    /// ⚠️ 代价是**接收循环已经停了**：`plugin_stop` 里 `td_send` 出去的请求没人收，
    /// 等响应会一直等到它自己的超时。插件不许在 `stop` 里等 TDLib 的响应。
    ///
    /// `receive` 由调用方按值交回来（它可能从没被创建过：登录失败、`--no-tdlib`）。
    pub async fn shutdown(
        mut self,
        receive: Option<ReceiveLoop>,
        close_budget: Duration,
    ) -> Result<bool, AppError> {
        // 每一段都上报一次：Android 的 `nativeStop` 超时（8s）后，`last_error()` 里会带上
        // 「最后停在哪个阶段」——这是那时唯一还拿得到的现场（日志可能还没刷进文件）。
        self.host.state(AppState::Stopping, "停接收循环");
        if let Some(receive) = receive {
            receive.stop();
            receive.join().await;
        }

        // 插件先退：此刻 TDLib 还活着，`plugin_stop` 里调宿主是合法状态。
        self.host
            .state(AppState::Stopping, "卸载插件（等 plugin_stop）");
        self.plugins.clear();
        let stopped = self.kernel.stop().await;
        // 管线攥着客户端的一份 `Arc`；先放手，后面的 `close` 才是唯一碰它的人。
        drop(self.pipeline);

        // 最后关 TDLib：`close` 之后句柄不该再被任何人用（已经没人了）。
        self.host
            .state(AppState::Stopping, "TDLib 关会话（等数据库刷盘）");
        let closed = match self.session.take() {
            Some(session) => session.close(close_budget),
            None => true,
        };
        // 卸载有错也**先把会话关干净**再往上抛：刷盘比报错重要（否则下次冷启动要重建）。
        stopped?;
        self.host.state(AppState::Stopped, "已停止");
        Ok(closed)
    }

    pub fn pipeline(&self) -> &Arc<UpdatePipeline> {
        &self.pipeline
    }

    pub fn runtime(&self) -> &Arc<HostRuntime> {
        self.loader.runtime()
    }

    pub fn kernel(&mut self) -> &mut Kernel {
        &mut self.kernel
    }

    /// 登录后的账户信息（`getMe` 的原始 JSON）。
    pub fn me(&self) -> Option<&Value> {
        self.session.as_ref().and_then(Session::me)
    }

    /// 本次要不要走登录（`tdlib.login`）。
    pub fn should_login(&self) -> bool {
        self.settings
            .as_ref()
            .is_some_and(|settings| settings.login)
    }

    /// TDLib 客户端句柄（插件 / 测试用；没有 TDLib 时是 `None`）。
    pub fn client(&self) -> Option<&Arc<TdClient>> {
        self.session.as_ref().map(Session::client)
    }

    /// 造一份**可以在别的线程上用**的只读视图（插件清单 / 插件详情 / TDLib JSON 请求）。
    ///
    /// 视图里只有 `Arc` 与一份不可变的插件元信息；`App` 仍然独占内核的 `&mut`、
    /// 接收循环把手与收摊的所有权。插件**状态**是现算的（走 `HostRuntime` 里那个
    /// `PluginReporter`），所以拿到的不是「装插件那一刻」的旧快照。
    ///
    /// 时机：`load_plugins` 之后（否则插件表是空的）。登录前后都行——`td_request`
    /// 在登录期间也能用，登录循环会把杂项响应转给管线。
    pub fn live_view(&self) -> LiveView {
        LiveView::new(
            Arc::clone(self.runtime()),
            self.plugin_infos(),
            self.load.clone(),
            self.client().cloned(),
            self.td_state.clone(),
        )
    }

    /// TDLib 的授权 / 连接状态（`live_view` 给的也是同一份）。
    ///
    /// 登录期由 `Prompter::auth_state` 更新，运行期由事件总线更新；`--no-tdlib` /
    /// 回放时永远是 `None`（读出来是 `known:false`）。
    pub fn td_state(&self) -> &TdState {
        &self.td_state
    }

    /// 此刻的插件报告 JSON（形状与 [`LiveView::plugins_json`] 同一份）。
    ///
    /// 装完插件、登录之前就可用——`AppHost::plugin_report` 收的就是它。
    pub fn plugin_report_json(&self) -> String {
        self.live_view().plugins_json()
    }

    /// 这次扫描的结果：扫到几个、哪些没进内核。
    pub fn load_report(&self) -> &PluginLoadReport {
        &self.load
    }

    /// 已装插件的静态自描述（名字 / 版本 / ABI / 依赖 / 文件路径），按扫描顺序。
    ///
    /// 只看**装上的**：探测失败的插件连 `DylibPlugin` 都没造出来，它们只出现在日志里。
    pub fn plugin_infos(&self) -> Vec<PluginInfo> {
        self.plugins
            .iter()
            .map(|plugin| PluginInfo {
                name: plugin.name().to_owned(),
                version: plugin
                    .manifest()
                    .map(|manifest| manifest.version.clone())
                    .unwrap_or_default(),
                abi_version: plugin.abi_version(),
                inject: plugin.inject(),
                soft_inject: plugin.soft_inject(),
                path: plugin.path().to_path_buf(),
            })
            .collect()
    }

    fn plugin_dir(&self) -> &Path {
        self.plugin_dir.as_path()
    }

    fn report_plugins(&mut self) {
        let pending = self.kernel.plugins().pending();
        let failed = self.kernel.plugins().failed();
        for (name, missing) in &pending {
            tracing::warn!(
                target: "fuyubot::app",
                "插件 {name} 的依赖还没齐，先不启动：缺 {}",
                missing.join(", ")
            );
        }
        for (name, reason) in &failed {
            tracing::error!(target: "fuyubot::app", "插件 {name} 启动失败：{reason}");
        }
        let active = self.kernel.plugins().active().len();
        tracing::info!(
            target: "fuyubot::app",
            "插件：扫到 {} 个，交给内核 {} 个（active {active} / pending {} / failed {}），跳过 {} 个",
            self.load.scanned,
            self.plugins.len(),
            pending.len(),
            failed.len(),
            self.load.skipped.len()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::{AppHost, AppState, AskKind};
    use fuyubot_loader::DLL_EXTENSION;
    use std::sync::Mutex;

    /// 只记录状态 / 插件报告、不提供交互的宿主。
    #[derive(Default)]
    struct Recorder {
        states: Mutex<Vec<(AppState, String)>>,
        reports: Mutex<Vec<String>>,
    }

    impl AppHost for Recorder {
        fn state(&self, state: AppState, detail: &str) {
            self.states
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push((state, detail.to_owned()));
        }

        fn ask(&self, _kind: AskKind, _prompt: &str) -> Option<String> {
            None
        }

        fn qr(&self, _link: &str) {}

        fn plugin_report(&self, report: &str) {
            self.reports
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push(report.to_owned());
        }
    }

    /// 一个空的临时目录（每个测试一个，互不打扰）。
    fn scratch(name: &str) -> PathBuf {
        // 临时目录放 `target/test-tmp/`：`std::env::temp_dir()` 会被 MSYS 改成 `/tmp`，
        // 在 Windows 上解析到盘根、建目录直接被拒（pitfalls 第 70 条）。
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-tmp/fuyubot-app")
            .join(name)
            .join(std::process::id().to_string())
    }

    /// 装插件的结果必须**在登录之前**交给宿主，而且扫不动的文件要带着原因记下来——
    /// Android 上日志只落文件，APK 拿不到 `tracing`，只有这一条 `onPlugins` 能看见。
    #[tokio::test]
    async fn load_plugins_reports_scanned_skipped_and_counts_before_login() {
        let dir = scratch("report");
        let plugins = dir.join("plugins");
        std::fs::create_dir_all(&plugins).expect("建插件目录失败");
        // 一个不是动态库的 `.so`：探测必然失败 → 进 `skipped_plugins`，而不是悄悄消失。
        let broken = plugins.join(format!("broken.{DLL_EXTENSION}"));
        std::fs::write(&broken, b"definitely not a shared library").expect("写坏插件失败");

        let recorder = Arc::new(Recorder::default());
        let host: Arc<dyn AppHost> = recorder.clone();
        let mut app = App::boot(AppOptions {
            data_dir: dir.join("data"),
            plugin_dir: plugins,
            log_level: LogLevel::Info,
            log_plugins: Vec::new(),
            config: serde_json::json!({}),
            config_path: None,
            tdjson_path: None,
            tdlib: None,
            host,
        })
        .await
        .expect("没接 TDLib 的 boot 应该能起来");

        assert_eq!(app.load_plugins().expect("装插件不该整批失败"), 0);

        let reports = recorder
            .reports
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        assert_eq!(reports.len(), 1, "装一次插件只上报一次");
        let report: Value = serde_json::from_str(&reports[0]).expect("上报的必须是合法 JSON");
        assert_eq!(report["count"], serde_json::json!(0));
        assert_eq!(report["scanned"], serde_json::json!(1));
        assert_eq!(report["skipped"], serde_json::json!(1));
        assert_eq!(report["failed"], serde_json::json!(0));
        let skipped = &report["skipped_plugins"][0];
        assert_eq!(skipped["path"], serde_json::json!(broken.to_string_lossy()));
        assert!(
            skipped["reason"]
                .as_str()
                .is_some_and(|text| !text.is_empty()),
            "跳过必须带原因：{skipped}"
        );

        // 同一份数据走查询接口（`nativePlugins()`）也拿得到，形状一致。
        let live: Value =
            serde_json::from_str(&app.plugin_report_json()).expect("查询接口也必须是合法 JSON");
        assert_eq!(live["skipped"], serde_json::json!(1));
        assert_eq!(app.load_report().scanned, 1);
        assert_eq!(app.live_view().plugins_json(), reports[0]);
    }

    /// 收摊必须**逐段上报**：Android 的 `nativeStop` 超时后，`last_error` 就靠
    /// 这些阶段名说清「卡在哪一步」。顺序也是契约（停循环 → 卸插件 → 关会话）。
    #[tokio::test]
    async fn shutdown_reports_every_stage_in_order() {
        let dir = scratch("shutdown");
        let plugins = dir.join("plugins");
        std::fs::create_dir_all(&plugins).expect("建插件目录失败");

        let recorder = Arc::new(Recorder::default());
        let host: Arc<dyn AppHost> = recorder.clone();
        let app = App::boot(AppOptions {
            data_dir: dir.join("data"),
            plugin_dir: plugins,
            log_level: LogLevel::Info,
            log_plugins: Vec::new(),
            config: serde_json::json!({}),
            config_path: None,
            tdjson_path: None,
            tdlib: None,
            host,
        })
        .await
        .expect("没接 TDLib 的 boot 应该能起来");

        assert!(
            app.shutdown(None, Duration::from_millis(100))
                .await
                .unwrap()
        );

        let states = recorder
            .states
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        let phases: Vec<&str> = states
            .iter()
            .filter(|(state, _)| *state == AppState::Stopping)
            .map(|(_, detail)| detail.as_str())
            .collect();
        // 插件必须在 TDLib 关会话**之前**收摊：`plugin_stop` 里还要用客户端。
        assert_eq!(
            phases,
            [
                "停接收循环",
                "卸载插件（等 plugin_stop）",
                "TDLib 关会话（等数据库刷盘）"
            ]
        );
        assert_eq!(
            states.last().map(|(state, _)| *state),
            Some(AppState::Stopped)
        );
    }
}
