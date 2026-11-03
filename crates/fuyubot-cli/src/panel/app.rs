//! 操作面板的状态层：采集数据、解释按键、执行启停。
//!
//! **面板自己不加载插件、不接 TDLib**：TDLib 的 sqlite / binlog 同一时刻只允许一个进程打开，
//! 面板要是在旁边再开一份，就会把跑着的实例拆坏（约束 47 的那把锁正是为此）。
//! 所以这里只做三件事：
//!
//! 1. **读**：`.fuyubot/status`、`fuyubot.toml`、插件目录、`logs/fuyubot.log`；
//! 2. **启停**：派生子进程走 `fuyubot run --background`，停止走 `.fuyubot/stop` 标记，
//!    与用户手敲的命令完全同一条路（没有第二套行为）；
//! 3. **记**：页面、滚动位置、确认框、提示条这些纯 UI 状态。
//!
//! 所有会阻塞的动作（等子进程、等实例收摊）都放到工作线程里，结果经 channel 回投，
//! 事件循环永远不会卡在「启动要 60 秒」上。

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

use crate::instance::{self, PluginDetail, State};
use crate::panel::clipboard;
use crate::panel::logfmt::{self, LevelFilter, LogFilter, SourceFilter};
use crate::panel::login::{self, LoginEvent, LoginModal};
use crate::panel::store::{self, StoreKey};
use crate::scaffold;

/// 首屏从文件尾部装多少行（再往回看就按段加载）。
const LOG_TAIL_LINES: usize = 800;
/// 首屏从文件尾部读多少字节；日志涨到几百 MB 也不会整份读进内存。
const LOG_TAIL_BYTES: u64 = 512 * 1024;
/// 往回翻一次加载多少字节：**分段读**，不会为了看一点旧日志把整个文件读一遍。
const LOG_SEGMENT_BYTES: u64 = 256 * 1024;
/// 日志窗口上限（行 / 字节）：翻过头就从**另一头**丢，窗口是滑动窗口，内存不随翻页增长。
const LOG_WINDOW_MAX_LINES: usize = 20_000;
const LOG_WINDOW_MAX_BYTES: u64 = 8 * 1024 * 1024;
/// 提示条停留多久。
const TOAST_SECS: u64 = 4;

// ── 页面 ───────────────────────────────────────────────────────────

/// 左侧导航的六个页面。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Overview,
    Plugins,
    Store,
    Logs,
    Config,
    Help,
}

impl Page {
    pub const ALL: [Self; 6] = [
        Self::Overview,
        Self::Plugins,
        Self::Store,
        Self::Logs,
        Self::Config,
        Self::Help,
    ];

    pub const fn title(self) -> &'static str {
        match self {
            Self::Overview => "概览",
            Self::Plugins => "插件",
            Self::Store => "商店",
            Self::Logs => "日志",
            Self::Config => "配置",
            Self::Help => "帮助",
        }
    }

    pub const fn index(self) -> usize {
        match self {
            Self::Overview => 0,
            Self::Plugins => 1,
            Self::Store => 2,
            Self::Logs => 3,
            Self::Config => 4,
            Self::Help => 5,
        }
    }

    pub fn next(self) -> Self {
        Self::ALL[(self.index() + 1) % Self::ALL.len()]
    }

    pub fn prev(self) -> Self {
        Self::ALL[(self.index() + Self::ALL.len() - 1) % Self::ALL.len()]
    }
}

// ── 插件目录 ───────────────────────────────────────────────────────

/// 插件目录里看得见的东西（不 dlopen 也能列出来）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginKind {
    /// 原生插件包（`plugins/<id>/plugin.json`，kind = native）。
    Bundle,
    /// 原生动态库（旧形态，`.dll` / `.so` / `.dylib`）。
    Native,
    /// 脚本插件目录（`plugins/scripts/<id>/`，或带 `package.json` 的
    /// `fuyubot.name` / `pyproject.toml` 的 `[tool.fuyubot]`）。
    Script,
}

impl PluginKind {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Bundle => "包",
            Self::Native => "dylib",
            Self::Script => "脚本",
        }
    }
}

/// 插件目录里的一项。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskPlugin {
    /// 文件名主干（原生）或目录名 / 包 id（脚本、包）。禁用之后仍然是**原来的**名字：
    /// `foo.dll.disable` 报 `foo`，面板上认得出是同一个插件。
    pub name: String,
    pub kind: PluginKind,
    /// 磁盘上真实的路径（禁用的原生插件是 `…​.dll.disable`；禁用的包在 `.disabled/` 下）。
    pub path: PathBuf,
    /// 启用着没有。禁用项仍然列出来，才能在这里再启用回去。
    pub enabled: bool,
}

impl DiskPlugin {
    /// 能在这个面板里开关吗。
    ///
    /// 三种都能：文件走改名（`.dll` ↔ `.dll.disable`），目录（包 / 脚本）整个挪到
    /// `plugins/.disabled/<name>/` 再挪回来（契约 §2）。
    pub fn toggleable(&self) -> bool {
        matches!(
            self.kind,
            PluginKind::Bundle | PluginKind::Native | PluginKind::Script
        )
    }
}

/// 扫插件目录。**不加载**：名字与版本都来自磁盘（清单 / 文件名），一个 dll 都不开。
///
/// 收四类东西（契约 §2）：
/// - `plugins/<id>/plugin.json` 的插件包；
/// - 旧形态平铺 dylib（含被改名的 `<stem>.dll.disable`）；
/// - `plugins/scripts/<dir>/` 的脚本插件（有清单的按 `id` 展示）；
/// - 禁用项：`plugins/.disabled/<name>/`（包 / 脚本目录）与旧面板的 `<id>.disable/` 目录。
///
/// 目录不存在 / 读不了都当空表（面板刚打开时项目可能还没装插件）。
pub fn scan_plugins(dir: &Path) -> Vec<DiskPlugin> {
    let mut plugins = Vec::new();

    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            let disk_name = entry.file_name().to_string_lossy().into_owned();
            if path.is_file() {
                if path
                    .extension()
                    .is_some_and(|extension| extension == fuyubot_loader::DLL_EXTENSION)
                {
                    if let Some(stem) = path.file_stem() {
                        plugins.push(DiskPlugin {
                            name: stem.to_string_lossy().into_owned(),
                            kind: PluginKind::Native,
                            path,
                            enabled: true,
                        });
                    }
                } else if fuyubot_loader::is_disabled(&path) {
                    // 禁用的原生插件：`foo.dll.disable`。
                    let enabled = fuyubot_loader::enabled_path(&path);
                    if enabled
                        .extension()
                        .is_some_and(|extension| extension == fuyubot_loader::DLL_EXTENSION)
                    {
                        if let Some(stem) = enabled.file_stem() {
                            plugins.push(DiskPlugin {
                                name: stem.to_string_lossy().into_owned(),
                                kind: PluginKind::Native,
                                path,
                                enabled: false,
                            });
                        }
                    }
                }
                continue;
            }
            // 顶层目录：`scripts/` 与 `.disabled/` 在下面单独扫。
            if !path.is_dir()
                || disk_name == fuyubot_loader::SCRIPTS_DIR
                || disk_name.starts_with('.')
            {
                continue;
            }
            // 旧面板的禁用形态：整个目录改名为 `<id>.disable`。
            let (name, enabled) = match disk_name.strip_suffix(fuyubot_loader::DISABLED_SUFFIX) {
                Some(stem) => (stem.to_owned(), false),
                None => (disk_name.clone(), true),
            };
            if fuyubot_loader::is_bundle_dir(&path) {
                plugins.push(DiskPlugin {
                    name: bundle_display_name(&path, &name),
                    kind: bundle_panel_kind(&path),
                    path,
                    enabled,
                });
            } else if scaffold::is_script_plugin_dir(&path) {
                plugins.push(DiskPlugin {
                    name,
                    kind: PluginKind::Script,
                    path,
                    enabled,
                });
            }
        }
    }

    // 脚本插件：`plugins/scripts/<dir>/`（有 `plugin.json` 的按 id 展示）。
    if let Ok(entries) = std::fs::read_dir(dir.join(fuyubot_loader::SCRIPTS_DIR)) {
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            let disk_name = entry.file_name().to_string_lossy().into_owned();
            if !path.is_dir()
                || disk_name.starts_with('.')
                || disk_name.ends_with(fuyubot_loader::DISABLED_SUFFIX)
            {
                continue;
            }
            if fuyubot_loader::is_bundle_dir(&path) {
                plugins.push(DiskPlugin {
                    name: bundle_display_name(&path, &disk_name),
                    kind: PluginKind::Script,
                    path,
                    enabled: true,
                });
            } else if scaffold::is_script_plugin_dir(&path) {
                plugins.push(DiskPlugin {
                    name: disk_name,
                    kind: PluginKind::Script,
                    path,
                    enabled: true,
                });
            }
        }
    }

    // 禁用项：`plugins/.disabled/<name>/`（包或脚本目录）。
    if let Ok(entries) = std::fs::read_dir(dir.join(fuyubot_loader::DISABLED_DIR)) {
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            let disk_name = entry.file_name().to_string_lossy().into_owned();
            if !path.is_dir() || disk_name.starts_with('.') {
                continue;
            }
            if fuyubot_loader::is_bundle_dir(&path) {
                plugins.push(DiskPlugin {
                    name: bundle_display_name(&path, &disk_name),
                    kind: bundle_panel_kind(&path),
                    path,
                    enabled: false,
                });
            } else if scaffold::is_script_plugin_dir(&path) {
                plugins.push(DiskPlugin {
                    name: disk_name,
                    kind: PluginKind::Script,
                    path,
                    enabled: false,
                });
            }
        }
    }

    // 同名的一对（有人把禁用文件和启用文件同时留着）只留启用那个：
    // 排序把 `enabled` 放前面，`dedup_by` 只丢掉紧挨着的重复项。
    plugins.sort_by(|a, b| {
        a.name
            .cmp(&b.name)
            .then_with(|| b.enabled.cmp(&a.enabled))
            .then_with(|| a.kind.label().cmp(b.kind.label()))
    });
    plugins.dedup_by(|a, b| a.name == b.name && a.kind == b.kind);
    plugins
}

/// 包在面板上显示的名字：清单里的 `id` 优先（契约 §2：目录名与 id 不一致时以 id 为准）。
fn bundle_display_name(dir: &Path, fallback: &str) -> String {
    fuyubot_loader::BundleManifest::load(dir)
        .map(|manifest| manifest.id)
        .unwrap_or_else(|_| fallback.to_owned())
}

/// 包在面板里的分类：脚本包显示成「脚本」，其余算「包」。
fn bundle_panel_kind(dir: &Path) -> PluginKind {
    fuyubot_loader::BundleManifest::load(dir)
        .map(|manifest| {
            if manifest.kind.is_script() {
                PluginKind::Script
            } else {
                PluginKind::Bundle
            }
        })
        .unwrap_or(PluginKind::Bundle)
}

/// 把字节按 `\n` 切成行，顺带记下每行在文件里占的字节数（含换行）。
///
/// 自己切而不是 `str::lines`：窗口起点要按「第一行的字节偏移」往回挪，光有字符串
/// 算不出这个偏移；`\r\n` 与末尾没有换行的半行也要能处理。
fn split_log_lines(bytes: &[u8]) -> Vec<(String, usize)> {
    let mut lines = Vec::new();
    let mut start = 0;
    while start < bytes.len() {
        let (end, consumed) = match bytes[start..].iter().position(|&byte| byte == b'\n') {
            Some(offset) => (start + offset, offset + 1),
            None => (bytes.len(), bytes.len() - start),
        };
        let mut line = &bytes[start..end];
        if line.last() == Some(&b'\r') {
            line = &line[..line.len() - 1];
        }
        lines.push((String::from_utf8_lossy(line).into_owned(), consumed));
        start += consumed;
    }
    lines
}

/// 读到的一段日志：行 + 每行字节数（含换行）+ 这段在文件里的起点。
struct LogChunk {
    lines: Vec<String>,
    /// 每行占的字节数（含换行）：往回裁剪 / 记账都靠它。
    bytes: Vec<u32>,
    /// 第一行在文件里的字节偏移（开头那半行已经丢掉）。
    start: u64,
    /// 末行没有换行（还在写），下次要从它的行首续读。
    partial: bool,
}

impl LogChunk {
    fn empty(start: u64) -> Self {
        Self {
            lines: Vec::new(),
            bytes: Vec::new(),
            start,
            partial: false,
        }
    }

    fn total_bytes(&self) -> u64 {
        self.bytes.iter().map(|size| u64::from(*size)).sum()
    }
}

/// 从 `[from, to)` 读一段日志，只留完整行，最多 `max_lines` 行（从头部裁）。
///
/// `from > 0` 时先往前让一个字节再读：起点落在行中间就丢掉那半行，起点正好是行首
/// 就把上一行的换行当「半行」丢掉——两种情况都不会误删一整行，偏移也一路跟着走，
/// 下次能从同一行开头续读。
///
/// 文件不存在 / 读不了都当空段（面板刚打开时日志可能还没写）。
fn read_log_chunk(path: &Path, from: u64, to: u64, max_lines: usize) -> LogChunk {
    let Ok(mut file) = std::fs::File::open(path) else {
        return LogChunk::empty(0);
    };
    let len = file.metadata().map(|meta| meta.len()).unwrap_or(0);
    let end = to.min(len);
    let mut start = if from > 0 {
        from.min(end).saturating_sub(1)
    } else {
        0
    };
    if start >= end {
        return LogChunk::empty(end);
    }
    if file.seek(SeekFrom::Start(start)).is_err() {
        return LogChunk::empty(start);
    }
    let mut raw = Vec::new();
    if file
        .by_ref()
        .take(end - start)
        .read_to_end(&mut raw)
        .is_err()
    {
        return LogChunk::empty(start);
    }
    // 末行有没有换行：没有就说明还在写，下次得从它开头重读。
    let partial = raw.last().is_some_and(|&byte| byte != b'\n');
    let entries = split_log_lines(&raw);
    let mut first = 0;
    if start > 0 {
        if !raw.contains(&b'\n') {
            // 整段没有换行：只是一条超长半行，属于更早的那一段。
            return LogChunk::empty(start);
        }
        start += entries[0].1 as u64;
        first = 1;
    }
    while entries.len() - first > max_lines {
        start += entries[first].1 as u64;
        first += 1;
    }
    let mut lines = Vec::with_capacity(entries.len() - first);
    let mut bytes = Vec::with_capacity(entries.len() - first);
    for (line, size) in entries.into_iter().skip(first) {
        lines.push(line);
        bytes.push(u32::try_from(size).unwrap_or(u32::MAX));
    }
    LogChunk {
        lines,
        bytes,
        start,
        partial,
    }
}

// ── 提示条 / 忙碌状态 ──────────────────────────────────────────────

/// 提示条级别（只影响颜色）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Warn,
    Error,
}

/// 底部第一行（操作行）里短暂显示的提示。
#[derive(Debug, Clone)]
pub struct Toast {
    pub text: String,
    pub level: Level,
    pub until: Instant,
}

/// 正在进行的后台动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Busy {
    Starting,
    Stopping,
}

impl Busy {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Starting => "启动中…",
            Self::Stopping => "停止中…",
        }
    }
}

/// 工作线程回来的结果。
#[derive(Debug)]
struct JobDone {
    ok: bool,
    message: String,
}

// ── 启停参数 ───────────────────────────────────────────────────────

/// `run --tui` 带给面板的「起子进程时原样透传」的参数。
///
/// 面板按 `s` 派生的是 `fuyubot run --background`：用户在 `run --tui` 上写的
/// `--api-id` / `--no-tdlib` / `--log-level` 必须跟着走，否则前后两次起的不是同一个 bot。
#[derive(Debug, Clone, Default)]
pub struct SpawnOptions {
    pub api_id: Option<i64>,
    pub no_tdlib: bool,
    pub no_login: bool,
    pub log_level: Option<String>,
    /// `--log-plugin`（可重复）：只看这些插件的日志。
    pub log_plugins: Vec<String>,
    pub trace_updates: bool,
}

// ── App ────────────────────────────────────────────────────────────

/// 运行中实例的资源占用（底部指标行）。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ProcessUsage {
    /// 进程 CPU 百分比（多核上可能超过 100，和 `sysinfo` 的语义一致）。
    pub cpu_percent: f32,
    /// 常驻内存（字节）。
    pub memory_bytes: u64,
}

/// 账号类型可选项（`[telegram].account_type`）。
const ACCOUNT_TYPES: [&str; 2] = ["bot", "user"];
/// 日志级别可选项（`[plugins].log_level`）。
const LOG_LEVEL_NAMES: [&str; 5] = ["error", "warn", "info", "debug", "trace"];

/// 可编辑配置项的类型 / 校验范围。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigKind {
    /// 普通字符串；`allow_empty` 区分「空串=默认」和「必须有值」。
    Text { allow_empty: bool },
    /// 整数（含上下界，闭区间）。
    Integer { min: i64, max: i64 },
    /// 固定枚举，保存前转小写。
    Choice(&'static [&'static str]),
    /// 只读行（凭据状态、插件段等）。
    ReadOnly,
}

impl ConfigKind {
    pub const fn editable(self) -> bool {
        !matches!(self, Self::ReadOnly)
    }
}

/// 配置页的一行：段 + 键 + 生效值 + 来源。
#[derive(Debug, Clone)]
pub struct ConfigItem {
    pub section: String,
    pub key: String,
    /// 编辑框里的初始值（未渲染的原始值）。
    pub value: String,
    /// 表格里展示的生效值（可能是绝对路径 / 带「默认」说明）。
    pub display: String,
    /// `文件` / `默认` / `只读` 之类。
    pub note: String,
    pub kind: ConfigKind,
}

