//! 实例锁、状态与运行期控制：`<项目根>/.fuyubot/` 下的几个文件。
//!
//! 一个目录一个 Bot、一个进程一个 Bot（`docs/decisions.md` 核心原则 1），所以 `fuyubot run`
//! 起来时**独占** `.fuyubot/lock`：
//!
//! - 独占靠 [`std::fs::File::try_lock`]，**不靠**「PID 还活着吗」——进程一死，锁由操作系统
//!   释放，陈旧锁自动就能重新拿下（不需要谁去清理，也不需要拿 PID 去别的进程表里问）。
//! - 内容（PID / 启动时间 / 版本 / 插件数）放**另一个文件** `.fuyubot/status`。
//!   ⚠️ **不能塞进 lock 里**：Windows 的文件锁是**强制**的，字节范围被锁住时别的进程
//!   `read` 直接报 `ERROR_LOCK_VIOLATION`——`status` 就永远读不到内容了（Unix 的 flock 是
//!   建议锁，读得到，所以这个坑只在 Windows 上现形）。
//!   `lock` 只需要「能不能锁上」这一个比特，内容另放一处反而更干净。
//! - 状态**在启动完成之后才发布**：`run --background` 的父进程等的就是它（拿上锁只说明进程
//!   活着，那时候插件还没加载、登录还没走）。
//! - 运行期控制各占一个文件：`stop` 是优雅停止请求，`log-level` 是面板在日志页调的插件
//!   日志门槛。两者都只表达「这一次运行」：新实例拿锁时会把它们清掉，不会带到下一轮。
//!
//! `--replay` 不占锁、也不发布状态：回放是离线验插件的开发路径，不该和跑着的实例互斥。

use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// 状态目录（`docs/decisions.md` 变更 1 的目录布局）。
pub const STATE_DIR: &str = ".fuyubot";

fn lock_path(root: &Path) -> PathBuf {
    root.join(STATE_DIR).join("lock")
}

fn status_path(root: &Path) -> PathBuf {
    root.join(STATE_DIR).join("status")
}

/// 「请你停下」的标记：`stop` 写它，运行中的实例在自己的主循环里看到就优雅退出。
///
/// 为什么不是直接发信号 / `taskkill`：那种硬杀**跑不到**插件的 `plugin_stop`、
/// 也不给 TDLib 收摊的机会；而且本机沙箱里 `taskkill` 直接被拒（`Access denied`），
/// 只有进程自己收摊这条路真的通。标记里带 PID，免得「上一次留下的标记」误伤新实例。
fn stop_path(root: &Path) -> PathBuf {
    root.join(STATE_DIR).join("stop")
}

/// 运行期调日志级别的控制文件：面板写它，运行中的实例在主循环里读。
///
/// 它**不是**配置：只表达「这一次运行」想要的门槛。`InstanceLock::acquire` 会在新实例
/// 启动时删掉它，避免上一轮临时开的 `trace` 悄悄带到下一次启动。
fn log_level_path(root: &Path) -> PathBuf {
    root.join(STATE_DIR).join("log-level")
}

/// 后台运行的日志文件路径。
pub fn log_path(root: &Path) -> PathBuf {
    root.join("logs").join("fuyubot.log")
}

/// 现在（Unix 秒）。没引 chrono——我们只需要算「已运行多久」，两个秒数相减就够。
pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

// ── 状态内容 ───────────────────────────────────────────────────────

/// 实例自报的身份：`status` 读它，`stop` 用它的 PID。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Info {
    pub pid: u32,
    /// 启动时刻（Unix 秒）。
    pub started: u64,
    pub version: String,
    /// 运行期插件日志门槛（`error` / `warn` / `info` / `debug` / `trace`）。
    ///
    /// 面板与 `fuyubot status` 都读它。老实例发布的 status 没有这一行 → 当 `info`
    /// （与 [`fuyubot_loader::DEFAULT_LOG_VERBOSITY`] 一致），不因此判整份状态失败。
    pub log_level: String,
    /// 已加载的插件数。
    ///
    /// ⚠️ 保留这个**独立**字段是为了兼容两处旧消费者：`--background` 的父进程等的是
    /// 整份文件、老的 `status` 文件也带这一行。新代码要看细节请用 [`detail`](Self::detail)；
    /// 真在跑的数量是 `detail.active.len()`，两者可能不等（有插件还在等依赖）。
    pub plugins: usize,
    pub name: String,
    /// 这份状态是不是一个**还活着**的实例发布的。
    ///
    /// - `Some(true)`：还在跑的实例（崩溃 / 硬杀后留下的最后一份也是它）；
    /// - `Some(false)`：优雅停止时补写的最后一份；
    /// - `None`：老版本写的 status，没有这一行——退出方式未知，不能诬赖成异常退出。
    pub running: Option<bool>,
    /// 优雅停止的时刻（Unix 秒）；还在跑 / 没有正常停止就是 `None`。
    pub stopped_at: Option<u64>,
    /// 最新一条 `updateConnectionState` 的 TDLib 状态名（`connectionStateReady` 等）。
    ///
    /// TDLib **没有**「现在连上了吗」这种可主动查的方法，连接状态只以更新的形式出现
    /// （`fuyubot_simplify::connection` 开头写着同一件事）：实例在收到更新的那一刻把它记下来，
    /// 再写进这份 status——面板是**另一个进程**，拿不到实例的内存，只能这样看到连接状态。
    ///
    /// `None` = 本次没接 TDLib（`--no-tdlib` / `--replay`）、还没收到过更新，
    /// 或者这份 status 是老实例写的（它还没有这一行）。
    pub connection: Option<String>,
    /// 最新一条 `updateAuthorizationState` 的 TDLib 状态名（`authorizationStateReady` 等）。
    ///
    /// 与 [`connection`](Self::connection) 同一套路：授权状态也只有更新、没有可主动查的方法，
    /// 实例收到就记下，面板 / `status` 从这份 status 里读。和**连接**状态相互独立——
    /// `connectionStateReady` 也可能还没登录。
    ///
    /// `None` = 没接 TDLib、还没收到过更新，或者这份 status 是老实例写的。
    pub authorization: Option<String>,
    /// 登录账户（`getMe`，之后跟着 `updateUser` 刷新）。
    ///
    /// `None` = 还没登录 / 没接 TDLib / 老实例写的 status。面板的「账户」行与
    /// `fuyubot status` 的「账户」行都读它——它们都是**另一个进程**，只能这样看到。
    pub account: Option<Account>,
    /// 插件速览：真在跑的、还在等的（含缺什么）。
    ///
    /// 放在 `status` 文件里而不是让 `status` 子命令自己去问进程——它是**另一个进程**，
    /// 没有共享内存可读，而报告要反映的是「那个进程里的状态」。
    /// 老实例写不出这个字段，所以缺了就当空表（`parse` 不因此失败）。
    pub detail: PluginDetail,
}

