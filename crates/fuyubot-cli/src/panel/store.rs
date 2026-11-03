//! TUI 插件商店：索引 / 搜索 / 详情 / 下载 / 校验 / 解压 / 安装 / 卸载。
//!
//! 边界（见[文档站](https://catmoecircle.github.io/FuyuBot-docs/docs/plugin/bundle/) §7 与约束 61）：索引、下载、zip 解压**只在这里**，
//! `fuyubot-loader` 与运行时永远不联网、不碰压缩包；安装落盘之后的目录布局仍由 bundle
//! 契约（`plugins/<id>/` 与 `plugins/scripts/<id>/`）说了算。
//!
//! 三件事保证面板不会被网络卡住：
//!
//! 1. 拉索引、下载 + 校验 + 解压各跑在**独立线程**里，结果经 `std::sync::mpsc` 回投
//!    （不用 tokio：面板是同步事件循环）；
//! 2. [`Store::tick`] 每帧只做两件轻活：排空通道、每秒重扫一次已安装目录；
//! 3. [`Store::render`] 只读状态，错误原文折行显示在底部状态区，PgUp/PgDn 可滚动。
//!
//! 状态机：索引 `Idle → Loading → Ready（或 Error 保留旧索引）`；安装
//! `Idle → Working（下载 % / 校验 / 解压）→ Done | Error`。同一时刻只允许一个安装任务
//! （`busy` 闸门），失败也不留半个包（`.staging/<id>/` 一定被清掉）。

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;
use std::time::{Duration, Instant};

use fuyubot_loader::abi::ABI_VERSION;
use fuyubot_loader::{BUNDLE_MANIFEST, BundleManifest, PluginKind, SCRIPTS_DIR, current_platform};

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState, Padding, Paragraph, Wrap};
use sha2::{Digest, Sha256};

/// 索引 JSON 的大小上限：正常索引几百 KB，2MB 已经非常宽松。
const INDEX_MAX_BYTES: u64 = 2 * 1024 * 1024;
/// 单个插件包的大小上限（契约 §7）。
const PLUGIN_MAX_BYTES: u64 = 50 * 1024 * 1024;
/// 解压之后允许占用的总字节数（防 zip bomb）。
const EXTRACT_MAX_BYTES: u64 = 200 * 1024 * 1024;
/// 单个包允许的文件数（目录条目不算）。
const EXTRACT_MAX_FILES: usize = 2000;
/// 本宿主认识的索引 schema 版本。
const INDEX_SCHEMA: u32 = 1;
/// 装到一半的目录：装完 `fs::rename` 走，失败整个删掉。
const STAGING_DIR: &str = ".staging";
/// 已安装目录的重扫间隔：安装 / 卸载之后会立刻扫，平时一秒一次够用。
const SCAN_INTERVAL: Duration = Duration::from_secs(1);
/// 连不上 / 解析不了 DNS 就别吊着：10 秒足够。
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// 等响应头最多 30 秒（服务器接了连接却不回话）。
const HTTP_HEADER_TIMEOUT: Duration = Duration::from_secs(30);
/// 读 body 的总预算：50MB 在 2Mbps 上约 200 秒，给 5 分钟，慢网也能下完。
const HTTP_BODY_TIMEOUT: Duration = Duration::from_secs(300);
/// 窄屏阈值：工作区宽度小于它就上下排（列表在上、详情在下），否则左右排。
const NARROW_WIDTH: u16 = 80;

// ── 对外类型 ───────────────────────────────────────────────────────

/// 按键被谁吃掉了。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreKey {
    /// 不归商店管：交回 `app.rs`（`q` 退出、`Tab` 换焦点这些）。
    Ignored,
    /// 商店处理了，别再往下传。
    Consumed,
    /// 商店要求关闭这一页（非输入态按 `Esc`）。
    Close,
}

/// 插件商店的全部状态。
pub struct Store {
    plugin_dir: PathBuf,
    /// `None` = 未配置。**不要**自己造默认 URL：托管方是外部商城。
    registry_url: Option<String>,

    // 索引
    index: Option<Index>,
    index_error: Option<String>,
    loading_index: bool,
    /// 首帧自动拉一次索引（有 URL 才拉；`r` 手动刷新走同一个入口）。
    auto_loaded: bool,

    // 已安装（从磁盘扫描，不读索引）
    installed: Vec<Installed>,
    last_scan: Instant,
    scan_dirty: bool,

    // 列表 / 详情 / 搜索
    query: String,
    /// `Some` = 正在输入过滤词（草稿与 `query` 同步，Esc 回滚到进入前的值）。
    search: Option<String>,
    query_before_search: Option<String>,
    /// 过滤之后的下标（指向 `index.plugins`）。
    filtered: Vec<usize>,
    selected: usize,
    list_offset: usize,
    view_rows: usize,
    detail_scroll: u16,

    // 后台任务
    busy: bool,
    status: Status,
    /// 状态区里错误可见的行数（`render` 回填，PgUp/PgDn 靠它算滚动上限）。
    status_rows: u16,
    error_rows: u16,
    error_scroll: u16,

    tx: Sender<Event>,
    rx: Receiver<Event>,
}

impl Store {
    /// 建一个商店页。
    ///
    /// `registry_url` 为 `None` / 空串 = **未配置**：界面显示去哪配，绝不自己编一个默认地址。
    /// 已安装列表立刻扫一次（纯本地目录，很快）。
    pub fn new(plugin_dir: PathBuf, registry_url: Option<String>) -> Self {
        let (tx, rx) = channel();
        let registry_url = registry_url
            .map(|url| url.trim().to_owned())
            .filter(|url| !url.is_empty());
        let mut store = Self {
            plugin_dir,
            registry_url,
            index: None,
            index_error: None,
            loading_index: false,
            auto_loaded: false,
            installed: Vec::new(),
            last_scan: Instant::now(),
            scan_dirty: true,
            query: String::new(),
            search: None,
            query_before_search: None,
            filtered: Vec::new(),
            selected: 0,
            list_offset: 0,
            view_rows: 10,
            detail_scroll: 0,
            busy: false,
            status: Status::Idle,
            status_rows: 0,
            error_rows: 0,
            error_scroll: 0,
            tx,
            rx,
        };
        store.rescan();
        store
    }

    /// 每帧调一次：排空后台线程消息、刷新已安装列表（最多一秒一次）。
    ///
    /// 首帧顺手拉一次索引（有 URL 才拉）。这里**绝不做网络 / 解压**，只搬结果。
    pub fn tick(&mut self) {
        self.drain_events();
        if !self.auto_loaded {
            self.auto_loaded = true;
            if self.registry_url.is_some() {
                self.load_index();
            }
        }
        if self.scan_dirty || self.last_scan.elapsed() >= SCAN_INTERVAL {
            self.rescan();
        }
    }

    /// 页面拿到焦点时，所有按键先交给它。
    ///
    /// 非输入态按 `Esc` 返回 [`StoreKey::Close`]；输入态按 `Esc` 只退出输入（不关页）。
    /// 没处理的键返回 [`StoreKey::Ignored`]，`q` / `Tab` 这些仍归 `app.rs`。
    pub fn handle_key(&mut self, key: KeyEvent) -> StoreKey {
        if key.kind != KeyEventKind::Press {
            return StoreKey::Ignored;
        }
        // Ctrl+C 一律交给 app.rs 退出，不当输入字符。
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return StoreKey::Ignored;
        }
        if self.search.is_some() {
            return self.handle_search_key(key);
        }
        match key.code {
            KeyCode::Esc => StoreKey::Close,
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_selection(-1);
                StoreKey::Consumed
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_selection(1);
                StoreKey::Consumed
            }
            KeyCode::PageUp => {
                self.page_key(-1);
                StoreKey::Consumed
            }
            KeyCode::PageDown => {
                self.page_key(1);
                StoreKey::Consumed
            }
            KeyCode::Home | KeyCode::Char('g') => {
                self.select_nth(0);
                StoreKey::Consumed
            }
            KeyCode::End | KeyCode::Char('G') => {
                if !self.filtered.is_empty() {
                    self.select_nth(self.filtered.len() - 1);
                }
                StoreKey::Consumed
            }
            KeyCode::Enter => {
                self.start_install();
                StoreKey::Consumed
            }
            KeyCode::Char('x') => {
                self.uninstall_selected();
                StoreKey::Consumed
            }
            KeyCode::Char('/') => {
                self.enter_search();
                StoreKey::Consumed
            }
            KeyCode::Char('r') => {
                self.refresh();
                StoreKey::Consumed
            }
            _ => StoreKey::Ignored,
        }
    }

    /// 画一帧。`area` 由 `app.rs` 传**工作区 inner 区**（已经去掉外框）：
    /// 这里自己画「左列表 + 右详情两个 bordered 块 + 底部状态区」。
    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let status_height = self.status_height(area);
        let [body, status_area] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(status_height)]).areas(area);
        let (list_area, detail_area) = if body.width >= NARROW_WIDTH {
            let [list_area, detail_area] =
                Layout::horizontal([Constraint::Percentage(42), Constraint::Min(24)]).areas(body);
            (list_area, detail_area)
        } else {
            // 窄屏（手机 / 小窗口）：上下排，列表略矮，详情优先。
            let [list_area, detail_area] =
                Layout::vertical([Constraint::Percentage(40), Constraint::Min(3)]).areas(body);
            (list_area, detail_area)
        };
        self.render_list(frame, list_area);
        self.render_detail(frame, detail_area);
        self.render_status(frame, status_area);
    }

    /// 底栏那句键提示（由 `app.rs` 放到它自己的底栏，或由本页状态区显示）。
    pub fn footer_hint(&self) -> String {
        if self.search.is_some() {
            return "输入过滤词（id / 名称 / 简介）· Enter 确认 · Esc 取消".to_owned();
        }
        if self.busy {
            return "安装中… 进度见上方状态行（不要关面板）".to_owned();
        }
        "↑↓ 选择 · Enter 安装 · x 卸载 · / 搜索 · r 刷新 · Esc 返回".to_owned()
    }

    /// 页面标题，例如 `插件商店 (12)`；有过滤词时显示 `插件商店 (3/12)`。
    pub fn title(&self) -> String {
        if self.registry_url.is_none() {
            return "插件商店（未配置）".to_owned();
        }
        if self.loading_index && self.index.is_none() {
            return "插件商店（加载中…）".to_owned();
        }
        match &self.index {
            Some(index) if self.query.trim().is_empty() => {
                format!("插件商店 ({})", index.plugins.len())
            }
            Some(index) => format!("插件商店 ({}/{})", self.filtered.len(), index.plugins.len()),
            None if self.index_error.is_some() => "插件商店（加载失败）".to_owned(),
            None => "插件商店".to_owned(),
        }
    }
}

