//! 实例锁与状态：`<项目根>/.fuyubot/` 下的两个文件。
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
    /// 已加载的插件数。
    ///
    /// ⚠️ 保留这个**独立**字段是为了兼容两处旧消费者：`--background` 的父进程等的是
    /// 整份文件、老的 `status` 文件也带这一行。新代码要看细节请用 [`detail`](Self::detail)；
    /// 真在跑的数量是 `detail.active.len()`，两者可能不等（有插件还在等依赖）。
    pub plugins: usize,
    pub name: String,
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

impl Info {
    pub fn render(&self) -> String {
        format!(
            "pid={}\nstarted={}\nversion={}\nplugins={}\nname={}\n{}",
            self.pid,
            self.started,
            self.version,
            self.plugins,
            self.name,
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
            plugins: field("plugins")
                .and_then(|value| value.parse().ok())
                .unwrap_or(0),
            name: field("name").unwrap_or("fuyubot").to_owned(),
            // 老实例（这次改动之前发布的 status）没有这三行：当空表，不因此判失败。
            detail: PluginDetail::parse(text),
        })
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
    /// 锁文件在但**没人持有**：上次没走正常退出。锁本身已经能再拿，不用谁去清理。
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
                Some(info) => lines.push(format!(
                    "  上次：{} pid {}，启动于 {}前（版本 {}，插件 {} 个）",
                    info.name,
                    info.pid,
                    uptime_text(info.started, unix_now()),
                    info.version,
                    info.plugins
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
                format!("  插件       {}", info.detail.summary()),
            ];
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
            plugins: 3,
            name: "mybot".to_owned(),
            detail: PluginDetail::default(),
        };
        assert_eq!(Info::parse(&info.render()).unwrap(), info);
        // 没有 pid 的（半份 / 别的东西）读不出来，不许编一个 pid 出来。
        assert_eq!(Info::parse("version=0.1.0\n"), None);
        assert_eq!(Info::parse(""), None);
        // 缺的字段给默认值，认不出的数字不当成 0 之外的鬼东西。
        let partial = Info::parse("pid=7\n").unwrap();
        assert_eq!(partial.pid, 7);
        assert_eq!(partial.plugins, 0);
        assert_eq!(partial.version, "?");
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
            plugins: 2,
            name: "bot".to_owned(),
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
                plugins: 0,
                name: "old".to_owned(),
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
            plugins: 1,
            name: "bot".to_owned(),
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
            plugins: 0,
            name: "me".to_owned(),
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
            plugins: 7,
            name: "bot".to_owned(),
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
        // 「谁在等什么」是这个功能存在的理由：只说数量人还是不知道该去配什么。
        assert!(lines.contains("c 在等 database"), "{lines}");
        assert!(lines.contains("d 起不来：apply 报错"), "{lines}");
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
            plugins: 2,
            name: "bot".to_owned(),
            detail: detail.clone(),
        };
        let parsed = Info::parse(&info.render()).expect("该能读回来");
        assert_eq!(parsed.detail, detail);
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