/// 插件速览（`/plugins` 与 `fuyubot status` 共用）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PluginDetail {
    /// 真在跑的插件名（激活顺序）。
    pub active: Vec<String>,
    /// 还在等的：`(名字, 缺哪些能力)`。
    pub pending: Vec<(String, Vec<String>)>,
    /// 起来失败的：`(名字, 原因)`。
    pub failed: Vec<(String, String)>,
}

impl PluginDetail {
    /// 从 [`fuyubot_core::Report`] 取。
    pub fn from_report(report: &fuyubot_core::Report) -> Self {
        let mut detail = Self::default();
        for entry in &report.plugins {
            match entry.state {
                fuyubot_core::ReportState::Active => detail.active.push(entry.name.clone()),
                fuyubot_core::ReportState::Pending => {
                    detail
                        .pending
                        .push((entry.name.clone(), entry.missing.clone()));
                }
                fuyubot_core::ReportState::Failed => detail.failed.push((
                    entry.name.clone(),
                    entry
                        .reason
                        .clone()
                        .unwrap_or_else(|| "未知原因".to_owned()),
                )),
                // `Activating` 是瞬时的：快照正好抓到它时按「还没跑起来」处理，
                // 报出去会让人以为插件卡住了。
                fuyubot_core::ReportState::Activating => {}
            }
        }
        detail
    }

    /// 一行摘要：「3 个在跑」/「3 个在跑，1 个等依赖」/「都还没起来」。
    pub fn summary(&self) -> String {
        if self.active.is_empty() && self.pending.is_empty() && self.failed.is_empty() {
            return "没有插件".to_owned();
        }
        let mut parts = vec![format!("{} 个在跑", self.active.len())];
        if !self.pending.is_empty() {
            parts.push(format!("{} 个等依赖", self.pending.len()));
        }
        if !self.failed.is_empty() {
            parts.push(format!("{} 个起不来", self.failed.len()));
        }
        parts.join("，")
    }

    /// 写进 status 文件的一行（名字用 `,` 分隔；名字里不会有逗号——插件名是点分标识符）。
    fn render(&self) -> String {
        let join = |items: &[String]| items.join(",");
        let pending = self
            .pending
            .iter()
            .map(|(name, missing)| format!("{name}:{}", missing.join("+")))
            .collect::<Vec<_>>()
            .join(",");
        let failed = self
            .failed
            .iter()
            .map(|(name, reason)| format!("{name}:{}", reason.replace(['\n', '\r'], " ")))
            .collect::<Vec<_>>()
            .join(",");
        format!(
            "active={}\npending={}\nfailed={}\n",
            join(&self.active),
            pending,
            failed
        )
    }

    /// 解析 [`Self::render`] 的产物；缺字段当空表。
    fn parse(text: &str) -> Self {
        let field = |key: &str| {
            text.lines()
                .find_map(|line| line.strip_prefix(key)?.strip_prefix('='))
                .map(str::trim)
                .unwrap_or("")
        };
        let split = |value: &str| -> Vec<String> {
            value
                .split(',')
                .filter(|item| !item.is_empty())
                .map(str::to_owned)
                .collect()
        };
        let pairs = |value: &str| -> Vec<(String, String)> {
            value
                .split(',')
                .filter(|item| !item.is_empty())
                .filter_map(|item| {
                    let (name, rest) = item.split_once(':')?;
                    Some((name.to_owned(), rest.to_owned()))
                })
                .collect()
        };
        Self {
            active: split(field("active")),
            pending: pairs(field("pending"))
                .into_iter()
                .map(|(name, missing)| {
                    let parts = missing
                        .split('+')
                        .filter(|item| !item.is_empty())
                        .map(str::to_owned)
                        .collect();
                    (name, parts)
                })
                .collect(),
            failed: pairs(field("failed")),
        }
    }
}

/// 登录账户的要点（`getMe` 的 `user` 里那几样）。
///
/// 面板 / `fuyubot status` 是**另一个进程**，拿不到实例的内存，所以这几行跟着
/// status 文件走。`getMe` 只是登录那一刻的快照，之后 TDLib 会用 `updateUser`
/// 改名字 / 用户名——实例把最新的那份写下来，这里显示的就不是死的。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Account {
    pub id: i64,
    /// 显示名（`first_name` + `last_name`，可能为空）。
    pub name: String,
    /// 主用户名（`usernames.active_usernames[0]`）。
    pub username: Option<String>,
    /// `user.type.@type == "userTypeBot"`（TDLib 的 `user` 没有顶层 `is_bot`）。
    pub is_bot: bool,
}

impl Account {
    /// 从 `getMe` / `updateUser` 的 `user` JSON 取；没有 `id` 就当取不到。
    ///
    /// 字段路径按真库核过（AGENTS 约束 42）：bot 判定在 `type.@type`、
    /// 用户名在 `usernames.active_usernames[0]`。
    pub fn from_user(user: &serde_json::Value) -> Option<Self> {
        let id = user.get("id")?.as_i64()?;
        let name = ["first_name", "last_name"]
            .iter()
            .filter_map(|key| user.get(key).and_then(serde_json::Value::as_str))
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        let username = user
            .pointer("/usernames/active_usernames/0")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        let is_bot = user
            .pointer("/type/@type")
            .and_then(serde_json::Value::as_str)
            == Some("userTypeBot");
        Some(Self {
            id,
            name,
            username,
            is_bot,
        })
    }

    /// 一行描述：`Fuyu Bot（id=7） @FuyuBot [bot]`。
    pub fn label(&self) -> String {
        let mut line = if self.name.is_empty() {
            format!("id={}", self.id)
        } else {
            format!("{}（id={}）", self.name, self.id)
        };
        if let Some(username) = &self.username {
            line.push_str(&format!(" @{username}"));
        }
        line.push_str(if self.is_bot { " [bot]" } else { " [user]" });
        line
    }

    /// 写进 status 的几行；没有用户名就不留 `account_username=`。
    fn render(&self) -> String {
        let username = self
            .username
            .as_deref()
            .map(|username| format!("account_username={username}\n"))
            .unwrap_or_default();
        format!(
            "account_id={}\naccount_name={}\n{username}account_bot={}\n",
            self.id,
            self.name.replace(['\n', '\r'], " "),
            u8::from(self.is_bot)
        )
    }

    /// 解析 [`Self::render`] 的产物；没有 `account_id`（老实例 / 没登录）就是 `None`。
    fn parse(text: &str) -> Option<Self> {
        let field = |key: &str| {
            text.lines()
                .find_map(|line| line.strip_prefix(key)?.strip_prefix('='))
                .map(str::trim)
        };
        Some(Self {
            id: field("account_id")?.parse().ok()?,
            name: field("account_name").unwrap_or("").to_owned(),
            username: field("account_username")
                .filter(|value| !value.is_empty())
                .map(str::to_owned),
            is_bot: field("account_bot") == Some("1"),
        })
    }
}