// ── 内部状态 ───────────────────────────────────────────────────────

/// 状态行：加载 / 安装进度 / 结果 / 错误。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Status {
    /// 什么都没发生。
    Idle,
    /// 正在拉索引。
    Loading,
    /// 安装进行中（下载 %、校验、解压）。
    Working(String),
    /// 最近一次成功（索引已更新 / 安装完成 / 已卸载）。
    Done(String),
    /// 最近一次失败。原文折行显示在状态区，PgUp/PgDn 可滚。
    Error(String),
}

/// 后台线程 → 界面的消息。
#[derive(Debug)]
enum Event {
    /// 索引拉取结果（成功时已经校验过 schema）。
    Index(Result<Index, String>),
    /// 安装进度文本（给人看的一句话）。
    Progress(String),
    /// 安装结束：成功给出落盘信息，失败给出可操作的原因。
    Installed(Result<InstalledOutcome, String>),
}

/// 装好的包。
#[derive(Debug)]
struct InstalledOutcome {
    id: String,
    name: String,
    version: String,
    kind: PluginKind,
    path: PathBuf,
}

/// 磁盘上已安装的一个包（只从 `plugin.json` 读，不 dlopen）。
#[derive(Debug, Clone)]
struct Installed {
    id: String,
    version: String,
    kind: PluginKind,
    path: PathBuf,
}

// ── 索引（契约 §7）─────────────────────────────────────────────────

/// 商城索引。只写商店真正用得到的字段，其余未知字段由 serde 忽略。
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub(crate) struct Index {
    #[serde(default)]
    pub schema: u32,
    #[serde(default)]
    pub plugins: Vec<IndexPlugin>,
}

/// 索引里的一个插件。
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub(crate) struct IndexPlugin {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub repo: String,
    #[serde(default)]
    pub license: String,
    #[serde(default)]
    pub homepage: String,
    #[serde(default)]
    pub tags: Vec<String>,
    /// 最新版本号（可能缺省或指向一个 yanked 版本）。
    #[serde(default)]
    pub latest: Option<String>,
    #[serde(default)]
    pub versions: Vec<IndexVersion>,
}

/// 一个版本。`versions` 的顺序按索引原样（我们从后往前找最新的可用版本）。
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub(crate) struct IndexVersion {
    #[serde(default)]
    pub version: String,
    /// 宿主 API 兼容范围（只展示，不解析）。
    #[serde(default)]
    pub api: String,
    /// 需要的 ABI；大于宿主的 [`ABI_VERSION`] 就不让装。
    #[serde(default)]
    pub abi: Option<u32>,
    #[serde(default)]
    pub depends: Vec<String>,
    /// 下架的版本不参与选择。
    #[serde(default)]
    pub yanked: bool,
    /// 平台 → 资产。
    #[serde(default)]
    pub assets: BTreeMap<String, Asset>,
}

/// 一个可下载的包。
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub(crate) struct Asset {
    #[serde(default)]
    pub url: String,
    /// 必填；大小写不敏感。
    #[serde(default)]
    pub sha256: String,
    /// 可选，只用于显示 / 预检。
    #[serde(default)]
    pub size: Option<u64>,
}

// ── 按键 ───────────────────────────────────────────────────────────

impl Store {
    /// 输入态：所有可打印字符都算过滤词（`q` 也是普通字符）。
    fn handle_search_key(&mut self, key: KeyEvent) -> StoreKey {
        let mut draft = self.search.clone().unwrap_or_default();
        match key.code {
            // Esc 只退出输入态，页面不关；过滤词回滚到进入输入前的值。
            KeyCode::Esc => {
                self.search = None;
                if let Some(previous) = self.query_before_search.take() {
                    self.query = previous;
                }
                self.refresh_filtered();
            }
            // Enter 确认当前草稿（留在 `query` 里，继续过滤）。
            KeyCode::Enter => {
                self.search = None;
                self.query_before_search = None;
                self.query = std::mem::take(&mut draft);
                self.refresh_filtered();
                self.set_status(Status::Done(format!(
                    "过滤 \"{}\"：{} 个插件（Esc 清空要按 / 再 Enter）",
                    self.query,
                    self.filtered.len()
                )));
            }
            KeyCode::Backspace => {
                draft.pop();
                self.search = Some(draft.clone());
                self.query = draft;
                self.refresh_filtered();
            }
            KeyCode::Char(character) => {
                draft.push(character);
                self.search = Some(draft.clone());
                self.query = draft;
                self.refresh_filtered();
            }
            // 方向键 / 翻页在输入态不办事，但草稿要留着。
            _ => self.search = Some(draft),
        }
        StoreKey::Consumed
    }

    /// 进入输入态：草稿从当前过滤词接着编，Esc 可以回滚到它。
    fn enter_search(&mut self) {
        self.query_before_search = Some(self.query.clone());
        self.search = Some(self.query.clone());
    }

    fn move_selection(&mut self, delta: isize) {
        if self.filtered.is_empty() {
            return;
        }
        let last = self.filtered.len() as isize - 1;
        let next = self.selected as isize + delta;
        self.selected = next.clamp(0, last) as usize;
        self.detail_scroll = 0;
    }

    fn select_nth(&mut self, row: usize) {
        if self.filtered.is_empty() {
            return;
        }
        self.selected = row.min(self.filtered.len() - 1);
        self.detail_scroll = 0;
    }

    /// PgUp/PgDn：错误文本溢出时先滚它，否则整页翻列表。
    fn page_key(&mut self, direction: isize) {
        if self.error_overflow() {
            let rows = self.status_rows.max(1) as isize;
            self.scroll_error(-direction * rows);
            return;
        }
        let rows = self.view_rows.max(1) as isize;
        self.move_selection(direction * rows);
    }

    fn scroll_error(&mut self, delta: isize) {
        let max = self.error_rows.saturating_sub(self.status_rows) as isize;
        let next = self.error_scroll as isize + delta;
        self.error_scroll = next.clamp(0, max) as u16;
    }
}

// ── 安装 / 卸载 ────────────────────────────────────────────────────

impl Store {
    /// 重新拉索引 + 重扫已安装目录。
    fn refresh(&mut self) {
        self.rescan();
        self.load_index();
    }

    /// 起后台线程拉索引；同一时刻只允许一个。
    fn load_index(&mut self) {
        let Some(url) = self.registry_url.clone() else {
            self.fail(
                "商店未配置：在 fuyubot.toml 写 [plugins].registry_url（或设 FUYUBOT_REGISTRY_URL），再按 r 重试",
            );
            return;
        };
        if self.loading_index {
            return;
        }
        self.loading_index = true;
        self.index_error = None;
        self.set_status(Status::Loading);
        let tx = self.tx.clone();
        let spawned = thread::Builder::new()
            .name("fuyubot-store-index".to_owned())
            .spawn(move || {
                let result = fetch_index(&url);
                let _ = tx.send(Event::Index(result));
            });
        if let Err(error) = spawned {
            self.loading_index = false;
            self.fail(format!("起不了索引线程：{error}（重启面板再试）"));
        }
    }

    /// Enter：按索引装当前选中的插件。
    ///
    /// 所有「不该下载就能判掉」的检查都在主线程做完（yanked / 平台 / ABI / https / sha256），
    /// 只有真正要下东西时才起线程。
    fn start_install(&mut self) {
        if self.busy {
            self.set_status(Status::Working("安装进行中：等这一次结束再操作".to_owned()));
            return;
        }
        if self.registry_url.is_none() {
            self.fail(
                "商店未配置：在 fuyubot.toml 写 [plugins].registry_url（或设 FUYUBOT_REGISTRY_URL），再按 r 重试",
            );
            return;
        }
        let Some(plugin) = self.selected_plugin().cloned() else {
            self.fail("先选一个插件：列表是空的就按 r 重新加载索引，或用 / 改过滤词");
            return;
        };
        let version = match select_version(&plugin) {
            Ok(version) => version,
            Err(error) => {
                self.fail(format!("装不了 {}：{error}", plugin.name));
                return;
            }
        };
        let Some((platform, asset)) = select_asset(version) else {
            self.fail(format!(
                "{} {} 没有当前平台（{}）的安装包，也没有 any 回退：换一个版本，或让作者补发包",
                plugin.name,
                version.version,
                current_platform()
            ));
            return;
        };
        if let Some(abi) = version.abi {
            if abi > ABI_VERSION {
                self.fail(format!(
                    "{} {} 需要 ABI {abi}，本宿主是 {ABI_VERSION}：需要更新的宿主（升级 fuyubot 之后再装）",
                    plugin.name, version.version
                ));
                return;
            }
        }
        if !is_https(&asset.url) {
            self.fail(format!(
                "{} {} 的下载地址不是 https（{:?}）：拒绝明文下载，找上架方换成 https",
                plugin.name, version.version, asset.url
            ));
            return;
        }
        let sha256 = asset.sha256.trim().to_owned();
        if sha256.is_empty() {
            self.fail(format!(
                "{} {} 的索引条目没有 sha256：没有校验值就不装（找上架方补上）",
                plugin.name, version.version
            ));
            return;
        }
        // id 会当目录名用，先按 bundle 契约的语法校验，绝不让索引把路径带出插件目录。
        if let Err(error) = check_id(&plugin.id) {
            self.fail(error);
            return;
        }

        self.busy = true;
        self.set_status(Status::Working(format!(
            "开始安装 {} {}（{platform}）…",
            plugin.id, version.version
        )));
        let tx = self.tx.clone();
        let plugin_dir = self.plugin_dir.clone();
        let id = plugin.id.clone();
        let expected_version = version.version.clone();
        let platform = platform.to_owned();
        let url = asset.url.clone();
        let spawned = thread::Builder::new()
            .name("fuyubot-store-install".to_owned())
            .spawn(move || {
                let outcome = run_install(
                    &tx,
                    &id,
                    &expected_version,
                    &platform,
                    &url,
                    &sha256,
                    &plugin_dir,
                );
                let _ = tx.send(Event::Installed(outcome));
            });
        if let Err(error) = spawned {
            self.busy = false;
            self.fail(format!("起不了安装线程：{error}（重启面板再试）"));
        }
    }

