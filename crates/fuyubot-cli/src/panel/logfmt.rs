//! 日志行的解析与过滤：日志页的级别 / 来源过滤都走这一份规则。
//!
//! 目前混在 `logs/fuyubot.log` 里的行：
//!
//! - **新格式**（`logging.rs` 产出，三条投递路各一个形状）：
//!   - 宿主：`YYYY-MM-DD HH:MM:SS source LEVEL  message`
//!   - 插件：`YYYY-MM-DD HH:MM:SS [plugins - 插件名] LEVEL  message`
//!   - TDLib：`YYYY-MM-DD HH:MM:SS TDLib Update {json}`
//! - **旧四列格式**（`HH:MM:SS.mmm  LEVEL  source  message`）：升级前的日志还留在文件里。
//! - **原样行**：TDLib C 库自己的 `[ 2][t …]`、`println!` 打到日志里的启动提示、panic 等。
//!
//! 过滤器要能同时处理这几类：新格式按词缀拆；旧格式按列拆；TDLib 原样行按开头
//! `[ N]` 的数字映射级别；其余认不出的行按「信息」对待（级别过滤为 error+ 时会被
//! 藏起来，info+ 时仍显示）。

use fuyubot_loader::LogLevel;

/// 日志来源大类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    /// 插件日志（新格式 `[plugins - 名字]` 行 / 旧的 `插件[名字]` / 旧四列里的 `plugin` 或插件名）。
    Plugin,
    /// 宿主自己的日志。
    Host,
    /// 更新管线。
    Pipeline,
    /// 原始更新 / 响应追踪（含 `TDLib Update` 行）。
    Update,
    /// TDLib 本体的原样行（C 库 `[ N][t …]`，或 `TDLib` 前缀的启动提示）。
    Tdlib,
    /// `println!` 启动提示、panic、认不出的 target。
    Other,
}

/// 新格式的时间列宽：`YYYY-MM-DD HH:MM:SS`。
const TIMESTAMP_WIDTH: usize = 19;

/// 拆列。返回 `(时间, 级别, 来源, 内容)`；`None` = 不是 `logging.rs` 那套格式。
///
/// 三种行共用这个返回形状：
/// - 宿主 `… pipeline WARN  内容`
/// - 插件 `… [plugins - fuyubot.start] WARN  内容`（`source` 是插件名）
/// - TDLib `… TDLib Update {json}`（`level` 用词缀 `TDLib Update` 占位）
///
/// 旧四列格式还能解析（老日志文件里还有），新格式优先。
pub fn split(line: &str) -> Option<(&str, &str, &str, &str)> {
    split_current(line).or_else(|| split_legacy(line))
}

fn split_current(line: &str) -> Option<(&str, &str, &str, &str)> {
    let (time, rest) = line.split_at_checked(TIMESTAMP_WIDTH)?;
    if !is_timestamp(time) {
        return None;
    }
    let rest = rest.strip_prefix(' ')?;

    // 插件：`[plugins - 名字] LEVEL  内容`。
    if let Some(rest) = rest.strip_prefix("[plugins - ") {
        let (source, rest) = rest.split_once(']')?;
        let rest = rest.strip_prefix(' ')?;
        let (level, message) = rest.split_once(' ')?;
        return Some((time, level, source, message.trim_start()));
    }

    // 升级前的插件行：`插件[名字] LEVEL  内容`（老日志文件里还有）。
    if let Some(rest) = rest.strip_prefix("插件[") {
        let (source, rest) = rest.split_once(']')?;
        let rest = rest.strip_prefix(' ')?;
        let (level, message) = rest.split_once(' ')?;
        return Some((time, level, source, message.trim_start()));
    }

    // TDLib：`TDLib Update {json}`，没有级别列。
    if let Some(message) = rest.strip_prefix("TDLib Update ") {
        return Some((time, "TDLib Update", "update", message));
    }

    // 升级前的一版宿主行：`LEVEL  source  内容`（级别在前；来源名不会叫 INFO 这些）。
    let (first, rest_after) = rest.split_once(' ')?;
    if is_level(first) {
        let rest = rest_after.trim_start();
        let (source, message) = rest.split_once(' ').unwrap_or((rest, ""));
        return Some((time, first, source, message.trim_start()));
    }

    // 宿主：`source LEVEL  内容`（来源在前，和插件 / TDLib 的词缀对齐）。
    let (level, message) = rest_after
        .trim_start()
        .split_once(' ')
        .unwrap_or((rest_after, ""));
    Some((time, level.trim(), first, message.trim_start()))
}

