//! 控制台日志的格式与颜色。
//!
//! 只装在 CLI 一侧：库 crate 只发 `tracing` 事件，订阅者由可执行文件决定——这样
//! `fuyubot-loader` 被测试 / 别的宿主复用时不会自己往 stderr 写。
//!
//! 三类行，靠词缀与颜色区分：
//!
//! ```text
//! 2026-10-31 07:05:13 pipeline WARN  内容                   ← 宿主：来源 + 级别
//! 2026-10-31 07:05:13 [plugins - fuyubot.start] WARN  内容  ← 插件：青色词缀
//! 2026-10-31 07:05:13 TDLib Update {...}                     ← TDLib 更新：品红词缀
//!
//! 日期灰、时间亮蓝（两段两种颜色）；级别、词缀、来源各有颜色。
//! ```
//!
//! TUI 的日志页按同一份格式解析着色（`panel::logfmt` 与 `panel::ui::log_line`），
//! 所以改这里要同步那边。
//!
//! **为什么不按级别过滤**：插件日志的门槛由 `HostRuntime::set_log_verbosity` 在源头挡，
//! 那个值还能被 `.fuyubot/log-level` 运行期改；订阅者这边再挡一层就会两边不一致
//! （调高门槛后事件根本到不了格式化器）。

use std::fmt;

use time::{OffsetDateTime, UtcOffset};
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::fmt::FmtContext;
use tracing_subscriber::fmt::format::{FormatEvent, FormatFields, Writer};
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt as _;

const RESET: &str = "\x1b[0m";
const DIM: &str = "\x1b[2;37m";
const CYAN: &str = "\x1b[36m";
const MAGENTA: &str = "\x1b[1;35m";
/// 日期列：灰。
const DATE: &str = "\x1b[90m";
/// 时间列：亮蓝。和级别（红/黄/绿/青）、插件（青）、TDLib（品红）都不撞。
const TIME: &str = "\x1b[94m";

/// 装一次全局订阅者。重复调用（测试里同时走多条路径）只保留第一个。
pub fn init() {
    let ansi = color_enabled();
    let layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_ansi(ansi)
        .event_format(LineFormat {
            ansi,
            clock: Clock::new(),
        });

    // 不挂 `LevelFilter`：级别是 `HostRuntime` 的事（见文件头）。
    let _ = tracing_subscriber::registry().with(layer).try_init();
}

/// 颜色只在真终端上开；重定向到文件（含 `--background`）时必须是纯文本，
/// 否则 `logs/fuyubot.log` 里全是转义码，TUI 的日志页也没法按级别着色。
///
/// `CLICOLOR_FORCE=1` 可以强制开（`less -R` / CI 里想看颜色时用），`NO_COLOR` 优先级最高。
fn color_enabled() -> bool {
    use std::io::IsTerminal as _;
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    let forced = std::env::var("CLICOLOR_FORCE").is_ok_and(|value| value != "0");
    forced
        || (std::io::stderr().is_terminal()
            && std::env::var("CLICOLOR").map(|v| v != "0").unwrap_or(true))
}

struct LineFormat {
    ansi: bool,
    clock: Clock,
}

impl<S, N> FormatEvent<S, N> for LineFormat
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        _ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let mut fields = Fields::default();
        event.record(&mut fields);

        let meta = event.metadata();
        let level = *meta.level();
        let message = fields.message.unwrap_or_default();

        self.clock.write(&mut writer, self.ansi)?;
        writer.write_str(" ")?;

        // TDLib 更新 / 响应：没有级别列，固定词缀——它们只在 `--trace-updates` 时出现，
        // 混在 TRACE 里反而难认。
        if is_tdlib_update(meta.target()) {
            write_tag(&mut writer, "TDLib Update", self.ansi, MAGENTA)?;
            writer.write_str(" ")?;
            writer.write_str(&message)?;
            return writer.write_str("\n");
        }

        // 插件日志：`[plugins - 名字] LEVEL  内容`，青色词缀一眼区分来源。
        if let Some(plugin) = fields.plugin {
            write_tag(
                &mut writer,
                &format!("[plugins - {plugin}]"),
                self.ansi,
                CYAN,
            )?;
            writer.write_str(" ")?;
            write_tag(&mut writer, level.as_str(), self.ansi, level_color(level))?;
            writer.write_str("  ")?;
            writer.write_str(&message)?;
            return writer.write_str("\n");
        }

        // 宿主日志：`source LEVEL  内容`（来源在前，和插件 / TDLib 的词缀对齐）。
        write_tag(&mut writer, &short_target(meta.target()), self.ansi, DIM)?;
        writer.write_str(" ")?;
        write_tag(&mut writer, level.as_str(), self.ansi, level_color(level))?;
        writer.write_str("  ")?;
        writer.write_str(&message)?;
        writer.write_str("\n")
    }
}