    /// x：删掉已安装的目录（原生 `plugins/<id>/`，脚本 `plugins/scripts/<id>/`）。
    fn uninstall_selected(&mut self) {
        if self.busy {
            self.fail("安装进行中：等它结束再卸载");
            return;
        }
        let Some(plugin) = self.selected_plugin().cloned() else {
            self.fail("先选一个插件：列表是空的就按 r 重新加载索引");
            return;
        };
        let Some(installed) = self.installed(&plugin.id).cloned() else {
            self.fail(format!(
                "{} 没有安装，没什么可卸的：按 Enter 安装",
                plugin.name
            ));
            return;
        };
        // 只删插件目录下的东西：索引是外部数据，不能让 id 把删除引到别处。
        if !installed.path.starts_with(&self.plugin_dir) || installed.path == self.plugin_dir {
            self.fail(format!(
                "拒绝卸载 {}：目标 {} 不在插件目录里（索引数据异常，别手动改它）",
                plugin.name,
                installed.path.display()
            ));
            return;
        }
        match std::fs::remove_dir_all(&installed.path) {
            Ok(()) => {
                self.rescan();
                self.set_status(Status::Done(format!(
                    "已卸载 {} {}：运行中的实例需要重启才会少掉它",
                    plugin.name, installed.version
                )));
            }
            Err(error) => self.fail(format!(
                "卸载 {} 失败：{error}（可能被占用：先停掉实例再试）",
                plugin.name
            )),
        }
    }
}

/// 后台线程里跑的整套：下载 → 校验 → 解压 → 落盘。
fn run_install(
    tx: &Sender<Event>,
    id: &str,
    version: &str,
    platform: &str,
    url: &str,
    expected_sha256: &str,
    plugin_dir: &Path,
) -> Result<InstalledOutcome, String> {
    let label = format!("{id} {version}（{platform}）");
    send_progress(tx, format!("下载 {label}…"));
    let bytes = download(tx, &label, url, PLUGIN_MAX_BYTES)?;
    send_progress(
        tx,
        format!(
            "下载完成（{}），校验 sha256…",
            human_bytes(bytes.len() as u64)
        ),
    );
    verify_sha256(&bytes, expected_sha256)?;
    send_progress(tx, "校验通过，解压到 .staging…".to_owned());
    let path = install_from_bytes(&bytes, id, version, plugin_dir)?;
    let manifest = BundleManifest::load(&path).ok();
    let kind = manifest
        .as_ref()
        .map_or(PluginKind::Native, |manifest| manifest.kind);
    let name = manifest.as_ref().map_or_else(
        || id.to_owned(),
        |manifest| {
            if manifest.name.is_empty() {
                id.to_owned()
            } else {
                manifest.name.clone()
            }
        },
    );
    Ok(InstalledOutcome {
        id: id.to_owned(),
        name,
        version: version.to_owned(),
        kind,
        path,
    })
}

// ── 纯函数（可单测）───────────────────────────────────────────────

/// 选版本：`latest` 对应且未 yanked → 用它；否则按索引顺序往回找最新的未下架版本。
fn select_version(plugin: &IndexPlugin) -> Result<&IndexVersion, String> {
    let latest = plugin
        .latest
        .as_deref()
        .map(str::trim)
        .filter(|latest| !latest.is_empty());
    if let Some(latest) = latest {
        if let Some(version) = plugin
            .versions
            .iter()
            .find(|version| version.version == latest && !version.yanked)
        {
            return Ok(version);
        }
    }
    plugin
        .versions
        .iter()
        .rev()
        .find(|version| !version.yanked)
        .ok_or_else(|| {
            format!(
                "{} 没有未下架的版本（yanked 的不能装）：等上架方发布新版本",
                plugin.name
            )
        })
}

/// 选资产：`current_platform()` 精确匹配 → `any` 回退；都没有 = 这个平台装不了。
pub(crate) fn select_asset(version: &IndexVersion) -> Option<(&str, &Asset)> {
    let platform = current_platform();
    if let Some((key, asset)) = version.assets.get_key_value(platform) {
        return Some((key.as_str(), asset));
    }
    version
        .assets
        .get_key_value("any")
        .map(|(key, asset)| (key.as_str(), asset))
}

/// 校验 sha256（大小写不敏感；索引里空着 = 拒绝安装）。
pub(crate) fn verify_sha256(bytes: &[u8], expected_hex: &str) -> Result<(), String> {
    let expected = expected_hex.trim();
    if expected.is_empty() {
        return Err("索引里没有 sha256：没有校验值就不装（找上架方补上）".to_owned());
    }
    if expected.len() != 64
        || !expected
            .chars()
            .all(|character| character.is_ascii_hexdigit())
    {
        return Err(format!(
            "索引里的 sha256 {expected:?} 不是 64 位十六进制：口径不对，找上架方核对"
        ));
    }
    let actual = hex_lower(&Sha256::digest(bytes));
    if actual.eq_ignore_ascii_case(expected) {
        Ok(())
    } else {
        Err(format!(
            "sha256 不匹配：索引写 {expected}，实际下载到 {actual}\
             （文件被篡改或下载不完整；按 r 重试，仍不对就找上架方）"
        ))
    }
}

/// 从内存里的 zip 装一个包，返回落盘目录。
///
/// 流程与安全检查（契约 §7 / 任务书）：
/// 1. 解开到 `plugin_dir/.staging/<id>/`（`extract_zip` 里防 zip-slip / 符号链接 / zip bomb）；
/// 2. 用 [`BundleManifest`] 再校验一次 `id` / `version` / `entry`；
/// 3. 目标 = `plugin_dir/<id>/`（native）或 `plugin_dir/scripts/<id>/`（python / js）；
/// 4. 目标已存在 → 报错（**不覆盖**，提示先卸载）；
/// 5. `fs::rename(staging, dest)`；成功 / 失败都清掉 staging。
pub(crate) fn install_from_bytes(
    bytes: &[u8],
    expected_id: &str,
    expected_version: &str,
    plugin_dir: &Path,
) -> Result<PathBuf, String> {
    check_id(expected_id)?;
    if expected_version.trim().is_empty() {
        return Err("索引里没有版本号：拒绝安装（索引条目坏了，找上架方）".to_owned());
    }
    let staging = plugin_dir.join(STAGING_DIR).join(expected_id);
    // 上一趟的残留先清掉：不清会让解压和后面的 rename 撞在半个包上。
    cleanup(&staging);

    let outcome = (|| -> Result<PathBuf, String> {
        std::fs::create_dir_all(&staging).map_err(|error| {
            format!(
                "建暂存目录 {} 失败：{error}（检查插件目录权限）",
                staging.display()
            )
        })?;
        extract_zip(bytes, &staging)?;

        let manifest = BundleManifest::load(&staging).map_err(|error| {
            format!("包里的 plugin.json 不合法：{error}（这不是你的问题，找上架方）")
        })?;
        if manifest.id != expected_id {
            return Err(format!(
                "包的 id 是 {:?}，索引写的是 {expected_id:?}：拒绝安装（上架方把包配错了）",
                manifest.id
            ));
        }
        if manifest.version != expected_version {
            return Err(format!(
                "包的版本是 {:?}，索引写的是 {expected_version:?}：拒绝安装（上架方把包配错了）",
                manifest.version
            ));
        }
        // 入口必须存在、且在包内（symlink 也带不出去）；顺带再核一次平台。
        manifest.validate(&staging).map_err(|error| {
            format!("包里的入口 / 平台不可用：{error}（包不完整或不是给这个平台的，找上架方重发）")
        })?;

        let dest = if manifest.kind.is_script() {
            plugin_dir.join(SCRIPTS_DIR).join(expected_id)
        } else {
            plugin_dir.join(expected_id)
        };
        if dest.exists() {
            return Err(format!(
                "{} 已经存在：先卸载旧版本（按 x），再装",
                dest.display()
            ));
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                format!("建 {} 失败：{error}（检查插件目录权限）", parent.display())
            })?;
        }
        std::fs::rename(&staging, &dest).map_err(|error| {
            format!(
                "把包挪到 {} 失败：{error}（被杀软 / 占用挡住了；关掉占用再试）",
                dest.display()
            )
        })?;
        Ok(dest)
    })();

    // 成功时 staging 已经被 rename 走；失败时把半成品清掉，别让下次误用。
    cleanup(&staging);
    outcome
}