/// 升级前的四列格式（`HH:MM:SS.mmm  LEVEL  source  message`）。
fn split_legacy(line: &str) -> Option<(&str, &str, &str, &str)> {
    let (time, rest) = line.split_at_checked(12)?;
    let rest = rest.strip_prefix("  ")?;
    let (level, rest) = rest.split_at_checked(5)?;
    let rest = rest.strip_prefix("  ")?.trim_start();
    let (source, message) = match rest.split_once(' ') {
        Some((source, message)) => (source, message.trim_start()),
        None => (rest, ""),
    };
    Some((time, level.trim(), source, message))
}

fn is_level(token: &str) -> bool {
    matches!(token, "ERROR" | "WARN" | "INFO" | "DEBUG" | "TRACE")
}

/// TDLib Update 行的完整 JSON（只有能解析的才算；面板复制用）。
pub fn tdlib_update_payload(line: &str) -> Option<&str> {
    let (_, level, _, message) = split(line)?;
    if level != "TDLib Update" {
        return None;
    }
    serde_json::from_str::<serde_json::Value>(message)
        .ok()
        .map(|_| message)
}

/// TDLib Update 的 `@type`（面板里只显示这个类型，不铺整段 JSON）。
///
/// 这里不解析整段 JSON：`--trace-updates` 的更新可能很大，而日志页每帧都要画；
/// `@type` 固定在最前面，直接扫字符串就够。复制时才会做完整 JSON 校验。
pub fn tdlib_update_type(line: &str) -> Option<String> {
    let (_, level, _, message) = split(line)?;
    if level != "TDLib Update" {
        return None;
    }
    let rest = message.get(message.find("\"@type\"")? + "\"@type\"".len()..)?;
    let rest = rest.get(rest.find(':')? + 1..)?.trim_start();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_owned())
}

/// 只认 `YYYY-MM-DD HH:MM:SS`，免得把恰好 19 个字符的普通文本当日志。
fn is_timestamp(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() == TIMESTAMP_WIDTH
        && bytes.iter().enumerate().all(|(index, byte)| match index {
            4 | 7 => *byte == b'-',
            10 => *byte == b' ',
            13 | 16 => *byte == b':',
            _ => byte.is_ascii_digit(),
        })
}

/// 这一行的级别。新格式读级别词 / `TDLib Update` 词缀；旧格式读第二列；TDLib 原样行读 `[ N]`。
pub fn level(line: &str) -> Option<LogLevel> {
    if let Some((_, token, _, _)) = split(line) {
        return match token {
            // TDLib 更新只在 `--trace-updates` 时出现，按 TRACE 过级别过滤。
            "TDLib Update" => Some(LogLevel::Trace),
            "ERROR" => Some(LogLevel::Error),
            "WARN" => Some(LogLevel::Warn),
            "INFO" => Some(LogLevel::Info),
            "DEBUG" => Some(LogLevel::Debug),
            "TRACE" => Some(LogLevel::Trace),
            _ => None,
        };
    }
    tdlib_level(line)
}