impl Info {
    pub fn render(&self) -> String {
        format!(
            "pid={}\nstarted={}\nversion={}\nlog_level={}\nplugins={}\nname={}\n{}{}{}{}{}{}",
            self.pid,
            self.started,
            self.version,
            self.log_level,
            self.plugins,
            self.name,
            match self.running {
                Some(true) => "running=1\n",
                Some(false) => "running=0\n",
                None => "",
            },
            self.stopped_at
                .map(|stopped| format!("stopped_at={stopped}\n"))
                .unwrap_or_default(),
            // 没接 TDLib / 还没收到更新就不写这一行：缺行 = 不知道，别编一个「已连接」。
            self.connection
                .as_deref()
                .map(|connection| format!("connection={connection}\n"))
                .unwrap_or_default(),
            // 授权状态同理：不知道就不写，别默认成「已登录」。
            self.authorization
                .as_deref()
                .map(|authorization| format!("authorization={authorization}\n"))
                .unwrap_or_default(),
            // 账户信息：没登录就不写（面板显示「—」），别编一个 id=0。
            self.account
                .as_ref()
                .map(Account::render)
                .unwrap_or_default(),
            self.detail.render()
        )
    }

    /// 解析。**没有 `pid` 就算读不出来**：没有 PID 的这份内容对 `status` / `stop` 都没用。
    pub fn parse(text: &str) -> Option<Self> {
        let field = |key: &str| {
            text.lines()
                .find_map(|line| line.strip_prefix(key)?.strip_prefix('='))
                .map(str::trim)
        };
        Some(Self {
            pid: field("pid")?.parse().ok()?,
            started: field("started")
                .and_then(|value| value.parse().ok())
                .unwrap_or(0),
            version: field("version").unwrap_or("?").to_owned(),
            // 老实例没有这一行：默认 info，不因此判失败。
            log_level: field("log_level").unwrap_or("info").to_owned(),
            plugins: field("plugins")
                .and_then(|value| value.parse().ok())
                .unwrap_or(0),
            name: field("name").unwrap_or("fuyubot").to_owned(),
            // 老实例没有 `running=`：`None` = 退出方式未知（不是「异常」）。
            running: field("running").map(|value| value != "0"),
            stopped_at: field("stopped_at").and_then(|value| value.parse().ok()),
            // 老实例没有 `connection=`（那时还不报连接状态）：`None` = 不知道。
            // 空值也当没有：写坏的文件不该让面板显示「未知状态（）」。
            connection: field("connection")
                .filter(|value| !value.is_empty())
                .map(str::to_owned),
            // 授权状态同一套兼容：没有这一行就是不知道。
            authorization: field("authorization")
                .filter(|value| !value.is_empty())
                .map(str::to_owned),
            // 账户信息：没有 `account_id=`（没登录 / 老实例）就是不知道。
            account: Account::parse(text),
            // 老实例（这次改动之前发布的 status）没有这三行：当空表，不因此判失败。
            detail: PluginDetail::parse(text),
        })
    }

    /// 这份状态是不是**优雅停止**时补写的最后一份。
    ///
    /// 崩溃 / 硬杀留下的状态没有 `stopped_at`，老版本的状态没有 `running=`，
    /// 两者都只会被当成「无法确认正常停止」。
    pub fn stopped_gracefully(&self) -> bool {
        self.running == Some(false) && self.stopped_at.is_some()
    }
}

/// 「已运行 3 分 12 秒」。纯函数，好测。
pub fn uptime_text(started: u64, now: u64) -> String {
    let seconds = now.saturating_sub(started);
    match seconds {
        0..=59 => format!("{seconds} 秒"),
        s if s < 3600 => format!("{} 分 {} 秒", s / 60, s % 60),
        s if s < 86_400 => format!("{} 小时 {} 分", s / 3600, (s % 3600) / 60),
        s => format!("{} 天 {} 小时", s / 86_400, (s % 86_400) / 3600),
    }
}

/// TDLib 连接状态的中文短标签（面板顶部与 `fuyubot status` 共用同一份说法）。
///
/// 五个构造器的名字与解析都来自 [`fuyubot_simplify::ConnectionState`]——那是
/// `updateConnectionState` 最早被收在一处的地方，这里不另抄一份合法状态表。
pub fn connection_label(state: fuyubot_simplify::ConnectionState) -> &'static str {
    use fuyubot_simplify::ConnectionState;
    match state {
        ConnectionState::WaitingForNetwork => "等待网络",
        ConnectionState::ConnectingToProxy => "连代理中",
        ConnectionState::Connecting => "连接中",
        ConnectionState::Updating => "同步中",
        ConnectionState::Ready => "已连接",
    }
}

/// TDLib 授权状态的中文短标签（面板概览与 `fuyubot status` 共用同一份说法）。
///
/// 状态表与字段路径都来自 [`fuyubot_tdlib::AuthState`]——登录流程用的就是它，
/// 这里只负责把 `authorizationState*` 翻成人话，不另维护一份合法状态表。
pub fn authorization_label(state: &fuyubot_tdlib::AuthState) -> &'static str {
    use fuyubot_tdlib::AuthState;
    match state {
        AuthState::WaitTdlibParameters => "等待参数",
        AuthState::WaitPhoneNumber => "等待手机号",
        AuthState::WaitOtherDeviceConfirmation { .. } => "等待扫码确认",
        AuthState::WaitCode { .. } => "等待验证码",
        AuthState::WaitPassword { .. } => "等待两步密码",
        AuthState::WaitRegistration => "等待注册",
        AuthState::WaitEmailAddress => "等待邮箱",
        AuthState::WaitEmailCode => "等待邮箱验证码",
        AuthState::WaitPremiumPurchase { .. } => "等待购买",
        AuthState::Ready => "已登录",
        AuthState::LoggingOut => "正在登出",
        AuthState::Closing => "正在关闭",
        AuthState::Closed => "已关闭",
        // 具体名字在 `AuthState::Unknown` 里，调用方想显示就自己取 `name()`。
        AuthState::Unknown(_) => "未知状态",
    }
}

/// 读 `.fuyubot/status`；没有 / 读不出来（比如实例刚起来还没发布）就是 `None`。
pub fn read_status(root: &Path) -> Option<Info> {
    let text = std::fs::read_to_string(status_path(root)).ok()?;
    Info::parse(&text)
}

/// 请 `pid` 那个实例停下（写标记）。真正的收摊由**它自己**做（见 [`InstanceLock::stop_requested`]）。
pub fn request_stop(root: &Path, pid: u32) -> Result<(), String> {
    let path = stop_path(root);
    std::fs::create_dir_all(root.join(STATE_DIR))
        .map_err(|e| format!("建不了 {}: {e}", root.join(STATE_DIR).display()))?;
    std::fs::write(&path, format!("{pid}\n")).map_err(|e| format!("写不了 {}: {e}", path.display()))
}