/// 把 zip 解开到 `dest`，只接受「安全的名字 + 普通文件 / 目录」。
///
/// 安全规则（契约 §7）：
/// - `enclosed_name()`：拒绝绝对路径与能跑出目标目录的 `..`（zip-slip）；
/// - `unix_mode()` 的文件类型位只允许普通文件 / 目录，符号链接与 FIFO 之类一律拒绝；
/// - 路径段拒绝 Windows 保留名 / 尾点 / 尾空格 / `:`（ADS），条目名做大小写折叠查重；
/// - 文件数 ≤ 2000、解压后总大小 ≤ 200MB（边写边数，不信任头里写的大小）。
pub(crate) fn extract_zip(bytes: &[u8], dest: &Path) -> Result<(), String> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes))
        .map_err(|error| format!("插件包不是合法的 zip：{error}（重新下载；仍然坏就找上架方）"))?;
    // 粗筛：条目（含目录）太多就根本不进循环；真正的「文件数」在下面按文件数。
    if archive.len() > EXTRACT_MAX_FILES * 2 {
        return Err(format!(
            "插件包里有 {} 个条目，超过上限：拒绝解压（找上架方精简包内容）",
            archive.len()
        ));
    }
    std::fs::create_dir_all(dest)
        .map_err(|error| format!("建暂存目录 {} 失败：{error}", dest.display()))?;

    let mut total: u64 = 0;
    let mut files = 0usize;
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(|error| {
            format!("读插件包第 {index} 个条目失败：{error}（包损坏，重新下载）")
        })?;
        let name = entry.name().to_owned();
        let Some(relative) = entry.enclosed_name() else {
            return Err(format!(
                "插件包里的条目 {name:?} 是绝对路径或含 `..`（zip-slip），拒绝解压"
            ));
        };
        if relative.as_os_str().is_empty() {
            continue;
        }
        // Windows 文件系统大小写不敏感：`bin/x.dll` 与 `BIN/X.DLL` 解压后是同一个文件，
        // 后一个会静默覆盖前一个。不同分隔符写法也归一化后再比。
        let key = relative.to_string_lossy().replace('\\', "/").to_lowercase();
        if !seen.insert(key) {
            return Err(format!(
                "插件包里的条目 {name:?} 与前面的条目只差大小写 / 分隔符：\
                 Windows 上会互相覆盖，拒绝解压"
            ));
        }
        validate_entry_path(&relative)
            .map_err(|reason| format!("插件包里的条目 {name:?} {reason}，拒绝解压"))?;
        // 文件类型位：0（没写 unix mode）与 0o100000（普通文件）放行，目录单独处理，
        // 其余（0o120000 符号链接、FIFO、设备节点…）一律拒绝。
        let is_dir = entry.is_dir();
        let file_type = entry.unix_mode().unwrap_or(0) & 0o170000;
        if !is_dir && file_type != 0 && file_type != 0o100000 {
            return Err(format!(
                "插件包里的条目 {name:?} 是符号链接或特殊文件，拒绝解压"
            ));
        }
        let path = dest.join(&relative);
        if !path.starts_with(dest) {
            // `enclosed_name` 已经挡住了，这里是冗余防线：真的走到这说明实现被改坏了。
            return Err(format!(
                "插件包里的条目 {name:?} 解析后跑到目标目录之外，拒绝解压"
            ));
        }
        if is_dir {
            std::fs::create_dir_all(&path)
                .map_err(|error| format!("建目录 {} 失败：{error}", path.display()))?;
            continue;
        }
        files += 1;
        if files > EXTRACT_MAX_FILES {
            return Err(format!(
                "插件包里的文件超过 {EXTRACT_MAX_FILES} 个：拒绝解压（找上架方精简包内容）"
            ));
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("建目录 {} 失败：{error}", parent.display()))?;
        }
        let mut out = std::fs::File::create(&path)
            .map_err(|error| format!("写 {} 失败：{error}", path.display()))?;
        copy_entry(&mut entry, &mut out, &mut total, &name)?;
    }
    Ok(())
}

/// 逐段校验解压路径：拒绝 Windows 保留名、尾点 / 尾空格、`:`（NTFS 数据流）。
///
/// 这些名字在 Linux 上能建出来、到 Windows 上会失败或静默改名；插件包要跨平台，
/// 所以统一在解压前拒绝，别让「同一份包在作者机器上好好的」变成用户侧故障。
fn validate_entry_path(relative: &Path) -> Result<(), &'static str> {
    for component in relative.components() {
        let std::path::Component::Normal(part) = component else {
            return Err("含非普通路径段");
        };
        let text = part.to_string_lossy();
        if text.ends_with('.') || text.ends_with(' ') {
            return Err("的路径段以点或空格结尾（Windows 会静默改名）");
        }
        if text.contains(':') {
            return Err("的路径段含 `:`（盘符 / NTFS 数据流）");
        }
        let stem = text.split('.').next().unwrap_or_default();
        if is_windows_reserved_name(stem) {
            return Err("是 Windows 保留设备名");
        }
    }
    Ok(())
}

/// `CON` / `NUL` / `COM1..9` / `LPT1..9`（大小写不敏感，`CON.txt` 同样非法）。
fn is_windows_reserved_name(segment: &str) -> bool {
    let upper = segment.trim().to_ascii_uppercase();
    if matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL") {
        return true;
    }
    let bytes = upper.as_bytes();
    bytes.len() == 4
        && (upper.starts_with("COM") || upper.starts_with("LPT"))
        && bytes[3].is_ascii_digit()
        && bytes[3] != b'0'
}

// ── 网络 ───────────────────────────────────────────────────────────

/// 拉索引（后台线程入口）：≤ 2MB，解析后用 [`parse_index`] 校验 schema。
fn fetch_index(url: &str) -> Result<Index, String> {
    if !is_https(url) {
        return Err(format!(
            "索引地址 {url:?} 不是 https：拒绝明文拉取（改成 https，或按 r 重试）"
        ));
    }
    let mut response = http_agent()
        .get(url)
        .call()
        .map_err(|error| http_error("索引地址", url, error))?;
    let bytes = response
        .body_mut()
        .with_config()
        .limit(INDEX_MAX_BYTES)
        .read_to_vec()
        .map_err(|error| {
            format!(
                "读索引内容失败：{error}（上限 {}，索引太大或连接断了；按 r 重试）",
                human_bytes(INDEX_MAX_BYTES)
            )
        })?;
    parse_index(&bytes)
}

/// 解析索引 JSON 并确认 schema 是本宿主认识的。
fn parse_index(bytes: &[u8]) -> Result<Index, String> {
    let index: Index = serde_json::from_slice(bytes).map_err(|error| {
        format!("索引不是合法 JSON：{error}（对照文档站「插件分发格式 v1」§7 的字段）")
    })?;
    if index.schema != INDEX_SCHEMA {
        return Err(format!(
            "索引的 schema 是 {}，本宿主只认 {INDEX_SCHEMA}：升级宿主，或换一份兼容的索引",
            index.schema
        ));
    }
    Ok(index)
}

/// 下载插件包（≤ `limit`），边下边报进度。
fn download(tx: &Sender<Event>, label: &str, url: &str, limit: u64) -> Result<Vec<u8>, String> {
    if !is_https(url) {
        return Err(format!(
            "下载地址不是 https：{url}（拒绝明文下载，找上架方换成 https）"
        ));
    }
    let mut response = http_agent()
        .get(url)
        .call()
        .map_err(|error| http_error("下载地址", url, error))?;
    let total = response.body().content_length();
    if let Some(total) = total {
        if total > limit {
            return Err(format!(
                "插件包 {}，超过 {} 上限：拒绝下载（找上架方精简）",
                human_bytes(total),
                human_bytes(limit)
            ));
        }
    }
    let mut reader = response.body_mut().with_config().limit(limit).reader();
    let mut bytes: Vec<u8> = Vec::with_capacity(total.unwrap_or(0).min(limit) as usize);
    let mut buffer = [0u8; 64 * 1024];
    let mut shown_percent = 0u64;
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|error| format!("下载中断：{error}（网络不稳；按 r 重试）"))?;
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..read]);
        if bytes.len() as u64 > limit {
            return Err(format!(
                "插件包超过 {} 上限：已中止下载（找上架方精简）",
                human_bytes(limit)
            ));
        }
        if let Some(total) = total.filter(|total| *total > 0) {
            let percent = (bytes.len() as u64 * 100 / total).min(100);
            if percent >= shown_percent + 5 || percent == 100 {
                shown_percent = percent;
                send_progress(
                    tx,
                    format!(
                        "下载 {label}：{} / {}（{percent}%）",
                        human_bytes(bytes.len() as u64),
                        human_bytes(total)
                    ),
                );
            }
        }
    }
    if let Some(total) = total {
        if bytes.len() as u64 != total {
            return Err(format!(
                "下载不完整：拿到 {} / {} 字节（按 r 重试）",
                bytes.len(),
                total
            ));
        }
    }
    Ok(bytes)
}

/// 统一的 HTTP 错误文案：状态码与网络问题分开说，都给出下一步。
fn http_error(what: &str, url: &str, error: ureq::Error) -> String {
    match error {
        ureq::Error::StatusCode(code) => format!(
            "{what} {url} 返回 HTTP {code}（地址错了 / 需要登录 / 服务端故障；按 r 重试或找上架方）"
        ),
        other => {
            format!("连不上{what} {url}：{other}（检查网络 / 代理 / 系统时间，稍后按 r 重试）")
        }
    }
}