/// 这一行属于哪个来源大类。
pub fn source_kind(line: &str) -> SourceKind {
    if let Some((_, _, source, _)) = split(line) {
        return classify_source(source);
    }
    let trimmed = line.trim_start();
    if tdlib_level(line).is_some()
        || trimmed.starts_with("TDLib ")
        || trimmed.starts_with("[TDLib]")
    {
        return SourceKind::Tdlib;
    }
    // 旧版日志（tracing 之前）也留在文件里，按前缀认一下。
    if trimmed.starts_with("[plugin/") {
        return SourceKind::Plugin;
    }
    if trimmed.starts_with("[fuyubot-host]") {
        return SourceKind::Host;
    }
    SourceKind::Other
}

fn classify_source(source: &str) -> SourceKind {
    match source {
        // CLI 自己的命令结果 / 运行期诊断都走 `fuyubot::cli`，归到宿主一类。
        "host" | "cli" => SourceKind::Host,
        "pipeline" => SourceKind::Pipeline,
        "update" => SourceKind::Update,
        "plugin" => SourceKind::Plugin,
        // `fuyubot_tdlib::client` 这类 target 归 TDLib。
        source if source.contains("tdlib") => SourceKind::Tdlib,
        // 插件名是点分标识符（`fuyubot.start`）；带 `::` 的是别的 crate target。
        source if source.contains('.') && !source.contains("::") => SourceKind::Plugin,
        _ => SourceKind::Other,
    }
}

/// TDLib C 库的行：`[ 2][t 0.001][…]`（0=fatal / 1=error / 2=warn / 3=info / 4=debug / 5=verbose）。
fn tdlib_level(line: &str) -> Option<LogLevel> {
    let rest = line.trim_start().strip_prefix('[')?.trim_start();
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return None;
    }
    // 数字后面必须紧跟 `]`，否则是普通方括号文本。
    let after = rest[digits.len()..].trim_start();
    if !after.starts_with(']') {
        return None;
    }
    let number: u32 = digits.parse().ok()?;
    Some(match number {
        0 | 1 => LogLevel::Error,
        2 => LogLevel::Warn,
        3 => LogLevel::Info,
        4 => LogLevel::Debug,
        _ => LogLevel::Trace,
    })
}

// ── 过滤条件 ───────────────────────────────────────────────────────

/// 级别下限：`All` 不过滤；其余是「>= 这一档」。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LevelFilter {
    #[default]
    All,
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl LevelFilter {
    pub const ALL: [Self; 6] = [
        Self::All,
        Self::Error,
        Self::Warn,
        Self::Info,
        Self::Debug,
        Self::Trace,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::All => "全部",
            Self::Error => "error+",
            Self::Warn => "warn+",
            Self::Info => "info+",
            Self::Debug => "debug+",
            Self::Trace => "trace+",
        }
    }

    pub fn shifted(self, delta: isize) -> Self {
        let index = Self::ALL.iter().position(|item| *item == self).unwrap_or(0);
        let len = Self::ALL.len() as isize;
        Self::ALL[(index as isize + delta).rem_euclid(len) as usize]
    }

    fn threshold(self) -> Option<u8> {
        match self {
            Self::All => None,
            Self::Error => Some(1),
            Self::Warn => Some(2),
            Self::Info => Some(3),
            Self::Debug => Some(4),
            Self::Trace => Some(5),
        }
    }

    /// 认不出级别的行按 `info` 对待：`info+` / `debug+` 仍会显示启动提示，
    /// `warn+` / `error+` 会把它们藏起来（与「更安静」的直觉一致）。
    pub fn allows(self, level: Option<LogLevel>) -> bool {
        let Some(min) = self.threshold() else {
            return true;
        };
        let rank = match level {
            Some(LogLevel::Error) => 1,
            Some(LogLevel::Warn) => 2,
            Some(LogLevel::Info) | None => 3,
            Some(LogLevel::Debug) => 4,
            Some(LogLevel::Trace) => 5,
        };
        // 数字越小越严重（error=1 … trace=5）：门槛 info 允许 error / warn / info。
        rank <= min
    }
}