/// 运行中实例请求的日志级别（`.fuyubot/log-level`）。
///
/// 没有文件 / 空文件都算「没请求」。内容由写入方保证是白名单里的名字；
/// 运行中的实例还会再解析一次，写坏了也只是不生效，不会把实例带崩。
pub fn requested_log_level(root: &Path) -> Option<String> {
    let text = std::fs::read_to_string(log_level_path(root)).ok()?;
    let level = text.trim();
    (!level.is_empty()).then(|| level.to_owned())
}

/// 请运行中的实例把插件日志门槛切到 `level`（`error` / `warn` / `info` / `debug` / `trace`）。
///
/// 只在实例运行中调用才有意义：它自己每 500ms 在主循环里读一次这个文件。
pub fn request_log_level(root: &Path, level: &str) -> Result<(), String> {
    // 写进控制文件之前先验一遍：与其让实例每 500ms 读到一个不认识的名字，
    // 不如在这里把错误指回调用方（面板能把这句话原样显示出来）。
    crate::scaffold::parse_log_level(level)?;
    let path = log_level_path(root);
    std::fs::create_dir_all(root.join(STATE_DIR))
        .map_err(|e| format!("建不了 {}: {e}", root.join(STATE_DIR).display()))?;
    std::fs::write(&path, format!("{level}\n"))
        .map_err(|e| format!("写不了 {}: {e}", path.display()))
}

// ── 实例锁 ─────────────────────────────────────────────────────────

/// 本进程拿下的实例锁。**drop 即释放**（进程死了操作系统也会放）。
pub struct InstanceLock {
    /// 锁就靠这个句柄活着；名字带下划线是因为我们只「持有」它，从不读它。
    _file: File,
    root: PathBuf,
}

impl InstanceLock {
    /// 拿锁。拿不到就是「这个目录已经有 fuyubot 在跑」（变更 7）。
    ///
    /// ⚠️ 顺手清掉上一个实例留下的 `.fuyubot/status` 与 `.fuyubot/stop`：前者是**旧的**
    /// 身份（留着会让「启动中」看起来像「上次那个还在跑」），后者是**旧的停止请求**
    /// （留着会让新实例刚起来就自己停）。
    pub fn acquire(root: &Path) -> Result<Self, String> {
        let path = lock_path(root);
        let parent = path
            .parent()
            .ok_or_else(|| format!("{} 没有父目录", path.display()))?;
        std::fs::create_dir_all(parent).map_err(|e| format!("建不了 {}: {e}", parent.display()))?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| format!("打不开 {}: {e}", path.display()))?;

        match file.try_lock() {
            Ok(()) => {
                let _ = std::fs::remove_file(status_path(root));
                let _ = std::fs::remove_file(stop_path(root));
                // 上一轮临时调的日志级别不能带到这一轮：它只是运行期控制，不是配置。
                let _ = std::fs::remove_file(log_level_path(root));
                Ok(Self {
                    _file: file,
                    root: root.to_path_buf(),
                })
            }
            Err(TryLockError::WouldBlock) => Err(match read_status(root) {
                Some(info) => format!(
                    "{} 已经有 fuyubot 在跑（{}，pid {}，已运行 {}）——`fuyubot status` 看详情，\
                     `fuyubot stop` 停掉它",
                    root.display(),
                    info.name,
                    info.pid,
                    uptime_text(info.started, unix_now())
                ),
                None => format!(
                    "{} 已经有 fuyubot 在跑（状态还没发布）——`fuyubot status` 看详情，\
                     `fuyubot stop` 停掉它",
                    root.display()
                ),
            }),
            Err(TryLockError::Error(e)) => Err(format!("锁不上 {}: {e}", path.display())),
        }
    }

    /// 发布状态：**启动完成之后**才调，`run --background` 的父进程靠它判断子进程真起来了。
    pub fn publish(&self, info: &Info) -> Result<(), String> {
        let path = status_path(&self.root);
        std::fs::write(&path, info.render()).map_err(|e| format!("写不了 {}: {e}", path.display()))
    }

    /// 有没有人请**本实例**停下（标记里的 PID 得是我们自己的——别人家的标记不算）。
    ///
    /// 由 `run` 的主循环每 500ms 问一次；看到就优雅退出（停接收循环 → 卸载插件 → 放锁）。
    pub fn stop_requested(&self) -> bool {
        stopped_pid(&self.root) == Some(std::process::id())
    }

    /// 收摊：把标记删掉（免得下次启动又看到它）。
    pub fn clear_stop(&self) {
        let _ = std::fs::remove_file(stop_path(&self.root));
    }
}

/// `.fuyubot/stop` 里写的是谁的 PID（没有标记 / 读不出来就是 `None`）。
pub fn stopped_pid(root: &Path) -> Option<u32> {
    std::fs::read_to_string(stop_path(root))
        .ok()?
        .trim()
        .parse()
        .ok()
}

// ── 探测 / status / stop ───────────────────────────────────────────

/// 这个目录上的实例现在是什么状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// 连锁文件都没有：这个目录从没跑过（或者跑完自己收了）。
    Absent,
    /// 锁文件在但**没人持有**：上次实例已经退出。
    ///
    /// 是正常停止还是异常退出要看 [`Info::stopped_gracefully`]：优雅退出会补写
    /// `running=0` + `stopped_at`，崩溃 / 硬杀留下的最后一份只有 `running=1`。
    /// 锁本身已经能再拿，不用谁去清理。
    Stale(Option<Info>),
    /// 有人持有：实例在跑（`None` = 还没发布状态，也就是「启动中」）。
    Running(Option<Info>),
}

/// 探测：能不能锁上 `.fuyubot/lock`。
///
/// ⚠️ 宁可把「锁不上但读不出内容」当成**在跑**：反过来会把活着的实例报成死的，
/// 用户接着 `run` 第二个实例，然后两个实例抢同一个 TDLib 数据库。
pub fn probe(root: &Path) -> State {
    let info = read_status(root);
    let Ok(file) = OpenOptions::new()
        .read(true)
        .write(true)
        .open(lock_path(root))
    else {
        return State::Absent;
    };
    match file.try_lock() {
        Ok(()) => State::Stale(info),
        Err(_) => State::Running(info),
    }
}