/// 带超时的 agent：**不用**整体超时，否则慢网上下 50MB 会被一刀切掉；
/// 连接 / 响应头 / body 分段计时，既有上限又不至于把正常的慢下载判死。
fn http_agent() -> ureq::Agent {
    ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .timeout_resolve(Some(HTTP_CONNECT_TIMEOUT))
            .timeout_connect(Some(HTTP_CONNECT_TIMEOUT))
            .timeout_recv_response(Some(HTTP_HEADER_TIMEOUT))
            .timeout_recv_body(Some(HTTP_BODY_TIMEOUT))
            .build(),
    )
}

fn send_progress(tx: &Sender<Event>, text: String) {
    let _ = tx.send(Event::Progress(text));
}

// ── 通道 / 已安装扫描 ─────────────────────────────────────────────

impl Store {
    fn drain_events(&mut self) {
        // 先收干净再处理：处理里会 `&mut self`，直接借用 `self.rx` 会和它打架。
        let events: Vec<Event> = std::iter::from_fn(|| self.rx.try_recv().ok()).collect();
        for event in events {
            self.handle_event(event);
        }
    }

    fn handle_event(&mut self, event: Event) {
        match event {
            Event::Index(Ok(index)) => {
                self.loading_index = false;
                self.index_error = None;
                let count = index.plugins.len();
                self.index = Some(index);
                self.refresh_filtered();
                self.scan_dirty = true;
                self.set_status(Status::Done(format!(
                    "索引已更新：{count} 个插件（/ 搜索，Enter 安装，x 卸载）"
                )));
            }
            Event::Index(Err(error)) => {
                self.loading_index = false;
                // 旧索引留着：网络抖一下不该把列表清空。
                self.index_error = Some(error);
                self.status = Status::Idle;
                self.error_scroll = 0;
            }
            Event::Progress(text) => self.set_status(Status::Working(text)),
            Event::Installed(Ok(outcome)) => {
                self.busy = false;
                self.scan_dirty = true;
                self.rescan();
                self.select_by_id(&outcome.id);
                let message = if outcome.kind.is_script() {
                    format!(
                        "已安装 {} {}（脚本插件 → {}）",
                        outcome.name,
                        outcome.version,
                        outcome.path.display()
                    )
                } else {
                    format!(
                        "已安装 {} {} → {}：重启后生效（原生动态库）",
                        outcome.name,
                        outcome.version,
                        outcome.path.display()
                    )
                };
                self.set_status(Status::Done(message));
            }
            Event::Installed(Err(error)) => {
                self.busy = false;
                self.scan_dirty = true;
                self.fail(format!("安装失败：{error}"));
            }
        }
    }

    /// 重扫已安装目录（只读文件系统，不联网、不 dlopen）。
    fn rescan(&mut self) {
        self.installed = scan_installed(&self.plugin_dir);
        self.last_scan = Instant::now();
        self.scan_dirty = false;
    }

    fn installed(&self, id: &str) -> Option<&Installed> {
        self.installed.iter().find(|item| item.id == id)
    }

    /// 安装完成后把列表选到刚装的那个插件上。
    fn select_by_id(&mut self, id: &str) {
        let Some(index) = self.index.as_ref() else {
            return;
        };
        let Some(position) = index.plugins.iter().position(|plugin| plugin.id == id) else {
            return;
        };
        if let Some(row) = self
            .filtered
            .iter()
            .position(|candidate| *candidate == position)
        {
            self.selected = row;
            self.detail_scroll = 0;
        }
    }

    fn selected_plugin(&self) -> Option<&IndexPlugin> {
        let index = self.index.as_ref()?;
        let position = *self.filtered.get(self.selected)?;
        index.plugins.get(position)
    }

    /// 重算过滤结果（索引变了 / 过滤词变了才需要调）。
    fn refresh_filtered(&mut self) {
        let query = self.query.trim().to_ascii_lowercase();
        self.filtered = match &self.index {
            Some(index) => index
                .plugins
                .iter()
                .enumerate()
                .filter(|(_, plugin)| plugin_matches(plugin, &query))
                .map(|(position, _)| position)
                .collect(),
            None => Vec::new(),
        };
        if self.selected >= self.filtered.len() {
            self.selected = self.filtered.len().saturating_sub(1);
        }
        self.detail_scroll = 0;
    }

    fn set_status(&mut self, status: Status) {
        if matches!(status, Status::Error(_)) {
            self.error_scroll = 0;
        }
        self.status = status;
    }

    fn fail(&mut self, text: impl Into<String>) {
        self.set_status(Status::Error(text.into()));
    }

    /// 状态区里要不要画错误正文（错误 / 索引错误都走同一块）。
    fn error_text(&self) -> Option<String> {
        match &self.status {
            Status::Error(text) => Some(text.clone()),
            _ => self.index_error.clone(),
        }
    }

    fn error_overflow(&self) -> bool {
        self.error_text().is_some() && self.error_rows > self.status_rows
    }

    fn status_line(&self) -> (String, Color) {
        match &self.status {
            Status::Error(text) => (format!("✕ {}", first_line(text)), Color::Red),
            Status::Working(text) => (format!("… {}", first_line(text)), Color::Yellow),
            Status::Loading => ("… 正在加载商店索引".to_owned(), Color::Yellow),
            Status::Done(text) => (format!("✓ {}", first_line(text)), Color::Green),
            Status::Idle => {
                if self.index_error.is_some() {
                    (
                        "✕ 商店索引加载失败（错误原文见下 · r 重试）".to_owned(),
                        Color::Red,
                    )
                } else if self.registry_url.is_none() {
                    (
                        "商店未配置：在 fuyubot.toml 写 [plugins].registry_url（或设 FUYUBOT_REGISTRY_URL）"
                            .to_owned(),
                        Color::Yellow,
                    )
                } else if self.loading_index {
                    ("… 正在加载商店索引".to_owned(), Color::Yellow)
                } else {
                    ("就绪：↑↓ 选择 · Enter 安装".to_owned(), Color::DarkGray)
                }
            }
        }
    }

    /// 状态区多高：平时两行（状态 + 键提示），有错误时给正文留几行。
    fn status_height(&self, area: Rect) -> u16 {
        let wanted = if self.error_text().is_some() {
            (area.height / 3).clamp(4, 10)
        } else {
            2
        };
        let max = area.height.saturating_sub(2).max(1);
        wanted.min(max)
    }

    fn empty_list_message(&self) -> &'static str {
        if self.loading_index {
            "正在加载索引…"
        } else if self.registry_url.is_none() {
            "商店未配置：在 fuyubot.toml 的 [plugins] 里写 registry_url（或设 FUYUBOT_REGISTRY_URL），再按 r"
        } else if self.index.is_none() {
            "索引还没加载成功：按 r 重试（错误原文在下面）"
        } else if self.query.trim().is_empty() {
            "索引里一个插件都没有——按 r 重新拉一次"
        } else {
            "没有匹配的插件：Esc 退出搜索后改过滤词"
        }
    }
}

/// 扫已安装的包：`plugin_dir/<id>/plugin.json` 与 `plugin_dir/scripts/<id>/plugin.json`。
///
/// 只下一层、跳过 `.` 开头的目录与 `*.disable`（契约 §2）。读不出来的清单直接跳过：
/// 商店页只列「能玩的」，坏包留给 `plugin list` 报错。
fn scan_installed(plugin_dir: &Path) -> Vec<Installed> {
    let mut out = Vec::new();
    collect_installed(plugin_dir, &mut out);
    collect_installed(&plugin_dir.join(SCRIPTS_DIR), &mut out);
    out.sort_by(|left, right| left.id.cmp(&right.id));
    out
}

fn collect_installed(dir: &Path, out: &mut Vec<Installed>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        if name.starts_with('.') || name.ends_with(".disable") {
            continue;
        }
        if !path.join(BUNDLE_MANIFEST).is_file() {
            continue;
        }
        let Ok(manifest) = BundleManifest::load(&path) else {
            continue;
        };
        out.push(Installed {
            id: manifest.id,
            version: manifest.version,
            kind: manifest.kind,
            path,
        });
    }
}

/// 过滤：id / name / summary 任一命中（大小写不敏感）。
fn plugin_matches(plugin: &IndexPlugin, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let contains = |haystack: &str| haystack.to_ascii_lowercase().contains(query);
    contains(&plugin.id) || contains(&plugin.name) || contains(&plugin.summary)
}

// ── 绘制 ───────────────────────────────────────────────────────────