/// 来源过滤：只留某一类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SourceFilter {
    #[default]
    All,
    Plugin,
    Host,
    Pipeline,
    Update,
    Tdlib,
    Other,
}

impl SourceFilter {
    pub const ALL: [Self; 7] = [
        Self::All,
        Self::Plugin,
        Self::Host,
        Self::Pipeline,
        Self::Update,
        Self::Tdlib,
        Self::Other,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::All => "全部",
            Self::Plugin => "插件",
            Self::Host => "宿主",
            Self::Pipeline => "管道",
            Self::Update => "更新",
            Self::Tdlib => "TDLib",
            Self::Other => "其他",
        }
    }

    pub fn shifted(self, delta: isize) -> Self {
        let index = Self::ALL.iter().position(|item| *item == self).unwrap_or(0);
        let len = Self::ALL.len() as isize;
        Self::ALL[(index as isize + delta).rem_euclid(len) as usize]
    }

    pub const fn allows(self, kind: SourceKind) -> bool {
        match self {
            Self::All => true,
            Self::Plugin => matches!(kind, SourceKind::Plugin),
            Self::Host => matches!(kind, SourceKind::Host),
            Self::Pipeline => matches!(kind, SourceKind::Pipeline),
            Self::Update => matches!(kind, SourceKind::Update),
            Self::Tdlib => matches!(kind, SourceKind::Tdlib),
            Self::Other => matches!(kind, SourceKind::Other),
        }
    }
}

/// 日志页当前的过滤条件。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LogFilter {
    pub level: LevelFilter,
    pub source: SourceFilter,
}

impl LogFilter {
    pub fn matches(self, line: &str) -> bool {
        self.level.allows(level(line)) && self.source.allows(source_kind(line))
    }

    /// `info+ · 全部` 这种一行摘要（日志页底栏用）。
    pub fn summary(self) -> String {
        format!("{} · {}", self.level.label(), self.source.label())
    }

