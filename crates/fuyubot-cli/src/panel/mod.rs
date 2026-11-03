//! `fuyubot panel`：终端操作面板（ratatui）。
//!
//! 布局固定为**顶部版本栏 + 左侧导航 + 中央工作区 + 底部两行**：顶部是 `[fuyubot - v…]`、
//! 实例状态与 TDLib 连接状态（实例把 `updateConnectionState` 记进 `.fuyubot/status`，
//! 面板只读那一份），底部第一行是当前可用操作、第二行是 CPU / 内存 / 运行时长 / 插件数 / 日志 / PID；
//! `Tab` 在导航与工作区之间切换焦点（边框高亮），日志页在工作区焦点下可滚动（到顶再往上
//! 会**分段回读**更早的日志；窗口有上限、翻在中间时冻结，`End` 重读尾部回到最新），
//! 调日志级别、按级别 / 来源过滤，
//! 并可选中一行复制（TDLib Update 复制完整 JSON）；配置页用表格展示生效值，
//! 可选中单个配置项编辑（写回保留注释 / CRLF）。五个页面分别回答「现在什么状态 / 装了哪些插件 /
//! 日志在说什么 / 生效的配置是什么 / 能按哪些键」。
//!
//! 设计边界（与 `run` / `status` 一致）：
//!
//! - 面板**不占实例锁、不加载插件、不接 TDLib**——同一个目录里的 TDLib 数据库
//!   同一时刻只能被一个进程打开（约束 47）。它读 `.fuyubot/status` 与日志就够了。
//! - 「启动」= 派生 `fuyubot run --background`，「停止」= 写 `.fuyubot/stop`
//!   并等实例自己收摊：**与用户手敲命令同一条路**，没有第二套行为。
//! - 退出时恢复终端（`ratatui::restore`）；初始化失败（不是 TTY）就在进 alternate
//!   screen 之前报错，免得给人留一屏花掉的终端。

mod app;
mod clipboard;
mod logfmt;
mod login;
pub mod store;
mod ui;

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::time::Duration;

use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event};

use crate::{existing_root, parent_or_dot, resolved_config_path, scaffold};
use app::App;
pub use app::SpawnOptions;

/// 面板多久重读一次状态 / 插件目录 / 日志。
///
/// 再快只是白烧 CPU（日志不会长那么快），再慢状态栏就会显得「卡住」。
const REFRESH_INTERVAL: Duration = Duration::from_secs(1);

/// 运行面板，直到用户按 `q` / `Esc` / `Ctrl+C`。
///
/// 面板只能在真终端里跑。
///
/// `ratatui::try_init` 在非终端上也能「成功」（把控制序列写进管道），可那对用户毫无意义，
/// 还会把重定向出去的日志弄脏。`run_tui` 在自动 init **之前**调它，免得在脚本里凭空
/// 建出一个项目目录。
pub fn require_terminal() -> Result<(), Box<dyn std::error::Error>> {
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        return Ok(());
    }
    Err(
        "`fuyubot run --tui` 需要真终端（stdin / stdout 都是 TTY）——\
         脚本或重定向里请用 `fuyubot status` / `fuyubot config` / `fuyubot plugin list`"
            .into(),
    )
}

/// 运行面板，直到用户按 `q` / `Esc` / `Ctrl+C`。
///
/// 失败都发生在**进入 TUI 之前**（不是终端、没有项目、配置读不出来），所以错误能
/// 正常打到 stderr 上；进入 TUI 之后的 IO 错误会先 `restore()` 再返回。
pub fn run(
    config: &Path,
    plugin_dir: Option<PathBuf>,
    spawn: SpawnOptions,
) -> Result<(), Box<dyn std::error::Error>> {
    require_terminal()?;

    // 与 `status` / `stop` 同一套定位：没有 `fuyubot.toml` 就说明这里不是 bot 目录。
    // （`run_tui` 会先自动 init，所以从空目录敲 `run --tui` 也能走到这里。）
    let root = existing_root(config)?;
    let config_path = resolved_config_path(config, &root);
    let root = parent_or_dot(&config_path);

    // 配置写错要在进 alternate screen 之前报清楚；运行中改错了也不退出，
    // 由 `App::reload_config` 把错误原文放到配置页（见 app.rs）。
    scaffold::load_config(&config_path)?;

    let mut app = App::new(root, config_path, plugin_dir, REFRESH_INTERVAL, spawn);

    let mut terminal = ratatui::try_init().map_err(|error| format!("初始化终端失败：{error}"))?;
    let outcome = event_loop(&mut terminal, &mut app);
    ratatui::restore();
    outcome.map_err(Into::into)
}

/// 事件循环：画一帧 → 等按键（最多 150ms）→ 推进 tick。
///
/// `poll` 的超时就是刷新节拍：没有按键时也至少每 150ms 醒一次，
/// 这样实例状态、日志、提示条不会停在旧画面上。
fn event_loop(terminal: &mut DefaultTerminal, app: &mut App) -> std::io::Result<()> {
    loop {
        terminal.draw(|frame| ui::draw(frame, app))?;
        if event::poll(Duration::from_millis(150))? {
            // 只关心按键；Resize / Mouse / Focus 读了就丢，下一帧自然按新尺寸画。
            if let Event::Key(key) = event::read()? {
                app.on_key(key);
            }
        }
        app.tick();
        if app.quit {
            break;
        }
    }
    Ok(())
}