impl Store {
    fn render_list(&mut self, frame: &mut Frame, area: Rect) {
        let title = match (&self.search, self.query.is_empty()) {
            (Some(draft), _) => format!(" 搜索 /{draft}▏ "),
            (None, false) => format!(" 过滤 \"{}\" · {} ", self.query, self.filtered.len()),
            (None, true) => format!(" 索引 · {} ", self.filtered.len()),
        };
        let block = Block::bordered()
            .title(Line::from(Span::styled(
                title,
                Style::new().add_modifier(Modifier::BOLD),
            )))
            .border_style(Style::new().fg(Color::Cyan));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        self.view_rows = inner.height.max(1) as usize;

        if self.filtered.is_empty() {
            let message = self.empty_list_message();
            frame.render_widget(
                Paragraph::new(message)
                    .wrap(Wrap { trim: false })
                    .style(Style::new().fg(Color::DarkGray)),
                inner,
            );
            return;
        }
        let Some(index) = self.index.as_ref() else {
            return;
        };
        let items: Vec<ListItem> = self
            .filtered
            .iter()
            .filter_map(|position| index.plugins.get(*position))
            .map(|plugin| self.list_item(plugin))
            .collect();
        let mut state = ListState::default()
            .with_selected(Some(self.selected))
            .with_offset(self.list_offset);
        let list = List::new(items)
            .highlight_style(Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD))
            .highlight_symbol("▸ ")
            .block(Block::new().padding(Padding::new(1, 0, 0, 0)));
        frame.render_stateful_widget(list, inner, &mut state);
        self.list_offset = state.offset();
    }

    fn list_item(&self, plugin: &IndexPlugin) -> ListItem<'static> {
        let version = plugin
            .latest
            .as_deref()
            .map(str::trim)
            .filter(|version| !version.is_empty())
            .unwrap_or("—");
        let mut spans = vec![
            Span::styled(
                plugin.name.clone(),
                Style::new().add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("  {version}"), Style::new().fg(Color::DarkGray)),
        ];
        if self.installed(&plugin.id).is_some() {
            spans.push(Span::styled(
                "  [已安装]",
                Style::new().fg(Color::Green).add_modifier(Modifier::BOLD),
            ));
        }
        ListItem::new(Line::from(spans))
    }

    fn render_detail(&mut self, frame: &mut Frame, area: Rect) {
        let title = match self.selected_plugin() {
            Some(plugin) => format!(" 详情 · {} ", plugin.name),
            None => " 详情 ".to_owned(),
        };
        let block = Block::bordered()
            .title(Line::from(Span::styled(
                title,
                Style::new().fg(Color::Gray),
            )))
            .border_style(Style::new().fg(Color::DarkGray));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let Some(plugin) = self.selected_plugin().cloned() else {
            frame.render_widget(
                Paragraph::new("（左列表里选一个插件看详情）")
                    .style(Style::new().fg(Color::DarkGray)),
                inner,
            );
            return;
        };
        let lines = self.detail_lines(&plugin);
        // `Paragraph::line_count` 在 ratatui 里是 unstable API（下游拿不到），自己数折行。
        let rows = wrapped_rows(&lines, inner.width);
        let max = rows.saturating_sub(inner.height as usize) as u16;
        self.detail_scroll = self.detail_scroll.min(max);
        let paragraph = Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((self.detail_scroll, 0));
        frame.render_widget(paragraph, inner);
    }

    fn detail_lines(&self, plugin: &IndexPlugin) -> Vec<Line<'static>> {
        let mut lines = vec![
            Line::from(Span::styled(
                plugin.name.clone(),
                Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                plugin.id.clone(),
                Style::new().fg(Color::DarkGray),
            )),
            Line::from(""),
        ];

        let version = select_version(plugin).ok();
        let latest = plugin
            .latest
            .as_deref()
            .map(str::trim)
            .filter(|latest| !latest.is_empty())
            .unwrap_or("—");
        lines.push(kv("最新版本", latest.to_owned()));
        if let Some(installed) = self.installed(&plugin.id) {
            lines.push(kv(
                "已安装",
                format!("{}（{}）", installed.version, installed.kind.as_str()),
            ));
        }
        if let Some(version) = version {
            let abi = match version.abi {
                Some(abi) if abi > ABI_VERSION => kv_styled(
                    "ABI",
                    format!("{abi}（需要更新的宿主：本宿主 {ABI_VERSION}）"),
                    Color::Red,
                ),
                Some(abi) => kv("ABI", format!("{abi}（宿主 {ABI_VERSION}）")),
                None => kv("ABI", "—（清单没写，装完以 plugin_init 为准）".to_owned()),
            };
            lines.push(abi);
            if !version.api.is_empty() {
                lines.push(kv("宿主 API", version.api.clone()));
            }
            match select_asset(version) {
                Some((platform, asset)) => {
                    let size = match asset.size {
                        Some(size) if size > PLUGIN_MAX_BYTES => format!(
                            "{}，超过 {} 上限：装不了",
                            human_bytes(size),
                            human_bytes(PLUGIN_MAX_BYTES)
                        ),
                        Some(size) => human_bytes(size),
                        None => "大小未知".to_owned(),
                    };
                    lines.push(kv("平台", format!("{platform} · {size}")));
                }
                None => lines.push(kv_styled(
                    "平台",
                    format!("没有 {} 的包，也没有 any 回退", current_platform()),
                    Color::Red,
                )),
            }
            lines.push(kv(
                "依赖",
                if version.depends.is_empty() {
                    "无".to_owned()
                } else {
                    version.depends.join(", ")
                },
            ));
        }
        if !plugin.versions.is_empty() {
            let versions = plugin
                .versions
                .iter()
                .map(|version| {
                    if version.yanked {
                        format!("{}（已下架）", version.version)
                    } else {
                        version.version.clone()
                    }
                })
                .collect::<Vec<_>>()
                .join(", ");
            lines.push(kv("版本", versions));
        }
        if !plugin.tags.is_empty() {
            lines.push(kv("标签", plugin.tags.join(", ")));
        }
        lines.push(kv(
            "许可证",
            if plugin.license.is_empty() {
                "—".to_owned()
            } else {
                plugin.license.clone()
            },
        ));
        if !plugin.repo.is_empty() {
            lines.push(kv("仓库", plugin.repo.clone()));
        }
        if !plugin.homepage.is_empty() {
            lines.push(kv("主页", plugin.homepage.clone()));
        }
        if !plugin.summary.is_empty() {
            lines.push(Line::from(""));
            lines.push(section("简介"));
            lines.push(Line::from(plugin.summary.clone()));
        }
        if !plugin.description.is_empty() {
            lines.push(Line::from(""));
            lines.push(section("描述"));
            for line in plugin.description.lines() {
                lines.push(Line::from(line.to_owned()));
            }
        }
        lines
    }

    fn render_status(&mut self, frame: &mut Frame, area: Rect) {
        if area.height == 0 {
            return;
        }
        let [status_row, error_area, hint_row] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .areas(area);

        let (text, color) = self.status_line();
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(text, Style::new().fg(color)))),
            status_row,
        );

        self.status_rows = error_area.height;
        if let Some(error) = self.error_text() {
            self.error_rows = wrapped_text_rows(&error, error_area.width);
            let max = self.error_rows.saturating_sub(error_area.height);
            self.error_scroll = self.error_scroll.min(max);
            frame.render_widget(
                Paragraph::new(error)
                    .wrap(Wrap { trim: false })
                    .style(Style::new().fg(Color::Red))
                    .scroll((self.error_scroll, 0)),
                error_area,
            );
        }

        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                self.footer_hint(),
                Style::new().fg(Color::DarkGray),
            ))),
            hint_row,
        );
    }
}

fn kv(key: &str, value: String) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{key}  "), Style::new().fg(Color::DarkGray)),
        Span::raw(value),
    ])
}

fn kv_styled(key: &str, value: String, color: Color) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{key}  "), Style::new().fg(Color::DarkGray)),
        Span::styled(value, Style::new().fg(color)),
    ])
}

fn section(title: &str) -> Line<'static> {
    Line::from(Span::styled(
        title.to_owned(),
        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
    ))
}

/// 一组 [`Line`] 在给定宽度下折行后的行数（只用于滚动上限，不必精确到像素）。
fn wrapped_rows(lines: &[Line<'_>], width: u16) -> usize {
    let width = usize::from(width.max(1));
    lines
        .iter()
        .map(|line| line.width().max(1).div_ceil(width))
        .sum()
}

/// 一段纯文本折行后的行数（错误原文用；语义与 [`wrapped_rows`] 一致）。
fn wrapped_text_rows(text: &str, width: u16) -> u16 {
    let width = usize::from(width.max(1));
    let rows: usize = text
        .lines()
        .map(|line| {
            unicode_width::UnicodeWidthStr::width(line)
                .max(1)
                .div_ceil(width)
        })
        .sum();
    u16::try_from(rows).unwrap_or(u16::MAX)
}

// ── 小工具 ─────────────────────────────────────────────────────────

/// 下载 / 校验用：字节数换成人读的大小。
fn human_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    let value = bytes as f64;
    if value >= MB {
        format!("{:.1} MB", value / MB)
    } else if value >= KB {
        format!("{:.1} KB", value / KB)
    } else {
        format!("{bytes} B")
    }
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn is_https(url: &str) -> bool {
    url.trim().to_ascii_lowercase().starts_with("https://")
}

/// 错误正文只取第一行放到单行的状态行里（完整原文在下面的错误区）。
fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or(text)
}

/// 插件 id 的语法校验：与 `fuyubot-loader` 的 bundle 契约保持一致。
///
/// 索引是**外部数据**，`id` 会被拼进 `.staging/<id>/` 与 `plugins/<id>/`，
/// 所以在任何路径操作之前先挡掉 `/`、`\`、`..`、开头的 `.` 这些写法。
fn check_id(id: &str) -> Result<(), String> {
    let bad = || {
        format!(
            "商店给的插件 id {id:?} 不合法（只允许 [A-Za-z0-9._-] 且不能以 . 开头）：\
             这不是你的问题，找上架方修索引"
        )
    };
    if id.is_empty() || id.len() > 128 || id.starts_with('.') {
        return Err(bad());
    }
    let mut chars = id.chars();
    let first = chars.next().unwrap_or_default();
    if !first.is_ascii_alphanumeric() {
        return Err(bad());
    }
    if !id
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-'))
    {
        return Err(bad());
    }
    Ok(())
}

/// 把一个 zip 条目拷到文件里，同时累计解压后的总大小（不信头里写的 size）。
fn copy_entry(
    entry: &mut impl Read,
    out: &mut impl Write,
    total: &mut u64,
    name: &str,
) -> Result<(), String> {
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = entry
            .read(&mut buffer)
            .map_err(|error| format!("读插件包条目 {name:?} 失败：{error}（包损坏，重新下载）"))?;
        if read == 0 {
            return Ok(());
        }
        *total = total.saturating_add(read as u64);
        if *total > EXTRACT_MAX_BYTES {
            return Err(format!(
                "解压后总大小超过 {} 上限：拒绝解压（zip bomb 或包太大，找上架方）",
                human_bytes(EXTRACT_MAX_BYTES)
            ));
        }
        out.write_all(&buffer[..read])
            .map_err(|error| format!("写 {name:?} 失败：{error}（磁盘满 / 没权限）"))?;
    }
}