    /// 两个维度都是「全部」= 没有过滤。
    pub const fn is_default(self) -> bool {
        matches!(self.level, LevelFilter::All) && matches!(self.source, SourceFilter::All)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLUGIN: &str = "2026-10-31 07:05:13 [plugins - fuyubot.demo] INFO  hello";
    const HOST: &str = "2026-10-31 07:05:13 host WARN  something odd";
    const TDLIB_UPDATE: &str = "2026-10-31 07:05:13 TDLib Update {\"@type\":\"updateNewMessage\"}";
    const LEGACY: &str = "05:41:02.118  INFO   fuyubot.demo  hello";
    const TDLIB: &str = "[ 2][t 3.14159][client.cpp:123] connection failed";

    #[test]
    fn new_lines_split_into_time_level_source_message() {
        let (time, level, source, message) = split(PLUGIN).expect("是插件行");
        assert_eq!(time, "2026-10-31 07:05:13");
        assert_eq!(level, "INFO");
        assert_eq!(source, "fuyubot.demo");
        assert_eq!(message, "hello");
        assert_eq!(super::level(PLUGIN), Some(LogLevel::Info));
        assert_eq!(source_kind(PLUGIN), SourceKind::Plugin);

        let (_, level, source, message) = split(HOST).expect("是宿主行");
        assert_eq!(level, "WARN");
        assert_eq!(source, "host");
        assert_eq!(message, "something odd");
        assert_eq!(super::level(HOST), Some(LogLevel::Warn));
        assert_eq!(source_kind(HOST), SourceKind::Host);

        let (_, level, source, message) = split(TDLIB_UPDATE).expect("是 TDLib 行");
        assert_eq!(level, "TDLib Update");
        assert_eq!(source, "update");
        assert_eq!(message, "{\"@type\":\"updateNewMessage\"}");
        assert_eq!(super::level(TDLIB_UPDATE), Some(LogLevel::Trace));
        assert_eq!(source_kind(TDLIB_UPDATE), SourceKind::Update);
        assert_eq!(
            tdlib_update_type(TDLIB_UPDATE).as_deref(),
            Some("updateNewMessage")
        );
        assert_eq!(
            tdlib_update_payload(TDLIB_UPDATE),
            Some("{\"@type\":\"updateNewMessage\"}")
        );
        assert_eq!(
            tdlib_update_type("2026-10-31 07:05:13 TDLib Update not-json"),
            None
        );
    }

    #[test]
    fn previous_host_order_still_parses() {
        // 上一版新格式：级别在前（`WARN  pipeline`）。
        let line = "2026-10-31 07:05:13 WARN  pipeline  careful";
        let (_, level, source, message) = split(line).expect("旧的级别在前宿主行");
        assert_eq!(level, "WARN");
        assert_eq!(source, "pipeline");
        assert_eq!(message, "careful");
    }

    #[test]
    fn legacy_four_column_lines_still_parse() {
        let (time, level, source, message) = split(LEGACY).expect("旧格式");
        assert_eq!(time, "05:41:02.118");
        assert_eq!(level, "INFO");
        assert_eq!(source, "fuyubot.demo");
        assert_eq!(message, "hello");
        assert_eq!(source_kind(LEGACY), SourceKind::Plugin);
    }

    #[test]
    fn legacy_plugin_prefix_still_parses() {
        let line = "2026-10-31 07:05:13 插件[fuyubot.demo] WARN  old";
        let (_, level, source, message) = split(line).expect("旧的 插件[...] 词缀");
        assert_eq!(level, "WARN");
        assert_eq!(source, "fuyubot.demo");
        assert_eq!(message, "old");
    }

    #[test]
    fn tdlib_raw_lines_get_a_level_and_source() {
        assert_eq!(super::level(TDLIB), Some(LogLevel::Warn));
        assert_eq!(source_kind(TDLIB), SourceKind::Tdlib);
        assert_eq!(super::level("[ 5][t 1][x] verbose"), Some(LogLevel::Trace));
        // 普通方括号文本不能被当成 TDLib。
        assert_eq!(super::level("[fuyubot-host] 启动"), None);
        assert_eq!(source_kind("[fuyubot-host] 启动"), SourceKind::Host);
    }

    #[test]
    fn level_filter_is_a_threshold() {
        assert!(LevelFilter::Info.allows(Some(LogLevel::Info)));
        assert!(LevelFilter::Info.allows(Some(LogLevel::Warn)));
        assert!(LevelFilter::Info.allows(Some(LogLevel::Error)));
        assert!(!LevelFilter::Info.allows(Some(LogLevel::Debug)));
        assert!(!LevelFilter::Info.allows(Some(LogLevel::Trace)));
        // 认不出级别 ≈ info：info+ 留、warn+ 藏。
        assert!(LevelFilter::Info.allows(None));
        assert!(!LevelFilter::Warn.allows(None));
        assert!(LevelFilter::All.allows(None));
    }

    #[test]
    fn source_filter_keeps_only_the_selected_kind() {
        let filter = LogFilter {
            level: LevelFilter::All,
            source: SourceFilter::Tdlib,
        };
        assert!(filter.matches(TDLIB));
        assert!(!filter.matches(PLUGIN));
        assert!(!filter.matches(HOST));

        let filter = LogFilter {
            level: LevelFilter::Warn,
            source: SourceFilter::All,
        };
        assert!(filter.matches(HOST)); // WARN
        assert!(!filter.matches(PLUGIN)); // INFO
        assert!(filter.matches(TDLIB)); // 2 -> Warn
    }

    #[test]
    fn filters_cycle_and_wrap() {
        assert_eq!(LevelFilter::All.shifted(-1), LevelFilter::Trace);
        assert_eq!(LevelFilter::Trace.shifted(1), LevelFilter::All);
        assert_eq!(SourceFilter::All.shifted(-1), SourceFilter::Other);
        assert_eq!(SourceFilter::Other.shifted(1), SourceFilter::All);
    }
}