/// TDLib 原始更新走这个 target（`pipeline.rs`）；响应也共用它。
fn is_tdlib_update(target: &str) -> bool {
    target == "fuyubot::update"
}

/// 写一段带颜色的文本；不开颜色时原样写（落盘日志必须是纯文本）。
fn write_tag(writer: &mut Writer<'_>, text: &str, ansi: bool, color: &str) -> fmt::Result {
    if ansi {
        write!(writer, "{color}{text}{RESET}")
    } else {
        writer.write_str(text)
    }
}

fn level_color(level: Level) -> &'static str {
    match level {
        Level::ERROR => "\x1b[1;31m",
        Level::WARN => "\x1b[1;33m",
        Level::INFO => "\x1b[1;32m",
        // DEBUG 用亮青（时间列占了蓝），与 TUI 日志页的配色一致。
        Level::DEBUG => "\x1b[1;36m",
        _ => DIM,
    }
}

/// `fuyubot::pipeline` → `pipeline`；宿主 / 管线的 target 本来就是给人看的。
fn short_target(target: &str) -> String {
    target
        .strip_prefix("fuyubot::")
        .unwrap_or(target)
        .to_owned()
}

/// 只关心 `plugin` 与 `message` 两个字段；其余字段（将来可能加）暂不显示。
#[derive(Default)]
struct Fields {
    plugin: Option<String>,
    message: Option<String>,
}

impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "plugin" => self.plugin = Some(value.to_owned()),
            "message" => self.message = Some(value.to_owned()),
            _ => {}
        }
    }

    // `%value` 与格式串的 message 都走 `record_debug`（`format_args!`），
    // `{:?}` 出来就是原字符串、不带引号。
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        match field.name() {
            "plugin" => self.plugin = Some(format!("{value:?}")),
            "message" => self.message = Some(format!("{value:?}")),
            _ => {}
        }
    }
}

/// 本地时间列：`OffsetDateTime::now_local()` 在启动时抓一次 offset。
///
/// 每个事件都去问一次系统时区既慢、又可能在多线程下失败（Unix 的
/// `localtime_r` 不是线程安全的）；抓一次固定用，跨 DST 的那一小时偏差可以接受。
#[derive(Clone, Copy)]
struct Clock {
    offset: UtcOffset,
}

impl Clock {
    fn new() -> Self {
        let offset = OffsetDateTime::now_local()
            .map(|now| now.offset())
            .unwrap_or(UtcOffset::UTC);
        Self { offset }
    }
}