impl ConfigItem {
    pub fn editable(&self) -> bool {
        self.kind.editable()
    }

    pub fn path_label(&self) -> String {
        format!("[{}].{}", self.section, self.key)
    }
}

/// 正在编辑的单个配置项。
#[derive(Debug, Clone)]
pub struct ConfigEdit {
    pub section: String,
    pub key: String,
    pub label: String,
    pub current: String,
    pub note: String,
    pub kind: ConfigKind,
    pub input: String,
    /// 光标在第几个 `char`（不是字节）。
    pub cursor: usize,
    pub error: Option<String>,
}

impl ConfigEdit {
    fn new(item: &ConfigItem) -> Self {
        let cursor = item.value.chars().count();
        Self {
            section: item.section.clone(),
            key: item.key.clone(),
            label: item.path_label(),
            current: item.display.clone(),
            note: item.note.clone(),
            kind: item.kind,
            input: item.value.clone(),
            cursor,
            error: None,
        }
    }

    fn insert(&mut self, ch: char) {
        let byte = byte_at(&self.input, self.cursor);
        self.input.insert(byte, ch);
        self.cursor += 1;
        self.error = None;
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let byte = byte_at(&self.input, self.cursor - 1);
        self.input.remove(byte);
        self.cursor -= 1;
        self.error = None;
    }

    fn delete(&mut self) {
        if self.cursor >= self.input.chars().count() {
            return;
        }
        let byte = byte_at(&self.input, self.cursor);
        self.input.remove(byte);
        self.error = None;
    }

    fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    fn right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.input.chars().count());
    }

    fn home(&mut self) {
        self.cursor = 0;
    }

    fn end(&mut self) {
        self.cursor = self.input.chars().count();
    }
}

/// 第 `char_index` 个字符对应的字节偏移（越界就落在末尾）。
fn byte_at(text: &str, char_index: usize) -> usize {
    text.char_indices()
        .nth(char_index)
        .map(|(byte, _)| byte)
        .unwrap_or(text.len())
}

/// 把配置读成「段 / 键 / 生效值 / 来源」的行，供配置页表格与编辑使用。
fn build_config_items(
    config_path: &Path,
    root: &Path,
    config: &scaffold::Config,
) -> Vec<ConfigItem> {
    let file_exists = config_path.is_file();
    let file_value = if file_exists {
        std::fs::read_to_string(config_path)
            .ok()
            .and_then(|text| toml::from_str::<toml::Value>(&text).ok())
    } else {
        None
    };
    let has = |section: &str, key: &str| {
        file_value
            .as_ref()
            .and_then(toml::Value::as_table)
            .and_then(|table| table.get(section))
            .and_then(toml::Value::as_table)
            .and_then(|table| table.get(key))
            .is_some()
    };
    let note = |section: &str, key: &str| {
        if has(section, key) {
            "文件".to_owned()
        } else {
            "默认".to_owned()
        }
    };

    let mut items = Vec::new();

    let bot_name = config
        .bot
        .name
        .clone()
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_default();
    push_config_item(
        &mut items,
        "bot",
        "name",
        bot_name.clone(),
        if bot_name.is_empty() {
            "（未设置，默认 fuyubot）".to_owned()
        } else {
            bot_name.clone()
        },
        note("bot", "name"),
        ConfigKind::Text { allow_empty: true },
    );

    let account = config.telegram.account_type();
    let account_raw = account
        .as_ref()
        .map(|kind| kind.as_str().to_owned())
        .unwrap_or_else(|_| config.telegram.account_type.clone().unwrap_or_default());
    let (account_display, account_note) = match &account {
        Ok(kind) => (kind.as_str().to_owned(), note("telegram", "account_type")),
        Err(error) => (format!("⚠️ {error}"), "错误".to_owned()),
    };
    push_config_item(
        &mut items,
        "telegram",
        "account_type",
        account_raw,
        account_display,
        account_note,
        ConfigKind::Choice(&ACCOUNT_TYPES),
    );

    let api_id = config.telegram.api_id(&config.bot);
    let api_note = if has("telegram", "api_id") {
        "文件"
    } else if has("bot", "api_id") {
        "旧位置"
    } else {
        "默认"
    };
    push_config_item(
        &mut items,
        "telegram",
        "api_id",
        api_id.to_string(),
        if api_id == 0 {
            "0（未设置）".to_owned()
        } else {
            api_id.to_string()
        },
        api_note.to_owned(),
        ConfigKind::Integer {
            min: 0,
            max: i32::MAX as i64,
        },
    );

    let data_dir = config
        .telegram
        .data_dir
        .clone()
        .unwrap_or_else(|| "data".to_owned());
    push_config_item(
        &mut items,
        "telegram",
        "data_dir",
        data_dir,
        config.telegram.data_dir(root).display().to_string(),
        note("telegram", "data_dir"),
        ConfigKind::Text { allow_empty: false },
    );

    push_config_item(
        &mut items,
        "telegram",
        "api_hash",
        String::new(),
        if scaffold::load_api_hash(config_path).is_some() {
            "已保存（不回显）".to_owned()
        } else {
            "未保存（TG_API_HASH / 交互输入）".to_owned()
        },
        "只读".to_owned(),
        ConfigKind::ReadOnly,
    );

    let plugin_dir = config
        .plugins
        .dir
        .clone()
        .unwrap_or_else(|| "plugins".to_owned());
    push_config_item(
        &mut items,
        "plugins",
        "dir",
        plugin_dir,
        config.plugin_dir(root).display().to_string(),
        note("plugins", "dir"),
        ConfigKind::Text { allow_empty: false },
    );

    let level = config.plugins.log_level();
    let (level_raw, level_display, level_note) = match &level {
        Ok(level) => {
            let name = scaffold::log_level_name(*level).to_owned();
            (name.clone(), name, note("plugins", "log_level"))
        }
        Err(error) => (
            config.plugins.log_level.clone().unwrap_or_default(),
            format!("⚠️ {error}"),
            "错误".to_owned(),
        ),
    };
    push_config_item(
        &mut items,
        "plugins",
        "log_level",
        level_raw,
        level_display,
        level_note,
        ConfigKind::Choice(&LOG_LEVEL_NAMES),
    );

    let tdlib_path = config.tdlib.path.clone().unwrap_or_default();
    push_config_item(
        &mut items,
        "tdlib",
        "path",
        tdlib_path.clone(),
        if tdlib_path.trim().is_empty() {
            "自动查找（TDLIB_TDJSON_PATH → 自带 → exe 同级 → 系统库）".to_owned()
        } else {
            tdlib_path
        },
        note("tdlib", "path"),
        ConfigKind::Text { allow_empty: true },
    );

    let verbosity = config.tdlib.log_verbosity();
    push_config_item(
        &mut items,
        "tdlib",
        "log_verbosity",
        verbosity.to_string(),
        verbosity.to_string(),
        note("tdlib", "log_verbosity"),
        ConfigKind::Integer { min: 0, max: 5 },
    );

    push_config_item(
        &mut items,
        "凭据",
        "token / 手机号 / 密码",
        String::new(),
        "不落盘（TG_BOT_TOKEN 或交互输入）".to_owned(),
        "只读".to_owned(),
        ConfigKind::ReadOnly,
    );

    flatten_plugin_items(&config.plugin, "plugin", &mut items);
    items
}

fn push_config_item(
    items: &mut Vec<ConfigItem>,
    section: &str,
    key: &str,
    value: String,
    display: String,
    note: String,
    kind: ConfigKind,
) {
    items.push(ConfigItem {
        section: section.to_owned(),
        key: key.to_owned(),
        value,
        display,
        note,
        kind,
    });
}

/// 插件段只读展示：一个叶子表里的键各一行；子表继续下钻成 `[plugin.a.b]`。
fn flatten_plugin_items(value: &toml::Value, prefix: &str, out: &mut Vec<ConfigItem>) {
    let Some(table) = value.as_table() else {
        push_config_item(
            out,
            prefix,
            "（值）",
            String::new(),
            value.to_string(),
            "只读".to_owned(),
            ConfigKind::ReadOnly,
        );
        return;
    };

    if table.is_empty() {
        // 根部空表不单独占一行；子段空表给个提示。
        if prefix != "plugin" {
            push_config_item(
                out,
                prefix,
                "（空）",
                String::new(),
                "该段没有键".to_owned(),
                "只读".to_owned(),
                ConfigKind::ReadOnly,
            );
        }
    }

    for (key, value) in table {
        if value.is_table() {
            continue;
        }
        push_config_item(
            out,
            prefix,
            key,
            String::new(),
            if is_secret_key(key) {
                "已保存（不回显）".to_owned()
            } else {
                toml_value_display(value)
            },
            "只读".to_owned(),
            ConfigKind::ReadOnly,
        );
    }
    for (key, value) in table {
        if value.is_table() {
            flatten_plugin_items(value, &format!("{prefix}.{key}"), out);
        }
    }
}

/// 插件段里的常见凭据键在面板里不回显（它本来也是只读的）。
fn is_secret_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    [
        "token", "secret", "password", "api_hash", "api_key", "apikey",
    ]
    .iter()
    .any(|needle| key.contains(needle))
}

fn toml_value_display(value: &toml::Value) -> String {
    match value {
        toml::Value::String(text) if text.is_empty() => "（空字符串）".to_owned(),
        toml::Value::String(text) => text.clone(),
        _ => value.to_string(),
    }
}

/// 把编辑框里的文本校验成 TOML 字面量；错误消息直接展示给用户。
fn validate_config_value(kind: ConfigKind, input: &str) -> Result<String, String> {
    match kind {
        ConfigKind::ReadOnly => Err("该项只读，不能在面板里改".to_owned()),
        ConfigKind::Text { allow_empty } => {
            let value = input.trim();
            if value.is_empty() && !allow_empty {
                return Err("不能为空".to_owned());
            }
            Ok(scaffold::quote_toml_string(value))
        }
        ConfigKind::Integer { min, max } => {
            let range = if max == i64::MAX {
                format!("≥ {min}")
            } else {
                format!("{min}–{max}")
            };
            let value: i64 = input
                .trim()
                .parse()
                .map_err(|_| format!("要填整数（{range}）"))?;
            if value < min || value > max {
                return Err(format!("超出范围（{range}）"));
            }
            Ok(value.to_string())
        }
        ConfigKind::Choice(choices) => {
            let value = input.trim().to_ascii_lowercase();
            if choices.contains(&value.as_str()) {
                Ok(scaffold::quote_toml_string(&value))
            } else {
                Err(format!("只能填 {}", choices.join(" / ")))
            }
        }
    }
}

/// 焦点：按键是操作左侧导航，还是中央工作区。
///
/// `Tab` / `Shift+Tab` 在两个窗口之间切换；工作区里的滚动、日志级别、日志过滤
/// 都只在焦点落到它上面时才响应（边框会高亮）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Navigation,
    Workspace,
}

impl Focus {
    pub const fn toggled(self) -> Self {
        match self {
            Self::Navigation => Self::Workspace,
            Self::Workspace => Self::Navigation,
        }
    }
}

/// 日志过滤弹框里当前选中的行。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterRow {
    Level,
    Source,
}

/// 日志过滤弹框的草稿：`Enter` 才写回 App，`Esc` 原样丢弃。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FilterDraft {
    pub row: FilterRow,
    pub level: LevelFilter,
    pub source: SourceFilter,
}

impl FilterDraft {
    pub const fn new(filter: LogFilter) -> Self {
        Self {
            row: FilterRow::Level,
            level: filter.level,
            source: filter.source,
        }
    }

    pub const fn filter(self) -> LogFilter {
        LogFilter {
            level: self.level,
            source: self.source,
        }
    }

    fn shift_row(&mut self, delta: isize) {
        self.row = if delta.rem_euclid(2) == 0 {
            self.row
        } else {
            match self.row {
                FilterRow::Level => FilterRow::Source,
                FilterRow::Source => FilterRow::Level,
            }
        };
    }

    fn shift_value(&mut self, delta: isize) {
        match self.row {
            FilterRow::Level => self.level = self.level.shifted(delta),
            FilterRow::Source => self.source = self.source.shifted(delta),
        }
    }

    fn reset(&mut self) {
        self.level = LevelFilter::All;
        self.source = SourceFilter::All;
    }
}

/// 面板的全部状态。
pub struct App {
    pub root: PathBuf,
    pub config_path: PathBuf,
    pub plugin_dir: PathBuf,
    pub log_path: PathBuf,

    /// 实例自报的状态（每 `refresh_interval` 重新探测一次）。
    pub state: State,
    /// 配置页的结构化行（生效值 + 来源，支持选中 / 编辑）。
    pub config_items: Vec<ConfigItem>,
    /// 配置读不出来时的错误原文（配置页直接展示）。
    pub config_error: Option<String>,
    pub disk_plugins: Vec<DiskPlugin>,
    /// TUI 插件商店（懒建：第一次进商店页才建，建的时候才拉索引）。
    pub store: Option<store::Store>,
    /// 商店索引地址：`[plugins].registry_url` 或 `FUYUBOT_REGISTRY_URL`。
    pub registry_url: Option<String>,
    /// 插件页选中了磁盘清单里的第几项（开关 `Space` 作用在它身上）。
    pub plugin_selected: usize,
    /// 插件页磁盘清单的滚动位置与视口高度（表头占 1 行）。
    pub plugin_scroll: usize,
    pub plugin_view: usize,
    pub log_lines: Vec<String>,
    /// 每行折成几行（和 `log_lines` 对齐；`0` = 还没算，宽度变了就整表作废）。
    ///
    /// 有它才能只画视口里那几行：窗口可以往回翻到几万行，每帧却不折行整份窗口。
    pub log_heights: Vec<u32>,
    pub log_heights_width: u16,
    /// 日志窗口第一行在文件里的字节偏移（`> 0` = 还能往回翻）。
    pub log_window_start: u64,
    /// 窗口里这些行一共占多少字节（裁剪 / 上限记账用）。
    pub log_window_bytes: u64,
    /// 每行在文件里占的字节数（含换行，和 `log_lines` 对齐）。
    pub log_line_bytes: Vec<u32>,
    /// 跟随末尾时下一次该从哪读（末行是半行时就是它的行首）。
    pub log_resume: u64,
    /// 末行是半行（没有换行结尾，可能还在长）。
    pub log_partial: bool,
    /// 上次看到文件时的大小：没变就不重读（日志页每秒刷新一次）。
    pub log_file_len: u64,
    /// 刚往回加载过一段：下一帧把这一行（`log_lines` 下标）放回视口顶部。
    pub log_anchor: Option<usize>,
    /// 实例名（运行中取状态里的，否则取配置里的）。
    pub bot_name: String,
    /// 配置里的 `[plugins].log_level`（缺省 / 写错都按 `info`）。
    pub configured_level: String,

    pub page: Page,
    /// 当前焦点：导航 / 工作区。`Tab` 切换。
    pub focus: Focus,
    /// 日志页是否跟随末尾（默认跟随）。
    pub follow: bool,
    pub log_scroll: usize,
    /// 日志页选中的行（过滤后的下标；`Some` = 选择模式）。
    pub log_select: Option<usize>,
    /// 折行后的可见行数（`log_visible` 是逻辑行数）。
    pub log_rows: usize,
    /// 每个逻辑行在折行后的起始行号（由绘制过程回填）。
    pub log_row_starts: Vec<usize>,
    pub config_scroll: usize,
    /// 帮助页滚动位置 / 总行数 / 视口高度（窄屏放不下时用 ↑↓ 翻）。
    pub help_scroll: usize,
    pub help_rows: usize,
    pub help_view: usize,
    /// 配置页选中第几行（`config_items` 的下标）。
    pub config_selected: usize,
    /// 正在编辑的配置项（`Some` = 编辑框开着）。
    pub config_edit: Option<ConfigEdit>,
    /// 当前过滤条件；`log_visible` 是过滤后的行数（滚动 / 跟随都按它算）。
    pub log_filter: LogFilter,
    pub log_visible: usize,
    pub filter_picker: Option<FilterDraft>,
    /// 当前视口高度，由绘制过程回填——按键滚动要用它算「页首 / 页尾」。
    pub log_view: usize,
    pub config_view: usize,

    pub busy: Option<Busy>,
    /// 停止确认框开着没有。
    pub confirm_stop: bool,
    /// 清空日志确认框开着没有。
    pub confirm_clear_logs: bool,
    /// 日志级别选择框的游标（`Some` = 开着）。
    pub level_picker: Option<usize>,
    pub toast: Option<Toast>,
    /// 事件循环看到它就收摊。
    pub quit: bool,
    /// 上次刷新时间（状态栏显示「Ns 前」）。
    pub last_refresh: Instant,
    /// 运行中实例的 CPU / 内存；没有实例或取不到进程时是 `None`。
    pub usage: Option<ProcessUsage>,

    refresh_interval: Duration,
    plugin_dir_override: Option<PathBuf>,
    /// `run --tui` 上写的那些 `run` 参数：起后台实例时原样带上。
    spawn: SpawnOptions,
    /// `sysinfo` 的采样器与上次采样的 PID：PID 一变就得重建，
    /// 否则 CPU 百分比会拿旧进程的时间差当基线。
    system: Option<System>,
    usage_pid: Option<u32>,
    last_usage_sample: Instant,
    tx: Sender<JobDone>,
    rx: Receiver<JobDone>,
    /// 登录弹窗（`None` = 没有在走登录流程）。一次只显示一个问题。
    pub login: Option<LoginModal>,
    /// 后台登录线程 → 界面。
    login_rx: Receiver<LoginEvent>,
    login_tx: Sender<LoginEvent>,
    /// 登录线程还活着（模态框关掉、等下一个问题的时候也是 `true`）。
    login_busy: bool,
}