/// 删掉一个路径（文件或目录）；不存在当成功。符号链接按链接本身删，不跟进去。
fn cleanup(path: &Path) {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => {
            let _ = std::fs::remove_dir_all(path);
        }
        Ok(_) => {
            let _ = std::fs::remove_file(path);
        }
        Err(_) => {}
    }
}

// ── 测试 ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试临时目录：仓库约定放 `target/test-tmp`（不用系统 TEMP）。
    fn temp(name: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-tmp/fuyubot-store")
            .join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建测试临时目录失败");
        dir
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// 造一个 zip（Stored，不依赖 flate2 后端）。
    fn zip_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buffer = std::io::Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut buffer);
            for (name, contents) in entries {
                let options = zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Stored);
                writer.start_file(*name, options).expect("写 zip 条目失败");
                writer.write_all(contents).expect("写 zip 内容失败");
            }
            writer.finish().expect("收尾 zip 失败");
        }
        buffer.into_inner()
    }

    /// 一个最小可装的包：`plugin.json` + 入口文件。
    fn plugin_zip(id: &str, version: &str, kind: &str, entry: &str, entry_bytes: &[u8]) -> Vec<u8> {
        let manifest = format!(
            r#"{{"schema":1,"id":"{id}","name":"Demo","version":"{version}","kind":"{kind}","entry":"{entry}"}}"#
        );
        zip_bytes(&[(BUNDLE_MANIFEST, manifest.as_bytes()), (entry, entry_bytes)])
    }

    /// 一段索引 JSON：两个平台资产，一个 any 回退，一个 yanked 版本。
    const INDEX_JSON: &str = r#"
    {
      "schema": 1,
      "plugins": [{
        "id": "com.example.weather",
        "name": "Weather",
        "summary": "一句话天气",
        "description": "长描述\n第二行",
        "repo": "https://github.com/owner/repo",
        "license": "MIT",
        "homepage": "https://example.com",
        "tags": ["weather"],
        "latest": "1.2.0",
        "versions": [
          {
            "version": "1.0.0",
            "api": ">=1.0",
            "abi": 5,
            "depends": ["fuyubot.help.commands"],
            "yanked": true,
            "assets": {"windows-x86_64": {"url": "https://example.com/old.zip", "sha256": "aa", "size": 10}}
          },
          {
            "version": "1.2.0",
            "api": ">=1.0, <2.0",
            "abi": 5,
            "depends": [],
            "yanked": false,
            "assets": {
              "windows-x86_64": {"url": "https://example.com/new.zip", "sha256": "BB", "size": 1234},
              "any": {"url": "https://example.com/any.zip", "sha256": "cc", "size": 42}
            }
          }
        ]
      }]
    }
    "#;

    #[test]
    fn index_json_parses_with_defaults() {
        let index = parse_index(INDEX_JSON.as_bytes()).expect("示例索引该解析成功");
        assert_eq!(index.schema, INDEX_SCHEMA);
        assert_eq!(index.plugins.len(), 1);
        let plugin = &index.plugins[0];
        assert_eq!(plugin.id, "com.example.weather");
        assert_eq!(plugin.latest.as_deref(), Some("1.2.0"));
        assert_eq!(plugin.tags, ["weather"]);
        assert_eq!(plugin.versions.len(), 2);
        assert!(plugin.versions[0].yanked);
        assert_eq!(plugin.versions[1].abi, Some(5));
        assert_eq!(plugin.versions[1].depends, Vec::<String>::new());
        // 缺省字段全靠 serde(default) 兜住：空索引也能解析。
        let empty = parse_index(br#"{"schema":1}"#).expect("空索引该解析成功");
        assert!(empty.plugins.is_empty());
        let bare = parse_index(br#"{"schema":1,"plugins":[{"id":"x"}]}"#).unwrap();
        assert_eq!(bare.plugins[0].name, "");
        assert!(bare.plugins[0].versions.is_empty());
        assert!(bare.plugins[0].latest.is_none());
    }

    #[test]
    fn an_unknown_index_schema_is_reported() {
        let error = parse_index(br#"{"schema":2,"plugins":[]}"#).expect_err("schema=2 该被拒绝");
        assert!(error.contains("schema"), "{error}");
        let error = parse_index(b"{ not json").expect_err("坏 JSON 该被拒绝");
        assert!(error.contains("JSON"), "{error}");
    }

    #[test]
    fn select_asset_prefers_the_host_platform() {
        let index = parse_index(INDEX_JSON.as_bytes()).unwrap();
        let version = &index.plugins[0].versions[1];
        let (platform, asset) = select_asset(version).expect("应该有当前平台的包");
        if current_platform() == "windows-x86_64" {
            assert_eq!(platform, "windows-x86_64");
            assert_eq!(asset.url, "https://example.com/new.zip");
        } else {
            // 其它平台上精确匹配不到，应该回退到 any。
            assert_eq!(platform, "any");
        }
    }

    #[test]
    fn select_asset_falls_back_to_any() {
        let version: IndexVersion = serde_json::from_str(
            r#"{"version":"1.0.0","assets":{
                "plan9-mips":{"url":"https://example.com/p.zip","sha256":"aa"},
                "any":{"url":"https://example.com/any.zip","sha256":"bb"}}}"#,
        )
        .unwrap();
        let (platform, asset) = select_asset(&version).expect("any 该兜底");
        assert_eq!(platform, "any");
        assert_eq!(asset.url, "https://example.com/any.zip");
    }

    #[test]
    fn select_asset_is_none_without_a_matching_platform() {
        let version: IndexVersion = serde_json::from_str(
            r#"{"version":"1.0.0","assets":{"plan9-mips":{"url":"https://example.com/p.zip","sha256":"aa"}}}"#,
        )
        .unwrap();
        assert!(select_asset(&version).is_none());
    }

    #[test]
    fn select_version_skips_yanked_versions() {
        let index = parse_index(INDEX_JSON.as_bytes()).unwrap();
        let plugin = &index.plugins[0];
        let version = select_version(plugin).expect("1.2.0 没下架，该选它");
        assert_eq!(version.version, "1.2.0");

        // latest 指向一个 yanked 版本：跳过它，按顺序往回找可用的。
        let mut plugin = plugin.clone();
        plugin.latest = Some("1.0.0".to_owned());
        let version = select_version(&plugin).expect("该跳过 yanked 找到 1.2.0");
        assert_eq!(version.version, "1.2.0");

        // latest 缺省也一样往回找。
        plugin.latest = None;
        assert_eq!(select_version(&plugin).unwrap().version, "1.2.0");

        // 全被 yanked：明确报错，不装。
        for version in &mut plugin.versions {
            version.yanked = true;
        }
        let error = select_version(&plugin).expect_err("全 yanked 该报错");
        assert!(error.contains("yanked"), "{error}");
    }

    #[test]
    fn verify_sha256_is_case_insensitive() {
        let bytes = b"abc";
        let lower = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        verify_sha256(bytes, lower).expect("小写该通过");
        verify_sha256(bytes, &lower.to_ascii_uppercase()).expect("大写也该通过");
        // 前后空白不算数。
        verify_sha256(bytes, &format!("  {lower}  ")).expect("空白该被忽略");
    }

    #[test]
    fn verify_sha256_rejects_mismatch_and_missing_value() {
        let mismatch = verify_sha256(b"abc", "00").expect_err("长度不对该被拒绝");
        assert!(mismatch.contains("sha256"), "{mismatch}");
        let mismatch = verify_sha256(
            b"abc",
            "0000000000000000000000000000000000000000000000000000000000000000",
        )
        .expect_err("内容不匹配该被拒绝");
        assert!(mismatch.contains("不匹配"), "{mismatch}");
        let missing = verify_sha256(b"abc", "   ").expect_err("空 sha256 该被拒绝");
        assert!(missing.contains("sha256"), "{missing}");
    }

    #[test]
    fn extract_zip_rejects_zip_slip() {
        let root = temp("zip-slip");
        let bytes = zip_bytes(&[("../evil.txt", b"pwned")]);
        let dest = root.join("out");
        let error = extract_zip(&bytes, &dest).expect_err("zip-slip 必须被拒绝");
        assert!(
            error.contains("zip-slip") || error.contains(".."),
            "{error}"
        );
        assert!(!root.join("evil.txt").exists(), "不许在目标目录之外落文件");
        assert!(!dest.join("..").join("evil.txt").exists());
    }

    #[test]
    fn extract_zip_writes_regular_entries() {
        let root = temp("extract-ok");
        let bytes = zip_bytes(&[
            ("plugin.json", br#"{"schema":1}"#),
            ("bin/x.dll", b"fake dll"),
        ]);
        let dest = root.join("out");
        extract_zip(&bytes, &dest).expect("正常包该解开");
        assert_eq!(
            std::fs::read(dest.join("bin").join("x.dll")).unwrap(),
            b"fake dll"
        );
    }

    #[test]
    fn extract_zip_rejects_case_collisions_and_reserved_names() {
        let root = temp("zip-collision");
        let bytes = zip_bytes(&[("bin/x.dll", b"a"), ("BIN/X.DLL", b"b")]);
        let error = extract_zip(&bytes, &root.join("out")).expect_err("大小写折叠冲突该被拒绝");
        assert!(error.contains("大小写"), "{error}");

        let bytes = zip_bytes(&[("CON", b"x")]);
        let error = extract_zip(&bytes, &root.join("out2")).expect_err("保留名该被拒绝");
        assert!(error.contains("保留"), "{error}");

        let bytes = zip_bytes(&[("bin/x.dll.", b"x")]);
        let error = extract_zip(&bytes, &root.join("out3")).expect_err("尾点该被拒绝");
        assert!(error.contains("结尾"), "{error}");
    }

    #[test]
    fn install_from_bytes_lands_a_native_bundle() {
        let root = temp("install-native");
        let plugins = root.join("plugins");
        let bytes = plugin_zip(
            "com.example.demo",
            "1.2.0",
            "native",
            "bin/x.dll",
            b"fake dll bytes",
        );

        let dest = install_from_bytes(&bytes, "com.example.demo", "1.2.0", &plugins)
            .expect("本地安装该成功");
        assert_eq!(dest, plugins.join("com.example.demo"));
        assert_eq!(
            std::fs::read(dest.join("bin").join("x.dll")).unwrap(),
            b"fake dll bytes",
            "入口内容要原样落盘"
        );
        let manifest = BundleManifest::load(&dest).expect("装完该能读回 plugin.json");
        assert_eq!(manifest.id, "com.example.demo");
        assert_eq!(manifest.kind, PluginKind::Native);
        // staging 必须清理干净（成功时 rename 走了，目录不该还在）。
        assert!(!plugins.join(STAGING_DIR).join("com.example.demo").exists());
    }

    #[test]
    fn install_from_bytes_puts_scripts_under_scripts_dir() {
        let root = temp("install-script");
        let plugins = root.join("plugins");
        let bytes = plugin_zip(
            "com.example.py",
            "0.1.0",
            "python",
            "main.py",
            b"print('hi')",
        );
        let dest = install_from_bytes(&bytes, "com.example.py", "0.1.0", &plugins)
            .expect("脚本包该装成功");
        assert_eq!(dest, plugins.join(SCRIPTS_DIR).join("com.example.py"));
        assert_eq!(std::fs::read(dest.join("main.py")).unwrap(), b"print('hi')");
    }

    #[test]
    fn install_from_bytes_refuses_an_existing_target() {
        let root = temp("install-exists");
        let plugins = root.join("plugins");
        let bytes = plugin_zip("com.example.demo", "1.0.0", "native", "bin/x.dll", b"one");
        install_from_bytes(&bytes, "com.example.demo", "1.0.0", &plugins).expect("第一次该成功");

        let error = install_from_bytes(&bytes, "com.example.demo", "1.0.0", &plugins)
            .expect_err("已存在的目标不许覆盖");
        assert!(error.contains("先卸载"), "{error}");
        assert!(
            !plugins.join(STAGING_DIR).join("com.example.demo").exists(),
            "失败也要清掉 staging"
        );
        // 原内容没被覆盖。
        assert_eq!(
            std::fs::read(plugins.join("com.example.demo").join("bin").join("x.dll")).unwrap(),
            b"one"
        );
    }

    #[test]
    fn install_from_bytes_checks_id_and_version() {
        let root = temp("install-mismatch");
        let plugins = root.join("plugins");
        let bytes = plugin_zip("com.example.real", "1.0.0", "native", "bin/x.dll", b"x");

        let error = install_from_bytes(&bytes, "com.example.other", "1.0.0", &plugins)
            .expect_err("id 不一致该被拒绝");
        assert!(error.contains("id"), "{error}");
        assert!(!plugins.join("com.example.other").exists());
        assert!(!plugins.join(STAGING_DIR).join("com.example.other").exists());

        let error = install_from_bytes(&bytes, "com.example.real", "2.0.0", &plugins)
            .expect_err("版本不一致该被拒绝");
        assert!(error.contains("版本"), "{error}");
        assert!(!plugins.join("com.example.real").exists());
    }

    #[test]
    fn install_from_bytes_rejects_an_unsafe_id_and_a_missing_entry() {
        let root = temp("install-bad");
        let plugins = root.join("plugins");
        let error = install_from_bytes(b"whatever", "../escape", "1.0.0", &plugins)
            .expect_err("id 带 .. 必须被拒绝");
        assert!(error.contains("id"), "{error}");
        assert!(!root.join("escape").exists());

        // plugin.json 说 entry 是 bin/x.dll，但包里没有这个文件。
        let manifest = r#"{"schema":1,"id":"com.example.broken","name":"B","version":"1.0.0","kind":"native","entry":"bin/x.dll"}"#;
        let bytes = zip_bytes(&[(BUNDLE_MANIFEST, manifest.as_bytes())]);
        let error = install_from_bytes(&bytes, "com.example.broken", "1.0.0", &plugins)
            .expect_err("缺入口该被拒绝");
        assert!(error.contains("入口"), "{error}");
        assert!(!plugins.join("com.example.broken").exists());
    }

    #[test]
    fn search_matches_id_name_and_summary() {
        let plugin = IndexPlugin {
            id: "com.example.weather".to_owned(),
            name: "天气".to_owned(),
            summary: "一句话天气".to_owned(),
            ..IndexPlugin::default()
        };
        assert!(plugin_matches(&plugin, ""), "空过滤词匹配所有");
        assert!(plugin_matches(&plugin, "weather"));
        assert!(plugin_matches(&plugin, "com.example"));
        assert!(plugin_matches(&plugin, "天气"));
        assert!(!plugin_matches(&plugin, "calendar"));
    }

    #[test]
    fn esc_in_search_mode_only_leaves_the_input_state() {
        let mut store = Store::new(temp("keys").join("plugins"), None);
        assert_eq!(
            store.handle_key(key(KeyCode::Char('/'))),
            StoreKey::Consumed
        );
        // 输入态下 q 是普通字符，不是「退出」。
        assert_eq!(
            store.handle_key(key(KeyCode::Char('q'))),
            StoreKey::Consumed
        );
        assert_eq!(store.search.as_deref(), Some("q"));
        // Esc 只退出输入态（并把过滤词回滚成进入前的值）。
        assert_eq!(store.handle_key(key(KeyCode::Esc)), StoreKey::Consumed);
        assert_eq!(store.search, None);
        assert_eq!(store.query, "");
        // 非输入态再按 Esc 才关页。
        assert_eq!(store.handle_key(key(KeyCode::Esc)), StoreKey::Close);
    }

    #[test]
    fn enter_search_confirms_the_filter() {
        let mut store = Store::new(
            temp("keys-confirm").join("plugins"),
            Some("https://example.com/index.json".to_owned()),
        );
        store.index = Some(parse_index(INDEX_JSON.as_bytes()).unwrap());
        store.refresh_filtered();
        store.handle_key(key(KeyCode::Char('/')));
        for character in "weather".chars() {
            store.handle_key(key(KeyCode::Char(character)));
        }
        store.handle_key(key(KeyCode::Enter));
        assert_eq!(store.query, "weather");
        assert_eq!(store.filtered.len(), 1);
        assert_eq!(store.title(), "插件商店 (1/1)");
    }

    #[test]
    fn installing_without_a_registry_says_where_to_configure_it() {
        let mut store = Store::new(temp("unconfigured").join("plugins"), None);
        store.start_install();
        match store.status {
            Status::Error(ref text) => assert!(text.contains("registry_url"), "{text}"),
            ref other => panic!("该报未配置，实际 {other:?}"),
        }
        assert_eq!(store.title(), "插件商店（未配置）");
        assert!(store.footer_hint().contains("Enter 安装"));
    }

    #[test]
    fn a_yanked_or_wrong_abi_install_fails_before_downloading() {
        let mut store = Store::new(
            temp("precheck").join("plugins"),
            Some("https://example.com/index.json".to_owned()),
        );
        let mut index = parse_index(INDEX_JSON.as_bytes()).unwrap();
        // 把唯一可用版本的 abi 调到宿主之上：应该直接报「需要更新的宿主」。
        index.plugins[0].versions[1].abi = Some(ABI_VERSION + 1);
        store.index = Some(index);
        store.refresh_filtered();
        store.start_install();
        match store.status {
            Status::Error(text) => assert!(text.contains("ABI"), "{text}"),
            other => panic!("该报 ABI 不兼容，实际 {other:?}"),
        }
        assert!(!store.busy, "没起下载线程就不该 busy");
    }

    #[test]
    fn scanning_finds_native_and_script_bundles() {
        let plugins = temp("scan").join("plugins");
        let native = plugins.join("com.example.native");
        std::fs::create_dir_all(&native).unwrap();
        std::fs::write(
            native.join(BUNDLE_MANIFEST),
            r#"{"schema":1,"id":"com.example.native","name":"N","version":"1.0.0","kind":"native","entry":"bin/n.dll"}"#,
        )
        .unwrap();
        let script = plugins.join(SCRIPTS_DIR).join("com.example.script");
        std::fs::create_dir_all(&script).unwrap();
        std::fs::write(
            script.join(BUNDLE_MANIFEST),
            r#"{"schema":1,"id":"com.example.script","name":"S","version":"0.1.0","kind":"js","entry":"index.js"}"#,
        )
        .unwrap();
        // 隐藏目录与没有清单的目录不该进已安装列表。
        std::fs::create_dir_all(plugins.join(".staging").join("half")).unwrap();
        std::fs::write(
            plugins.join(".staging").join("half").join(BUNDLE_MANIFEST),
            r#"{"schema":1,"id":"half","name":"H","version":"1.0.0","kind":"native","entry":"x.dll"}"#,
        )
        .unwrap();
        std::fs::create_dir_all(plugins.join("no-manifest")).unwrap();

        let found = scan_installed(&plugins);
        assert_eq!(found.len(), 2, "{found:?}");
        assert_eq!(found[0].id, "com.example.native");
        assert_eq!(found[0].kind, PluginKind::Native);
        assert_eq!(found[1].id, "com.example.script");
        assert_eq!(found[1].kind, PluginKind::Js);
    }
}