/// `fuyubot status` 的实现。返回要打印的行（第一行之外都缩进两格）。
pub fn status_lines(root: &Path) -> Vec<String> {
    match probe(root) {
        State::Absent => vec![
            format!("{}：没有运行中的实例", root.display()),
            format!(
                "（没有 {}，从没跑过或者跑完自己收了）",
                lock_path(root).display()
            ),
        ],
        State::Stale(info) => {
            let mut lines = vec![
                format!("{}：没有运行中的实例", root.display()),
                "（`lock` 文件还在，但没人在持有它——不用手工清理，下次 `fuyubot run` 自己接管）"
                    .to_owned(),
            ];
            match info {
                Some(info) if info.stopped_gracefully() => {
                    let stopped = info.stopped_at.unwrap_or(info.started);
                    lines.push(format!(
                        "  上次：{} pid {}，{}前正常停止（版本 {}，插件 {} 个）",
                        info.name,
                        info.pid,
                        uptime_text(stopped, unix_now()),
                        info.version,
                        info.plugins
                    ));
                }
                Some(info) if info.running == Some(true) => lines.push(format!(
                    "  上次：{} pid {}，异常退出：没收到停止请求（版本 {}，插件 {} 个）",
                    info.name, info.pid, info.version, info.plugins
                )),
                Some(info) => lines.push(format!(
                    "  上次：{} pid {}，已退出（老版本状态，退出方式未知；版本 {}）",
                    info.name, info.pid, info.version
                )),
                None => lines.push("  上次没留下可读的状态".to_owned()),
            }
            lines
        }
        State::Running(None) => vec![
            format!("{}：有实例在跑（**启动中**，状态还没发布）", root.display()),
            "  稍等一两秒再看；起不来就查日志".to_owned(),
        ],
        State::Running(Some(info)) => {
            let mut lines = vec![
                format!("{}：运行中", root.display()),
                format!("  pid        {}", info.pid),
                format!("  已运行     {}", uptime_text(info.started, unix_now())),
                format!("  版本       {}", info.version),
                format!("  名字       {}", info.name),
                format!("  日志级别   {}", info.log_level),
                format!("  插件       {}", info.detail.summary()),
            ];
            // 连接状态是**实例**从 `updateConnectionState` 学到的，status 只负责转述；
            // 没有这一行（没接 TDLib / 还没收到）就不打，别让「不知道」看起来像「没连上」。
            if let Some(connection) = info.connection.as_deref() {
                let text = match fuyubot_simplify::ConnectionState::from_type_name(connection) {
                    Some(state) => connection_label(state).to_owned(),
                    None => format!("未知状态（{connection}）"),
                };
                lines.push(format!("  连接       TDLib {text}"));
            }
            // 授权状态同样只转述：`authorizationState*` 的名字来自实例收到的更新。
            if let Some(authorization) = info.authorization.as_deref() {
                let state = fuyubot_tdlib::AuthState::from_type_name(authorization);
                lines.push(match state {
                    fuyubot_tdlib::AuthState::Unknown(name) => {
                        format!("  授权       TDLib 未知状态（{name}）")
                    }
                    state => format!(
                        "  授权       TDLib {}（{authorization}）",
                        authorization_label(&state)
                    ),
                });
            }
            // 账户信息：实例登录时拿到 `getMe`，之后跟着 `updateUser` 刷新——只转述。
            if let Some(account) = &info.account {
                lines.push(format!("  账户       {}", account.label()));
            }
            // 「谁在等什么」是这个功能存在的理由：只说数量，人还是不知道该去配什么。
            for (name, missing) in &info.detail.pending {
                lines.push(format!("    · {name} 在等 {}", missing.join(", ")));
            }
            for (name, reason) in &info.detail.failed {
                lines.push(format!("    · {name} 起不来：{reason}"));
            }
            lines
        }
    }
}

/// `fuyubot stop` 的实现：写停止标记 → 等它自己收摊 → 还不走才硬杀。
///
/// 先「请」后「杀」：请的那条路会走完插件的 `plugin_stop` 与 `kernel.stop()`，
/// 硬杀那条约等于拔电源（只有在实例卡在登录 / 别的地方看不到标记时才用得上）。
pub fn stop(root: &Path) -> Result<String, String> {
    let info = match probe(root) {
        State::Absent => return Ok("没有运行中的实例".to_owned()),
        State::Stale(Some(info)) => {
            return Ok(format!(
                "没有运行中的实例（上次 pid {} 已经收摊）",
                info.pid
            ));
        }
        State::Stale(None) => return Ok("没有运行中的实例".to_owned()),
        State::Running(None) => {
            return Err("有实例在跑，但状态里还没有 pid（可能刚启动）——过一秒再试".to_owned());
        }
        State::Running(Some(info)) => info,
    };

    // 防手滑：PID 0 与自己的 PID 都不许杀（陈旧锁里的 PID 可能已被系统回收给别的进程，
    // 而那个进程恰好可能是我们自己）。
    if info.pid == 0 || info.pid == std::process::id() {
        return Err(format!(
            "拒绝：状态里的 pid 是 {}，那是本进程（或 0）——状态文件可能坏了：{}",
            info.pid,
            status_path(root).display()
        ));
    }

    request_stop(root, info.pid)?;
    if wait_until_gone(root, Duration::from_secs(8)) {
        let _ = std::fs::remove_file(stop_path(root));
        return Ok(format!(
            "已停止 {}（pid {}，优雅退出：插件 stop 与内核都已收摊）",
            info.name, info.pid
        ));
    }

    // 兜底：实例没理会标记（卡在登录窗口、或者根本没跑到主循环）。
    let _ = kill(info.pid);
    if wait_until_gone(root, Duration::from_secs(5)) {
        return Ok(format!("已停止 {}（pid {}，硬杀）", info.name, info.pid));
    }
    Err(format!(
        "给 pid {} 发过停止请求、也试过硬杀，但它还活着——日志：{}。\
         手工收拾：任务管理器结束它，或 `Stop-Process -Id {} -Force`",
        info.pid,
        log_path(root).display(),
        info.pid
    ))
}