impl App {
    /// 建面板状态。不碰锁、不建目录、不加载插件。
    pub fn new(
        root: PathBuf,
        config_path: PathBuf,
        plugin_dir_override: Option<PathBuf>,
        refresh_interval: Duration,
        spawn: SpawnOptions,
    ) -> Self {
        // `--log-level WARNING` 这种别名先归一成规范名字：选择框按名字定位当前项，
        // 起子进程时也用规范写法（`--tui` / `--background` 两次起的是同一档）。
        let mut spawn = spawn;
        spawn.log_level = spawn
            .log_level
            .as_deref()
            .and_then(|raw| scaffold::parse_log_level(raw).ok())
            .map(|level| scaffold::log_level_name(level).to_owned());
        let (tx, rx) = channel();
        let (login_tx, login_rx) = channel();
        let mut app = Self {
            log_path: instance::log_path(&root),
            plugin_dir: plugin_dir_override
                .clone()
                .unwrap_or_else(|| root.join("plugins")),
            root,
            config_path,
            state: State::Absent,
            config_items: Vec::new(),
            config_error: None,
            disk_plugins: Vec::new(),
            store: None,
            registry_url: None,
            plugin_selected: 0,
            plugin_scroll: 0,
            plugin_view: 10,
            log_lines: Vec::new(),
            log_heights: Vec::new(),
            log_heights_width: 0,
            log_window_start: 0,
            log_window_bytes: 0,
            log_line_bytes: Vec::new(),
            log_resume: 0,
            log_partial: false,
            log_file_len: 0,
            log_anchor: None,
            bot_name: "fuyubot".to_owned(),
            configured_level: "info".to_owned(),
            page: Page::Overview,
            focus: Focus::Navigation,
            follow: true,
            log_scroll: 0,
            log_select: None,
            log_rows: 0,
            log_row_starts: Vec::new(),
            config_scroll: 0,
            help_scroll: 0,
            help_rows: 0,
            help_view: 0,
            config_selected: 0,
            config_edit: None,
            log_filter: LogFilter::default(),
            log_visible: 0,
            filter_picker: None,
            log_view: 18,
            config_view: 18,
            busy: None,
            confirm_stop: false,
            confirm_clear_logs: false,
            level_picker: None,
            toast: None,
            quit: false,
            last_refresh: Instant::now(),
            usage: None,
            refresh_interval,
            plugin_dir_override,
            spawn,
            system: None,
            usage_pid: None,
            last_usage_sample: Instant::now(),
            tx,
            rx,
            login: None,
            login_rx,
            login_tx,
            login_busy: false,
        };
        app.refresh_all();
        app
    }

    /// **真在跑**的实例信息（`Stale` 不算）。
    pub fn running(&self) -> Option<&instance::Info> {
        match &self.state {
            State::Running(Some(info)) => Some(info),
            _ => None,
        }
    }

    /// 运行中实例报回来的插件明细。
    pub fn detail(&self) -> Option<&PluginDetail> {
        self.running().map(|info| &info.detail)
    }

    pub fn can_start(&self) -> bool {
        !matches!(self.state, State::Running(_))
    }

    pub fn can_stop(&self) -> bool {
        matches!(self.state, State::Running(_))
    }

    /// 当前焦点是不是工作区（滚动、日志级别、过滤都靠它）。
    pub fn workspace_focused(&self) -> bool {
        self.focus == Focus::Workspace
    }

    /// `Tab` / `Shift+Tab`：导航 ↔ 工作区。
    pub fn toggle_focus(&mut self) {
        self.focus = self.focus.toggled();
    }

    /// 当前过滤条件下的日志行（顺序不变）。
    pub fn filtered_logs(&self) -> impl Iterator<Item = &String> {
        self.log_lines
            .iter()
            .filter(|line| self.log_filter.matches(line))
    }

    /// 打开日志过滤弹框（只有「日志页 + 工作区焦点」才有意义）。
    pub fn open_filter_picker(&mut self) {
        if self.page == Page::Logs && self.workspace_focused() {
            self.filter_picker = Some(FilterDraft::new(self.log_filter));
        }
    }

    fn apply_filter(&mut self) {
        let Some(draft) = self.filter_picker.take() else {
            return;
        };
        let filter = draft.filter();
        if filter != self.log_filter {
            self.log_filter = filter;
            self.log_visible = self
                .log_lines
                .iter()
                .filter(|line| self.log_filter.matches(line))
                .count();
            // 过滤只改「看哪些行」：窗口不动（翻在中间也不会被拽回末尾），跟随状态也不变。
            self.log_scroll = 0;
            self.log_anchor = None;
            self.log_select = None;
            self.set_toast(
                format!("日志过滤：{}", self.log_filter.summary()),
                Level::Info,
            );
        }
    }

    /// 当前生效的日志级别：运行中实例上报的 > 面板启动参数 / 上次选择 > 配置。
    ///
    /// 运行中**以实例上报为准**：`spawn.log_level` 是「下次启动用什么」，
    /// 控制文件写下去之后还要等实例下一圈才生效，不能拿它冒充当前值。
    pub fn effective_level(&self) -> &str {
        if let State::Running(Some(info)) = &self.state {
            if !info.log_level.is_empty() {
                return &info.log_level;
            }
        }
        self.spawn
            .log_level
            .as_deref()
            .unwrap_or(&self.configured_level)
    }

    /// 打开日志级别选择框（只在日志页有意义）。
    pub fn open_level_picker(&mut self) {
        if self.page != Page::Logs || self.level_picker.is_some() {
            return;
        }
        let cursor = scaffold::LOG_LEVELS
            .iter()
            .position(|(name, _)| *name == self.effective_level())
            .unwrap_or(2); // info
        self.level_picker = Some(cursor);
    }

    fn move_level_picker(&mut self, delta: isize) {
        let Some(cursor) = self.level_picker else {
            return;
        };
        let len = scaffold::LOG_LEVELS.len() as isize;
        self.level_picker = Some((cursor as isize + delta).rem_euclid(len) as usize);
    }

    /// 应用选择：运行中就写控制文件；没跑就留给下一次启动。
    fn apply_level(&mut self, index: usize) {
        let Some((name, _)) = scaffold::LOG_LEVELS.get(index).copied() else {
            return;
        };
        self.level_picker = None;
        // 本会话下一次按 `s` 也沿用这个选择：运行时选择就是面板的启动默认。
        self.spawn.log_level = Some(name.to_owned());
        if self.can_stop() {
            match instance::request_log_level(&self.root, name) {
                Ok(()) => self.set_toast(
                    format!("已请求日志级别 {name}（实例下一圈生效）"),
                    Level::Info,
                ),
                Err(error) => {
                    self.set_toast(format!("写不了日志级别控制文件：{error}"), Level::Error);
                }
            }
        } else {
            self.set_toast(format!("实例没在跑：下次启动用 {name}"), Level::Info);
        }
    }

    /// 提示条还该不该画（`tick` 会清过期的，这里再兜一层）。
    pub fn visible_toast(&self) -> Option<&Toast> {
        self.toast
            .as_ref()
            .filter(|toast| Instant::now() < toast.until)
    }

    /// 刷新所有能读的东西：配置、状态、插件目录、日志。
    ///
    /// 配置文件改错了**不退出面板**：把错误原文放进配置页，用户能边看边改。
    pub fn refresh_all(&mut self) {
        self.reload_config();
        self.state = instance::probe(&self.root);
        self.refresh_usage();
        self.rescan_plugins();
        self.reload_logs();
        self.last_refresh = Instant::now();
    }

    /// 只刷新会变的东西（状态 / 插件目录 / 日志）——每秒一次，不重读配置。
    pub fn refresh_live(&mut self) {
        self.state = instance::probe(&self.root);
        self.refresh_usage();
        self.rescan_plugins();
        self.reload_logs();
        self.last_refresh = Instant::now();
    }

    /// 重扫插件目录，并把选中行夹回新清单里。
    ///
    /// 目录随时可能被外部改动（`fuyubot plugin install`、用户手删），选中行不能
    /// 因此指到空处：夹一下、重算滚动位置就够。
    fn rescan_plugins(&mut self) {
        self.disk_plugins = scan_plugins(&self.plugin_dir);
        self.plugin_select(self.plugin_selected);
    }

    /// 懒建商店页：第一次进商店才建（建的时候才拉索引，不在启动时联网）。
    pub fn ensure_store(&mut self) {
        if self.store.is_some() {
            return;
        }
        self.store = Some(store::Store::new(
            self.plugin_dir.clone(),
            self.registry_url.clone(),
        ));
    }