impl Clock {
    /// 写 `日期 时间`：日期灰、时间亮蓝，两段各用各的颜色。
    fn write(&self, writer: &mut Writer<'_>, ansi: bool) -> fmt::Result {
        let now = OffsetDateTime::now_utc().to_offset(self.offset);
        let (year, month, day) = (now.year(), now.month() as u8, now.day());
        let (hour, minute, second) = (now.hour(), now.minute(), now.second());
        if ansi {
            write!(
                writer,
                "{DATE}{year:04}-{month:02}-{day:02}{RESET} {TIME}{hour:02}:{minute:02}:{second:02}{RESET}"
            )
        } else {
            write!(
                writer,
                "{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}"
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// 把格式化结果收进内存：测「三行格式 + 颜色前的纯文本」，不测真实时间。
    #[derive(Clone, Default)]
    struct Buf(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Buf {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Buf {
        type Writer = Buf;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// 跑一段事件，返回格式化后的文本行。
    fn capture_with(run: impl FnOnce(), ansi: bool) -> String {
        let buf = Buf::default();
        let layer = tracing_subscriber::fmt::layer()
            .with_writer(buf.clone())
            .with_ansi(ansi)
            .event_format(LineFormat {
                ansi,
                clock: Clock {
                    offset: UtcOffset::UTC,
                },
            });
        let subscriber = tracing_subscriber::registry().with(layer);
        tracing::subscriber::with_default(subscriber, run);
        String::from_utf8(buf.0.lock().unwrap().clone()).unwrap()
    }

    /// 跑一段事件，返回格式化后的纯文本行（落盘日志的形状）。
    fn capture(run: impl FnOnce()) -> String {
        capture_with(run, false)
    }

    #[test]
    fn short_targets_drop_the_crate_prefix() {
        assert_eq!(short_target("fuyubot::pipeline"), "pipeline");
        assert_eq!(short_target("fuyubot::plugin"), "plugin");
        assert_eq!(short_target("other::thing"), "other::thing");
    }

    #[test]
    fn host_lines_are_date_time_source_level_message() {
        let text = capture(|| tracing::warn!(target: "fuyubot::pipeline", "careful"));
        let line = text.lines().next().unwrap();
        // 时间列：`YYYY-MM-DD HH:MM:SS`（19 个字符），之后是来源、级别。
        assert!(line.len() > 19, "{line}");
        assert_eq!(&line[4..5], "-", "{line}");
        assert_eq!(&line[7..8], "-", "{line}");
        assert_eq!(&line[10..11], " ", "{line}");
        assert_eq!(&line[13..14], ":", "{line}");
        assert_eq!(&line[16..17], ":", "{line}");
        assert!(line[19..].starts_with(" pipeline WARN  careful"), "{line}");
    }

    #[test]
    fn plugin_events_get_a_tag_and_keep_the_level() {
        let text = capture(|| {
            tracing::info!(target: "fuyubot::plugin", plugin = "fuyubot.demo", "hello");
            tracing::warn!(target: "fuyubot::pipeline", "careful");
        });

        let plugin_line = text.lines().next().unwrap();
        assert!(
            plugin_line.contains(" [plugins - fuyubot.demo] INFO  hello"),
            "{plugin_line}"
        );

        let host_line = text.lines().nth(1).unwrap();
        assert!(host_line.contains(" pipeline WARN  careful"), "{host_line}");
        assert!(
            !host_line.contains("插件"),
            "宿主行不该带插件词缀：{host_line}"
        );
    }

    #[test]
    fn tdlib_updates_get_their_own_prefix() {
        let text = capture(|| {
            tracing::trace!(
                target: "fuyubot::update",
                "{}",
                "{\"@type\":\"updateNewMessage\"}"
            );
        });

        let line = text.lines().next().unwrap();
        assert!(
            line.contains(" TDLib Update {\"@type\":\"updateNewMessage\"}"),
            "{line}"
        );
        assert!(!line.contains("TRACE"), "TDLib 行不显示级别：{line}");
    }

    #[test]
    fn ansi_colors_distinguish_host_plugin_and_tdlib() {
        let text = capture_with(
            || {
                tracing::warn!(target: "fuyubot::pipeline", "careful");
                tracing::info!(target: "fuyubot::plugin", plugin = "fuyubot.demo", "hello");
                tracing::trace!(target: "fuyubot::update", "{}", "{}");
            },
            true,
        );
        let lines: Vec<&str> = text.lines().collect();
        // 日期灰、时间亮蓝：两段两种颜色。
        assert!(lines[0].starts_with("\x1b[90m"), "{:?}", lines[0]);
        assert!(
            lines[0].contains("\x1b[90m2026-") && lines[0].contains("\x1b[0m \x1b[94m"),
            "日期和时间要分别上色：{:?}",
            lines[0]
        );
        // 宿主：来源在前（灰）、级别在后（黄）。
        assert!(
            lines[0].contains("\x1b[2;37mpipeline\x1b[0m \x1b[1;33mWARN\x1b[0m"),
            "{:?}",
            lines[0]
        );
        // 插件词缀：青。
        assert!(
            lines[1].contains("\x1b[36m[plugins - fuyubot.demo]\x1b[0m"),
            "{:?}",
            lines[1]
        );
        // TDLib 词缀：品红。
        assert!(
            lines[2].contains("\x1b[1;35mTDLib Update\x1b[0m"),
            "{:?}",
            lines[2]
        );
    }
}