/// 等到「没人持有锁」。返回 `false` = 超时（它还在）。
fn wait_until_gone(root: &Path, budget: Duration) -> bool {
    let deadline = Instant::now() + budget;
    loop {
        if !matches!(probe(root), State::Running(_)) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// 发停止信号。`/T` 连**进程树**一起收：脚本插件（Python / JS）跑在子进程里，
/// 只杀宿主会把它们留成孤儿。
#[cfg(windows)]
fn kill(pid: u32) -> Result<(), String> {
    run(Command::new("taskkill").args(["/PID", &pid.to_string(), "/T", "/F"]))
}

#[cfg(not(windows))]
fn kill(pid: u32) -> Result<(), String> {
    run(Command::new("kill").args(["-TERM", &pid.to_string()]))
}

fn run(command: &mut Command) -> Result<(), String> {
    let status = command
        .status()
        .map_err(|e| format!("起不了 {:?}: {e}", command.get_program()))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{:?} 退出码 {status}", command.get_program()))
    }
}

/// 日志的最后 `lines` 行（后台启动失败时给人看的就这个）。
pub fn tail(path: &Path, lines: usize) -> String {
    let Ok(text) = std::fs::read_to_string(path) else {
        return format!("（读不了 {}）", path.display());
    };
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::temp;

    #[test]
    fn info_round_trips_through_text() {
        let info = Info {
            pid: 4242,
            started: 1_700_000_000,
            version: "0.1.0".to_owned(),
            log_level: "debug".to_owned(),
            plugins: 3,
            name: "mybot".to_owned(),
            running: Some(false),
            stopped_at: Some(1_700_000_123),
            connection: Some("connectionStateReady".to_owned()),
            authorization: Some("authorizationStateReady".to_owned()),
            account: None,
            detail: PluginDetail::default(),
        };
        assert_eq!(Info::parse(&info.render()).unwrap(), info);
        // 没有 pid 的（半份 / 别的东西）读不出来，不许编一个 pid 出来。
        assert_eq!(Info::parse("version=0.1.0\n"), None);
        assert_eq!(Info::parse(""), None);
        // 缺的字段给默认值，认不出的数字不当成 0 之外的鬼东西。
        // 老实例没有 `log_level` 这一行：默认 info（不是空串）。
        let partial = Info::parse("pid=7\n").unwrap();
        assert_eq!(partial.pid, 7);
        assert_eq!(partial.plugins, 0);
        assert_eq!(partial.version, "?");
        assert_eq!(partial.log_level, "info");
        // 老 status 没有 `running=` / `stopped_at=`：退出方式未知，不能凭空报「正常停止」，
        // 也不能诬赖成「异常退出」。
        assert_eq!(partial.running, None);
        assert_eq!(partial.stopped_at, None);
        // 老 status 也没有 `connection=` / `authorization=`：不知道就说不知道。
        assert_eq!(partial.connection, None);
        assert_eq!(partial.authorization, None);
        // 空值当没有（写坏的文件不该变成「未知状态（）」）。
        let broken = Info::parse("pid=7\nconnection=\nauthorization=\n").unwrap();
        assert_eq!(broken.connection, None);
        assert_eq!(broken.authorization, None);
        assert!(!partial.stopped_gracefully());
    }

    /// 连接状态是**更新**、不是可查的状态：实例记下来之后要能跨进程原样读到。
    #[test]
    fn connection_state_survives_the_status_file_and_can_be_labelled() {
        use fuyubot_simplify::ConnectionState;

        let info = Info::parse("pid=1\nconnection=connectionStateReady\n").unwrap();
        assert_eq!(info.connection.as_deref(), Some("connectionStateReady"));
        assert_eq!(
            ConnectionState::from_type_name("connectionStateReady"),
            Some(ConnectionState::Ready)
        );
        assert_eq!(connection_label(ConnectionState::Ready), "已连接");
        assert_eq!(
            connection_label(ConnectionState::WaitingForNetwork),
            "等待网络"
        );

        // 认不出的状态名不能被当成 Ready（simplify 的规矩，这里跟着）。
        assert_eq!(
            ConnectionState::from_type_name("connectionStateSomethingNew"),
            None
        );
        // 写回文件时没有连接状态就不留 `connection=` 那一行。
        let mut bare = info.clone();
        bare.connection = None;
        assert!(!bare.render().contains("connection="));
    }

    /// 授权状态与连接状态是两件事：状态名跨进程往返，标签覆盖「登录完成」与「等待凭据」。
    #[test]
    fn authorization_state_survives_the_status_file_and_can_be_labelled() {
        use fuyubot_tdlib::AuthState;

        let info = Info::parse("pid=1\nauthorization=authorizationStateReady\n").unwrap();
        assert_eq!(
            info.authorization.as_deref(),
            Some("authorizationStateReady")
        );
        assert_eq!(
            authorization_label(&AuthState::from_type_name("authorizationStateReady")),
            "已登录"
        );
        assert_eq!(
            authorization_label(&AuthState::from_type_name("authorizationStateWaitCode")),
            "等待验证码"
        );
        assert_eq!(
            authorization_label(&AuthState::from_type_name("authorizationStateWaitPassword")),
            "等待两步密码"
        );
        // 认不出的状态不当成「已登录」，但名字要原样留着（面板能显示出来）。
        let unknown = AuthState::from_type_name("authorizationStateSomethingNew");
        assert_eq!(authorization_label(&unknown), "未知状态");
        assert_eq!(unknown.name(), "authorizationStateSomethingNew");

        // 没有授权状态时不留 `authorization=` 那一行。
        let mut bare = info.clone();
        bare.authorization = None;
        assert!(!bare.render().contains("authorization="));
    }

    /// 正常停止和异常退出必须分得开：前者有 `stopped_at`，后者只有最后一圈写的 `running=1`。
    #[test]
    fn a_graceful_stop_is_distinguishable_from_a_crash() {
        let stopped = Info::parse("pid=1\nrunning=0\nstopped_at=100\n").unwrap();
        assert!(stopped.stopped_gracefully());
        assert_eq!(stopped.stopped_at, Some(100));

        let crashed = Info::parse("pid=1\nrunning=1\n").unwrap();
        assert!(!crashed.stopped_gracefully());
        assert_eq!(crashed.stopped_at, None);
    }

    /// 面板写 `.fuyubot/log-level`，实例下一圈读；新实例启动时把旧请求清掉。
    #[test]
    fn log_level_requests_are_validated_and_cleared_by_a_new_instance() {
        let root = temp("instance-log-level");
        std::fs::create_dir_all(root.join(STATE_DIR)).unwrap();
        assert_eq!(requested_log_level(&root), None);

        request_log_level(&root, "debug").unwrap();
        assert_eq!(requested_log_level(&root).as_deref(), Some("debug"));

        // 白名单之外的名字在写文件之前就被拒，免得实例每圈解析一遍垃圾。
        assert!(request_log_level(&root, "verbose").is_err());
        assert_eq!(requested_log_level(&root).as_deref(), Some("debug"));

        // 上一轮的临时级别不能带到下一轮。
        {
            let _lock = InstanceLock::acquire(&root).unwrap();
        }
        assert_eq!(requested_log_level(&root), None);
    }

    #[test]
    fn uptime_is_human_readable() {
        assert_eq!(uptime_text(100, 130), "30 秒");
        assert_eq!(uptime_text(100, 100 + 192), "3 分 12 秒");
        assert_eq!(uptime_text(0, 7200), "2 小时 0 分");
        assert_eq!(uptime_text(0, 90_000), "1 天 1 小时");
        // 时钟倒退也不能变成负数。
        assert_eq!(uptime_text(500, 100), "0 秒");
    }

    /// 锁的三态：没有 → 拿上（在跑）→ 放掉（陈旧，且**不用手工清理**就能再拿）。
    #[test]
    fn the_lock_goes_absent_running_stale_and_back() {
        let root = temp("lock-lifecycle");
        assert_eq!(probe(&root), State::Absent);

        let lock = InstanceLock::acquire(&root).unwrap();
        // 拿上锁但还没发布 → 「启动中」。
        assert_eq!(probe(&root), State::Running(None));
        let info = Info {
            pid: 4242,
            started: unix_now(),
            version: "0.1.0".to_owned(),
            log_level: "info".to_owned(),
            plugins: 2,
            name: "bot".to_owned(),
            running: Some(true),
            stopped_at: None,
            connection: None,
            authorization: None,
            account: None,
            detail: PluginDetail::default(),
        };
        lock.publish(&info).unwrap();
        assert_eq!(probe(&root), State::Running(Some(info.clone())));

        // 第二个实例必须被挡住，而且报错里要带上在跑的那个 PID（不然用户不知道停谁）。
        let Err(refused) = InstanceLock::acquire(&root) else {
            panic!("同一个目录上的第二个实例必须被挡住");
        };
        assert!(refused.contains("4242"), "报错要带上 PID：{refused}");
        assert!(refused.contains("fuyubot stop"), "要指路：{refused}");

        // 放掉锁（等价于进程没了）：陈旧，但下次 acquire 自己就能接管。
        drop(lock);
        assert_eq!(probe(&root), State::Stale(Some(info)));
        let again = InstanceLock::acquire(&root).unwrap();
        // 接管时上一个实例的状态必须被清掉，否则「新的还在启动」看起来像「旧的在跑」。
        assert_eq!(probe(&root), State::Running(None));
        drop(again);
    }

    /// 状态文件在**发布之前**不能被当成本次的状态读到。
    #[test]
    fn taking_the_lock_throws_away_the_previous_status() {
        let root = temp("lock-clears-status");
        {
            let lock = InstanceLock::acquire(&root).unwrap();
            lock.publish(&Info {
                pid: 99,
                started: 1,
                version: "0".to_owned(),
                log_level: "info".to_owned(),
                plugins: 0,
                name: "old".to_owned(),
                running: Some(true),
                stopped_at: None,
                connection: None,
                authorization: None,
                account: None,
                detail: PluginDetail::default(),
            })
            .unwrap();
        }
        assert!(read_status(&root).is_some());
        let _lock = InstanceLock::acquire(&root).unwrap();
        assert_eq!(read_status(&root), None);
    }

    /// `stop` 在什么都没跑时必须是**无事发生**，绝不能去杀谁。
    #[test]
    fn stop_is_a_no_op_when_nothing_runs() {
        let root = temp("stop-noop");
        assert!(stop(&root).unwrap().contains("没有运行中的实例"));

        // 陈旧锁同理：只是报「已经不在跑了」。
        let lock = InstanceLock::acquire(&root).unwrap();
        lock.publish(&Info {
            pid: 424242,
            started: unix_now(),
            version: "0.1.0".to_owned(),
            log_level: "info".to_owned(),
            plugins: 1,
            name: "bot".to_owned(),
            running: Some(true),
            stopped_at: None,
            connection: None,
            authorization: None,
            account: None,
            detail: PluginDetail::default(),
        })
        .unwrap();
        drop(lock);
        let text = stop(&root).unwrap();
        assert!(
            text.contains("没有运行中的实例") && text.contains("424242"),
            "{text}"
        );
    }

    /// 状态里写着**本进程**的 PID 时不许杀（陈旧锁 + PID 被系统回收的保险）。
    #[test]
    fn stop_refuses_to_kill_our_own_pid() {
        let root = temp("stop-self");
        let lock = InstanceLock::acquire(&root).unwrap();
        lock.publish(&Info {
            pid: std::process::id(),
            started: unix_now(),
            version: "0.1.0".to_owned(),
            log_level: "info".to_owned(),
            plugins: 0,
            name: "me".to_owned(),
            running: Some(true),
            stopped_at: None,
            connection: None,
            authorization: None,
            account: None,
            detail: PluginDetail::default(),
        })
        .unwrap();
        let error = stop(&root).unwrap_err();
        assert!(error.contains("拒绝"), "{error}");
        drop(lock);
    }

    /// 停止标记：只有写给**自己**的才算数（别人家的标记不许让我们停），
    /// 而且旧的标记在新实例接管时会被清掉。
    #[test]
    fn only_our_own_stop_request_counts() {
        let root = temp("stop-marker");
        let lock = InstanceLock::acquire(&root).unwrap();
        assert!(!lock.stop_requested(), "刚起来不该有停止请求");

        request_stop(&root, 424242).unwrap();
        assert_eq!(stopped_pid(&root), Some(424242));
        assert!(!lock.stop_requested(), "别人家的 pid 不算");

        request_stop(&root, std::process::id()).unwrap();
        assert!(lock.stop_requested());
        lock.clear_stop();
        assert!(!lock.stop_requested());

        // 上一次留下的标记（比如 stop 打进来时实例刚退）不能让新实例自己停。
        request_stop(&root, std::process::id()).unwrap();
        drop(lock);
        let _fresh = InstanceLock::acquire(&root).unwrap();
        assert_eq!(stopped_pid(&root), None);
    }

    /// `status` 的几行输出：三态各说各的，运行中要带上 PID 与插件数。
    #[test]
    fn status_lines_describe_every_state() {
        let root = temp("status-lines");
        assert!(status_lines(&root)[0].contains("没有运行中的实例"));

        let lock = InstanceLock::acquire(&root).unwrap();
        assert!(status_lines(&root)[0].contains("启动中"));
        lock.publish(&Info {
            pid: 1234,
            started: unix_now(),
            version: "9.9.9".to_owned(),
            log_level: "info".to_owned(),
            plugins: 7,
            name: "bot".to_owned(),
            running: Some(true),
            stopped_at: None,
            connection: Some("connectionStateReady".to_owned()),
            authorization: Some("authorizationStateReady".to_owned()),
            account: Some(Account {
                id: 42,
                name: "Fuyu Bot".to_owned(),
                username: Some("FuyuBot".to_owned()),
                is_bot: true,
            }),
            detail: PluginDetail {
                active: vec!["a".to_owned(), "b".to_owned()],
                pending: vec![("c".to_owned(), vec!["database".to_owned()])],
                failed: vec![("d".to_owned(), "apply 报错".to_owned())],
            },
        })
        .unwrap();
        let lines = status_lines(&root).join("\n");
        assert!(lines.contains("运行中"), "{lines}");
        // 报的是**真在跑的**数量（detail 说了算），不是登记过的那个数。
        assert!(
            lines.contains("1234") && lines.contains("2 个在跑"),
            "{lines}"
        );
        assert!(lines.contains("1 个等依赖"), "{lines}");
        assert!(lines.contains("1 个起不来"), "{lines}");
        assert!(lines.contains("9.9.9"), "{lines}");
        // 连接状态是实例从 `updateConnectionState` 学来的，`status` 只转述。
        assert!(lines.contains("连接       TDLib 已连接"), "{lines}");
        // 授权状态同理（与连接状态相互独立）。
        assert!(
            lines.contains("授权       TDLib 已登录（authorizationStateReady）"),
            "{lines}"
        );
        // 账户也是实例从 `getMe` / `updateUser` 学来的，`status` 只转述。
        assert!(
            lines.contains("账户       Fuyu Bot（id=42） @FuyuBot [bot]"),
            "{lines}"
        );
        // 「谁在等什么」是这个功能存在的理由：只说数量人还是不知道该去配什么。
        assert!(lines.contains("c 在等 database"), "{lines}");
        assert!(lines.contains("d 起不来：apply 报错"), "{lines}");

        // 优雅停止：锁放掉之后，`status` 要能说清「上次是正常停止」而不是异常退出。
        drop(lock);
        let lock = InstanceLock::acquire(&root).unwrap();
        lock.publish(&Info {
            pid: 1234,
            started: unix_now().saturating_sub(60),
            version: "9.9.9".to_owned(),
            log_level: "info".to_owned(),
            plugins: 0,
            name: "bot".to_owned(),
            running: Some(false),
            stopped_at: Some(unix_now()),
            connection: None,
            authorization: None,
            account: None,
            detail: PluginDetail::default(),
        })
        .unwrap();
        drop(lock);
        let lines = status_lines(&root).join("\n");
        assert!(lines.contains("正常停止"), "{lines}");
        assert!(!lines.contains("异常退出"), "{lines}");

        // 崩溃 / 硬杀留下的最后一份状态没有 `stopped_at`：必须报异常退出。
        let lock = InstanceLock::acquire(&root).unwrap();
        lock.publish(&Info {
            pid: 1234,
            started: unix_now(),
            version: "9.9.9".to_owned(),
            log_level: "info".to_owned(),
            plugins: 0,
            name: "bot".to_owned(),
            running: Some(true),
            stopped_at: None,
            connection: None,
            authorization: None,
            account: None,
            detail: PluginDetail::default(),
        })
        .unwrap();
        drop(lock);
        let lines = status_lines(&root).join("\n");
        assert!(lines.contains("异常退出"), "{lines}");
        assert!(!lines.contains("正常停止"), "{lines}");
    }

    /// 插件速览要能跨进程往返：`run` 写、`status`（另一个进程）读。
    ///
    /// 这条守的是**格式**：`render` / `parse` 一旦不对称，`status` 就会静默地
    /// 显示「0 个在跑」——比报错更难查。
    #[test]
    fn plugin_detail_survives_the_status_file_round_trip() {
        let detail = PluginDetail {
            active: vec!["fuyubot.start".to_owned(), "fuyubot.help".to_owned()],
            pending: vec![(
                "fuyubot.demo".to_owned(),
                vec!["database".to_owned(), "cache".to_owned()],
            )],
            failed: vec![("broken".to_owned(), "plugin_start 返回 3".to_owned())],
        };
        let info = Info {
            pid: 7,
            started: 1,
            version: "1.0".to_owned(),
            log_level: "info".to_owned(),
            plugins: 2,
            name: "bot".to_owned(),
            running: Some(true),
            stopped_at: None,
            connection: None,
            authorization: None,
            account: None,
            detail: detail.clone(),
        };
        let parsed = Info::parse(&info.render()).expect("该能读回来");
        assert_eq!(parsed.detail, detail);
    }

    /// 账户信息要能跨进程往返：`run` 写进 status、面板 / `status` 读。
    ///
    /// 字段路径按约束 42 喂（`type.@type` 判 bot、`usernames.active_usernames[0]` 取用户名）。
    #[test]
    fn account_survives_the_status_file_round_trip() {
        let user = serde_json::json!({
            "id": 7,
            "first_name": "Fuyu",
            "last_name": "Bot",
            "usernames": { "@type": "usernames", "active_usernames": ["FuyuBot"] },
            "type": { "@type": "userTypeBot" },
        });
        let account = Account::from_user(&user).expect("有 id 就该解析出来");
        assert_eq!(account.id, 7);
        assert_eq!(account.name, "Fuyu Bot");
        assert_eq!(account.username.as_deref(), Some("FuyuBot"));
        assert!(account.is_bot);
        assert_eq!(account.label(), "Fuyu Bot（id=7） @FuyuBot [bot]");

        let mut info = Info {
            pid: 7,
            started: 1,
            version: "1.0".to_owned(),
            log_level: "info".to_owned(),
            plugins: 0,
            name: "bot".to_owned(),
            running: Some(true),
            stopped_at: None,
            connection: None,
            authorization: None,
            account: Some(account.clone()),
            detail: PluginDetail::default(),
        };
        let parsed = Info::parse(&info.render()).expect("该能读回来");
        assert_eq!(parsed.account, Some(account));

        // 没登录（或老实例）不写 `account_` 行：面板据此显示「还没登录」，而不是 id=0。
        info.account = None;
        assert!(!info.render().contains("account_"));
        assert_eq!(Info::parse(&info.render()).unwrap().account, None);

        // 没有 `id` 的 `user` 取不出账户；没有用户名也不编一个。
        assert_eq!(
            Account::from_user(&serde_json::json!({ "first_name": "x" })),
            None
        );
        let bare = Account::from_user(&serde_json::json!({ "id": 9 })).unwrap();
        assert_eq!(bare.username, None);
        assert_eq!(bare.label(), "id=9 [user]");
    }

    /// 「变了才重发」靠的是 `PluginDetail` 的相等判定——它要是永远相等，
    /// 运行循环就永远不会重发，`status` 会一直显示启动那一刻的样子。
    #[test]
    fn different_details_compare_unequal() {
        let base = PluginDetail {
            active: vec!["a".to_owned()],
            pending: Vec::new(),
            failed: Vec::new(),
        };
        // 插件从「跑着」变成「等依赖」：必须不等。
        let waiting = PluginDetail {
            active: Vec::new(),
            pending: vec![("a".to_owned(), vec!["database".to_owned()])],
            failed: Vec::new(),
        };
        assert_ne!(base, waiting);

        // 缺的能力多了一个：也得不等（否则依赖变化就看不见了）。
        let more_missing = PluginDetail {
            active: Vec::new(),
            pending: vec![(
                "a".to_owned(),
                vec!["database".to_owned(), "cache".to_owned()],
            )],
            failed: Vec::new(),
        };
        assert_ne!(waiting, more_missing);

        // 什么都没变：必须相等（否则每 500ms 白写一次盘）。
        assert_eq!(base.clone(), base);
    }

    /// 老实例（这次改动之前发布的 `status` 文件）没有 `active=` / `pending=` / `failed=`
    /// 三行：必须当空表读出来，而不是判成「读不出状态」。
    #[test]
    fn an_old_status_file_without_plugin_detail_still_parses() {
        let text = "pid=42\nstarted=1\nversion=0.1\nplugins=5\nname=old\n";
        let info = Info::parse(text).expect("老格式还要能读");
        assert_eq!(info.plugins, 5);
        assert_eq!(info.detail, PluginDetail::default());
        assert_eq!(info.detail.summary(), "没有插件");
    }
}