    /// 采一次运行中实例的 CPU / 内存。
    ///
    /// `sysinfo` 的 CPU 是「两次刷新之间的差值」，所以：
    /// - PID 变了（新实例 / 重启）就重建 `System`，不能拿旧进程的基线；
    /// - 同 PID 的采样间隔太短（操作触发的额外刷新）就跳过，等下一次 1s 节拍。
    fn refresh_usage(&mut self) {
        let Some(pid) = self.running().map(|info| info.pid) else {
            self.usage = None;
            self.usage_pid = None;
            return;
        };
        if self.usage_pid != Some(pid) {
            self.system = System::new().ok();
            self.usage_pid = Some(pid);
            self.usage = None;
        } else if self.usage.is_some()
            && self.last_usage_sample.elapsed() < Duration::from_millis(400)
        {
            return;
        }
        let Some(system) = self.system.as_mut() else {
            self.usage = None;
            return;
        };
        let sysinfo_pid = Pid::from_u32(pid);
        system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[sysinfo_pid]),
            // 只采这一个进程：memory + cpu 就够，别顺手拉 disk / exe / tasks。
            true,
            ProcessRefreshKind::nothing().with_memory().with_cpu(),
        );
        self.usage = system.process(sysinfo_pid).map(|process| ProcessUsage {
            cpu_percent: process.cpu_usage(),
            memory_bytes: process.memory(),
        });
        self.last_usage_sample = Instant::now();
    }

    fn reload_config(&mut self) {
        let previous = self
            .config_items
            .get(self.config_selected)
            .map(|item| (item.section.clone(), item.key.clone()));
        match scaffold::load_config(&self.config_path) {
            Ok(config) => {
                self.bot_name = config
                    .bot
                    .name
                    .clone()
                    .filter(|name| !name.trim().is_empty())
                    .unwrap_or_else(|| "fuyubot".to_owned());
                let new_plugin_dir = if self.plugin_dir_override.is_none() {
                    config.plugin_dir(&self.root)
                } else {
                    self.plugin_dir.clone()
                };
                if new_plugin_dir != self.plugin_dir {
                    // 商店的安装目标跟着插件目录走：换目录就把旧状态丢掉。
                    self.plugin_dir = new_plugin_dir;
                    self.store = None;
                }
                // 配置里写错就按默认 info 显示；错误原文由配置页负责（不在这里吞掉）。
                self.configured_level = config
                    .plugins
                    .log_level()
                    .map(scaffold::log_level_name)
                    .unwrap_or("info")
                    .to_owned();
                // 商店索引地址：环境变量优先（便于临时换商城），其次 `[plugins].registry_url`。
                let registry_url = std::env::var("FUYUBOT_REGISTRY_URL")
                    .ok()
                    .map(|url| url.trim().to_owned())
                    .filter(|url| !url.is_empty())
                    .or_else(|| {
                        config
                            .plugins
                            .registry_url
                            .as_deref()
                            .map(str::trim)
                            .filter(|url| !url.is_empty())
                            .map(str::to_owned)
                    });
                if registry_url != self.registry_url {
                    // 地址变了：旧索引不该继续展示（安装目标可能已经不是同一个商城）。
                    self.store = None;
                    self.registry_url = registry_url;
                }
                let items = build_config_items(&self.config_path, &self.root, &config);
                self.config_error = None;
                self.config_items = items;
                self.config_selected = previous
                    .and_then(|(section, key)| {
                        self.config_items
                            .iter()
                            .position(|item| item.section == section && item.key == key)
                    })
                    .unwrap_or(0)
                    .min(self.config_items.len().saturating_sub(1));
                self.ensure_config_visible();
            }
            Err(error) => {
                self.config_items.clear();
                self.config_selected = 0;
                self.config_scroll = 0;
                self.config_edit = None;
                self.config_error =
                    Some(format!("{error}\n\n改好之后按 r 重新读取（不用退出面板）"));
            }
        }
    }

    fn reload_logs(&mut self) {
        let len = std::fs::metadata(&self.log_path)
            .map(|meta| meta.len())
            .unwrap_or(0);
        if self.log_lines.is_empty() {
            // 还没装过窗口（刚打开 / 刚清空）：装尾部。
            self.load_log_tail();
            return;
        }
        if !self.follow {
            // 翻到中间时窗口**冻结**：不追新日志、也不往后读，内存不再增长。
            // 只认「文件被清空 / 换了一份」——那时回尾部重来。
            if len < self.log_window_start {
                self.load_log_tail();
            }
            return;
        }
        if len < self.log_resume {
            // 截断 / 替换：续读位置都没了，只能重来。
            self.load_log_tail();
            return;
        }
        if len == self.log_file_len {
            return;
        }
        // 一次积了太多新日志（面板挂起过 / 实例突然很吵）：直接重读尾部，
        // 别把中间那一大段全读进内存再裁掉。
        if len - self.log_resume > LOG_TAIL_BYTES {
            self.load_log_tail();
            return;
        }
        // 跟随：从续读位置读新增内容。末行上次是半行的话，先删掉再拼（它长大了）。
        if self.log_partial {
            let cached = self.log_heights.len() == self.log_lines.len();
            self.log_lines.pop();
            if cached {
                self.log_heights.pop();
            }
            if let Some(bytes) = self.log_line_bytes.pop() {
                self.log_window_bytes = self.log_window_bytes.saturating_sub(u64::from(bytes));
            }
        }
        let chunk = read_log_chunk(&self.log_path, self.log_resume, len, usize::MAX);
        self.log_window_bytes += chunk.total_bytes();
        self.log_lines.extend(chunk.lines);
        self.log_line_bytes.extend(chunk.bytes);
        self.log_partial = chunk.partial;
        self.log_resume = self.window_end();
        self.log_file_len = len;
        // 跟随只留尾部一屏的量；再往回看由用户按 PgUp 分段加载。
        self.trim_log_front(LOG_TAIL_LINES);
        self.log_visible = self.filtered_logs().count();
        if let Some(selected) = self.log_select {
            if self.log_visible == 0 {
                self.log_select = None;
            } else if selected >= self.log_visible {
                self.log_select = Some(self.log_visible - 1);
            }
        }
    }

    /// 把窗口换成「文件尾部」（首屏 / 按 End 回到最新 / 文件被换掉）。
    fn load_log_tail(&mut self) {
        let len = std::fs::metadata(&self.log_path)
            .map(|meta| meta.len())
            .unwrap_or(0);
        let from = len.saturating_sub(LOG_TAIL_BYTES);
        let chunk = read_log_chunk(&self.log_path, from, len, LOG_TAIL_LINES);
        self.log_window_start = chunk.start;
        self.log_window_bytes = chunk.total_bytes();
        self.log_lines = chunk.lines;
        self.log_line_bytes = chunk.bytes;
        self.log_partial = chunk.partial;
        self.log_resume = self.window_end();
        self.log_file_len = len;
        self.log_heights.clear();
        self.log_anchor = None;
        self.log_select = None;
        self.log_scroll = 0;
        self.follow = true;
        self.log_visible = self.filtered_logs().count();
    }

    /// 窗口在文件里的字节终点（跟随末尾时就是「下次从哪续读」）。
    fn window_end(&self) -> u64 {
        let end = self.log_window_start + self.log_window_bytes;
        if self.log_partial {
            end.saturating_sub(self.log_line_bytes.last().copied().unwrap_or(0) as u64)
        } else {
            end
        }
    }

    /// 从窗口**头部**丢行：跟随末尾时只留尾部一屏（行数与字节都设上限）。
    ///
    /// 只看行数不够——TDLib Update 一行就可能很大，字节也要收到 `LOG_TAIL_BYTES`。
    fn trim_log_front(&mut self, keep: usize) {
        let mut drop = self.log_lines.len().saturating_sub(keep);
        let mut bytes = self.log_window_bytes;
        for size in &self.log_line_bytes[..drop] {
            bytes -= u64::from(*size);
        }
        // 再按字节收：至少留最后一行（否则超长行会把自己也裁掉）。
        while drop + 1 < self.log_lines.len() && bytes > LOG_TAIL_BYTES {
            bytes -= u64::from(self.log_line_bytes[drop]);
            drop += 1;
        }
        if drop == 0 {
            return;
        }
        let dropped = self.log_window_bytes - bytes;
        self.log_window_start += dropped;
        self.log_window_bytes = bytes;
        self.log_lines.drain(..drop);
        self.log_line_bytes.drain(..drop);
        let heights = drop.min(self.log_heights.len());
        self.log_heights.drain(..heights);
        if let Some(anchor) = self.log_anchor {
            self.log_anchor = anchor.checked_sub(drop);
        }
    }

    /// 事件循环每转一圈调一次：收工作线程结果、清过期提示、到点刷新。
    pub fn tick(&mut self) {
        self.poll_login();
        // 商店的后台线程（索引 / 安装）要靠这里回投；离开商店页也要排空，
        // 否则安装完成后状态会一直停在「进行中」。
        if let Some(store) = self.store.as_mut() {
            store.tick();
        }
        while let Ok(done) = self.rx.try_recv() {
            self.busy = None;
            let level = if done.ok { Level::Info } else { Level::Error };
            self.set_toast(done.message, level);
            // 动作完成后立刻重探一次，别让状态栏停在旧状态。
            self.refresh_live();
        }
        if self
            .toast
            .as_ref()
            .is_some_and(|toast| Instant::now() >= toast.until)
        {
            self.toast = None;
        }
        if self.last_refresh.elapsed() >= self.refresh_interval {
            self.refresh_live();
        }
    }

    pub fn set_toast(&mut self, text: impl Into<String>, level: Level) {
        self.toast = Some(Toast {
            text: text.into(),
            level,
            until: Instant::now() + Duration::from_secs(TOAST_SECS),
        });
    }

    // ── 登录（只登录；面板自己不接 TDLib）────────────────────────────

    /// 起一次「只登录」：后台线程跑 `fuyubot-app` 的 boot + login，问题弹成输入框。
    ///
    /// 为什么必须在实例停着的时候做：TDLib 的数据库同一时刻只允许一个进程打开（约束 47）。
    pub fn start_login(&mut self) {
        if self.login_busy {
            self.set_toast("登录进行中…", Level::Info);
            return;
        }
        if matches!(self.state, State::Running(_)) {
            self.set_toast(
                "实例在跑：先停止再登录（TDLib 数据库只能一个进程开）",
                Level::Info,
            );
            return;
        }
        let options = match self.login_options() {
            Ok(options) => options,
            Err(error) => {
                self.set_toast(error, Level::Info);
                return;
            }
        };
        self.login_busy = true;
        self.set_toast(
            "开始登录：登完 session 落在 data/，之后启动实例会直接 Ready",
            Level::Info,
        );
        let tx = self.login_tx.clone();
        let _ = login::spawn_login(options, tx);
    }

    /// `start_login` 的输入：目录 + 配置树 + TDLib 设置 + TUI 宿主。
    ///
    /// ⚠️ 凭据**必须已经在环境 / 配置里**：这里绝不调用会读 stdin 的交互路径
    /// （终端此刻在 alternate screen 里，读 stdin 等于把界面搞花）。
    fn login_options(&self) -> Result<fuyubot_app::AppOptions, String> {
        let config = scaffold::load_config(&self.config_path)?;

        let api_id = self.spawn.api_id.or(config.telegram.api_id).unwrap_or(0);
        let api_hash = std::env::var("TG_API_HASH")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| scaffold::load_api_hash(&self.config_path));
        let Some(api_hash) = api_hash else {
            return Err("缺 api_hash：先设 TG_API_HASH（或跑一次 `fuyubot login`）".to_owned());
        };
        if api_id <= 0 {
            return Err(
                "缺 api_id：设 TG_API_ID 或写进 fuyubot.toml 的 [telegram].api_id".to_owned(),
            );
        }

        let data_dir = self
            .root
            .join(config.telegram.data_dir.as_deref().unwrap_or("data"));
        let parameters = fuyubot_tdlib::TdlibParameters::new(data_dir, api_id as i32, api_hash);
        let account = config.telegram.account_type()?;
        let log_level = match &self.spawn.log_level {
            Some(raw) => scaffold::parse_log_level(raw)?,
            None => config.plugins.log_level()?,
        };

        Ok(fuyubot_app::AppOptions {
            data_dir: parameters.data_dir.clone(),
            plugin_dir: config.plugin_dir(&self.root),
            log_level,
            log_plugins: Vec::new(),
            config: config.to_json()?,
            config_path: Some(self.config_path.clone()),
            tdjson_path: config.tdlib.path.as_deref().map(PathBuf::from),
            tdlib: Some(fuyubot_app::TdlibSettings {
                parameters,
                account,
                bot_token: std::env::var("TG_BOT_TOKEN")
                    .ok()
                    .filter(|value| !value.is_empty()),
                log_verbosity: Some(config.tdlib.log_verbosity()),
                login_budget: fuyubot_tdlib::LOGIN_BUDGET,
                login: true,
            }),
            host: login::TuiHost::new(self.login_tx.clone()),
        })
    }

    /// 登录弹窗开着时的按键：只管输入。
    fn on_login_key(&mut self, key: KeyEvent) {
        let Some(mut modal) = self.login.take() else {
            return;
        };
        match key.code {
            KeyCode::Esc => {
                modal.cancel();
                self.login_busy = false;
                self.set_toast("已取消登录", Level::Info);
            }
            KeyCode::Enter => {
                modal.submit();
                // 下一个问题会由 `poll_login` 再弹；这里不设 busy=false。
            }
            KeyCode::Backspace => modal.backspace(),
            KeyCode::Char(character) => modal.push(character),
            _ => {
                self.login = Some(modal);
            }
        }
    }

    fn poll_login(&mut self) {
        // 先把事件取干净再处理：处理里会 `&mut self`，不先收集会和 `login_rx` 的借用打架。
        let events: Vec<LoginEvent> =
            std::iter::from_fn(|| self.login_rx.try_recv().ok()).collect();
        for event in events {
            match event {
                LoginEvent::Ask {
                    kind,
                    prompt,
                    reply,
                } => {
                    self.login = Some(LoginModal::new(kind, prompt, reply));
                }
                LoginEvent::State(state, detail) => {
                    if state == fuyubot_app::AppState::Ready {
                        self.set_toast("登录完成", Level::Info);
                    } else {
                        self.set_toast(detail, Level::Info);
                    }
                }
                LoginEvent::Done(result) => {
                    self.login_busy = false;
                    self.login = None;
                    match result {
                        Ok(()) => {
                            self.set_toast("登录成功：会话已落盘，可以启动实例了", Level::Info)
                        }
                        Err(error) => self.set_toast(format!("登录失败：{error}"), Level::Info),
                    }
                    self.refresh_all();
                }
            }
        }
    }

    // ── 按键 ──────────────────────────────────────────────────────

    /// 处理一次按键。只有 `Press` 算数：Windows 上松开也会来一条，不滤就会触发两次。
    pub fn on_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
            self.quit = true;
            return;
        }

        // 登录弹窗是模态的：开着的时候只认它自己的按键（输入 / 提交 / 取消）。
        if self.login.is_some() {
            self.on_login_key(key);
            return;
        }

        // 确认框开着时，只认 y / n / Esc。
        if self.confirm_stop {
            match key.code {
                KeyCode::Char('y' | 'Y') | KeyCode::Enter => {
                    self.confirm_stop = false;
                    self.stop();
                }
                KeyCode::Char('n' | 'N') | KeyCode::Esc => {
                    self.confirm_stop = false;
                    self.set_toast("已取消", Level::Info);
                }
                _ => {}
            }
            return;
        }

        // 清空日志确认框：只认 y / n / Esc。
        if self.confirm_clear_logs {
            match key.code {
                KeyCode::Char('y' | 'Y') | KeyCode::Enter => self.clear_logs(),
                KeyCode::Char('n' | 'N') | KeyCode::Esc => {
                    self.confirm_clear_logs = false;
                    self.set_toast("已取消", Level::Info);
                }
                _ => {}
            }
            return;
        }

        // 日志级别选择框开着：方向键 / 数字选，Enter 应用，Esc（或再按 l）取消。
        if let Some(cursor) = self.level_picker {
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => self.move_level_picker(-1),
                KeyCode::Down | KeyCode::Char('j') => self.move_level_picker(1),
                KeyCode::Char(digit @ '1'..='5') => {
                    self.apply_level((digit as u8 - b'1') as usize);
                }
                KeyCode::Enter => self.apply_level(cursor),
                KeyCode::Esc | KeyCode::Char('l' | 'q') => self.level_picker = None,
                _ => {}
            }
            return;
        }

        // 日志过滤弹框：↑↓ 选行、←→ 改值、c 重置、Enter 应用。
        if self.filter_picker.is_some() {
            let (mut apply, mut close) = (false, false);
            {
                let draft = self.filter_picker.as_mut().expect("上面刚判过 is_some");
                match key.code {
                    KeyCode::Up | KeyCode::Char('k') => draft.shift_row(-1),
                    KeyCode::Down | KeyCode::Char('j') => draft.shift_row(1),
                    KeyCode::Left | KeyCode::Char('h') => draft.shift_value(-1),
                    KeyCode::Right | KeyCode::Char('l') => draft.shift_value(1),
                    KeyCode::Char('c') => draft.reset(),
                    KeyCode::Enter => apply = true,
                    KeyCode::Esc | KeyCode::Char('F' | 'f' | 'q') => close = true,
                    _ => {}
                }
            }
            if apply {
                self.apply_filter();
            } else if close {
                self.filter_picker = None;
            }
            return;
        }

        // 商店页：按键先交给它（搜索输入要吃掉普通字符；`q` / `Tab` 仍交回全局）。
        if self.page == Page::Store && self.workspace_focused() {
            self.ensure_store();
            let outcome = self
                .store
                .as_mut()
                .map(|store| store.handle_key(key))
                .unwrap_or(StoreKey::Ignored);
            match outcome {
                StoreKey::Consumed => return,
                StoreKey::Close => {
                    self.page = Page::Plugins;
                    return;
                }
                // 没处理的（q 退出、Tab 换焦点…）继续走下面的全局按键。
                StoreKey::Ignored => {}
            }
        }

        // 配置编辑框：Enter 保存、Esc 取消、←→/Home/End 移光标。
        if self.config_edit.is_some() {
            let (mut save, mut close) = (false, false);
            {
                let edit = self.config_edit.as_mut().expect("上面刚判过 is_some");
                match key.code {
                    KeyCode::Enter => save = true,
                    KeyCode::Esc => close = true,
                    KeyCode::Backspace => edit.backspace(),
                    KeyCode::Delete => edit.delete(),
                    KeyCode::Left => edit.left(),
                    KeyCode::Right => edit.right(),
                    KeyCode::Home => edit.home(),
                    KeyCode::End => edit.end(),
                    KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        edit.input.clear();
                        edit.cursor = 0;
                        edit.error = None;
                    }
                    KeyCode::Char(ch)
                        if !key.modifiers.contains(KeyModifiers::CONTROL)
                            && !key.modifiers.contains(KeyModifiers::ALT)
                            && !ch.is_control() =>
                    {
                        edit.insert(ch);
                    }
                    _ => {}
                }
            }
            if save {
                self.save_config_edit();
            } else if close {
                self.config_edit = None;
            }
            return;
        }

        // 日志选择模式：↑↓ 选行，y / Enter 复制，Esc 退出选择。
        if self.page == Page::Logs && self.workspace_focused() && self.log_select.is_some() {
            let (mut copy, mut close) = (false, false);
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => self.move_log_select(-1),
                KeyCode::Down | KeyCode::Char('j') => self.move_log_select(1),
                KeyCode::PageUp => self.move_log_select(-10),
                KeyCode::PageDown => self.move_log_select(10),
                KeyCode::Home => self.select_log_home(),
                KeyCode::End => self.select_log_end(),
                KeyCode::Enter | KeyCode::Char('y') => copy = true,
                KeyCode::Esc => close = true,
                KeyCode::Char('q') => self.quit = true,
                _ => {}
            }
            if copy {
                self.copy_selected_log();
            } else if close {
                self.log_select = None;
            }
            return;
        }

        // `Tab` / `Shift+Tab` 在「导航」和「工作区」之间切换焦点。
        // `Enter` 从导航进入工作区；`Esc` 从工作区退回导航（在导航上才是退出）。
        match key.code {
            KeyCode::Tab | KeyCode::BackTab => {
                self.toggle_focus();
                return;
            }
            KeyCode::Enter if self.focus == Focus::Navigation => {
                self.focus = Focus::Workspace;
                return;
            }
            KeyCode::Esc if self.focus == Focus::Workspace => {
                self.focus = Focus::Navigation;
                return;
            }
            _ => {}
        }

        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('?') => self.page = Page::Help,
            // 焦点在导航：↑↓ 换页；焦点在工作区：↑↓ 滚动当前页。
            KeyCode::Up | KeyCode::Char('k') => {
                if self.workspace_focused() {
                    self.scroll_up(1);
                } else {
                    self.page = self.page.prev();
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.workspace_focused() {
                    self.scroll_down(1);
                } else {
                    self.page = self.page.next();
                }
            }
            // 顶部横向标签：←→ 直接换页（手机上没有方向键时也能用）；工作区里 l 另有用途。
            KeyCode::Left => self.page = self.page.prev(),
            KeyCode::Right => self.page = self.page.next(),
            KeyCode::Char('h') if !self.workspace_focused() => self.page = self.page.prev(),
            KeyCode::Char('l') if !self.workspace_focused() => self.page = self.page.next(),
            KeyCode::Char(digit @ '1'..='6') => {
                self.page = Page::ALL[(digit as u8 - b'1') as usize];
            }
            KeyCode::Enter if self.page == Page::Logs && self.workspace_focused() => {
                self.enter_log_select();
            }
            KeyCode::Enter | KeyCode::Char('e')
                if self.page == Page::Config && self.workspace_focused() =>
            {
                self.open_config_edit();
            }
            // 插件页：`Space` 启用 / 禁用选中的那个（禁用 = 文件加 `.disable` 后缀）。
            KeyCode::Char(' ') if self.page == Page::Plugins && self.workspace_focused() => {
                self.toggle_plugin();
            }
            KeyCode::Enter if self.page == Page::Plugins && self.workspace_focused() => {
                self.toggle_plugin();
            }
            KeyCode::Char('s') => self.start(),
            KeyCode::Char('x') => self.ask_stop(),
            // 大写 L：只在实例停着时有效（TDLib 数据库只能一个进程开）。
            KeyCode::Char('L') => self.start_login(),
            // 日志页的专属操作只在工作区获得焦点时响应。
            KeyCode::Char('c') if self.page == Page::Logs && self.workspace_focused() => {
                self.ask_clear_logs();
            }
            KeyCode::Char('l') if self.page == Page::Logs && self.workspace_focused() => {
                self.open_level_picker();
            }
            KeyCode::Char('F') if self.page == Page::Logs && self.workspace_focused() => {
                self.open_filter_picker();
            }
            KeyCode::Char('r') => {
                self.refresh_all();
                self.set_toast("已刷新", Level::Info);
            }
            KeyCode::Char('f') if self.page == Page::Logs && self.workspace_focused() => {
                if self.follow {
                    // 关跟着 = 冻结当前窗口（实例继续写也不往里塞，内存不涨）。
                    self.follow = false;
                    self.set_toast("日志跟随：关（End 回到最新）", Level::Info);
                } else {
                    // 开跟随 = 回到最新：窗口可能已经被往回翻成中间一段了。
                    self.load_log_tail();
                    self.set_toast("日志跟随：开（已回到最新）", Level::Info);
                }
            }
            KeyCode::PageUp if self.workspace_focused() => self.scroll_up(10),
            KeyCode::PageDown if self.workspace_focused() => self.scroll_down(10),
            KeyCode::Home if self.workspace_focused() => self.scroll_home(),
            KeyCode::End if self.workspace_focused() => self.scroll_end(),
            _ => {}
        }
    }

    fn scroll_up(&mut self, amount: usize) {
        match self.page {
            Page::Logs => {
                let offset = self.log_offset();
                // 已经在窗口顶部再往上翻 = 要更早的日志：按段回读，视图由 `log_anchor` 锚住。
                let loaded = offset == 0 && self.load_earlier();
                if !loaded {
                    self.log_scroll = offset.saturating_sub(amount);
                }
                self.follow = false;
            }
            Page::Config => self.config_move(-(amount as isize)),
            Page::Plugins => self.plugin_move(-(amount as isize)),
            Page::Help => {
                let max = self.help_rows.saturating_sub(self.help_view.max(1));
                self.help_scroll = self.help_scroll.saturating_sub(amount).min(max);
            }
            _ => {}
        }
    }

    fn scroll_down(&mut self, amount: usize) {
        match self.page {
            Page::Logs => {
                let max = self.log_rows.saturating_sub(self.log_view.max(1));
                self.log_scroll = self.log_offset().saturating_add(amount).min(max);
                self.follow = false;
            }
            Page::Config => self.config_move(amount as isize),
            Page::Plugins => self.plugin_move(amount as isize),
            Page::Help => {
                let max = self.help_rows.saturating_sub(self.help_view.max(1));
                self.help_scroll = self.help_scroll.saturating_add(amount).min(max);
            }
            _ => {}
        }
    }

    fn scroll_home(&mut self) {
        match self.page {
            Page::Logs => {
                // 已在窗口顶部：再按 Home = 再往回一段（跳到刚加载那段的最前面）。
                if self.log_offset() == 0 {
                    let _ = self.load_earlier();
                    self.log_anchor = None;
                }
                self.log_scroll = 0;
                self.follow = false;
            }
            Page::Config => self.config_select(0),
            Page::Plugins => self.plugin_select(0),
            Page::Help => self.help_scroll = 0,
            _ => {}
        }
    }

    fn scroll_end(&mut self) {
        match self.page {
            Page::Logs => {
                // 回到最新：窗口可能已经被往回翻成中间一段，End 重读尾部并恢复跟随。
                self.load_log_tail();
                self.set_toast("已回到日志末尾", Level::Info);
            }
            Page::Config => self.config_select(self.config_items.len().saturating_sub(1)),
            Page::Plugins => self.plugin_select(self.disk_plugins.len().saturating_sub(1)),
            Page::Help => {
                self.help_scroll = self.help_rows.saturating_sub(self.help_view.max(1));
            }
            _ => {}
        }
    }

    /// 配置页选中行上下移动。
    pub fn config_move(&mut self, delta: isize) {
        if self.config_items.is_empty() {
            return;
        }
        let last = self.config_items.len() - 1;
        let next = (self.config_selected as isize + delta).clamp(0, last as isize) as usize;
        self.config_selected = next;
        self.ensure_config_visible();
    }

    /// 插件页选中行上下移动（磁盘清单那一半；上半是实例自报的，只读）。
    pub fn plugin_move(&mut self, delta: isize) {
        if self.disk_plugins.is_empty() {
            return;
        }
        let last = self.disk_plugins.len() - 1;
        let next = (self.plugin_selected as isize + delta).clamp(0, last as isize) as usize;
        self.plugin_select(next);
    }

    fn plugin_select(&mut self, index: usize) {
        if self.disk_plugins.is_empty() {
            self.plugin_selected = 0;
            self.plugin_scroll = 0;
            return;
        }
        self.plugin_selected = index.min(self.disk_plugins.len() - 1);
        self.ensure_plugin_visible();
    }

    /// 让选中行留在表格视口里（表头占 1 行）。
    pub(crate) fn ensure_plugin_visible(&mut self) {
        let visible = self.plugin_view.saturating_sub(1).max(1);
        if self.plugin_selected < self.plugin_scroll {
            self.plugin_scroll = self.plugin_selected;
        } else if self.plugin_selected >= self.plugin_scroll + visible {
            self.plugin_scroll = self.plugin_selected + 1 - visible;
        }
        let max = self.disk_plugins.len().saturating_sub(visible);
        self.plugin_scroll = self.plugin_scroll.min(max);
    }

    /// 当前该从第几行开始画插件清单（表头另占一行）。
    pub fn plugin_offset(&self) -> usize {
        let visible = self.plugin_view.saturating_sub(1).max(1);
        let max = self.disk_plugins.len().saturating_sub(visible);
        self.plugin_scroll.min(max)
    }

    /// 选中的那个磁盘插件。
    pub fn selected_plugin(&self) -> Option<&DiskPlugin> {
        self.disk_plugins.get(self.plugin_selected)
    }

    /// 概览页那一行：`4 项`（都启用）/ `4 项（1 个已禁用）`。
    pub fn plugin_dir_summary(&self) -> String {
        let disabled = self
            .disk_plugins
            .iter()
            .filter(|plugin| !plugin.enabled)
            .count();
        if disabled == 0 {
            format!("{} 项", self.disk_plugins.len())
        } else {
            format!("{} 项（{disabled} 个已禁用）", self.disk_plugins.len())
        }
    }

    /// `Space`：把选中的插件启用 / 禁用。
    ///
    /// 两种形态、两条路：
    /// - **文件**（旧形态 dylib）：改名，`foo.dll` ↔ `foo.dll.disable`；
    /// - **目录**（插件包 / 脚本目录）：整个挪到 `plugins/.disabled/<name>/`，再按一次
    ///   挪回原来的家（包回 `plugins/<name>/`，脚本回 `plugins/scripts/<name>/`）；
    ///   旧面板留下的 `<name>.disable/` 目录仍然按「去掉后缀」恢复。
    ///
    /// 加载器只扫启用态的名字，所以改完下次 `run` 就不会再碰它——没有第二份清单要
    /// 维护，也没有残留状态。
    ///
    /// ⚠️ 运行中的实例**已经把插件映射进进程了**，改名 / 挪目录不会让它当场卸载：
    /// 停止再启动才生效（见提示语）。Windows 上动态库被占用时改名会失败，
    /// 那种情况把错误原文报出来就行，别假装成功。
    pub fn toggle_plugin(&mut self) {
        let Some(plugin) = self.selected_plugin().cloned() else {
            return;
        };
        if !plugin.toggleable() {
            self.set_toast("这一项没有开关，手动改插件目录吧", Level::Info);
            return;
        }
        let disabled_root = self.plugin_dir.join(fuyubot_loader::DISABLED_DIR);
        let (target, enabling) = if plugin.enabled {
            if plugin.path.is_dir() {
                (disabled_root.join(&plugin.name), false)
            } else {
                (fuyubot_loader::disabled_path(&plugin.path), false)
            }
        } else if plugin.path.is_dir() {
            let from_disabled = plugin.path.parent() == Some(disabled_root.as_path());
            if from_disabled {
                let parent = match plugin.kind {
                    // 包回 `plugins/<id>/`；脚本回 `plugins/scripts/<id>/`。
                    PluginKind::Bundle => self.plugin_dir.clone(),
                    PluginKind::Script => self.plugin_dir.join(fuyubot_loader::SCRIPTS_DIR),
                    PluginKind::Native => self.plugin_dir.clone(),
                };
                (parent.join(&plugin.name), true)
            } else {
                // 旧面板的 `<name>.disable/` 目录：去掉后缀就是启用态。
                (fuyubot_loader::enabled_path(&plugin.path), true)
            }
        } else {
            (fuyubot_loader::enabled_path(&plugin.path), true)
        };
        if target.exists() {
            self.set_toast(
                format!(
                    "{} 已经存在了——先把那个位置挪开再开关",
                    target.file_name().unwrap_or_default().to_string_lossy()
                ),
                Level::Warn,
            );
            return;
        }
        // 禁用目录可能还不存在（第一次禁用）；目标父目录也可能因为脚本根被删而没了。
        if !plugin.enabled && plugin.path.is_dir() {
            if let Some(parent) = target.parent() {
                if let Err(error) = std::fs::create_dir_all(parent) {
                    self.set_toast(
                        format!("建不了 {}: {error}", parent.display()),
                        Level::Error,
                    );
                    return;
                }
            }
        }
        if plugin.enabled && plugin.path.is_dir() && !disabled_root.is_dir() {
            if let Err(error) = std::fs::create_dir_all(&disabled_root) {
                self.set_toast(
                    format!("建不了 {}: {error}", disabled_root.display()),
                    Level::Error,
                );
                return;
            }
        }
        match std::fs::rename(&plugin.path, &target) {
            Ok(()) => {
                let name = plugin.name.clone();
                // 立刻重扫：`Space` 连按两下要能来回切，不能等下一次 tick。
                self.rescan_plugins();
                self.set_toast(
                    format!(
                        "已{}插件 {name}（重启后生效）",
                        if enabling { "启用" } else { "禁用" }
                    ),
                    Level::Info,
                );
            }
            Err(error) => {
                let hint = if self.can_stop() && plugin.enabled {
                    "（实例在跑：Windows 上库被占用时改名会失败，先 x 停止实例）"
                } else {
                    ""
                };
                self.set_toast(format!("开关失败：{error}{hint}"), Level::Error);
            }
        }
    }

    fn config_select(&mut self, index: usize) {
        if self.config_items.is_empty() {
            return;
        }
        self.config_selected = index.min(self.config_items.len() - 1);
        self.ensure_config_visible();
    }

    /// 让选中行留在表格视口里（表头占 1 行）。
    pub(crate) fn ensure_config_visible(&mut self) {
        let visible = self.config_view.saturating_sub(1).max(1);
        if self.config_selected < self.config_scroll {
            self.config_scroll = self.config_selected;
        } else if self.config_selected >= self.config_scroll + visible {
            self.config_scroll = self.config_selected + 1 - visible;
        }
        let max = self.config_items.len().saturating_sub(visible);
        self.config_scroll = self.config_scroll.min(max);
    }

    /// 打开当前选中项的编辑框（只读项只给提示）。
    pub fn open_config_edit(&mut self) {
        let Some(item) = self.config_items.get(self.config_selected) else {
            return;
        };
        if !item.editable() {
            self.set_toast(format!("{} 是只读项", item.path_label()), Level::Warn);
            return;
        }
        self.config_edit = Some(ConfigEdit::new(item));
    }

    /// 校验并写回当前编辑框；校验/写盘失败时保持编辑框打开，把原因显示在里面。
    fn save_config_edit(&mut self) {
        let Some(edit) = self.config_edit.as_ref() else {
            return;
        };
        let section = edit.section.clone();
        let key = edit.key.clone();
        let label = edit.label.clone();
        let kind = edit.kind;
        let input = edit.input.clone();
        if !self.config_path.is_file() {
            if let Some(edit) = self.config_edit.as_mut() {
                edit.error = Some(
                    "配置文件不存在：先跑 `fuyubot init`（或直接 `fuyubot run` 自动生成）"
                        .to_owned(),
                );
            }
            return;
        }
        let literal = match validate_config_value(kind, &input) {
            Ok(literal) => literal,
            Err(error) => {
                if let Some(edit) = self.config_edit.as_mut() {
                    edit.error = Some(error);
                }
                return;
            }
        };
        match scaffold::set_config_value(&self.config_path, &section, &key, &literal) {
            Ok(changed) => {
                self.config_edit = None;
                self.reload_config();
                let suffix = if self.can_stop() {
                    "（实例运行中，重启后生效）"
                } else {
                    ""
                };
                self.set_toast(
                    if changed {
                        format!("已保存 {label}{suffix}")
                    } else {
                        format!("{label} 没有变化")
                    },
                    Level::Info,
                );
            }
            Err(error) => {
                if let Some(edit) = self.config_edit.as_mut() {
                    edit.error = Some(format!("写不进去：{error}"));
                }
            }
        }
    }

    /// 清空日志前先确认；日志页 + 工作区焦点才有意义。
    pub fn ask_clear_logs(&mut self) {
        if self.page != Page::Logs || !self.workspace_focused() {
            return;
        }
        if self.log_lines.is_empty() && !self.log_path.exists() {
            self.set_toast("日志已经是空的", Level::Info);
            return;
        }
        self.confirm_clear_logs = true;
    }

    /// 截断 `logs/fuyubot.log` 并丢掉面板里的旧行；运行中的实例会继续追加新行。
    fn clear_logs(&mut self) {
        self.confirm_clear_logs = false;
        if let Some(parent) = self.log_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&self.log_path)
        {
            Ok(_) => {
                self.log_lines.clear();
                self.log_line_bytes.clear();
                self.log_heights.clear();
                self.log_file_len = 0;
                self.log_window_start = 0;
                self.log_window_bytes = 0;
                self.log_resume = 0;
                self.log_partial = false;
                self.log_anchor = None;
                self.log_visible = 0;
                self.log_rows = 0;
                self.log_row_starts.clear();
                self.log_scroll = 0;
                self.log_select = None;
                self.follow = true;
                self.set_toast(
                    if self.can_stop() {
                        "日志已清空（运行中的实例会继续写新日志）"
                    } else {
                        "日志已清空"
                    },
                    Level::Info,
                );
            }
            Err(error) => {
                self.set_toast(format!("清不了日志文件：{error}"), Level::Error);
            }
        }
    }

    /// 往回加载一段更早的日志（**分段读**：不会为了看一点旧日志把整份文件读一遍）。
    ///
    /// 返回是否真的加载到了东西；`false` = 已经到文件开头。
    /// 窗口超过上限时从**尾部**丢（滑动窗口），所以一直往上翻内存也不会涨。
    /// 加载后视图由 `log_anchor` 在下一帧锚回原来的那一行，不会突然跳走。
    pub fn load_earlier(&mut self) -> bool {
        if self.log_window_start == 0 {
            return false;
        }
        let anchor = self.log_anchor_index();
        let end = self.log_window_start;
        let mut from = end.saturating_sub(LOG_SEGMENT_BYTES);
        loop {
            // 一整条行比窗口字节上限还长：不再往前找，别把内存撑爆。
            if end.saturating_sub(from) > LOG_WINDOW_MAX_BYTES {
                return false;
            }
            let chunk = read_log_chunk(&self.log_path, from, end, usize::MAX);
            if !chunk.lines.is_empty() {
                self.prepend_logs(chunk, anchor);
                self.trim_log_back();
                return true;
            }
            if from == 0 {
                // 窗口第一行之前再没有换行（那一段是一条超长的行）：到头了。
                self.log_window_start = 0;
                return false;
            }
            from = from.saturating_sub(LOG_SEGMENT_BYTES);
        }
    }

    /// 把更早的一段落塞到窗口前面；`anchor` 是加载前视口顶部那一行（`log_lines` 下标）。
    fn prepend_logs(&mut self, chunk: LogChunk, anchor: Option<usize>) {
        let added = chunk.lines.len();
        let added_bytes = chunk.total_bytes();
        let added_visible = chunk
            .lines
            .iter()
            .filter(|line| self.log_filter.matches(line))
            .count();
        self.log_window_start = chunk.start;
        self.log_window_bytes += added_bytes;
        self.log_lines.splice(0..0, chunk.lines);
        self.log_line_bytes.splice(0..0, chunk.bytes);
        // 新行还没算折行高度：补 0 占位，下一帧按需补算。
        self.log_heights
            .splice(0..0, std::iter::repeat_n(0u32, added));
        self.log_visible = self.filtered_logs().count();
        // 往前翻就暂停跟随：窗口冻结，实例继续写也不往这个窗口里塞。
        self.follow = false;
        self.log_anchor = anchor.map(|anchor| anchor + added);
        if let Some(selected) = self.log_select {
            self.log_select = Some(selected + added_visible);
        }
    }

    /// 窗口超过上限就从**尾部**丢：翻得再远，内存也就一窗。
    ///
    /// 尾部被丢掉之后这个窗口就不再接到文件末尾了——想回去按 `End`（重读尾部）。
    fn trim_log_back(&mut self) {
        let mut keep = self.log_lines.len();
        let mut bytes = self.log_window_bytes;
        while keep > 0 && (keep > LOG_WINDOW_MAX_LINES || bytes > LOG_WINDOW_MAX_BYTES) {
            keep -= 1;
            bytes -= u64::from(self.log_line_bytes[keep]);
        }
        if keep == self.log_lines.len() {
            return;
        }
        self.log_lines.truncate(keep);
        self.log_line_bytes.truncate(keep);
        if self.log_heights.len() > keep {
            self.log_heights.truncate(keep);
        }
        self.log_window_bytes = bytes;
        self.log_visible = self.filtered_logs().count();
        if let Some(selected) = self.log_select {
            if selected >= self.log_visible {
                self.log_select = None;
            }
        }
    }

    /// 视口顶部那一行在 `log_lines` 里的下标（往回加载后靠它把视图锚住）。
    ///
    /// 还没画过（`log_row_starts` 空）时没有锚点：下一帧直接从头画就是对的。
    fn log_anchor_index(&self) -> Option<usize> {
        if self.log_row_starts.is_empty() {
            return None;
        }
        let logical = self.logical_at_row(self.log_offset());
        self.log_lines
            .iter()
            .enumerate()
            .filter(|(_, line)| self.log_filter.matches(line))
            .nth(logical)
            .map(|(index, _)| index)
    }

    /// 当前该从第几行开始画日志（折行后的可见行号）。
    pub fn log_offset(&self) -> usize {
        let max = self.log_rows.saturating_sub(self.log_view.max(1));
        if self.follow {
            max
        } else {
            self.log_scroll.min(max)
        }
    }

    /// 进入日志选择模式：从当前可见的最后一行开始；跟随模式会自动关掉。
    pub fn enter_log_select(&mut self) {
        if self.page != Page::Logs || !self.workspace_focused() {
            return;
        }
        let visible = self.filtered_logs().count();
        self.log_visible = visible;
        if visible == 0 {
            self.set_toast("没有可选日志", Level::Warn);
            return;
        }
        let offset = self.log_offset();
        self.follow = false;
        self.log_scroll = offset;
        let view = self.log_view.max(1);
        let target_row = (offset + view - 1).min(self.log_rows.saturating_sub(1));
        let index = if self.log_row_starts.is_empty() {
            (offset + view - 1).min(visible - 1)
        } else {
            self.logical_at_row(target_row)
        };
        self.log_select = Some(index.min(visible - 1));
        self.ensure_log_select_visible();
        self.set_toast(
            "日志选择：↑↓ 选行 · y 复制完整 JSON · Esc 退出",
            Level::Info,
        );
    }

    /// 选择模式下上下移动；PgUp/PgDn 一次 10 行。
    pub fn move_log_select(&mut self, delta: isize) {
        let visible = self.filtered_logs().count();
        self.log_visible = visible;
        if visible == 0 {
            self.log_select = None;
            return;
        }
        let last = (visible - 1) as isize;
        let current = self.log_select.unwrap_or(0) as isize;
        self.log_select = Some((current + delta).clamp(0, last) as usize);
        self.follow = false;
        self.ensure_log_select_visible();
    }

    pub fn select_log_home(&mut self) {
        self.select_log_to(0);
    }

    pub fn select_log_end(&mut self) {
        let last = self.filtered_logs().count().saturating_sub(1);
        self.select_log_to(last);
    }

    fn select_log_to(&mut self, index: usize) {
        let visible = self.filtered_logs().count();
        self.log_visible = visible;
        if visible == 0 {
            self.log_select = None;
            return;
        }
        self.log_select = Some(index.min(visible - 1));
        self.follow = false;
        self.ensure_log_select_visible();
    }

    /// 让选中行留在日志视口里。
    pub(crate) fn ensure_log_select_visible(&mut self) {
        let Some(selected) = self.log_select else {
            return;
        };
        let view = self.log_view.max(1);
        if self.log_row_starts.is_empty() || selected >= self.log_row_starts.len() {
            // 还没画过（测试 / 刚切页）：先按逻辑行保守估算，下一帧会用真实折行高度。
            let offset = self.log_offset();
            if selected < offset {
                self.log_scroll = selected;
            } else if selected >= offset + view {
                self.log_scroll = selected + 1 - view;
            }
            let max = self.log_visible.saturating_sub(view);
            self.log_scroll = self.log_scroll.min(max);
            return;
        }
        let start = self.log_row_starts[selected];
        let end = self
            .log_row_starts
            .get(selected + 1)
            .copied()
            .unwrap_or(self.log_rows);
        let offset = self.log_offset();
        if start < offset {
            self.log_scroll = start;
        } else if end > offset + view {
            self.log_scroll = end.saturating_sub(view);
        }
        let max = self.log_rows.saturating_sub(view);
        self.log_scroll = self.log_scroll.min(max);
    }

    /// 折行后的第 `row` 行属于哪个逻辑日志行。
    fn logical_at_row(&self, row: usize) -> usize {
        match self.log_row_starts.binary_search(&row) {
            Ok(index) => index,
            Err(index) => index.saturating_sub(1),
        }
    }

    /// 复制选中行：TDLib Update 只复制完整 JSON，其它行复制整行。
    fn copy_selected_log(&mut self) {
        let Some(index) = self.log_select else {
            return;
        };
        let line = self.filtered_logs().nth(index).cloned();
        let Some(line) = line else {
            self.log_select = None;
            self.set_toast("选中的日志行已经不存在", Level::Warn);
            return;
        };
        let payload = logfmt::tdlib_update_payload(&line);
        let text = payload.unwrap_or(line.as_str()).to_owned();
        let kind = if payload.is_some() {
            "完整 JSON"
        } else {
            "整行日志"
        };
        match clipboard::copy(&text) {
            Ok(()) => self.set_toast(format!("已复制{kind}（{} 字节）", text.len()), Level::Info),
            Err(error) => self.set_toast(format!("复制失败：{error}"), Level::Error),
        }
    }

    /// 当前该从第几行开始画配置（表头另占一行）。
    pub fn config_offset(&self) -> usize {
        let visible = self.config_view.saturating_sub(1).max(1);
        let max = self.config_items.len().saturating_sub(visible);
        self.config_scroll.min(max)
    }

    // ── 启停 ──────────────────────────────────────────────────────

    /// `s`：后台起一个实例。**与手敲 `fuyubot run --background` 同一条路**。
    pub fn start(&mut self) {
        if self.busy.is_some() {
            return;
        }
        if !self.can_start() {
            self.set_toast("已经在跑了（x 停止 / r 刷新）", Level::Warn);
            return;
        }
        let mut command = match self.spawn_command() {
            Ok(command) => command,
            Err(error) => {
                self.set_toast(error, Level::Error);
                return;
            }
        };

        let tx = self.tx.clone();
        self.busy = Some(Busy::Starting);
        self.set_toast("正在起后台实例…", Level::Info);
        thread::spawn(move || {
            let done = match command.output() {
                Ok(output) if output.status.success() => {
                    // CLI 的所有输出都走 `tracing` 了（stderr），两条流合并着找 pid 那一行。
                    let mut output_text = output.stdout;
                    output_text.extend_from_slice(&output.stderr);
                    JobDone {
                        ok: true,
                        message: format!(
                            "已发起启动：{}",
                            startup_line(&output_text)
                                .unwrap_or_else(|| "后台进程已派生".to_owned())
                        ),
                    }
                }
                Ok(output) => JobDone {
                    ok: false,
                    message: format!(
                        "启动失败：{}",
                        last_line(&output.stderr).unwrap_or_else(|| "看日志页".to_owned())
                    ),
                },
                Err(error) => JobDone {
                    ok: false,
                    message: format!("启动失败：起不了子进程（{error}）"),
                },
            };
            let _ = tx.send(done);
        });
    }

    /// 拼出「起后台实例」的命令。
    ///
    /// 抽成独立函数是为了能测参数透传（真起进程在测试里代价太大）。
    /// `run --tui` 上的非秘密参数原样带给子进程：两次启动必须是同一个 bot。
    fn spawn_command(&self) -> Result<Command, String> {
        let exe =
            std::env::current_exe().map_err(|error| format!("找不到当前可执行文件：{error}"))?;
        let mut command = Command::new(exe);
        command
            .arg("run")
            .arg("--background")
            .arg("--config")
            .arg(&self.config_path)
            .stdin(Stdio::null());
        if let Some(dir) = &self.plugin_dir_override {
            command.arg("--plugin-dir").arg(dir);
        }
        if let Some(api_id) = self.spawn.api_id {
            command.arg("--api-id").arg(api_id.to_string());
        }
        if self.spawn.no_tdlib {
            command.arg("--no-tdlib");
        }
        if self.spawn.no_login {
            command.arg("--no-login");
        }
        for name in &self.spawn.log_plugins {
            command.arg("--log-plugin").arg(name);
        }
        if let Some(level) = &self.spawn.log_level {
            command.arg("--log-level").arg(level);
        }
        if self.spawn.trace_updates {
            command.arg("--trace-updates");
        }
        Ok(command)
    }

    /// `x`：先弹确认框——停止会走完插件的 `plugin_stop`，点错了代价不小。
    pub fn ask_stop(&mut self) {
        if let Some(busy) = self.busy {
            // 不静默吞掉：启动/停止还在路上时按 `x`，用户至少要看到「为什么没反应」。
            self.set_toast(format!("{}，完成后再按 x", busy.label()), Level::Info);
            return;
        }
        if !self.can_stop() {
            self.set_toast("没有在跑的实例", Level::Info);
            return;
        }
        self.confirm_stop = true;
    }

    /// 确认之后真正去停（`y` 或 Enter）。
    pub fn stop(&mut self) {
        if self.busy.is_some() {
            return;
        }
        let root = self.root.clone();
        let tx = self.tx.clone();
        self.busy = Some(Busy::Stopping);
        self.set_toast("正在请实例收摊…", Level::Info);
        thread::spawn(move || {
            let done = match instance::stop(&root) {
                Ok(message) => JobDone { ok: true, message },
                Err(error) => JobDone {
                    ok: false,
                    message: format!("停止失败：{error}"),
                },
            };
            let _ = tx.send(done);
        });
    }
}

/// `run --background` 输出的第一条「已在后台运行：pid …」行。
/// 启动成功时要报的那一行：优先「已在后台运行：pid …」。
///
/// 前几行可能是「已安装内置插件 …」——首次运行更常见，直接取第一行会把 pid 丢掉。
fn startup_line(bytes: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(bytes);
    let lines = || text.lines().map(str::trim).filter(|line| !line.is_empty());
    lines()
        .find(|line| line.contains("已在后台运行"))
        .or_else(|| lines().next_back())
        .map(message_of)
}

/// 输出的最后一条非空行（错误信息通常在最后）。
fn last_line(bytes: &[u8]) -> Option<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .next_back()
        .map(message_of)
}

/// 日志行只留内容列：`2026-10-31 07:05:13 cli INFO  内容` → `内容`。
///
/// 认不出格式的行原样返回（面板也读普通文本，比如旧版 CLI 的输出）。
fn message_of(line: &str) -> String {
    match super::logfmt::split(line) {
        Some((_, _, _, message)) if !message.is_empty() => message.to_owned(),
        _ => line.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instance::{Info, PluginDetail};
    use crate::test_support::temp;
    use ratatui::crossterm::event::KeyModifiers;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 每个测试一个目录：`test_support::temp` 会先删目录，同名并发跑会互删。
    fn app() -> App {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let name = format!("panel-app-{}", NEXT.fetch_add(1, Ordering::Relaxed));
        let root = temp(&name);
        App::new(
            root.clone(),
            root.join("fuyubot.toml"),
            None,
            Duration::from_secs(1),
            SpawnOptions::default(),
        )
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn running() -> State {
        State::Running(Some(Info {
            pid: 4321,
            started: instance::unix_now().saturating_sub(90),
            version: "0.1.2".to_owned(),
            log_level: "info".to_owned(),
            plugins: 2,
            name: "demo".to_owned(),
            running: Some(true),
            stopped_at: None,
            connection: None,
            authorization: None,
            account: None,
            detail: PluginDetail {
                active: vec!["fuyubot.start".to_owned(), "fuyubot.help".to_owned()],
                pending: vec![("demo".to_owned(), vec!["fuyubot.host.tdlib".to_owned()])],
                failed: Vec::new(),
            },
        }))
    }

    #[test]
    fn pages_cycle_in_both_directions() {
        assert_eq!(Page::Overview.prev(), Page::Help);
        assert_eq!(Page::Help.next(), Page::Overview);
        assert_eq!(
            Page::ALL
                .iter()
                .map(|page| page.title())
                .collect::<Vec<_>>(),
            ["概览", "插件", "商店", "日志", "配置", "帮助"]
        );
        // 导航顺序与 `1..=6` 的编号一致。
        for (index, page) in Page::ALL.iter().enumerate() {
            assert_eq!(page.index(), index);
        }
    }

    #[test]
    fn number_keys_jump_to_pages() {
        let mut app = app();
        app.on_key(key(KeyCode::Char('4')));
        assert_eq!(app.page, Page::Logs);
        app.on_key(key(KeyCode::Char('6')));
        assert_eq!(app.page, Page::Help);
        // Tab 不再换页，只换焦点。
        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.page, Page::Help);
        assert_eq!(app.focus, Focus::Workspace);
    }

    /// 商店页的 Esc 只回插件页，不退出面板（退出仍走 `q`）。
    #[test]
    fn store_page_esc_goes_back_and_q_still_quits() {
        let mut app = app();
        app.page = Page::Store;
        app.focus = Focus::Workspace;
        app.on_key(key(KeyCode::Esc));
        assert_eq!(app.page, Page::Plugins);
        assert!(!app.quit, "Esc 只关商店页");

        app.page = Page::Store;
        app.on_key(key(KeyCode::Char('q')));
        assert!(app.quit, "商店页不吞 q");
    }

    #[test]
    fn tab_and_enter_switch_focus_between_navigation_and_workspace() {
        let mut app = app();
        assert_eq!(app.focus, Focus::Navigation);
        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Workspace);
        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Navigation);

        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.focus, Focus::Workspace);
        app.on_key(key(KeyCode::Esc));
        assert_eq!(app.focus, Focus::Navigation);
        assert!(!app.quit, "Esc 从工作区退回导航，不该退出");
    }

    #[test]
    fn workspace_arrows_scroll_instead_of_changing_page() {
        let mut app = app();
        app.page = Page::Logs;
        app.focus = Focus::Workspace;
        app.log_lines = (0..100).map(|index| format!("line {index}")).collect();
        app.log_visible = 100;
        app.log_rows = 100;
        app.log_view = 10;

        app.on_key(key(KeyCode::Up));
        assert_eq!(app.page, Page::Logs, "工作区焦点下 ↑ 不换页");
        assert!(!app.follow, "滚动会暂停跟随");
        assert_eq!(app.log_offset(), 89);

        // 回到导航后 ↓ 才换页。
        app.on_key(key(KeyCode::Tab));
        app.on_key(key(KeyCode::Down));
        assert_eq!(app.page, Page::Config);
    }

    #[test]
    fn left_right_switch_top_tabs_even_from_the_workspace() {
        let mut app = app();
        app.page = Page::Overview;
        app.on_key(key(KeyCode::Right));
        assert_eq!(app.page, Page::Plugins);
        app.on_key(key(KeyCode::Left));
        assert_eq!(app.page, Page::Overview);

        // 导航焦点下 h / l 也能左右切。
        app.on_key(key(KeyCode::Char('l')));
        assert_eq!(app.page, Page::Plugins);
        app.on_key(key(KeyCode::Char('h')));
        assert_eq!(app.page, Page::Overview);

        // 工作区的 l 仍是日志级别选择，不会误切页。
        app.page = Page::Logs;
        app.focus = Focus::Workspace;
        app.on_key(key(KeyCode::Char('l')));
        assert_eq!(app.page, Page::Logs);
        assert!(app.level_picker.is_some());
    }

    #[test]
    fn help_page_scrolls_with_workspace_keys() {
        let mut app = app();
        app.page = Page::Help;
        app.focus = Focus::Workspace;
        app.help_rows = 100;
        app.help_view = 10;

        app.on_key(key(KeyCode::Down));
        assert_eq!(app.help_scroll, 1);
        app.on_key(key(KeyCode::PageDown));
        assert_eq!(app.help_scroll, 11);
        app.on_key(key(KeyCode::End));
        assert_eq!(app.help_scroll, 90);
        app.on_key(key(KeyCode::Home));
        assert_eq!(app.help_scroll, 0);
    }

    #[test]
    fn esc_quits_only_from_the_navigation_focus() {
        let mut app = app();
        app.focus = Focus::Workspace;
        app.on_key(key(KeyCode::Esc));
        assert!(!app.quit);
        app.on_key(key(KeyCode::Esc));
        assert!(app.quit);
    }

    #[test]
    fn logs_follow_the_end_until_you_scroll_up() {
        let mut app = app();
        std::fs::create_dir_all(app.log_path.parent().unwrap()).unwrap();
        let body = (0..100)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&app.log_path, body).unwrap();
        app.refresh_live();
        app.page = Page::Logs;
        app.focus = Focus::Workspace;
        app.log_rows = 100;
        app.log_view = 10;

        assert!(app.follow);
        assert_eq!(app.log_offset(), 90, "跟随模式永远贴底");

        app.on_key(key(KeyCode::PageUp));
        assert!(!app.follow);
        assert_eq!(app.log_offset(), 80);

        app.on_key(key(KeyCode::End));
        assert!(app.follow, "End 重读尾部并恢复跟随");
        assert_eq!(app.log_offset(), 90);
        assert_eq!(app.log_lines.last().unwrap(), "line 99");
    }

    #[test]
    fn log_selection_starts_at_the_bottom_and_moves() {
        let mut app = app();
        app.page = Page::Logs;
        app.focus = Focus::Workspace;
        app.log_lines = (0..10).map(|index| format!("line {index}")).collect();
        app.log_visible = 10;
        app.log_rows = 10;
        app.log_row_starts = (0..10).collect();
        app.log_view = 5;

        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.log_select, Some(9), "从当前可见的最后一行开始");
        assert!(!app.follow, "选择模式要暂停跟随");

        app.on_key(key(KeyCode::Up));
        assert_eq!(app.log_select, Some(8));
        app.on_key(key(KeyCode::Home));
        assert_eq!(app.log_select, Some(0));
        app.on_key(key(KeyCode::End));
        assert_eq!(app.log_select, Some(9));

        app.on_key(key(KeyCode::Esc));
        assert_eq!(app.log_select, None);
        assert!(!app.quit, "Esc 只退出选择，不退出面板");
    }

    #[test]
    fn config_selection_end_does_not_overflow() {
        let mut app = app();
        app.page = Page::Config;
        app.focus = Focus::Workspace;
        app.config_items = (0..30)
            .map(|index| ConfigItem {
                section: "test".to_owned(),
                key: format!("key{index}"),
                value: String::new(),
                display: format!("value {index}"),
                note: "默认".to_owned(),
                kind: ConfigKind::ReadOnly,
            })
            .collect();
        app.config_view = 10;
        app.on_key(key(KeyCode::End));
        assert_eq!(app.config_selected, 29);
        assert_eq!(app.config_offset(), 21);

        app.on_key(key(KeyCode::PageUp));
        assert_eq!(app.config_selected, 19);
        assert_eq!(app.config_offset(), 19);

        app.on_key(key(KeyCode::Home));
        assert_eq!(app.config_selected, 0);
        assert_eq!(app.config_offset(), 0);
    }

    #[test]
    fn tail_reader_only_reads_the_end() {
        let dir = temp("panel-tail");
        let path = dir.join("fuyubot.log");

        assert!(
            read_log_chunk(&path, 0, 1024, 10).lines.is_empty(),
            "文件不在就是空表"
        );

        std::fs::write(&path, "a\nb\nc\nd\n").unwrap();
        let tail = read_log_chunk(&path, 0, 8, 2);
        assert_eq!(tail.lines, ["c", "d"]);
        assert_eq!(tail.start, 4, "窗口起点要跟着被裁掉的行走");
        assert!(!tail.partial, "以换行结尾就不是半行");
        let all = read_log_chunk(&path, 0, 8, 10);
        assert_eq!(all.lines, ["a", "b", "c", "d"]);
        assert_eq!(all.start, 0);

        // 只从尾部读：被截断的首行必须丢掉，不能画半行给人看。
        let body = (0..50)
            .map(|index| format!("line-{index:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&path, body).unwrap();
        let len = std::fs::metadata(&path).unwrap().len();
        let tail = read_log_chunk(&path, len.saturating_sub(40), len, 5);
        assert!(
            tail.lines.iter().all(|line| line.starts_with("line-")),
            "{:?}",
            tail.lines
        );
        assert_eq!(tail.lines.last().unwrap(), "line-49");
        assert!(tail.partial, "文件末尾没有换行：最后一行是半行");
        // 起点落在完整行的行首：下一次刷新从同一行续读，不会把半截行又读一遍。
        assert_eq!(
            &std::fs::read(&path).unwrap()[tail.start as usize..],
            b"line-45\nline-46\nline-47\nline-48\nline-49"
        );
    }

    #[test]
    fn following_a_growing_log_replaces_the_half_written_last_line() {
        let mut app = app();
        std::fs::create_dir_all(app.log_path.parent().unwrap()).unwrap();
        std::fs::write(&app.log_path, "a\nb\nc").unwrap();
        app.refresh_live();
        assert_eq!(app.log_lines, ["a", "b", "c"]);
        assert!(app.log_partial, "末行没换行：是半行");

        // 实例把最后一行写完并追加一行：不能出现重复的 "c"。
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&app.log_path)
            .unwrap();
        std::io::Write::write_all(&mut file, b"\nd\n").unwrap();
        app.refresh_live();
        assert_eq!(app.log_lines, ["a", "b", "c", "d"]);
        assert!(!app.log_partial);
        assert_eq!(app.log_lines.len(), 4, "半行被替换，不是追加");
    }

    #[test]
    fn following_keeps_the_tail_within_its_byte_budget() {
        let mut app = app();
        std::fs::create_dir_all(app.log_path.parent().unwrap()).unwrap();
        let big = "x".repeat(1024);
        // 首屏就是 512KB 的尾部窗口（约 500 行、每行 1KB）。
        let body = (0..900)
            .map(|index| format!("{index:03} {big}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&app.log_path, body).unwrap();
        app.refresh_live();
        assert!(app.log_window_bytes <= LOG_TAIL_BYTES);

        // 再追加约 300KB：行数没到 800，但字节会超预算，必须按字节再裁。
        let extra = (900..1200)
            .map(|index| format!("{index:03} {big}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&app.log_path)
            .unwrap();
        std::io::Write::write_all(&mut file, format!("\n{extra}").as_bytes()).unwrap();
        app.refresh_live();

        assert!(
            app.log_window_bytes <= LOG_TAIL_BYTES,
            "窗口字节数 {}",
            app.log_window_bytes
        );
        assert!(
            app.log_lines.len() < 800,
            "按字节裁过：{}",
            app.log_lines.len()
        );
        assert!(
            app.log_lines.last().unwrap().starts_with("1199 "),
            "最新一行还在"
        );
    }

    #[test]
    fn scrolling_up_loads_earlier_logs_in_segments() {
        let mut app = app();
        std::fs::create_dir_all(app.log_path.parent().unwrap()).unwrap();
        let body = (0..4000)
            .map(|index| format!("line-{index:04}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&app.log_path, body).unwrap();
        app.refresh_live();
        app.page = Page::Logs;
        app.focus = Focus::Workspace;

        // 首屏只装尾部窗口，不是整份日志。
        assert_eq!(app.log_lines.len(), LOG_TAIL_LINES);
        assert_eq!(app.log_lines.last().unwrap(), "line-3999");
        assert!(app.log_window_start > 0, "尾巴之前还有日志");
        let tail_start = app.log_window_start;

        app.log_rows = app.log_lines.len();
        app.log_view = 20;
        app.follow = false;
        app.log_scroll = 0;

        // 在窗口顶部继续往上翻：自动往回加载一段，行不重复、顺序不乱。
        app.on_key(key(KeyCode::PageUp));
        assert!(app.log_window_start < tail_start);
        assert_eq!(app.log_lines.first().unwrap(), "line-0000");
        assert_eq!(app.log_lines.last().unwrap(), "line-3999");
        assert_eq!(app.log_lines.len(), 4000);

        // 到文件开头之后再往上翻：不再加载。
        app.on_key(key(KeyCode::PageUp));
        assert_eq!(app.log_window_start, 0);
        assert_eq!(app.log_lines.len(), 4000);
    }

    #[test]
    fn a_paged_window_freezes_and_end_jumps_back_to_the_tail() {
        let mut app = app();
        std::fs::create_dir_all(app.log_path.parent().unwrap()).unwrap();
        let body = (0..4000)
            .map(|index| format!("line-{index:04}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&app.log_path, body).unwrap();
        app.refresh_live();
        app.follow = false;
        app.log_scroll = 0;
        assert!(app.load_earlier());
        assert_eq!(app.log_lines.first().unwrap(), "line-0000");
        assert_eq!(app.log_lines.len(), 4000);

        // 翻在中间时窗口冻结：实例继续写也不往窗口里塞（内存不涨、视图不跳）。
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&app.log_path)
            .unwrap();
        std::io::Write::write_all(&mut file, b"\nline-4000").unwrap();
        app.refresh_live();
        assert_eq!(app.log_lines.len(), 4000);
        assert_eq!(app.log_lines.last().unwrap(), "line-3999");

        // End = 回到最新：重读尾部、恢复跟随。
        app.page = Page::Logs;
        app.focus = Focus::Workspace;
        app.on_key(key(KeyCode::End));
        assert!(app.follow);
        assert_eq!(app.log_lines.last().unwrap(), "line-4000");
        assert_eq!(app.log_lines.len(), LOG_TAIL_LINES, "回到尾部窗口");
        assert!(app.log_window_start > 0);
    }

    #[test]
    fn f_returns_to_the_newest_logs_and_pauses_when_already_following() {
        let mut app = app();
        std::fs::create_dir_all(app.log_path.parent().unwrap()).unwrap();
        let body = (0..4000)
            .map(|index| format!("line-{index:04}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&app.log_path, body).unwrap();
        app.refresh_live();
        app.page = Page::Logs;
        app.focus = Focus::Workspace;
        app.follow = false;
        app.log_scroll = 0;
        assert!(app.load_earlier());
        assert!(!app.follow);

        // f 打开跟随 = 回到最新（重读尾部），不是只在旧窗口里贴底。
        app.on_key(key(KeyCode::Char('f')));
        assert!(app.follow);
        assert_eq!(app.log_lines.last().unwrap(), "line-3999");
        assert_eq!(app.log_lines.len(), LOG_TAIL_LINES);

        // 再按一次 = 暂停：窗口冻结，实例继续写也不往里塞。
        app.on_key(key(KeyCode::Char('f')));
        assert!(!app.follow);
    }

    #[test]
    fn paging_back_forever_keeps_the_window_bounded() {
        let mut app = app();
        std::fs::create_dir_all(app.log_path.parent().unwrap()).unwrap();
        // 每行 11 字节、4 万行：分段一次 256KB，一定会撞上窗口上限。
        let body = (0..40_000)
            .map(|index| format!("line-{index:06}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&app.log_path, body).unwrap();
        app.refresh_live();
        app.page = Page::Logs;
        app.focus = Focus::Workspace;
        app.log_rows = app.log_lines.len();
        app.log_view = 20;
        app.follow = false;
        app.log_scroll = 0;

        let mut rounds = 0;
        while app.log_window_start > 0 {
            app.on_key(key(KeyCode::PageUp));
            rounds += 1;
            assert!(
                app.log_lines.len() <= LOG_WINDOW_MAX_LINES,
                "窗口行数不能无限长"
            );
            assert!(
                app.log_window_bytes <= LOG_WINDOW_MAX_BYTES,
                "窗口字节数不能无限大"
            );
            assert!(rounds < 100, "一直翻不到文件开头");
        }
        assert_eq!(app.log_lines.first().unwrap(), "line-000000");
        assert!(rounds > 1, "是分了几段翻过来的");
    }

    #[test]
    fn scan_lists_native_libraries_and_script_directories() {
        let dir = temp("panel-scan");
        std::fs::write(
            dir.join(format!("bot_plugin.{}", fuyubot_loader::DLL_EXTENSION)),
            "",
        )
        .unwrap();
        let script = dir.join("demo-script");
        std::fs::create_dir_all(&script).unwrap();
        std::fs::write(
            script.join("package.json"),
            r#"{"name":"demo","fuyubot":{"name":"demo"}}"#,
        )
        .unwrap();
        // 不认识的东西不许混进清单。
        std::fs::write(dir.join("notes.txt"), "").unwrap();
        std::fs::create_dir_all(dir.join("empty-dir")).unwrap();

        let plugins = scan_plugins(&dir);
        assert_eq!(plugins.len(), 2, "{plugins:?}");
        assert_eq!(plugins[0].name, "bot_plugin");
        assert_eq!(plugins[0].kind, PluginKind::Native);
        assert!(plugins[0].enabled);
        assert_eq!(plugins[1].name, "demo-script");
        assert_eq!(plugins[1].kind, PluginKind::Script);
        assert!(plugins[1].enabled, "脚本目录当前没有开关，恒为启用");
    }

    /// 禁用过的 dylib 仍要出现在清单里（不然面板上没法再启用它），
    /// 而且名字要报**原来的**主干、`enabled = false`。
    #[test]
    fn scan_also_lists_disabled_libraries() {
        let dir = temp("panel-scan-disabled");
        let dll = dir.join(format!("off_plugin.{}", fuyubot_loader::DLL_EXTENSION));
        let off = fuyubot_loader::disabled_path(&dll);
        std::fs::write(&off, "").unwrap();
        // 名字里带 `.disable` 但不是动态库后面的，不认（脚本目录的约定不是这个）。
        std::fs::write(dir.join("notes.disable"), "").unwrap();

        let plugins = scan_plugins(&dir);
        assert_eq!(plugins.len(), 1, "{plugins:?}");
        assert_eq!(plugins[0].name, "off_plugin");
        assert_eq!(plugins[0].kind, PluginKind::Native);
        assert!(!plugins[0].enabled);
        assert_eq!(plugins[0].path, off);
    }

    /// 造一个插件包：根上有 `plugin.json`，入口文件真的存在。
    fn write_bundle(dir: &Path, id: &str, kind: &str) {
        let entry = if kind == "native" {
            "bin/plugin.dll"
        } else {
            "index.ts"
        };
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
    }

    /// 插件包与禁用包都要出现；脚本目录继续从 `plugins/scripts/` 列。
    #[test]
    fn scan_lists_bundles_and_disabled_bundles() {
        let dir = temp("panel-scan-bundles");
        write_bundle(
            &dir.join("com.example.weather"),
            "com.example.weather",
            "native",
        );
        write_bundle(
            &dir.join(".disabled/com.example.gone"),
            "com.example.gone",
            "native",
        );
        let script = dir.join("scripts/js-demo");
        std::fs::create_dir_all(&script).unwrap();
        std::fs::write(
            script.join("package.json"),
            r#"{"name":"demo","fuyubot":{"name":"fuyubot.example.js"}}"#,
        )
        .unwrap();
        std::fs::write(script.join("index.ts"), b"export {}").unwrap();

        let plugins = scan_plugins(&dir);
        assert_eq!(plugins.len(), 3, "{plugins:?}");
        // 排序按名字：gone < weather < js-demo。
        assert_eq!(plugins[0].name, "com.example.gone");
        assert_eq!(plugins[0].kind, PluginKind::Bundle);
        assert!(!plugins[0].enabled, "`.disabled/` 下的是禁用包");
        assert_eq!(plugins[0].path, dir.join(".disabled/com.example.gone"));

        assert_eq!(plugins[1].name, "com.example.weather");
        assert_eq!(plugins[1].kind, PluginKind::Bundle);
        assert!(plugins[1].enabled);
        assert_eq!(plugins[1].path, dir.join("com.example.weather"));

        assert_eq!(plugins[2].name, "js-demo");
        assert_eq!(plugins[2].kind, PluginKind::Script);
        assert!(plugins[2].enabled);
    }

    /// 插件包在面板里显示清单的 `id`（不是目录名）。
    #[test]
    fn a_bundle_is_shown_by_its_manifest_id() {
        let dir = temp("panel-bundle-id");
        write_bundle(&dir.join("some-dir-name"), "com.example.weather", "native");

        let plugins = scan_plugins(&dir);
        assert_eq!(plugins.len(), 1, "{plugins:?}");
        assert_eq!(plugins[0].name, "com.example.weather");
        assert_eq!(plugins[0].kind, PluginKind::Bundle);
    }

    /// `Space` 对目录形态 = 整个挪进 `plugins/.disabled/<name>/`，再挪回来。
    #[test]
    fn space_toggles_a_bundle_into_the_disabled_directory() {
        let dir = temp("panel-toggle-bundle");
        let bundle = dir.join("com.example.weather");
        write_bundle(&bundle, "com.example.weather", "native");

        let mut app = app();
        // 走 `plugin_dir_override`：`refresh_all` 会按配置重算 `plugin_dir`。
        app.plugin_dir = dir.clone();
        app.plugin_dir_override = Some(dir.clone());
        app.page = Page::Plugins;
        app.focus = Focus::Workspace;
        app.refresh_all();
        assert_eq!(app.disk_plugins.len(), 1);
        assert!(app.disk_plugins[0].enabled);

        app.on_key(key(KeyCode::Char(' ')));
        let off = dir.join(".disabled/com.example.weather");
        assert!(!bundle.exists(), "包目录该被整个挪走");
        assert!(off.is_dir(), "禁用 = 挪进 .disabled/<id>/");
        assert!(!app.disk_plugins[0].enabled, "面板立刻反映新状态");
        assert!(
            app.toast.as_ref().is_some_and(
                |toast| toast.text.contains("已禁用") && toast.text.contains("重启后生效")
            ),
            "{:?}",
            app.toast
        );

        app.on_key(key(KeyCode::Char(' ')));
        assert!(
            bundle.is_dir() && !off.exists(),
            "再按一次挪回 plugins/<id>/"
        );
        assert!(app.disk_plugins[0].enabled);
        assert!(
            app.toast
                .as_ref()
                .is_some_and(|toast| toast.text.contains("已启用")),
            "{:?}",
            app.toast
        );
    }

    /// `Space` 的整个开关就是一次改名：`.dll` ↔ `.dll.disable`，来回都能切。
    #[test]
    fn space_toggles_a_dylib_by_renaming_it() {
        let dir = temp("panel-toggle");
        let dll = dir.join(format!("toggle_me.{}", fuyubot_loader::DLL_EXTENSION));
        std::fs::write(&dll, b"lib").unwrap();

        let mut app = app();
        // 走 `plugin_dir_override`：`refresh_all` 会按配置重算 `plugin_dir`，
        // 只改字段的话下一秒就被指回项目自己的 `plugins/`。
        app.plugin_dir = dir.clone();
        app.plugin_dir_override = Some(dir.clone());
        app.page = Page::Plugins;
        app.focus = Focus::Workspace;
        app.refresh_all();
        assert_eq!(app.disk_plugins.len(), 1);
        assert!(app.disk_plugins[0].enabled);

        app.on_key(key(KeyCode::Char(' ')));
        let off = fuyubot_loader::disabled_path(&dll);
        assert!(!dll.exists(), "启用文件应该已经改名");
        assert!(off.exists(), "禁用 = 加 .disable 后缀");
        assert!(!app.disk_plugins[0].enabled, "面板立刻反映新状态");
        assert!(
            app.toast
                .as_ref()
                .is_some_and(|toast| toast.text.contains("已禁用")),
            "{:?}",
            app.toast
        );
        // 加载器那边同步看不见它了——这正是「禁用」的实际效果。
        assert!(
            fuyubot_loader::plugin_files(&dir).unwrap().is_empty(),
            "加载器不该再把禁用的库当候选"
        );

        app.on_key(key(KeyCode::Char(' ')));
        assert!(dll.exists() && !off.exists(), "再按一次改回来");
        assert!(app.disk_plugins[0].enabled);
        assert!(
            app.toast
                .as_ref()
                .is_some_and(|toast| toast.text.contains("已启用")),
            "{:?}",
            app.toast
        );
    }

    /// 脚本插件目录也能开关：整个挪进 `.disabled/`，再挪回 `plugins/scripts/`。
    #[test]
    fn script_directories_can_be_disabled_and_restored() {
        let dir = temp("panel-toggle-script");
        let script = dir.join("scripts").join("demo-script");
        std::fs::create_dir_all(&script).unwrap();
        std::fs::write(
            script.join("package.json"),
            r#"{"name":"demo","fuyubot":{"name":"demo"}}"#,
        )
        .unwrap();

        let mut app = app();
        // 走 `plugin_dir_override`：`refresh_all` 会按配置重算 `plugin_dir`，
        // 只改字段的话下一秒就被指回项目自己的 `plugins/`。
        app.plugin_dir = dir.clone();
        app.plugin_dir_override = Some(dir.clone());
        app.page = Page::Plugins;
        app.focus = Focus::Workspace;
        app.refresh_all();
        assert_eq!(app.disk_plugins.len(), 1, "{:?}", app.disk_plugins);

        app.on_key(key(KeyCode::Char(' ')));
        let off = dir.join(".disabled").join("demo-script");
        assert!(!script.exists(), "脚本目录该被整个挪走");
        assert!(off.is_dir(), "禁用 = 挪进 .disabled/<目录名>/");
        assert!(!app.disk_plugins[0].enabled);
        assert!(
            app.toast
                .as_ref()
                .is_some_and(|toast| toast.text.contains("已禁用")),
            "{:?}",
            app.toast
        );

        app.on_key(key(KeyCode::Char(' ')));
        assert!(
            script.is_dir() && !off.exists(),
            "再按一次挪回 plugins/scripts/<目录名>/"
        );
        assert!(app.disk_plugins[0].enabled);
        assert!(
            app.toast
                .as_ref()
                .is_some_and(|toast| toast.text.contains("已启用")),
            "{:?}",
            app.toast
        );
    }

    /// 上下移动只在插件页动选中行，不影响别的页。
    #[test]
    fn plugin_page_moves_the_selection_with_the_arrow_keys() {
        let dir = temp("panel-move");
        for index in 0..3 {
            std::fs::write(
                dir.join(format!("p{index}.{}", fuyubot_loader::DLL_EXTENSION)),
                b"x",
            )
            .unwrap();
        }

        let mut app = app();
        app.plugin_dir = dir.clone();
        app.plugin_dir_override = Some(dir);
        app.page = Page::Plugins;
        app.focus = Focus::Workspace;
        app.refresh_all();
        assert_eq!(app.disk_plugins.len(), 3);

        app.on_key(key(KeyCode::Down));
        assert_eq!(app.plugin_selected, 1);
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Down));
        assert_eq!(app.plugin_selected, 2, "到底了不越界");
        app.on_key(key(KeyCode::Char('k')));
        assert_eq!(app.plugin_selected, 1);
        app.on_key(key(KeyCode::End));
        assert_eq!(app.plugin_selected, 2);
        app.on_key(key(KeyCode::Home));
        assert_eq!(app.plugin_selected, 0);
    }

    #[test]
    fn stop_is_behind_a_confirmation() {
        let mut app = app();
        app.state = running();
        app.on_key(key(KeyCode::Char('x')));
        assert!(app.confirm_stop, "x 只开确认框，不直接停");

        // 确认框开着时导航键无效。
        app.on_key(key(KeyCode::Down));
        assert_eq!(app.page, Page::Overview);
        app.on_key(key(KeyCode::Esc));
        assert!(!app.confirm_stop);
        assert!(!app.quit, "Esc 只关确认框，不该退出面板");
    }

    /// 启动 / 停止还在路上时按 `x`：不许静默无视——那看起来就像「x 坏了」。
    #[test]
    fn stop_while_busy_explains_why_it_is_ignored() {
        let mut app = app();
        app.state = running();
        app.busy = Some(Busy::Starting);
        app.on_key(key(KeyCode::Char('x')));
        assert!(!app.confirm_stop, "忙的时候不该再开一个确认框");
        assert!(
            app.toast
                .as_ref()
                .is_some_and(|toast| toast.text.contains("启动中") && toast.text.contains('x')),
            "{:?}",
            app.toast
        );
    }

    #[test]
    fn clearing_logs_is_behind_a_confirmation_and_truncates_the_file() {
        let mut app = app();
        std::fs::create_dir_all(app.log_path.parent().unwrap()).unwrap();
        std::fs::write(&app.log_path, "old line\nanother old line\n").unwrap();
        app.page = Page::Logs;
        app.focus = Focus::Workspace;
        app.log_lines = vec!["old line".to_owned()];
        app.log_visible = 1;

        app.on_key(key(KeyCode::Char('c')));
        assert!(app.confirm_clear_logs, "c 只开确认框");
        app.on_key(key(KeyCode::Char('y')));
        assert!(!app.confirm_clear_logs);
        assert!(app.log_lines.is_empty());
        assert_eq!(app.log_visible, 0);
        assert_eq!(std::fs::read_to_string(&app.log_path).unwrap(), "");
        assert!(
            app.toast
                .as_ref()
                .is_some_and(|toast| toast.text.contains("日志已清空")),
            "{:?}",
            app.toast
        );
    }

    #[test]
    fn start_refuses_when_the_instance_is_already_running() {
        let mut app = app();
        app.state = running();
        app.on_key(key(KeyCode::Char('s')));
        assert!(app.busy.is_none(), "已经在跑就不该再派子进程");
        assert!(
            app.toast
                .as_ref()
                .is_some_and(|toast| toast.text.contains("已经在跑")),
            "{:?}",
            app.toast
        );
    }

    /// 面板里按 `s` 起的子进程必须和用户敲的那条 `run` 是同一组参数。
    #[test]
    fn start_command_passes_run_options_through() {
        let mut app = app();
        app.spawn = SpawnOptions {
            api_id: Some(42),
            no_tdlib: true,
            no_login: true,
            log_level: Some("debug".to_owned()),
            log_plugins: vec!["fuyubot.demo".to_owned()],
            trace_updates: true,
        };

        let command = app.spawn_command().expect("当前可执行文件总该找得到");
        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();

        assert_eq!(args[0], "run");
        assert_eq!(args[1], "--background");
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--config", app.config_path.to_string_lossy().as_ref()])
        );
        assert!(args.windows(2).any(|pair| pair == ["--api-id", "42"]));
        assert!(args.windows(2).any(|pair| pair == ["--log-level", "debug"]));
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--log-plugin", "fuyubot.demo"]),
            "日志过滤也要透传给后台子进程：{args:?}"
        );
        assert!(args.contains(&"--no-tdlib".to_owned()));
        assert!(args.contains(&"--no-login".to_owned()));
        assert!(args.contains(&"--trace-updates".to_owned()));
    }

    #[test]
    fn startup_line_prefers_the_pid_over_plugin_install_notices() {
        let stdout = "已安装内置插件 a\n已在后台运行：pid 42，日志 logs/fuyubot.log\n`fuyubot status` 看状态\n";
        assert_eq!(
            startup_line(stdout.as_bytes()).as_deref(),
            Some("已在后台运行：pid 42，日志 logs/fuyubot.log")
        );
        // 没有那条就退到最后一行的非空行；空输出不编一条出来。
        assert_eq!(startup_line(b"hello\n").as_deref(), Some("hello"));
        assert_eq!(startup_line(b" \n\t\n"), None);
        // 现在 CLI 全走 tracing：同一行带时间戳/级别/来源，只留内容列。
        let logged = "2026-10-31 07:05:13 cli INFO  已在后台运行：pid 42，日志 logs/fuyubot.log\n";
        assert_eq!(
            startup_line(logged.as_bytes()).as_deref(),
            Some("已在后台运行：pid 42，日志 logs/fuyubot.log")
        );
        assert_eq!(
            last_line(logged.as_bytes()).as_deref(),
            Some("已在后台运行：pid 42，日志 logs/fuyubot.log")
        );
    }

    #[test]
    fn log_level_picker_only_opens_on_the_logs_page_with_workspace_focus() {
        let mut app = app();
        app.on_key(key(KeyCode::Char('l')));
        assert_eq!(app.level_picker, None, "概览页按 l 不该弹框");
        assert_eq!(app.page, Page::Plugins, "导航焦点下 l 切到下一页");

        app.page = Page::Logs;
        app.on_key(key(KeyCode::Char('l')));
        assert_eq!(app.level_picker, None, "日志页但焦点在导航时也不该弹框");
        assert_eq!(app.page, Page::Config, "导航焦点下 l 仍然只切页");

        app.page = Page::Logs;
        app.focus = Focus::Workspace;
        app.on_key(key(KeyCode::Char('l')));
        assert_eq!(app.level_picker, Some(2), "默认 info 应该预选中");

        // 选择框打开时方向键只动游标，不换页。
        app.on_key(key(KeyCode::Down));
        assert_eq!(app.level_picker, Some(3));
        assert_eq!(app.page, Page::Logs);
        app.on_key(key(KeyCode::Esc));
        assert_eq!(app.level_picker, None);
        assert!(!app.quit);
    }

    #[test]
    fn choosing_a_level_while_stopped_applies_to_the_next_start() {
        let mut app = app();
        app.page = Page::Logs;
        app.focus = Focus::Workspace;
        app.on_key(key(KeyCode::Char('l')));
        app.on_key(key(KeyCode::Char('5'))); // trace
        assert_eq!(app.spawn.log_level.as_deref(), Some("trace"));
        assert_eq!(app.effective_level(), "trace");
        assert_eq!(
            instance::requested_log_level(&app.root),
            None,
            "实例没跑就不该写控制文件"
        );
        assert!(app.toast.as_ref().unwrap().text.contains("下次启动"));
    }

    #[test]
    fn choosing_a_level_while_running_writes_the_control_file() {
        let mut app = app();
        app.page = Page::Logs;
        app.focus = Focus::Workspace;
        app.state = running();
        app.on_key(key(KeyCode::Char('l')));
        app.on_key(key(KeyCode::Down)); // info -> debug
        app.on_key(key(KeyCode::Enter));

        assert_eq!(
            instance::requested_log_level(&app.root).as_deref(),
            Some("debug")
        );
        // 运行中以实例上报为准：控制文件还没被读到之前，显示仍是旧级别。
        assert_eq!(app.effective_level(), "info");
    }

    #[test]
    fn filter_picker_is_gated_to_logs_page_and_workspace_focus() {
        let mut app = app();
        app.page = Page::Logs;
        app.on_key(key(KeyCode::Char('F')));
        assert_eq!(app.filter_picker, None, "导航焦点不弹过滤框");

        app.focus = Focus::Workspace;
        app.on_key(key(KeyCode::Char('F')));
        assert!(app.filter_picker.is_some());
        app.on_key(key(KeyCode::Esc));
        assert!(app.filter_picker.is_none());
    }

    #[test]
    fn filter_applies_level_and_source_and_resets() {
        let mut app = app();
        app.page = Page::Logs;
        app.focus = Focus::Workspace;
        app.log_lines = vec![
            "05:41:02.118  INFO   host          started".to_owned(),
            "05:41:03.000  DEBUG  fuyubot.demo  debug".to_owned(),
            "[ 2][t 1][client.cpp:1] tdlib warn".to_owned(),
        ];
        app.log_visible = 3;
        app.open_filter_picker();
        // 级别：全部 -> error+ -> warn+ -> info+ -> debug+
        app.on_key(key(KeyCode::Right));
        app.on_key(key(KeyCode::Right));
        app.on_key(key(KeyCode::Right));
        app.on_key(key(KeyCode::Right));
        // 来源：全部 -> 插件
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Right));
        app.on_key(key(KeyCode::Enter));

        assert_eq!(app.log_filter.level, LevelFilter::Debug);
        assert_eq!(app.log_filter.source, SourceFilter::Plugin);
        assert_eq!(app.log_visible, 1, "只剩插件 DEBUG 那一行");
        assert!(app.follow, "应用过滤后回到跟随末尾");

        // `c` 在弹框里重置两个维度。
        app.open_filter_picker();
        app.on_key(key(KeyCode::Char('c')));
        app.on_key(key(KeyCode::Enter));
        assert!(app.log_filter.is_default());
        assert_eq!(app.log_visible, 3);
    }

    #[test]
    fn log_level_aliases_from_the_command_line_are_canonicalized() {
        let root = temp("panel-level-alias");
        let app = App::new(
            root.clone(),
            root.join("fuyubot.toml"),
            None,
            Duration::from_secs(1),
            SpawnOptions {
                log_level: Some("WARNING".to_owned()),
                ..SpawnOptions::default()
            },
        );
        assert_eq!(app.spawn.log_level.as_deref(), Some("warn"));
        assert_eq!(app.effective_level(), "warn");

        // `run_tui` 已经先校验过一次；直接构造时写坏的名字也不能让选择框错位。
        let app = App::new(
            root.clone(),
            root.join("fuyubot.toml"),
            None,
            Duration::from_secs(1),
            SpawnOptions {
                log_level: Some("verbose".to_owned()),
                ..SpawnOptions::default()
            },
        );
        assert_eq!(app.spawn.log_level, None);
        assert_eq!(app.effective_level(), "info");
    }

    /// 资源采样真能读到本进程（sysinfo 在 Windows 上要能初始化、拿到 RSS）。
    #[test]
    fn usage_sampler_reads_memory_for_the_running_pid() {
        let mut app = app();
        app.state = State::Running(Some(Info {
            pid: std::process::id(),
            started: instance::unix_now(),
            version: "0.1.2".to_owned(),
            log_level: "info".to_owned(),
            plugins: 0,
            name: "self".to_owned(),
            running: Some(true),
            stopped_at: None,
            connection: None,
            authorization: None,
            account: None,
            detail: PluginDetail::default(),
        }));
        app.refresh_usage();
        let usage = app.usage.expect("sysinfo 应该能读到当前测试进程");
        assert!(usage.memory_bytes > 0, "RSS 不该是 0");
        assert!(usage.cpu_percent >= 0.0);
    }

    fn config_index(app: &App, section: &str, key: &str) -> usize {
        app.config_items
            .iter()
            .position(|item| item.section == section && item.key == key)
            .unwrap_or_else(|| panic!("缺少配置项 [{section}].{key}"))
    }

    #[test]
    fn config_page_builds_editable_and_readonly_rows() {
        let root = temp("panel-config-items");
        let path = scaffold::init(&root, "demo", 12345).unwrap();
        let app = App::new(
            root,
            path,
            None,
            Duration::from_secs(1),
            SpawnOptions::default(),
        );
        assert!(app.config_error.is_none());

        let api_id = &app.config_items[config_index(&app, "telegram", "api_id")];
        assert!(api_id.editable());
        assert_eq!(api_id.display, "12345");
        assert_eq!(api_id.note, "文件");

        let hash = &app.config_items[config_index(&app, "telegram", "api_hash")];
        assert!(!hash.editable(), "凭据只读");
        assert!(!hash.display.is_empty());

        // 模板里的 [plugin.*] 即使为空也不该让配置页崩；核心项必须齐全。
        for (section, key) in [
            ("bot", "name"),
            ("plugins", "dir"),
            ("plugins", "log_level"),
            ("tdlib", "path"),
            ("tdlib", "log_verbosity"),
        ] {
            let _ = config_index(&app, section, key);
        }
    }

    #[test]
    fn editing_a_single_config_item_writes_and_reloads() {
        let root = temp("panel-config-edit");
        let path = scaffold::init(&root, "demo", 12345).unwrap();
        let mut app = App::new(
            root,
            path.clone(),
            None,
            Duration::from_secs(1),
            SpawnOptions::default(),
        );
        app.page = Page::Config;
        app.focus = Focus::Workspace;
        app.config_selected = config_index(&app, "telegram", "api_id");

        app.on_key(key(KeyCode::Enter));
        let edit = app.config_edit.as_mut().expect("应该打开编辑框");
        edit.input = "777".to_owned();
        edit.cursor = 3;
        app.on_key(key(KeyCode::Enter));

        assert!(app.config_edit.is_none(), "保存后应关闭编辑框");
        assert_eq!(
            scaffold::load_config(&path).unwrap().telegram.api_id,
            Some(777)
        );
        assert_eq!(
            app.config_items[app.config_selected].key, "api_id",
            "刷新后选中行要留在同一项"
        );
        assert!(
            app.toast
                .as_ref()
                .is_some_and(|toast| toast.text.contains("已保存")),
            "{:?}",
            app.toast
        );
    }

    #[test]
    fn editing_a_choice_config_item_canonicalizes_and_writes() {
        let root = temp("panel-config-choice");
        let path = scaffold::init(&root, "demo", 12345).unwrap();
        let mut app = App::new(
            root,
            path.clone(),
            None,
            Duration::from_secs(1),
            SpawnOptions::default(),
        );
        app.page = Page::Config;
        app.focus = Focus::Workspace;
        app.config_selected = config_index(&app, "plugins", "log_level");
        app.on_key(key(KeyCode::Enter));
        let edit = app.config_edit.as_mut().unwrap();
        edit.input = "DEBUG".to_owned();
        edit.cursor = edit.input.chars().count();
        app.on_key(key(KeyCode::Enter));

        assert!(app.config_edit.is_none());
        assert_eq!(
            scaffold::load_config(&path)
                .unwrap()
                .plugins
                .log_level
                .as_deref(),
            Some("debug"),
            "枚举要写成规范小写"
        );
        assert_eq!(app.configured_level, "debug");
    }

    #[test]
    fn invalid_config_input_stays_in_the_editor() {
        let root = temp("panel-config-invalid");
        let path = scaffold::init(&root, "demo", 12345).unwrap();
        let mut app = App::new(
            root,
            path.clone(),
            None,
            Duration::from_secs(1),
            SpawnOptions::default(),
        );
        app.page = Page::Config;
        app.focus = Focus::Workspace;
        app.config_selected = config_index(&app, "telegram", "api_id");
        app.on_key(key(KeyCode::Enter));
        let edit = app.config_edit.as_mut().unwrap();
        edit.input = "not-a-number".to_owned();
        edit.cursor = edit.input.chars().count();
        app.on_key(key(KeyCode::Enter));

        let edit = app.config_edit.as_ref().expect("校验失败不该关闭编辑框");
        assert!(
            edit.error
                .as_deref()
                .is_some_and(|error| error.contains("整数")),
            "{:?}",
            edit.error
        );
        assert_eq!(
            scaffold::load_config(&path).unwrap().telegram.api_id,
            Some(12345),
            "校验失败不能写盘"
        );
    }

    #[test]
    fn readonly_config_item_does_not_open_the_editor() {
        let root = temp("panel-config-readonly");
        let path = scaffold::init(&root, "demo", 12345).unwrap();
        let mut app = App::new(
            root,
            path,
            None,
            Duration::from_secs(1),
            SpawnOptions::default(),
        );
        app.page = Page::Config;
        app.focus = Focus::Workspace;
        app.config_selected = config_index(&app, "telegram", "api_hash");
        app.on_key(key(KeyCode::Enter));
        assert!(app.config_edit.is_none());
        assert!(
            app.toast
                .as_ref()
                .is_some_and(|toast| toast.text.contains("只读")),
            "{:?}",
            app.toast
        );
    }

    #[test]
    fn plugin_config_rows_are_readonly_and_mask_secrets() {
        let root = temp("panel-config-plugin");
        let path = root.join("fuyubot.toml");
        std::fs::write(
            &path,
            "[bot]\nname = \"demo\"\n\n[plugin.demo]\ntoken = \"secret\"\nname = \"d\"\n",
        )
        .unwrap();
        let app = App::new(
            root,
            path,
            None,
            Duration::from_secs(1),
            SpawnOptions::default(),
        );
        let token = &app.config_items[config_index(&app, "plugin.demo", "token")];
        assert!(!token.editable());
        assert_eq!(token.display, "已保存（不回显）");
        let name = &app.config_items[config_index(&app, "plugin.demo", "name")];
        assert!(!name.editable());
        assert_eq!(name.display, "d");
    }

    #[test]
    fn editing_without_a_config_file_explains_how_to_create_it() {
        let mut app = app();
        app.page = Page::Config;
        app.focus = Focus::Workspace;
        app.config_selected = config_index(&app, "telegram", "api_id");
        app.on_key(key(KeyCode::Enter));
        assert!(app.config_edit.is_some());
        let edit = app.config_edit.as_mut().unwrap();
        edit.input = "1".to_owned();
        edit.cursor = 1;
        app.on_key(key(KeyCode::Enter));

        let edit = app.config_edit.as_ref().expect("写不了要留着编辑框");
        assert!(
            edit.error
                .as_deref()
                .is_some_and(|error| error.contains("配置文件不存在")),
            "{:?}",
            edit.error
        );
        assert!(!app.config_path.exists(), "不能凭空创建配置");
    }
}
