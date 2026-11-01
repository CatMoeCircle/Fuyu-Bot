//! 操作面板的绘制：**顶部版本栏 + 左侧导航 + 中央工作区 + 底部两行**。
//!
//! 布局是固定的四段（顶部 / 底部不铺背景，靠边框高亮和 `[导航]` / `[工作区]` 标记焦点）：
//!
//! ```text
//! [fuyubot - v0.1.2]  终端操作面板        ● 运行中  demo  · TDLib 已连接  已刷新 1s 前
//! ┌ fuyubot ────────┐┌ 概览 ──────────────────────────────────────────┐
//! │ ▸ 概览          ││ ● 运行中  demo                                  │
//! │   插件 (3)      ││ pid 1234 · 已运行 3 分 12 秒 · 版本 0.1.2       │
//! │   日志          ││                                                 │
//! │   配置          ││                                                 │
//! │   帮助          ││                                                 │
//! └─────────────────┘└─────────────────────────────────────────────────┘
//! [导航] ↑↓ 切页 · 1–5 直达 · Tab 聚焦 · s 启动 · r 刷新 · ? 帮助 · q 退出
//! CPU 1.2% · 内存 85.3 MB · 运行 3 分 12 秒 · 插件 3 · 日志 info · PID 1234
//! ```
//!
//! 这一层**只读 `App`**（除了回填视口高度）：所有会阻塞的事都在 `app.rs` 的工作线程里。

use crate::instance::State;
use crate::scaffold;

use fuyubot_simplify::ConnectionState;
use fuyubot_tdlib::AuthState;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Cell, Clear, List, ListItem, Padding, Paragraph, Row, Table, TableState, Wrap,
};
use unicode_width::UnicodeWidthChar;

use super::app::{
    App, ConfigEdit, ConfigItem, ConfigKind, FilterRow, Focus, Level, Page, PluginKind,
    ProcessUsage,
};
use super::logfmt;

/// 左侧导航固定宽度（够放「插件 (12)」这种带计数的条目）。
const NAV_WIDTH: u16 = 22;
const NAV_WIDTH_MEDIUM: u16 = 16;
/// 顶部标题栏：版本号 + 实例状态。
const HEADER_HEIGHT: u16 = 1;

/// 窄终端（Termux / 手机）下导航逐级收缩，太窄就干脆让工作区占满。
fn nav_width_for(width: u16) -> u16 {
    if width >= 90 {
        NAV_WIDTH
    } else if width >= 60 {
        NAV_WIDTH_MEDIUM
    } else {
        0
    }
}

/// 底部固定两行：操作 + 资源（窄屏资源只留 CPU / 内存 / 级别）。
fn footer_height_for(_width: u16) -> u16 {
    2
}

/// 画一帧：顶部版本栏 + 中间（导航 / 工作区）+ 底部操作 / 资源。
pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let footer_height = footer_height_for(area.width);
    let [header, main, footer] = Layout::vertical([
        Constraint::Length(HEADER_HEIGHT),
        Constraint::Min(3),
        Constraint::Length(footer_height),
    ])
    .areas(area);

    // 手机上不用左侧导航：改成工作区上方一行横向标签，方向键左右切换。
    let top_nav_height = if area.width < 60 { 1 } else { 0 };
    let (top_nav, body) = if top_nav_height > 0 {
        let [top, body] = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(main);
        (top, body)
    } else {
        (Rect::new(main.x, main.y, 0, 0), main)
    };

    // 终端很窄时让工作区至少留 10 列：导航跟着缩，别把内容挤没。
    let nav_width = nav_width_for(area.width).min(body.width.saturating_sub(10));
    let (nav, workspace) = if nav_width == 0 {
        (Rect::new(body.x, body.y, 0, body.height), body)
    } else {
        let [nav, workspace] =
            Layout::horizontal([Constraint::Length(nav_width), Constraint::Min(0)]).areas(body);
        (nav, workspace)
    };

    draw_header(frame, app, header);
    if top_nav_height > 0 {
        draw_top_nav(frame, app, top_nav);
    } else if nav_width > 0 {
        draw_nav(frame, app, nav);
    }
    draw_workspace(frame, app, workspace);
    draw_footer(frame, app, footer);

    if app.confirm_stop {
        draw_confirm(frame, area);
    } else if app.confirm_clear_logs {
        draw_clear_confirm(frame, area);
    } else if app.level_picker.is_some() {
        draw_level_picker(frame, app, area);
    } else if app.filter_picker.is_some() {
        draw_filter_picker(frame, app, area);
    } else if app.config_edit.is_some() {
        draw_config_editor(frame, app, area);
    }
}

// ── 左侧导航 ───────────────────────────────────────────────────────

fn draw_nav(frame: &mut Frame, app: &App, area: Rect) {
    let focused = app.focus == Focus::Navigation;
    let block = Block::bordered()
        .title(Line::from(vec![Span::styled(
            " fuyubot ",
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )]))
        .title_bottom(
            Line::from(vec![Span::styled(
                format!(" v{} ", env!("CARGO_PKG_VERSION")),
                Style::new().fg(Color::DarkGray),
            )])
            .centered(),
        )
        .border_style(Style::new().fg(if focused {
            Color::Cyan
        } else {
            Color::DarkGray
        }));

    let active = app.detail().map_or(0, |detail| detail.active.len());
    let items: Vec<ListItem> = Page::ALL
        .iter()
        .map(|page| {
            let selected = *page == app.page;
            let marker = if selected { "▸ " } else { "  " };
            // 日志页不报行数：那是窗口里装了多少行（随滚动变），不是「有多少日志」。
            let count = match page {
                Page::Plugins if active > 0 => format!(" ({active})"),
                Page::Plugins if !app.disk_plugins.is_empty() => {
                    format!(" ({})", app.disk_plugins.len())
                }
                _ => String::new(),
            };
            let label = format!("{marker}{}{count}", page.title());
            let style = if selected && focused {
                Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
            } else if selected {
                Style::new().add_modifier(Modifier::BOLD)
            } else {
                Style::new()
            };
            ListItem::new(Line::from(Span::styled(label, style)))
        })
        .collect();

    // 上下留白：导航条目别贴着边框。
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let list = List::new(items).block(Block::new().padding(Padding::new(1, 1, 1, 0)));
    frame.render_widget(list, inner);
}

fn draw_top_nav(frame: &mut Frame, app: &App, area: Rect) {
    let focused = app.focus == Focus::Navigation;
    let full = area.width >= 36;
    let mut spans = vec![Span::raw(" ")];
    for (index, page) in Page::ALL.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(" │ ", Style::new().fg(Color::DarkGray)));
        }
        let selected = *page == app.page;
        let short = match page {
            Page::Overview => "概",
            Page::Plugins => "插",
            Page::Logs => "日",
            Page::Config => "配",
            Page::Help => "帮",
        };
        let marker = if selected { "▸" } else { " " };
        let style = if selected && focused {
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
        } else if selected {
            Style::new().add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(Color::Gray)
        };
        spans.push(Span::styled(
            format!("{marker}{}", if full { page.title() } else { short }),
            style,
        ));
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans)).block(Block::new().padding(Padding::new(0, 0, 0, 0))),
        area,
    );
}

// ── 中央工作区 ─────────────────────────────────────────────────────

fn draw_workspace(frame: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.workspace_focused();
    let block = Block::bordered()
        .title(Line::from(vec![Span::styled(
            format!(" {} ", app.page.title()),
            Style::new().add_modifier(Modifier::BOLD),
        )]))
        .border_style(Style::new().fg(if focused {
            Color::Cyan
        } else {
            Color::DarkGray
        }));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // 工作区太小（比如 30x10 的窗口）时不再往下切，交给各页自己截断。
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    match app.page {
        Page::Overview => draw_overview(frame, app, inner),
        Page::Plugins => draw_plugins(frame, app, inner),
        Page::Logs => draw_logs(frame, app, inner),
        Page::Config => draw_config(frame, app, inner),
        Page::Help => draw_help(frame, app, inner),
    }
}

fn draw_overview(frame: &mut Frame, app: &App, area: Rect) {
    let lines = overview_lines(app, area.width < 60);
    let paragraph = Paragraph::new(lines)
        .block(Block::new().padding(Padding::new(1, 1, 0, 0)))
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

fn overview_lines(app: &App, compact: bool) -> Vec<Line<'static>> {
    // 手机窄屏上绝对路径会占满整屏：只显示相对项目根的路径。
    let shown_path = |path: &std::path::Path| {
        if !compact {
            return path.display().to_string();
        }
        path.strip_prefix(&app.root)
            .ok()
            .map(|relative| {
                if relative.as_os_str().is_empty() {
                    ".".to_owned()
                } else {
                    relative.display().to_string()
                }
            })
            .unwrap_or_else(|| path.display().to_string())
    };
    let (badge, color) = badge(&app.state);
    // 运行中 / 上次退出都优先用实例自报的名字（可能和配置里的 `[bot].name` 不同）。
    let name = match &app.state {
        State::Running(Some(info)) | State::Stale(Some(info)) => info.name.clone(),
        _ => app.bot_name.clone(),
    };
    let mut lines = Vec::new();
    // 状态行**总是**要有：实例停了之后，「上次是正常停止还是异常退出」正是回到面板
    // 最想知道的一件事（以前把它省掉，结果只剩顶栏一个刷新时间）。
    lines.push(Line::from(vec![
        Span::styled(badge, Style::new().fg(color).add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::styled(name, Style::new().add_modifier(Modifier::BOLD)),
    ]));
    lines.push(Line::from(Span::styled(
        if compact {
            state_detail_compact(app)
        } else {
            state_detail(app)
        },
        Style::new().fg(Color::DarkGray),
    )));
    if !compact {
        lines.push(Line::from(""));
    }
    if compact {
        // 窄屏不重复「插件目录」数量，也不放「操作」段——底部操作栏已经写了。
        lines.extend([
            section("位置"),
            kv("项目根", shown_path(&app.root)),
            kv("配置文件", shown_path(&app.config_path)),
            kv("插件目录", shown_path(&app.plugin_dir)),
            kv("日志文件", shown_path(&app.log_path)),
            section("运行"),
            kv(
                "插件",
                app.detail()
                    .map(|detail| detail.summary())
                    .unwrap_or_else(|| "实例没有在跑".to_owned()),
            ),
            connection_line(app, true),
            authorization_line(app, true),
            account_line(app, true),
            kv("日志级别", app.effective_level().to_owned()),
        ]);
        if app.config_error.is_some() {
            lines.push(Line::from(Span::styled(
                "配置读不出来——按 4 看错误",
                Style::new().fg(Color::Red),
            )));
        }
        return lines;
    }
    lines.extend([
        section("位置"),
        kv("项目根", shown_path(&app.root)),
        kv("配置文件", shown_path(&app.config_path)),
        kv("插件目录", shown_path(&app.plugin_dir)),
        kv("日志文件", shown_path(&app.log_path)),
        Line::from(""),
        section("运行"),
        kv(
            "插件",
            app.detail()
                .map(|detail| detail.summary())
                .unwrap_or_else(|| "实例没有在跑（看到的是磁盘上的插件）".to_owned()),
        ),
        connection_line(app, false),
        authorization_line(app, false),
        account_line(app, false),
        kv("插件目录", format!("{} 项", app.disk_plugins.len())),
        kv(
            "日志级别",
            format!("{}（日志页按 l 调整）", app.effective_level()),
        ),
        Line::from(""),
        section("操作"),
    ]);
    {
        let mut actions = Vec::new();
        if app.can_start() {
            actions.push(key("s"));
            actions.push(Span::raw(" 启动    "));
        } else {
            actions.push(key("x"));
            actions.push(Span::raw(" 停止    "));
        }
        actions.push(key("r"));
        actions.push(Span::raw(" 刷新    "));
        actions.push(key("?"));
        actions.push(Span::raw(" 帮助"));
        lines.push(Line::from(actions));
    }
    // 配置读不出来时，在概览页也给一条显眼的提示，别让人只看到默认值。
    if app.config_error.is_some() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "配置文件读不出来——按 4 看错误原文，改好按 r 重读",
            Style::new().fg(Color::Red),
        )));
    }
    lines
}

fn draw_plugins(frame: &mut Frame, app: &App, area: Rect) {
    let compact = area.width < 60;
    let [top, bottom] =
        Layout::vertical([Constraint::Percentage(55), Constraint::Percentage(45)]).areas(area);

    // 上半：运行中实例自报的插件（名字与状态只有实例知道）。
    let detail = app.detail();
    let title = match detail {
        Some(detail) if compact => format!(" 运行中 · {} ", detail.summary()),
        Some(detail) => format!(" 运行中的插件 · {} ", detail.summary()),
        None if compact => " 运行中 · 未启动 ".to_owned(),
        None => " 运行中的插件 · 实例没有在跑 ".to_owned(),
    };
    let header = Row::new(if compact {
        vec![Cell::from(""), Cell::from("插件 / 状态")]
    } else {
        vec![Cell::from(""), Cell::from("名字"), Cell::from("说明")]
    })
    .style(
        Style::new()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    );
    let rows: Vec<Row> = match detail {
        Some(detail) => {
            let mut rows = Vec::new();
            for name in &detail.active {
                rows.push(plugin_row_compact(compact, "●", Color::Green, name, "在跑"));
            }
            for (name, missing) in &detail.pending {
                rows.push(plugin_row_compact(
                    compact,
                    "◐",
                    Color::Yellow,
                    name,
                    &format!("在等 {}", missing.join(", ")),
                ));
            }
            for (name, reason) in &detail.failed {
                rows.push(plugin_row_compact(compact, "✕", Color::Red, name, reason));
            }
            rows
        }
        None => Vec::new(),
    };

    if rows.is_empty() {
        let message = match detail {
            Some(_) => "实例报回来的是空表——插件目录里可能一个插件都没有",
            None if compact => "实例没有在跑，只能看下半部分的磁盘插件",
            None => "实例没有在跑：这里只能看磁盘上的文件（下半部分）",
        };
        let block = Block::bordered()
            .title(title)
            .border_style(Style::new().fg(Color::DarkGray));
        frame.render_widget(
            Paragraph::new(format!(" {message}"))
                .wrap(Wrap { trim: false })
                .block(block),
            top,
        );
    } else {
        let widths = if compact {
            vec![Constraint::Length(2), Constraint::Min(0)]
        } else {
            vec![
                Constraint::Length(2),
                Constraint::Length(30),
                Constraint::Min(0),
            ]
        };
        let table = Table::new(rows, widths)
            .header(header)
            .column_spacing(1)
            .block(
                Block::bordered()
                    .title(title)
                    .border_style(Style::new().fg(Color::DarkGray)),
            );
        frame.render_widget(table, top);
    }

    // 下半：磁盘上的插件目录（不 dlopen 也能列）。
    let bottom_title = format!(" 插件目录 · {} 项 ", app.disk_plugins.len());
    let mut rows: Vec<Row> = app
        .disk_plugins
        .iter()
        .map(|plugin| {
            let color = match plugin.kind {
                PluginKind::Native => Color::Cyan,
                PluginKind::Script => Color::Magenta,
            };
            let file = plugin
                .path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            if compact {
                Row::new(vec![
                    Cell::from(plugin.kind.label()).style(Style::new().fg(color)),
                    Cell::from(plugin.name.clone()),
                ])
            } else {
                Row::new(vec![
                    Cell::from(plugin.kind.label()).style(Style::new().fg(color)),
                    Cell::from(plugin.name.clone()),
                    Cell::from(file).style(Style::new().fg(Color::DarkGray)),
                ])
            }
        })
        .collect();

    let mut bottom_block = Block::bordered()
        .title(bottom_title)
        .border_style(Style::new().fg(Color::DarkGray));
    if !compact {
        bottom_block = bottom_block.title_bottom(
            Line::from(Span::styled(
                format!(" {} ", app.plugin_dir.display()),
                Style::new().fg(Color::DarkGray),
            ))
            .centered(),
        );
    }

    if rows.is_empty() {
        frame.render_widget(
            Paragraph::new(" 目录是空的（或者不存在）——`fuyubot plugin install` 装一个")
                .wrap(Wrap { trim: false })
                .block(bottom_block),
            bottom,
        );
    } else {
        rows.insert(
            0,
            Row::new(if compact {
                vec![Cell::from("类型"), Cell::from("名字")]
            } else {
                vec![Cell::from("类型"), Cell::from("名字"), Cell::from("文件")]
            })
            .style(
                Style::new()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            ),
        );
        let widths = if compact {
            vec![Constraint::Length(6), Constraint::Min(0)]
        } else {
            vec![
                Constraint::Length(6),
                Constraint::Length(30),
                Constraint::Min(0),
            ]
        };
        let table = Table::new(rows, widths)
            .column_spacing(1)
            .block(bottom_block);
        frame.render_widget(table, bottom);
    }
}

fn plugin_row_compact(
    compact: bool,
    marker: &str,
    color: Color,
    name: &str,
    note: &str,
) -> Row<'static> {
    if compact {
        Row::new(vec![
            Cell::from(marker.to_owned()).style(Style::new().fg(color)),
            Cell::from(format!("{name}  {note}")),
        ])
    } else {
        plugin_row(marker, color, name, note)
    }
}

fn plugin_row(marker: &str, color: Color, name: &str, note: &str) -> Row<'static> {
    Row::new(vec![
        Cell::from(marker.to_owned()).style(Style::new().fg(color)),
        Cell::from(name.to_owned()),
        Cell::from(note.to_owned()),
    ])
}

fn draw_logs(frame: &mut Frame, app: &mut App, area: Rect) {
    let [body, footer] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(area);
    app.log_view = body.height.max(1) as usize;

    // 往回加载后要锚住的那一行：取走就用掉，别让它在切页 / 过滤后还留在状态里。
    let anchor = app.log_anchor.take();

    // 过滤后的行数每帧对一次（刷新时也算，但过滤条件可能刚改过）。
    let visible_count = app.filtered_logs().count();
    app.log_visible = visible_count;
    if visible_count == 0 {
        app.log_rows = 0;
        app.log_row_starts.clear();
    }

    if app.log_lines.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled(
                "（还没有日志——实例没跑过，或者日志还没写）",
                Style::new().fg(Color::DarkGray),
            ))
            .block(Block::new().padding(Padding::new(1, 1, 0, 0))),
            body,
        );
    } else if visible_count == 0 {
        frame.render_widget(
            Paragraph::new(Span::styled(
                "（当前过滤条件下没有日志——按 F 调整过滤）",
                Style::new().fg(Color::Yellow),
            ))
            .block(Block::new().padding(Padding::new(1, 1, 0, 0))),
            body,
        );
    } else {
        let width = body.width.max(1);
        let compact = width < 60;
        ensure_log_heights(app, width, compact);

        let selected = app.log_select.filter(|index| *index < visible_count);
        // 先把每个逻辑行折行后的起始行号累出来（只加缓存高度，不真的折行）。
        let mut starts = Vec::with_capacity(visible_count);
        let mut anchor_row = None;
        let mut row_count = 0usize;
        for (line_index, line) in app.log_lines.iter().enumerate() {
            if !app.log_filter.matches(line) {
                continue;
            }
            starts.push(row_count);
            if anchor == Some(line_index) {
                anchor_row = Some(row_count);
            }
            row_count += app.log_heights[line_index] as usize;
        }
        app.log_row_starts = starts;
        app.log_rows = row_count;
        // 刚往回加载过：把原来视口顶部那一行放回原位，内容不会突然跳一段。
        if let Some(row) = anchor_row {
            app.log_scroll = row;
        }
        if app.log_select.is_some() {
            app.ensure_log_select_visible();
        }
        let offset = app.log_offset();

        // 只折行 / 着色视口里那几行：窗口往回翻到几万行，每帧也只看这几行。
        let first = app
            .log_row_starts
            .partition_point(|&start| start <= offset)
            .saturating_sub(1);
        let mut skip = offset.saturating_sub(app.log_row_starts[first]);
        let mut rows = Vec::with_capacity(app.log_view.saturating_add(1));
        let mut visible_index = 0usize;
        for line in &app.log_lines {
            if !app.log_filter.matches(line) {
                continue;
            }
            if visible_index < first {
                visible_index += 1;
                continue;
            }
            let highlight = Some(visible_index) == selected;
            visible_index += 1;
            for mut row in wrap_log_line(log_line(line, compact), width) {
                if skip > 0 {
                    skip -= 1;
                    continue;
                }
                if highlight {
                    row = row.style(
                        Style::new()
                            .bg(Color::Indexed(238))
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    );
                }
                rows.push(row);
                if rows.len() >= app.log_view {
                    break;
                }
            }
            if rows.len() >= app.log_view {
                break;
            }
        }
        frame.render_widget(Paragraph::new(rows), body);
    }

    if let Some(index) = app.log_select {
        let (label, what) = app
            .filtered_logs()
            .nth(index)
            .map(|line| match logfmt::tdlib_update_type(line) {
                Some(kind) => (kind, "完整 JSON"),
                None => ("普通日志".to_owned(), "整行"),
            })
            .unwrap_or_else(|| ("—".to_owned(), "—"));
        let hint = if area.width >= 60 {
            Line::from(vec![
                Span::styled("已选 ", Style::new().fg(Color::DarkGray)),
                Span::styled(
                    label,
                    Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(" · y / Enter 复制{what} · ↑↓ 行 · Esc 退出"),
                    Style::new().fg(Color::DarkGray),
                ),
            ])
        } else {
            Line::from(vec![
                Span::styled("已选 ", Style::new().fg(Color::DarkGray)),
                Span::styled(
                    label,
                    Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                ),
                Span::styled(" · y 复制", Style::new().fg(Color::DarkGray)),
            ])
        };
        frame.render_widget(Paragraph::new(hint), footer);
        return;
    }

    let level = app.effective_level().to_owned();
    let color = level_color(&level);
    let follow = if app.follow {
        "跟随末尾"
    } else {
        "已暂停（End 回到最新）"
    };
    let filter_color = if app.log_filter.is_default() {
        Color::DarkGray
    } else {
        Color::Cyan
    };
    // 不报行数：窗口里有多少行随滚动变，用户要看的是「还能不能往回翻」。
    let earlier = if app.log_window_start > 0 {
        " · ↑ 更早"
    } else {
        ""
    };
    let hint = if area.width >= 60 {
        Line::from(vec![
            Span::styled("日志级别 ", Style::new().fg(Color::DarkGray)),
            Span::styled(level, Style::new().fg(color).add_modifier(Modifier::BOLD)),
            Span::styled("（l）", Style::new().fg(Color::DarkGray)),
            Span::styled(" · 过滤 ", Style::new().fg(Color::DarkGray)),
            Span::styled(
                app.log_filter.summary(),
                Style::new().fg(filter_color).add_modifier(Modifier::BOLD),
            ),
            Span::styled("（F）", Style::new().fg(Color::DarkGray)),
            Span::styled(
                format!(" · Enter 选择 · {follow}{earlier}"),
                Style::new().fg(Color::DarkGray),
            ),
        ])
    } else {
        Line::from(vec![
            Span::styled("级别 ", Style::new().fg(Color::DarkGray)),
            Span::styled(level, Style::new().fg(color).add_modifier(Modifier::BOLD)),
            Span::styled(
                format!(" · 过滤 {}{earlier}", app.log_filter.summary()),
                Style::new().fg(Color::DarkGray),
            ),
        ])
    };
    frame.render_widget(Paragraph::new(hint), footer);
}

fn draw_config(frame: &mut Frame, app: &mut App, area: Rect) {
    let [summary, table_area, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(area);
    app.config_view = if table_area.width < 45 {
        (table_area.height / 2) as usize + 1
    } else {
        table_area.height.max(1) as usize
    };
    app.ensure_config_visible();

    let missing = if app.config_path.is_file() {
        ""
    } else {
        "（不存在，先 fuyubot init；现在全是默认值）"
    };
    let summary_line = if area.width >= 70 {
        Line::from(vec![
            Span::styled("配置文件  ", Style::new().fg(Color::DarkGray)),
            Span::styled(
                app.config_path.display().to_string(),
                Style::new().fg(Color::Gray),
            ),
            Span::styled(
                format!("  ·  生效值（含默认值）{missing}"),
                Style::new().fg(Color::DarkGray),
            ),
        ])
    } else {
        Line::from(Span::styled(
            "配置 · 生效值（含默认值）",
            Style::new().fg(Color::DarkGray),
        ))
    };
    frame.render_widget(Paragraph::new(summary_line), summary);

    if let Some(error) = &app.config_error {
        frame.render_widget(
            Paragraph::new(error.clone())
                .wrap(Wrap { trim: false })
                .style(Style::new().fg(Color::Red)),
            table_area,
        );
        frame.render_widget(
            Paragraph::new(Span::styled(
                "按 r 重新读取 · Esc 返回导航",
                Style::new().fg(Color::DarkGray),
            )),
            footer,
        );
        return;
    }

    if app.config_items.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled(
                "（没有可显示的配置项）",
                Style::new().fg(Color::DarkGray),
            )),
            table_area,
        );
    } else {
        let focused = app.workspace_focused();
        let width = table_area.width;
        if width >= 70 {
            let header = Row::new(vec!["段", "键", "生效值", "来源"]).style(
                Style::new()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            );
            let rows: Vec<Row> = app
                .config_items
                .iter()
                .map(|item| {
                    Row::new(vec![
                        Cell::from(item.section.clone()).style(Style::new().fg(Color::DarkGray)),
                        Cell::from(item.key.clone()).style(config_key_style(item)),
                        Cell::from(item.display.clone()),
                        Cell::from(item.note.clone()).style(Style::new().fg(Color::DarkGray)),
                    ])
                })
                .collect();
            render_config_table(
                frame,
                table_area,
                header,
                rows,
                &[
                    Constraint::Length(14),
                    Constraint::Length(16),
                    Constraint::Min(0),
                    Constraint::Length(6),
                ],
                app.config_selected,
                app.config_offset(),
                focused,
            );
        } else if width >= 45 {
            let header = Row::new(vec!["段", "键", "生效值（来源）"]).style(
                Style::new()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            );
            let rows: Vec<Row> = app
                .config_items
                .iter()
                .map(|item| {
                    Row::new(vec![
                        Cell::from(item.section.clone()).style(Style::new().fg(Color::DarkGray)),
                        Cell::from(item.key.clone()).style(config_key_style(item)),
                        Cell::from(format!("{}（{}）", item.display, item.note)),
                    ])
                })
                .collect();
            render_config_table(
                frame,
                table_area,
                header,
                rows,
                &[
                    Constraint::Length(10),
                    Constraint::Length(14),
                    Constraint::Min(0),
                ],
                app.config_selected,
                app.config_offset(),
                focused,
            );
        } else {
            draw_config_compact(frame, app, table_area, focused);
        }
    }

    let hint = if let Some(item) = app.config_items.get(app.config_selected) {
        if table_area.width >= 70 {
            format!("{}（{}） = {}", item.path_label(), item.note, item.display)
        } else {
            format!("{} = {}", item.key, item.display)
        }
    } else {
        "↑↓ 选择 · Enter 编辑 · Esc 返回".to_owned()
    };
    frame.render_widget(
        Paragraph::new(Span::styled(hint, Style::new().fg(Color::DarkGray))),
        footer,
    );
}

fn config_key_style(item: &ConfigItem) -> Style {
    if item.editable() {
        Style::new()
    } else {
        Style::new().fg(Color::DarkGray)
    }
}

/// 超窄屏的配置列表：每个配置占两行（完整键 + 值 / 来源），不再横向截断键。
fn draw_config_compact(frame: &mut Frame, app: &App, area: Rect, focused: bool) {
    let offset = app.config_offset();
    let selected = app.config_selected;
    let max_rows = area.height as usize;
    let mut lines = Vec::new();
    for (index, item) in app.config_items.iter().enumerate().skip(offset) {
        if lines.len() + 2 > max_rows {
            break;
        }
        let is_selected = index == selected;
        let key_style = if is_selected {
            if focused {
                Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
            } else {
                Style::new().add_modifier(Modifier::BOLD)
            }
        } else {
            config_key_style(item)
        };
        let marker = if is_selected { "▸ " } else { "  " };
        lines.push(Line::from(vec![
            Span::styled(marker.to_owned(), key_style),
            Span::styled(item.path_label(), key_style),
        ]));
        let value = if item.note.is_empty() {
            item.display.clone()
        } else {
            format!("{}  · {}", item.display, item.note)
        };
        let value_style = if is_selected {
            key_style
        } else {
            Style::new().fg(Color::Gray)
        };
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(value, value_style),
        ]));
    }
    frame.render_widget(
        Paragraph::new(lines).block(Block::new().padding(Padding::new(1, 1, 0, 0))),
        area,
    );
}

fn render_config_table(
    frame: &mut Frame,
    area: Rect,
    header: Row<'static>,
    rows: Vec<Row<'static>>,
    widths: &[Constraint],
    selected: usize,
    offset: usize,
    focused: bool,
) {
    let table = Table::new(rows, widths.to_vec())
        .column_spacing(1)
        .header(header)
        .block(Block::new().padding(Padding::new(1, 1, 0, 0)))
        .row_highlight_style(if focused {
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
        } else {
            Style::new().add_modifier(Modifier::BOLD)
        })
        .highlight_symbol("▸ ");
    let mut state = TableState::new()
        .with_selected(Some(selected))
        .with_offset(offset);
    frame.render_stateful_widget(table, area, &mut state);
}

fn draw_help(frame: &mut Frame, app: &mut App, area: Rect) {
    let bindings = [
        (
            "Tab / Shift+Tab",
            "在「导航」与「工作区」之间切换焦点（边框高亮）",
        ),
        (
            "Enter / Esc",
            "导航 → 工作区 / 工作区 → 导航（在导航上 Esc 退出）",
        ),
        (
            "↑ / ↓ 或 j / k",
            "导航焦点：切页；工作区焦点：滚动日志 / 选择配置项 / 移动日志选择",
        ),
        ("← / →", "切换顶部标签页（窄屏横向导航；工作区焦点也生效）"),
        ("h / l", "导航焦点：上一页 / 下一页"),
        ("1 – 5", "概览 / 插件 / 日志 / 配置 / 帮助"),
        ("s", "启动 bot（等价于 fuyubot run --background）"),
        ("x", "停止 bot（再按 y 确认）"),
        ("r", "刷新状态、插件、日志与配置"),
        ("Enter / e", "日志页：进入选择模式；配置页：编辑选中项"),
        (
            "y / Enter",
            "日志选择模式：复制选中行（TDLib Update 复制完整 JSON）",
        ),
        ("f", "日志页（工作区焦点）：跟随末尾开关（打开 = 回到最新）"),
        ("l", "日志页（工作区焦点）：调整运行中实例的日志级别"),
        ("F", "日志页（工作区焦点）：按级别 / 来源过滤日志"),
        ("c", "日志页（工作区焦点）：清空当前日志文件（要确认）"),
        (
            "PgUp / PgDn",
            "日志 / 帮助翻页；日志到顶再往上会分段回读更早的日志（工作区焦点）",
        ),
        (
            "Home / End",
            "日志：Home 到窗口最早，End 回到最新（重读尾部）；帮助 / 配置跳到首尾",
        ),
        ("q / Ctrl+C", "退出面板"),
    ];

    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                "fuyubot 终端操作面板",
                Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!("  v{}", env!("CARGO_PKG_VERSION"))),
        ]),
        Line::from(Span::styled(
            "只读面板：不加载插件、不接 TDLib；启停都走子进程，和手敲命令同一条路。",
            Style::new().fg(Color::DarkGray),
        )),
        Line::from(""),
        section("按键"),
    ];
    for (keys, meaning) in bindings {
        if area.width < 60 {
            // 窄屏把键和说明分成两行，说明缩进，避免挤成一坨。
            lines.push(Line::from(Span::styled(
                format!("  {keys}"),
                Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(vec![
                Span::raw("    "),
                Span::styled(meaning, Style::new().fg(Color::Gray)),
            ]));
        } else {
            lines.push(Line::from(vec![
                Span::styled(format!("  {:<18}", keys), Style::new().fg(Color::Cyan)),
                Span::raw(meaning),
            ]));
        }
    }
    lines.push(Line::from(""));
    lines.push(section("界面"));
    lines.push(Line::from(
        "  顶部版本栏 · 导航（宽屏左侧 / 窄屏顶部） · 中央工作区 · 底部两行（操作 / 资源）",
    ));
    lines.push(Line::from(
        "  插件目录里的动态库不会被加载：插件名与状态来自运行中实例发布的 .fuyubot/status",
    ));
    lines.push(Line::from(
        "  日志页只装一个窗口（首屏是尾部，往上翻按段回读）；底栏提示还能不能往回翻",
    ));

    let text_width = area.width.saturating_sub(2).max(1) as usize;
    app.help_view = area.height.max(1) as usize;
    app.help_rows = visual_rows(&lines, text_width);
    let max = app.help_rows.saturating_sub(app.help_view);
    app.help_scroll = app.help_scroll.min(max);

    let paragraph = Paragraph::new(lines)
        .block(Block::new().padding(Padding::new(1, 1, 0, 0)))
        .wrap(Wrap { trim: false });
    frame.render_widget(
        paragraph.scroll((app.help_scroll.min(u16::MAX as usize) as u16, 0)),
        area,
    );
}

/// 估算一组行在给定宽度下折行后的可见行数（帮助页滚动用）。
fn visual_rows(lines: &[Line<'_>], width: usize) -> usize {
    let width = width.max(1);
    lines
        .iter()
        .map(|line| line.width().max(1).div_ceil(width))
        .sum()
}

// ── 顶部版本栏 ─────────────────────────────────────────────────────

/// 顶部右侧状态区最多占多少列：窄终端整块不画（左边版本号本身就要 19 列）。
///
/// 实际宽度由内容决定（见 [`header_status`]），这里只是上限——名字长的时候
/// 让位给左边标题，而不是把左边挤没。
fn header_status_width(width: u16) -> u16 {
    if width >= 100 {
        54
    } else if width >= 80 {
        40
    } else if width >= 55 {
        24
    } else {
        0
    }
}

fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    let status = header_status(app, header_status_width(area.width));
    let right_width = status
        .as_ref()
        .map_or(0, |line| line.width().min(usize::from(area.width)) as u16);
    let (left, right) = if right_width == 0 {
        (area, Rect::new(area.x, area.y, 0, area.height))
    } else {
        let [left, right] =
            Layout::horizontal([Constraint::Min(0), Constraint::Length(right_width)]).areas(area);
        (left, right)
    };

    let mut title = vec![Span::styled(
        format!("[fuyubot - v{}]", env!("CARGO_PKG_VERSION")),
        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
    )];
    if area.width >= 50 {
        title.push(Span::styled(
            "  终端操作面板",
            Style::new().fg(Color::DarkGray),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(title)), left);

    if let Some(status) = status {
        frame.render_widget(Paragraph::new(status), right);
    }
}

/// 顶部右侧那一行：状态徽标 + 实例名 + TDLib 连接状态 + 刷新时间。
///
/// 从「全都要」往「只剩徽标」退：第一个塞得进 `max_width` 的形态就是这一帧画的那个。
/// 之所以不硬编码几档宽度，是因为实例名是用户起的；右对齐的行一旦比区域宽，
/// 被截掉的是**左边**——那正好是最不该丢的徽标。
fn header_status(app: &App, max_width: u16) -> Option<Line<'static>> {
    if max_width == 0 {
        return None;
    }
    // 启停动作进行中只报这一件事：那几百毫秒里状态徽标是旧值，画出来只会误导。
    if let Some(busy) = app.busy {
        return Some(
            Line::from(Span::styled(
                busy.label(),
                Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ))
            .right_aligned(),
        );
    }

    let limit = usize::from(max_width);
    let (badge_text, badge_color) = badge(&app.state);
    let state_span = || {
        Span::styled(
            format!("{badge_text} "),
            Style::new().fg(badge_color).add_modifier(Modifier::BOLD),
        )
    };
    let name_span = || Span::styled(header_name(app), Style::new().fg(Color::Gray));
    let tail_span = || {
        Span::styled(
            format!("  已刷新 {}s 前", app.last_refresh.elapsed().as_secs()),
            Style::new().fg(Color::DarkGray),
        )
    };
    // 只有**运行中**的实例才谈得上连接：陈旧状态里留着的是上次断线前的历史值。
    let connection = app
        .running()
        .and_then(|info| info.connection.as_deref())
        .and_then(ConnectionState::from_type_name);

    let mut candidates: Vec<Line<'static>> = Vec::new();
    if let Some(state) = connection {
        let label = crate::instance::connection_label(state);
        let color = connection_color(state);
        let full = || Span::styled(format!("  · TDLib {label}"), Style::new().fg(color));
        let short = || Span::styled(format!("  · {label}"), Style::new().fg(color));
        candidates.push(Line::from(vec![
            state_span(),
            name_span(),
            full(),
            tail_span(),
        ]));
        candidates.push(Line::from(vec![state_span(), name_span(), full()]));
        candidates.push(Line::from(vec![state_span(), name_span(), short()]));
        candidates.push(Line::from(vec![state_span(), short()]));
    }
    candidates.push(Line::from(vec![state_span(), name_span(), tail_span()]));
    candidates.push(Line::from(vec![state_span(), name_span()]));
    candidates.push(Line::from(vec![state_span()]));

    candidates
        .into_iter()
        .find(|line| line.width() <= limit)
        .map(Line::right_aligned)
}

/// 连接状态的颜色：绿=可用，红=网络不通，其余都是「正在往可用走」。
fn connection_color(state: ConnectionState) -> Color {
    match state {
        ConnectionState::Ready => Color::Green,
        ConnectionState::Updating => Color::Cyan,
        ConnectionState::Connecting => Color::Yellow,
        ConnectionState::ConnectingToProxy => Color::Magenta,
        ConnectionState::WaitingForNetwork => Color::Red,
    }
}

fn header_name(app: &App) -> String {
    match &app.state {
        State::Running(Some(info)) => info.name.clone(),
        State::Running(None) => app.bot_name.clone(),
        State::Stale(Some(info)) => info.name.clone(),
        State::Stale(None) => "上次状态丢失".to_owned(),
        State::Absent => app.bot_name.clone(),
    }
}

// ── 底部：可用操作 / 资源占用 ─────────────────────────────────────

fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    let compact = area.width < 70;
    let [operations, metrics] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);

    // 第一行：提示条优先（临时），否则按焦点 / 页面 / 实例状态给「现在真能按的键」。
    let line = match app.visible_toast() {
        Some(toast) => {
            let color = match toast.level {
                Level::Info => Color::Green,
                Level::Warn => Color::Yellow,
                Level::Error => Color::Red,
            };
            Line::from(vec![
                Span::styled(" ◆ ", Style::new().fg(color)),
                Span::styled(toast.text.clone(), Style::new().fg(color)),
            ])
        }
        None => operations_line(app, compact),
    };
    frame.render_widget(Paragraph::new(line), operations);
    frame.render_widget(Paragraph::new(metrics_lines(app, area.width)), metrics);
}

/// 当前可用操作：按焦点（导航 / 工作区）与页面给键位，不列用不上的键。
fn operations_line(app: &App, compact: bool) -> Line<'static> {
    let focused = app.workspace_focused();
    let selecting = focused && app.page == Page::Logs && app.log_select.is_some();
    let tag = if selecting {
        "[选择]"
    } else if focused {
        if compact { "[工作]" } else { "[工作区]" }
    } else {
        "[导航]"
    };
    let mut spans = vec![
        Span::styled(
            tag,
            Style::new()
                .fg(if selecting {
                    Color::Yellow
                } else if focused {
                    Color::Cyan
                } else {
                    Color::Yellow
                })
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
    ];

    let mut items: Vec<(&'static str, &'static str)> = Vec::new();
    if selecting {
        if compact {
            items.extend([("↑↓", "行"), ("y", "复制"), ("Esc", "退出")]);
        } else {
            items.extend([
                ("↑↓", "行"),
                ("PgUp/Dn", "翻页"),
                ("y", "复制 JSON"),
                ("Esc", "退出选择"),
                ("q", "退出"),
            ]);
        }
    } else if focused {
        let start_or_stop = if app.can_start() {
            ("s", "启动")
        } else {
            ("x", "停止")
        };
        match app.page {
            Page::Overview if compact => {
                items.extend([start_or_stop, ("r", "刷新"), ("Esc", "返回")])
            }
            Page::Overview => {
                items.extend([start_or_stop, ("r", "刷新"), ("?", "帮助"), ("Esc", "返回")])
            }
            Page::Logs if compact => items.extend([
                ("↑↓", "滚动"),
                ("Enter", "选"),
                ("l", "级"),
                ("c", "清"),
                ("Esc", "返回"),
            ]),
            Page::Logs => items.extend([
                ("↑↓", "滚动"),
                ("Enter", "选择"),
                ("l", "级别"),
                ("F", "过滤"),
                (
                    "f",
                    if app.follow {
                        "暂停跟随"
                    } else {
                        "恢复跟随"
                    },
                ),
                ("c", "清空"),
                ("Esc", "返回"),
            ]),
            Page::Config if compact => {
                items.extend([("↑↓", "选"), ("Enter", "编辑"), ("Esc", "返回")])
            }
            Page::Config => items.extend([
                ("↑↓", "选择"),
                ("Enter", "编辑"),
                ("PgUp/Dn", "翻页"),
                ("Esc", "返回"),
            ]),
            Page::Help if compact => items.extend([("↑↓", "滚动"), ("Esc", "返回")]),
            Page::Help => items.extend([("↑↓", "滚动"), ("PgUp/Dn", "翻页"), ("Esc", "返回")]),
            _ => items.push(("Esc", "返回")),
        }
        if !compact {
            items.push(("q", "退出"));
        }
    } else {
        let start_or_stop = if app.can_start() {
            ("s", "启动")
        } else {
            ("x", "停止")
        };
        if compact {
            items.extend([
                ("←→", "换页"),
                ("Tab", "聚焦"),
                start_or_stop,
                ("q", "退出"),
            ]);
        } else {
            items.extend([
                ("←→", "换页"),
                ("1–5", "直达"),
                ("Tab", "聚焦"),
                start_or_stop,
                ("r", "刷新"),
                ("?", "帮助"),
                ("q", "退出"),
            ]);
        }
    }

    for (index, (keys, label)) in items.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(" · ", Style::new().fg(Color::DarkGray)));
        }
        spans.push(Span::styled(
            format!("{keys} "),
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw(*label));
    }
    Line::from(spans)
}

/// 资源占用：宽终端一行，窄终端拆成两行。
fn metrics_lines(app: &App, width: u16) -> Vec<Line<'static>> {
    let (cpu, memory) = match app.usage {
        Some(ProcessUsage {
            cpu_percent,
            memory_bytes,
        }) => (cpu_percent, memory_bytes),
        None => (0.0, 0),
    };
    let has_usage = app.usage.is_some();
    let uptime_full = app
        .running()
        .map(|info| crate::instance::uptime_text(info.started, crate::instance::unix_now()))
        .unwrap_or_else(|| "—".to_owned());
    let plugins = app
        .running()
        .map(|info| info.detail.active.len())
        .unwrap_or(0);
    let pid = app
        .running()
        .map(|info| info.pid.to_string())
        .unwrap_or_else(|| "—".to_owned());
    let level = app.effective_level().to_owned();

    if width >= 70 {
        let fields: [(&str, String); 6] = [
            (
                "CPU",
                if has_usage {
                    format!("{cpu:.1}%")
                } else {
                    "—".to_owned()
                },
            ),
            (
                "内存",
                if has_usage {
                    format!("{:.1} MB", memory as f64 / (1024.0 * 1024.0))
                } else {
                    "—".to_owned()
                },
            ),
            ("运行", uptime_full),
            ("插件", plugins.to_string()),
            ("日志", level.clone()),
            ("PID", pid),
        ];
        return vec![metric_line(&fields, &level)];
    }

    let cpu_short = if has_usage {
        format!("{cpu:.0}%")
    } else {
        "—".to_owned()
    };
    let memory_short = if has_usage {
        format!("{:.0}M", memory as f64 / (1024.0 * 1024.0))
    } else {
        "—".to_owned()
    };
    // 手机上只留 CPU / 内存 / 当前级别；运行时长、插件数、PID 不占地方。
    vec![Line::from(vec![
        Span::styled(" CPU ", Style::new().fg(Color::DarkGray)),
        Span::styled(cpu_short, Style::new().fg(Color::Gray)),
        Span::styled(" · 内存 ", Style::new().fg(Color::DarkGray)),
        Span::styled(memory_short, Style::new().fg(Color::Gray)),
        Span::styled(" · ", Style::new().fg(Color::DarkGray)),
        Span::styled(
            level.clone(),
            Style::new()
                .fg(level_color(&level))
                .add_modifier(Modifier::BOLD),
        ),
    ])]
}

fn metric_line(fields: &[(&str, String)], level: &str) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    for (index, (label, value)) in fields.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(" · ", Style::new().fg(Color::DarkGray)));
        }
        spans.push(Span::styled(
            format!("{label} "),
            Style::new().fg(Color::DarkGray),
        ));
        let color = if *label == "日志" {
            level_color(level)
        } else if *label == "PID" {
            Color::DarkGray
        } else {
            Color::Gray
        };
        spans.push(Span::styled(value.clone(), Style::new().fg(color)));
    }
    Line::from(spans)
}

/// 窄栏的运行时长：`42s` / `3m` / `2h3m`。
fn level_color(level: &str) -> Color {
    match level {
        "trace" | "debug" => Color::Cyan,
        "warn" => Color::Yellow,
        "error" => Color::Red,
        _ => Color::Green,
    }
}

// ── 确认框 ─────────────────────────────────────────────────────────

fn draw_confirm(frame: &mut Frame, area: Rect) {
    let popup = centered(area, 56, 7);
    frame.render_widget(Clear, popup);

    let block = Block::bordered()
        .title(Line::from(vec![Span::styled(
            " 停止 bot ",
            Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        )]))
        .border_style(Style::new().fg(Color::Yellow));
    let lines = vec![
        Line::from(""),
        Line::from("确认停止当前实例？").centered(),
        Line::from(""),
        Line::from(vec![
            key("y"),
            Span::raw(" 停止    "),
            key("n / Esc"),
            Span::raw(" 取消"),
        ])
        .centered(),
    ];
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            // 浅色主题下默认前景色是黑的：黑底必须配白字，否则整个框看不见。
            .style(Style::new().bg(Color::Black).fg(Color::White)),
        popup,
    );
}

/// 清空日志确认框（日志页按 `c`）。
fn draw_clear_confirm(frame: &mut Frame, area: Rect) {
    let popup = centered(area, 64, 8);
    frame.render_widget(Clear, popup);
    let block = Block::bordered()
        .title(Line::from(vec![Span::styled(
            " 清空日志 ",
            Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        )]))
        .border_style(Style::new().fg(Color::Yellow));
    let lines = vec![
        Line::from(""),
        Line::from("清空当前日志文件？").centered(),
        Line::from(Span::styled(
            "运行中的实例会继续写新日志；这里只清当前文件。",
            Style::new().fg(Color::DarkGray),
        ))
        .centered(),
        Line::from(""),
        Line::from(vec![
            key("y"),
            Span::raw(" 清空    "),
            key("n / Esc"),
            Span::raw(" 取消"),
        ])
        .centered(),
    ];
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .style(Style::new().bg(Color::Black).fg(Color::White)),
        popup,
    );
}

/// 日志级别选择框（日志页按 `l`）。
fn draw_level_picker(frame: &mut Frame, app: &App, area: Rect) {
    let Some(cursor) = app.level_picker else {
        return;
    };
    let popup = centered(area, 72, 10);
    frame.render_widget(Clear, popup);

    let current = app.effective_level().to_owned();
    let block = Block::bordered()
        .title(Line::from(vec![Span::styled(
            " 日志级别 ",
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )]))
        .border_style(Style::new().fg(Color::Cyan));

    let mut lines = Vec::new();
    for (index, (name, _)) in scaffold::LOG_LEVELS.iter().enumerate() {
        let selected = index == cursor;
        let is_current = *name == current;
        let marker = if selected { "▸ " } else { "  " };
        let dot = if is_current { "●" } else { " " };
        let description = match *name {
            "error" => "只保留错误",
            "warn" => "警告及以上",
            "info" => "默认（推荐）",
            "debug" => "插件调试日志",
            "trace" => "全部日志（很吵）",
            _ => "",
        };
        let style = if selected {
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
        } else if is_current {
            Style::new().fg(Color::Green)
        } else {
            Style::new()
        };
        lines.push(Line::from(vec![
            Span::styled(marker, style),
            Span::styled(format!("{dot} {name:<6}"), style),
            Span::styled(description, Style::new().fg(Color::DarkGray)),
        ]));
    }
    lines.push(Line::from(""));
    let footer = if popup.width >= 60 {
        if app.can_stop() {
            "↑↓ 选择 · 1–5 直达 · Enter 应用 · Esc 取消（运行时生效；重启回到配置）"
        } else {
            "↑↓ 选择 · 1–5 直达 · Enter 应用 · Esc 取消（实例没跑：下次启动生效）"
        }
    } else {
        "↑↓ 选择 · Enter 应用 · Esc 取消"
    };
    lines.push(Line::from(Span::styled(footer, Style::new().fg(Color::DarkGray))).centered());

    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .style(Style::new().bg(Color::Black).fg(Color::White)),
        popup,
    );
}

/// 日志过滤弹框（日志页按 `F`）。
fn draw_filter_picker(frame: &mut Frame, app: &App, area: Rect) {
    let Some(draft) = app.filter_picker else {
        return;
    };
    let popup = centered(area, 64, 7);
    frame.render_widget(Clear, popup);

    let block = Block::bordered()
        .title(Line::from(vec![Span::styled(
            " 日志过滤 ",
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )]))
        .border_style(Style::new().fg(Color::Cyan));

    let lines = vec![
        filter_row(draft.row == FilterRow::Level, "级别", draft.level.label()),
        filter_row(draft.row == FilterRow::Source, "来源", draft.source.label()),
        Line::from(""),
        Line::from(Span::styled(
            "过滤只影响显示，不改日志文件",
            Style::new().fg(Color::DarkGray),
        ))
        .centered(),
        Line::from(Span::styled(
            if popup.width >= 60 {
                "↑↓ 选行 · ←→ 改值 · c 重置 · Enter 应用 · Esc 取消"
            } else {
                "↑↓ 选行 · ←→ 改值 · Enter 应用 · Esc 取消"
            },
            Style::new().fg(Color::DarkGray),
        ))
        .centered(),
    ];
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .style(Style::new().bg(Color::Black).fg(Color::White)),
        popup,
    );
}

fn filter_row(selected: bool, label: &'static str, value: &'static str) -> Line<'static> {
    let marker = if selected { "▸ " } else { "  " };
    let style = if selected {
        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(Color::Gray)
    };
    Line::from(vec![
        Span::styled(marker, style),
        Span::styled(format!("{label}  "), style),
        Span::styled(format!("◂ {value} ▸"), style),
    ])
}

/// 编辑单个配置项（配置页按 Enter / e）。
fn draw_config_editor(frame: &mut Frame, app: &App, area: Rect) {
    let Some(edit) = app.config_edit.as_ref() else {
        return;
    };
    let popup_width = area.width.saturating_sub(2).min(78).max(20);
    let popup = centered(area, popup_width, 9);
    frame.render_widget(Clear, popup);

    let block = Block::bordered()
        .title(Line::from(vec![Span::styled(
            " 编辑配置 ",
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )]))
        .border_style(Style::new().fg(Color::Cyan));

    let type_hint = match edit.kind {
        ConfigKind::Text { allow_empty: true } => "文本（可留空 = 用默认）".to_owned(),
        ConfigKind::Text { allow_empty: false } => "文本（不能为空）".to_owned(),
        ConfigKind::Integer { min, max } => {
            if max == i64::MAX {
                format!("整数 ≥ {min}")
            } else {
                format!("整数 {min}–{max}")
            }
        }
        ConfigKind::Choice(choices) => format!("可选：{}", choices.join(" / ")),
        ConfigKind::ReadOnly => "只读".to_owned(),
    };
    let error = edit
        .error
        .as_ref()
        .map(|error| {
            Line::from(Span::styled(
                format!("⚠ {error}"),
                Style::new().fg(Color::Red),
            ))
        })
        .unwrap_or_else(|| Line::from(""));

    let lines = vec![
        Line::from(Span::styled(
            edit.label.clone(),
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )),
        Line::from(vec![
            Span::styled("当前  ", Style::new().fg(Color::DarkGray)),
            Span::raw(edit.current.clone()),
            Span::styled(
                format!("（{}）", edit.note),
                Style::new().fg(Color::DarkGray),
            ),
        ]),
        config_input_line(edit, popup_width.saturating_sub(8).max(8) as usize),
        Line::from(""),
        Line::from(Span::styled(type_hint, Style::new().fg(Color::DarkGray))),
        error,
        Line::from(Span::styled(
            if popup_width >= 60 {
                "Enter 保存 · Esc 取消 · ←→ 移光标 · Ctrl+U 清空"
            } else {
                "Enter 保存 · Esc 取消 · ←→ 光标"
            },
            Style::new().fg(Color::DarkGray),
        )),
    ];

    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .style(Style::new().bg(Color::Black).fg(Color::White)),
        popup,
    );
}

fn config_input_line(edit: &ConfigEdit, max_chars: usize) -> Line<'static> {
    let chars: Vec<char> = edit.input.chars().collect();
    let max_chars = max_chars.max(8);
    let start = edit.cursor.saturating_sub(max_chars - 1);
    let end = (start + max_chars).min(chars.len());

    let mut spans = vec![Span::styled("新值  ", Style::new().fg(Color::DarkGray))];
    if start > 0 {
        spans.push(Span::styled("…", Style::new().fg(Color::DarkGray)));
    }
    for (index, ch) in chars[start..end].iter().enumerate() {
        let index = start + index;
        if index == edit.cursor {
            spans.push(Span::styled(
                ch.to_string(),
                Style::new().bg(Color::Cyan).fg(Color::Black),
            ));
        } else {
            spans.push(Span::raw(ch.to_string()));
        }
    }
    if edit.cursor >= chars.len() {
        spans.push(Span::styled("▏", Style::new().fg(Color::Cyan)));
    }
    Line::from(spans)
}

/// 按显示宽度把一行日志折成多行（中文按两列算）；续行缩进两格。
///
/// 用预折行而不是 `Paragraph::wrap`：日志页要按「逻辑行」选择 / 复制，滚动又按
/// 折行后的可见行算，自己折才能把两者的映射记下来。
fn wrap_log_line(line: Line<'static>, width: u16) -> Vec<Line<'static>> {
    let width = width.max(1);
    let indent = if width > 2 { "  " } else { "" };
    let mut rows: Vec<Vec<Span<'static>>> = vec![Vec::new()];
    let mut used: u16 = 0;

    for span in line.spans {
        let style = span.style;
        let mut chunk = String::new();
        for ch in span.content.chars() {
            let char_width = UnicodeWidthChar::width(ch).unwrap_or(0) as u16;
            if used > 0 && used.saturating_add(char_width) > width {
                if !chunk.is_empty() {
                    rows.last_mut()
                        .expect("至少有一行")
                        .push(Span::styled(std::mem::take(&mut chunk), style));
                }
                rows.push(Vec::new());
                used = 0;
                if !indent.is_empty() {
                    rows.last_mut()
                        .expect("刚 push 过")
                        .push(Span::styled(indent, Style::new().fg(Color::DarkGray)));
                    used = indent.len() as u16;
                }
            }
            chunk.push(ch);
            used = used.saturating_add(char_width);
        }
        if !chunk.is_empty() {
            rows.last_mut()
                .expect("至少有一行")
                .push(Span::styled(chunk, style));
        }
    }

    rows.into_iter().map(Line::from).collect()
}

/// 折行后的行数：和 `wrap_log_line` 同一套规则，但只数行、不造 `Line`。
///
/// 日志页每帧都要知道「视口从第几行开始」，窗口大起来不可能每帧把每行都折一遍；
/// 这里只数宽度，结果缓存在 `App::log_heights` 里。
fn wrapped_height(line: &Line<'_>, width: u16) -> u32 {
    let width = width.max(1);
    // 续行缩进两格——`wrap_log_line` 的 `indent` 也是这么算的。
    let indent: u16 = if width > 2 { 2 } else { 0 };
    let mut rows: u32 = 1;
    let mut used: u16 = 0;
    for span in &line.spans {
        for ch in span.content.chars() {
            let char_width = UnicodeWidthChar::width(ch).unwrap_or(0) as u16;
            if used > 0 && used.saturating_add(char_width) > width {
                rows = rows.saturating_add(1);
                used = indent;
            }
            used = used.saturating_add(char_width);
        }
    }
    rows
}

/// 保证每行的折行高度都有缓存：宽度变了整表作废，缺的（含往回加载进来的）现算。
fn ensure_log_heights(app: &mut App, width: u16, compact: bool) {
    if app.log_heights_width != width {
        app.log_heights.clear();
        app.log_heights_width = width;
    }
    app.log_heights.resize(app.log_lines.len(), 0);
    for index in 0..app.log_lines.len() {
        if app.log_heights[index] != 0 {
            continue;
        }
        // 先算完高度再写缓存，别同时借 `app` 的两个字段。
        let height = {
            let line = log_line(&app.log_lines[index], compact);
            wrapped_height(&line, width)
        };
        app.log_heights[index] = height;
    }
}

/// 居中的矩形（宽高不够时贴边，不越界）。
fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    )
}

// ── 小工具 ─────────────────────────────────────────────────────────

/// 状态徽标与颜色。每一种退出方式各一条，测试逐个钉住。
fn badge(state: &State) -> (&'static str, Color) {
    match state {
        State::Running(Some(_)) => ("● 运行中", Color::Green),
        State::Running(None) => ("◐ 启动中", Color::Yellow),
        State::Stale(Some(info)) if info.stopped_gracefully() => ("○ 上次已停止", Color::DarkGray),
        State::Stale(Some(info)) if info.running == Some(true) => ("◌ 上次异常退出", Color::Yellow),
        State::Stale(Some(_)) => ("○ 上次已退出", Color::DarkGray),
        State::Stale(None) => ("○ 上次状态丢失", Color::DarkGray),
        State::Absent => ("○ 未运行", Color::DarkGray),
    }
}

fn state_detail(app: &App) -> String {
    match &app.state {
        State::Running(Some(info)) => format!(
            "pid {} · 已运行 {} · 版本 {}",
            info.pid,
            crate::instance::uptime_text(info.started, crate::instance::unix_now()),
            info.version
        ),
        State::Running(None) => "状态还没发布——等一两秒，起不来就按 3 看日志".to_owned(),
        State::Stale(Some(info)) if info.stopped_gracefully() => format!(
            "上次 pid {} · 版本 {} · 停止于 {}前",
            info.pid,
            info.version,
            crate::instance::uptime_text(
                info.stopped_at.unwrap_or(info.started),
                crate::instance::unix_now()
            )
        ),
        State::Stale(Some(info)) if info.running == Some(true) => format!(
            "上次 pid {} · 版本 {} · 没收到停止请求就退了",
            info.pid, info.version
        ),
        State::Stale(Some(info)) => {
            format!(
                "上次 pid {} · 版本 {}（退出方式未知）",
                info.pid, info.version
            )
        }
        State::Stale(None) => "上次没有留下可读状态".to_owned(),
        State::Absent => "这个目录还没跑过实例——按 s 启动".to_owned(),
    }
}

fn state_detail_compact(app: &App) -> String {
    match &app.state {
        State::Running(Some(info)) => format!("pid {} · 版本 {}", info.pid, info.version),
        State::Running(None) => "状态还没发布".to_owned(),
        State::Stale(Some(info)) if info.stopped_gracefully() => {
            format!("上次 pid {} · 已停止", info.pid)
        }
        State::Stale(Some(info)) if info.running == Some(true) => {
            format!("上次 pid {} · 异常退出", info.pid)
        }
        State::Stale(Some(info)) => format!("上次 pid {} · 版本 {}", info.pid, info.version),
        State::Stale(None) => "上次没有可读状态".to_owned(),
        State::Absent => "还没跑过".to_owned(),
    }
}

fn section(title: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled("▍", Style::new().fg(Color::Cyan)),
        Span::styled(
            title.to_owned(),
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
    ])
}

fn kv(label: &str, value: impl Into<String>) -> Line<'static> {
    Line::from(vec![kv_label(label), Span::raw(value.into())])
}

/// `kv` 的标签那一半（按**显示宽度**补到 10 列）：给需要单独给值上色的行复用。
fn kv_label(label: &str) -> Span<'static> {
    // 中文一个字两列，`{:<10}` 按字符数算会歪。
    let padding = 10usize.saturating_sub(Line::from(label).width());
    Span::styled(
        format!("  {label}{}", " ".repeat(padding)),
        Style::new().fg(Color::DarkGray),
    )
}

/// 概览页的「TDLib 连接」行：与顶栏同一套标签与颜色。
///
/// 没在跑 / 没接 TDLib / 还没收到更新时也照样占一行——「到底连上了没有」正是
/// 用户会来概览页找的答案，而顶栏那一格在窄屏上根本不画。
fn connection_line(app: &App, compact: bool) -> Line<'static> {
    let Some(info) = app.running() else {
        return Line::from(vec![
            kv_label("TDLib 连接"),
            Span::styled("— 实例没有在跑", Style::new().fg(Color::DarkGray)),
        ]);
    };
    let Some(state) = info
        .connection
        .as_deref()
        .and_then(ConnectionState::from_type_name)
    else {
        // `--no-tdlib` / 回放 / 刚起来还没收到第一条更新，都是这一种。
        return Line::from(vec![
            kv_label("TDLib 连接"),
            Span::styled(
                "— 还没收到连接更新（本次没接 TDLib？）",
                Style::new().fg(Color::DarkGray),
            ),
        ]);
    };
    let mut spans = vec![
        kv_label("TDLib 连接"),
        Span::styled(
            format!("● {}", crate::instance::connection_label(state)),
            Style::new()
                .fg(connection_color(state))
                .add_modifier(Modifier::BOLD),
        ),
    ];
    if !compact {
        // 原始状态名留着排查用：「面板说已连接、可请求还是发不出去」时要能对上 TDLib 的说法。
        spans.push(Span::styled(
            format!("（{}）", state.type_name()),
            Style::new().fg(Color::DarkGray),
        ));
    }
    Line::from(spans)
}

/// 概览页的「TDLib 授权」行：与连接状态并排——**连上网**和**登进去**是两件事。
///
/// 登录窗口里的 `Wait*` 一般看不到（那时实例还没发布 status），但运行期被下线、
/// 重登、登出时会实时反映出来。
fn authorization_line(app: &App, compact: bool) -> Line<'static> {
    let Some(info) = app.running() else {
        return Line::from(vec![
            kv_label("TDLib 授权"),
            Span::styled("— 实例没有在跑", Style::new().fg(Color::DarkGray)),
        ]);
    };
    let Some(raw) = info.authorization.as_deref() else {
        // `--no-tdlib` / 回放 / 刚起来还没收到第一条更新，都是这一种。
        return Line::from(vec![
            kv_label("TDLib 授权"),
            Span::styled(
                "— 还没收到授权更新（本次没接 TDLib？）",
                Style::new().fg(Color::DarkGray),
            ),
        ]);
    };
    let state = AuthState::from_type_name(raw);
    let mut spans = vec![
        kv_label("TDLib 授权"),
        Span::styled(
            format!("● {}", crate::instance::authorization_label(&state)),
            Style::new()
                .fg(authorization_color(&state))
                .add_modifier(Modifier::BOLD),
        ),
    ];
    if !compact {
        // 原始状态名对排查有用（「面板说已登录、可 TDLib 还在等验证码」时要能对上说法）。
        spans.push(Span::styled(
            format!("（{raw}）"),
            Style::new().fg(Color::DarkGray),
        ));
    }
    Line::from(spans)
}

/// 授权状态的颜色：绿=已登录，灰=正在退出 / 认不出，黄=还在等用户或 TDLib。
fn authorization_color(state: &AuthState) -> Color {
    match state {
        AuthState::Ready => Color::Green,
        AuthState::LoggingOut | AuthState::Closing | AuthState::Closed | AuthState::Unknown(_) => {
            Color::DarkGray
        }
        _ => Color::Yellow,
    }
}

/// 概览页的「账户」行：登录之后是**谁**（`getMe`，之后跟着 `updateUser` 刷新）。
///
/// 面板是另一个进程，只能从 status 文件读；没登录 / 没接 TDLib / 老实例都没有这一行，
/// 这时照样占一行写「—」，别让「不知道」看起来像「没有账户」。
fn account_line(app: &App, compact: bool) -> Line<'static> {
    let Some(info) = app.running() else {
        return Line::from(vec![
            kv_label("账户"),
            Span::styled("— 实例没有在跑", Style::new().fg(Color::DarkGray)),
        ]);
    };
    let Some(account) = info.account.as_ref() else {
        return Line::from(vec![
            kv_label("账户"),
            Span::styled(
                if compact {
                    "— 还没登录"
                } else {
                    "— 还没登录（登录成功后显示账户）"
                },
                Style::new().fg(Color::DarkGray),
            ),
        ]);
    };
    let mut spans = vec![
        kv_label("账户"),
        Span::styled(
            if account.name.is_empty() {
                format!("id={}", account.id)
            } else {
                account.name.clone()
            },
            Style::new().add_modifier(Modifier::BOLD),
        ),
    ];
    if let Some(username) = &account.username {
        spans.push(Span::styled(
            format!(" @{username}"),
            Style::new().fg(Color::Cyan),
        ));
    }
    // 窄屏只留「名字 @用户名」；宽屏补上 id 与 bot/user，排查时对得上 getMe。
    if !compact {
        spans.push(Span::styled(
            format!(
                "（id={}，{}）",
                account.id,
                if account.is_bot { "bot" } else { "user" }
            ),
            Style::new().fg(Color::DarkGray),
        ));
    }
    Line::from(spans)
}

fn key(keys: &str) -> Span<'static> {
    Span::styled(
        format!(" {keys} "),
        Style::new()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )
}

/// 把 `logging::LineFormat` 的三种日志行折成着色行；认不出的行原样显示（旧日志、
/// panic 输出、tdjson 自己打的东西都可能混进来）。
///
/// - 宿主 `时间 来源 LEVEL 内容`：来源灰、级别按级别上色。
/// - 插件 `时间 [plugins - 名字] LEVEL 内容`：词缀青、级别按级别上色。
/// - TDLib `时间 TDLib Update …`：词缀品红。
///
/// 时间列拆两段：日期灰、时间亮蓝（和 console 的两种颜色一致）。
fn log_line(line: &str, compact: bool) -> Line<'static> {
    let text = expand_tabs(line);
    let Some((time, level, source, message)) = split_log_line(&text) else {
        return Line::from(text);
    };

    let mut spans = time_spans(time, compact);

    if level == "TDLib Update" {
        let kind = logfmt::tdlib_update_type(&text).unwrap_or_else(|| "JSON".to_owned());
        spans.push(Span::styled(
            if compact { "TDLib" } else { "TDLib Update" },
            Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            kind,
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ));
        return Line::from(spans);
    }

    let level_style = match level {
        "ERROR" => Style::new().fg(Color::Red),
        "WARN" => Style::new().fg(Color::Yellow),
        "INFO" => Style::new().fg(Color::Green),
        "DEBUG" => Style::new().fg(Color::Cyan),
        _ => Style::new().fg(Color::DarkGray),
    };

    if matches!(logfmt::source_kind(&text), logfmt::SourceKind::Plugin) {
        let label = if compact {
            // 窄屏只留插件名的最后一段：`[plugins - fuyubot.manage]` → `[manage]`。
            let short = source.rsplit('.').next().unwrap_or(source);
            format!("[{short}]")
        } else {
            format!("[plugins - {source}]")
        };
        spans.push(Span::styled(label, Style::new().fg(Color::Cyan)));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(level.to_owned(), level_style));
        spans.push(Span::raw(format!("  {message}")));
    } else {
        spans.push(Span::styled(
            source.to_owned(),
            Style::new().fg(Color::DarkGray),
        ));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(level.to_owned(), level_style));
        spans.push(Span::raw(format!("  {message}")));
    }
    Line::from(spans)
}

/// 时间列：新格式 `日期 时间` 拆成灰 / 亮蓝两段；窄屏只留时间。
fn time_spans(time: &str, compact: bool) -> Vec<Span<'static>> {
    match time.split_once(' ') {
        Some((date, clock)) if !compact => vec![
            Span::styled(format!("{date} "), Style::new().fg(Color::DarkGray)),
            Span::styled(format!("{clock} "), Style::new().fg(Color::LightBlue)),
        ],
        Some((_, clock)) => vec![Span::styled(
            format!("{clock} "),
            Style::new().fg(Color::LightBlue),
        )],
        None => {
            let shown = if compact {
                time.split_once('.').map_or(time, |(clock, _)| clock)
            } else {
                time
            };
            vec![Span::styled(
                format!("{shown} "),
                Style::new().fg(Color::LightBlue),
            )]
        }
    }
}

/// 解析日志列。`None` = 不是这套格式。
fn split_log_line(line: &str) -> Option<(&str, &str, &str, &str)> {
    logfmt::split(line)
}

fn expand_tabs(line: &str) -> String {
    if line.contains('\t') {
        line.replace('\t', "    ")
    } else {
        line.to_owned()
    }
}

#[cfg(test)]
mod log_line_tests {
    use super::*;

    #[test]
    fn plugin_lines_split_into_time_level_source_message() {
        let line = "2026-10-31 07:05:13 [plugins - fuyubot.demo] INFO  hello  world";
        let (time, level, source, message) = split_log_line(line).expect("是插件行");
        assert_eq!(time, "2026-10-31 07:05:13");
        assert_eq!(level, "INFO");
        assert_eq!(source, "fuyubot.demo");
        assert_eq!(message, "hello  world");
    }

    #[test]
    fn host_and_tdlib_lines_keep_their_own_shapes() {
        let line = "2026-10-31 07:05:13 pipeline WARN  careful";
        let (time, level, source, message) = split_log_line(line).expect("是宿主行");
        assert_eq!(time, "2026-10-31 07:05:13");
        assert_eq!(level, "WARN");
        assert_eq!(source, "pipeline");
        assert_eq!(message, "careful");

        let line = "2026-10-31 07:05:13 TDLib Update {\"@type\":\"updateNewMessage\"}";
        let (_, level, source, message) = split_log_line(line).expect("是 TDLib 行");
        assert_eq!(level, "TDLib Update");
        assert_eq!(source, "update");
        assert_eq!(message, "{\"@type\":\"updateNewMessage\"}");
    }

    #[test]
    fn legacy_four_column_lines_still_split() {
        let line = "05:41:02.118  INFO   fuyubot.demo  hello  world";
        let (time, level, source, message) = split_log_line(line).expect("旧格式");
        assert_eq!(time, "05:41:02.118");
        assert_eq!(level, "INFO");
        assert_eq!(source, "fuyubot.demo");
        assert_eq!(message, "hello  world");
    }

    #[test]
    fn foreign_lines_are_left_alone() {
        assert!(split_log_line("hello from the log").is_none());
        assert!(split_log_line("").is_none());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instance::{Info, PluginDetail, State};
    use crate::panel::app::{App, Page, SpawnOptions};
    use crate::test_support::temp;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[test]
    fn cached_heights_match_the_wrapped_rows() {
        for width in [20u16, 40, 80, 120] {
            for line in [
                "2026-10-31 07:05:13 host INFO  hello".to_owned(),
                format!("2026-10-31 07:05:13 host INFO  {}", "中文abc".repeat(40)),
                "plain text without a timestamp".to_owned(),
                String::new(),
            ] {
                let rendered = log_line(&line, width < 60);
                let wrapped = wrap_log_line(rendered.clone(), width).len() as u32;
                assert_eq!(
                    wrapped_height(&rendered, width),
                    wrapped,
                    "width={width} line={line}"
                );
            }
        }
    }

    #[test]
    fn a_deep_log_window_renders_the_visible_slice() {
        let mut app = app();
        app.page = Page::Logs;
        app.focus = Focus::Workspace;
        app.log_lines = (0..20_000)
            .map(|index| format!("2026-10-31 07:05:13 host INFO  line {index}"))
            .collect();
        app.log_window_start = 1;

        let screen = text(&render(&mut app, 110, 24));
        assert!(screen.contains("line 19999"), "默认跟随末尾：\n{screen}");
        assert!(screen.contains("↑ 更早"), "{screen}");
        assert_eq!(app.log_rows, 20_000);
        assert_eq!(app.log_heights.len(), 20_000);

        // 往回翻：画的是这一窗口里的行，不用重新读文件。
        app.follow = false;
        app.log_scroll = 0;
        let screen = text(&render(&mut app, 110, 24));
        assert!(screen.contains("line 0"), "{screen}");
    }

    /// 每个测试一个目录：`test_support::temp` 会先删目录，同名并发跑会互删。
    fn app() -> App {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let name = format!("panel-ui-{}", NEXT.fetch_add(1, Ordering::Relaxed));
        let root = temp(&name);
        App::new(
            root.clone(),
            root.join("fuyubot.toml"),
            None,
            Duration::from_secs(1),
            SpawnOptions::default(),
        )
    }

    fn render(app: &mut App, width: u16, height: u16) -> ratatui::buffer::Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let result = terminal.draw(|frame| draw(frame, app));
        assert!(result.is_ok(), "绘制失败: {result:?}");
        terminal.backend().buffer().clone()
    }

    /// 把 Buffer 摊平成带换行的文本，方便断言「这一屏里有没有这段字」。
    ///
    /// 宽字符（中文）占两格：第二格是占位，按显示宽度跳过，否则「概览」会变成「概 览」。
    fn text(buffer: &ratatui::buffer::Buffer) -> String {
        let mut out = String::new();
        for y in 0..buffer.area.height {
            let mut x = 0;
            while x < buffer.area.width {
                let symbol = buffer[(x, y)].symbol();
                out.push_str(symbol);
                let width = Line::from(symbol).width().max(1) as u16;
                x += width;
            }
            out.push('\n');
        }
        out
    }

    fn running() -> State {
        State::Running(Some(Info {
            pid: 1234,
            started: crate::instance::unix_now(),
            version: "0.1.2".to_owned(),
            log_level: "info".to_owned(),
            plugins: 1,
            name: "demo".to_owned(),
            running: Some(true),
            stopped_at: None,
            connection: None,
            authorization: None,
            account: None,
            detail: PluginDetail {
                active: vec!["fuyubot.start".to_owned()],
                pending: Vec::new(),
                failed: Vec::new(),
            },
        }))
    }

    /// 与 [`running`] 同一份状态，只是带上实例报回来的 TDLib 连接状态。
    fn running_with(connection: Option<&str>) -> State {
        running_with_states(connection, None)
    }

    /// 连接状态与授权状态都可以单独指定（两者本来就没有先后关系）。
    fn running_with_states(connection: Option<&str>, authorization: Option<&str>) -> State {
        let State::Running(Some(mut info)) = running() else {
            unreachable!("running() 永远是 Running(Some(..))")
        };
        info.connection = connection.map(str::to_owned);
        info.authorization = authorization.map(str::to_owned);
        State::Running(Some(info))
    }

    /// 顶部状态区要显示实例报回来的 TDLib 连接状态：宽屏带 `TDLib` 前缀，
    /// 窄屏退成短标签——但都不能把版本号那一侧挤没。
    #[test]
    fn header_shows_the_tdlib_connection_state() {
        let mut app = app();
        app.state = running_with(Some("connectionStateReady"));

        let screen = text(&render(&mut app, 110, 24));
        assert!(screen.contains("TDLib 已连接"), "{screen}");

        // 70 列只留得下短标签；状态徽标与名字仍在，字不能被截在屏幕外。
        let screen = text(&render(&mut app, 70, 24));
        assert!(screen.contains("已连接"), "{screen}");
        assert!(!screen.contains("TDLib 已连接"), "{screen}");
        assert!(screen.contains("● 运行中"), "{screen}");
    }

    /// 五种连接状态各说各的；上次退出留下的历史值不该出现在顶部。
    #[test]
    fn header_connection_follows_the_state_and_ignores_stale_ones() {
        let mut app = app();
        for (name, label) in [
            ("connectionStateWaitingForNetwork", "等待网络"),
            ("connectionStateConnectingToProxy", "连代理中"),
            ("connectionStateConnecting", "连接中"),
            ("connectionStateUpdating", "同步中"),
            ("connectionStateReady", "已连接"),
        ] {
            app.state = running_with(Some(name));
            let screen = text(&render(&mut app, 110, 24));
            assert!(screen.contains(label), "{name} 应显示 {label}：\n{screen}");
        }

        // 不是运行中：连接状态是上次的历史值，顶部不显示。
        let State::Running(Some(info)) = running_with(Some("connectionStateReady")) else {
            unreachable!()
        };
        app.state = State::Stale(Some(info));
        let screen = text(&render(&mut app, 110, 24));
        assert!(!screen.contains("已连接"), "{screen}");
    }

    #[test]
    fn header_footer_and_three_regions_are_all_present() {
        let mut app = app();
        let buffer = render(&mut app, 100, 30);
        let screen = text(&buffer);

        // 顶部版本栏占第 0 行；导航 / 工作区从第 1 行开始。
        assert!(
            screen.lines().next().is_some_and(
                |line| line.starts_with(&format!("[fuyubot - v{}]", env!("CARGO_PKG_VERSION")))
            ),
            "顶部要是 [fuyubot - v…]：\n{screen}"
        );
        assert!(screen.contains("终端操作面板"), "{screen}");
        assert_eq!(buffer[(NAV_WIDTH - 1, 1)].symbol(), "┐");
        assert_eq!(buffer[(NAV_WIDTH, 1)].symbol(), "┌");

        for expected in ["概览", "插件", "日志", "配置", "帮助"] {
            assert!(screen.contains(expected), "缺少 {expected:?}：\n{screen}");
        }

        let lines: Vec<&str> = screen.lines().collect();
        let operations = lines[lines.len() - 2];
        let metrics = lines[lines.len() - 1];
        // 底部第一行：当前可用操作（默认焦点在导航）。
        assert!(operations.contains("[导航]"), "{operations:?}");
        assert!(operations.contains("Tab 聚焦"), "{operations:?}");
        assert!(operations.contains("s 启动"), "{operations:?}");
        assert!(operations.contains("q 退出"), "{operations:?}");
        // 底部第二行：资源占用。
        for label in ["CPU", "内存", "运行", "插件", "日志", "PID"] {
            assert!(metrics.contains(label), "缺少 {label}：{metrics:?}");
        }
    }

    #[test]
    fn focus_highlights_the_active_pane() {
        let mut app = app();
        let buffer = render(&mut app, 100, 24);
        // 默认焦点导航：导航左边框青色，工作区暗色。
        assert_eq!(buffer[(0, 1)].fg, Color::Cyan);
        assert_eq!(buffer[(NAV_WIDTH, 1)].fg, Color::DarkGray);

        app.focus = Focus::Workspace;
        let buffer = render(&mut app, 100, 24);
        assert_eq!(buffer[(0, 1)].fg, Color::DarkGray);
        assert_eq!(buffer[(NAV_WIDTH, 1)].fg, Color::Cyan);
    }

    #[test]
    fn header_and_footer_have_no_background() {
        let mut app = app();
        let buffer = render(&mut app, 100, 24);
        for y in [0u16, 22, 23] {
            for x in 0..100u16 {
                assert_eq!(buffer[(x, y)].bg, Color::Reset, "({x},{y}) 不该有背景色");
            }
        }
    }

    /// 实例停了之后，概览与顶栏都要留下「上次退出」的状态，而且正常 / 异常要分得开。
    #[test]
    fn stopped_state_shows_the_last_exit() {
        // 没跑过：明确写「未运行」，而不是只剩一个刷新时间。
        let mut app = app();
        let screen = text(&render(&mut app, 100, 24));
        assert!(screen.contains("未运行"), "{screen}");
        assert!(screen.contains("还没跑过实例"), "{screen}");
        assert!(screen.contains("位置"), "{screen}");
        assert!(screen.contains("s 启动"), "{screen}");

        // 优雅停止：顶栏 + 概览都要写「上次已停止」，并带停止时间与 pid。
        let stopped_at = crate::instance::unix_now().saturating_sub(90);
        app.state = State::Stale(Some(Info {
            pid: 29888,
            started: stopped_at.saturating_sub(600),
            version: "0.1.2".to_owned(),
            log_level: "info".to_owned(),
            plugins: 0,
            name: "fuyubot".to_owned(),
            running: Some(false),
            stopped_at: Some(stopped_at),
            connection: None,
            authorization: None,
            account: None,
            detail: PluginDetail::default(),
        }));
        let screen = text(&render(&mut app, 100, 24));
        assert!(screen.contains("上次已停止"), "{screen}");
        assert!(screen.contains("停止于"), "{screen}");
        assert!(screen.contains("29888"), "{screen}");

        // 崩溃 / 硬杀：不能冒充正常停止。
        app.state = State::Stale(Some(Info {
            pid: 29888,
            started: crate::instance::unix_now().saturating_sub(600),
            version: "0.1.2".to_owned(),
            log_level: "info".to_owned(),
            plugins: 0,
            name: "fuyubot".to_owned(),
            running: Some(true),
            stopped_at: None,
            connection: None,
            authorization: None,
            account: None,
            detail: PluginDetail::default(),
        }));
        let screen = text(&render(&mut app, 100, 24));
        assert!(screen.contains("上次异常退出"), "{screen}");
        assert!(!screen.contains("上次已停止"), "{screen}");

        // 老版本写的 status 没有 `running` 行：只能说「退出方式未知」，不能诬赖成异常。
        app.state = State::Stale(Some(Info {
            pid: 29888,
            started: crate::instance::unix_now().saturating_sub(600),
            version: "0.1.2".to_owned(),
            log_level: "info".to_owned(),
            plugins: 0,
            name: "fuyubot".to_owned(),
            running: None,
            stopped_at: None,
            connection: None,
            authorization: None,
            account: None,
            detail: PluginDetail::default(),
        }));
        let screen = text(&render(&mut app, 100, 24));
        assert!(screen.contains("上次已退出"), "{screen}");
        assert!(screen.contains("退出方式未知"), "{screen}");
        assert!(!screen.contains("上次异常退出"), "{screen}");
    }

    #[test]
    fn header_and_metrics_report_the_running_instance() {
        let mut app = app();
        app.state = running();
        app.focus = Focus::Workspace;
        let buffer = render(&mut app, 110, 24);
        let screen = text(&buffer);
        assert!(screen.contains("● 运行中"), "{screen}");
        assert!(screen.contains("1234"), "{screen}");
        assert!(screen.contains("PID 1234"), "{screen}");
        assert!(screen.contains("已刷新"), "{screen}");
        assert!(screen.contains("[工作区]"), "{screen}");

        // 导航焦点下，跑着时底部只提示 x 停止（不再提示 s 启动）。
        app.focus = Focus::Navigation;
        let screen = text(&render(&mut app, 110, 24));
        assert!(screen.contains("x 停止"), "{screen}");
        assert!(!screen.contains("s 启动"), "{screen}");
    }

    #[test]
    fn metrics_use_placeholders_when_nothing_runs() {
        let mut app = app();
        let screen = text(&render(&mut app, 120, 24));
        assert!(screen.contains("CPU —"), "{screen}");
        assert!(screen.contains("PID —"), "{screen}");
        assert!(screen.contains("日志 info"), "{screen}");
    }

    #[test]
    fn metrics_render_cpu_and_memory() {
        let mut app = app();
        app.usage = Some(ProcessUsage {
            cpu_percent: 12.3,
            memory_bytes: 64 * 1024 * 1024,
        });
        let screen = text(&render(&mut app, 120, 24));
        assert!(screen.contains("CPU 12.3%"), "{screen}");
        assert!(screen.contains("内存 64.0 MB"), "{screen}");
    }

    #[test]
    fn switching_pages_changes_the_workspace_title_and_body() {
        let mut app = app();
        app.log_lines = vec!["hello from the log".to_owned()];
        app.page = Page::Logs;
        let screen = text(&render(&mut app, 100, 24));
        assert!(screen.contains("hello from the log"), "{screen}");
        assert!(screen.contains("跟随末尾"), "{screen}");

        app.page = Page::Config;
        app.config_items = vec![ConfigItem {
            section: "bot".to_owned(),
            key: "name".to_owned(),
            value: "demo".to_owned(),
            display: "demo".to_owned(),
            note: "默认".to_owned(),
            kind: ConfigKind::Text { allow_empty: true },
        }];
        app.config_selected = 0;
        let screen = text(&render(&mut app, 100, 24));
        assert!(screen.contains("name"), "{screen}");
        assert!(screen.contains("demo"), "{screen}");
        assert!(screen.contains("生效值"), "{screen}");
    }

    #[test]
    fn config_editor_modal_renders_the_selected_item() {
        let mut app = app();
        app.page = Page::Config;
        app.focus = Focus::Workspace;
        app.config_items = vec![ConfigItem {
            section: "telegram".to_owned(),
            key: "api_id".to_owned(),
            value: "12345".to_owned(),
            display: "12345".to_owned(),
            note: "文件".to_owned(),
            kind: ConfigKind::Integer {
                min: 0,
                max: i64::MAX,
            },
        }];
        app.config_selected = 0;
        app.config_edit = Some(ConfigEdit {
            section: "telegram".to_owned(),
            key: "api_id".to_owned(),
            label: "[telegram].api_id".to_owned(),
            current: "12345".to_owned(),
            note: "文件".to_owned(),
            kind: ConfigKind::Integer {
                min: 0,
                max: i64::MAX,
            },
            input: "777".to_owned(),
            cursor: 3,
            error: None,
        });
        let screen = text(&render(&mut app, 110, 30));
        assert!(screen.contains("编辑配置"), "{screen}");
        assert!(screen.contains("[telegram].api_id"), "{screen}");
        assert!(screen.contains("新值"), "{screen}");
        assert!(screen.contains("777"), "{screen}");
        assert!(screen.contains("Enter 保存"), "{screen}");
    }

    #[test]
    fn config_table_shows_selection_and_columns() {
        let mut app = app();
        app.page = Page::Config;
        app.focus = Focus::Workspace;
        app.config_items = vec![
            ConfigItem {
                section: "telegram".to_owned(),
                key: "api_id".to_owned(),
                value: "12345".to_owned(),
                display: "12345".to_owned(),
                note: "文件".to_owned(),
                kind: ConfigKind::Integer {
                    min: 0,
                    max: i64::MAX,
                },
            },
            ConfigItem {
                section: "plugins".to_owned(),
                key: "log_level".to_owned(),
                value: "info".to_owned(),
                display: "info".to_owned(),
                note: "默认".to_owned(),
                kind: ConfigKind::Choice(&["error", "warn", "info", "debug", "trace"]),
            },
        ];
        app.config_selected = 1;
        let screen = text(&render(&mut app, 110, 30));
        for expected in ["段", "键", "生效值", "来源", "api_id", "12345", "log_level"] {
            assert!(screen.contains(expected), "缺少 {expected}：\n{screen}");
        }
        assert!(screen.contains("Enter 编辑"), "{screen}");
        assert!(screen.contains("[plugins].log_level"), "{screen}");
    }

    #[test]
    fn tdlib_update_shows_only_the_type() {
        let mut app = app();
        app.page = Page::Logs;
        app.focus = Focus::Workspace;
        app.log_lines = vec![
            "2026-10-31 07:05:13 TDLib Update {\"@type\":\"updateNewMessage\",\"message\":{\"id\":1}}"
                .to_owned(),
        ];
        let screen = text(&render(&mut app, 110, 24));
        assert!(screen.contains("TDLib Update"), "{screen}");
        assert!(screen.contains("updateNewMessage"), "{screen}");
        assert!(!screen.contains("@type"), "不能把 JSON 铺出来：{screen}");
        assert!(!screen.contains("\"message\""), "{screen}");
    }

    #[test]
    fn selecting_a_tdlib_update_shows_the_copy_hint() {
        let mut app = app();
        app.page = Page::Logs;
        app.focus = Focus::Workspace;
        app.log_lines = vec![
            "2026-10-31 07:05:13 TDLib Update {\"@type\":\"updateNewMessage\",\"message\":{\"id\":1}}"
                .to_owned(),
        ];
        app.log_select = Some(0);
        let screen = text(&render(&mut app, 110, 24));
        assert!(screen.contains("[选择]"), "{screen}");
        assert!(screen.contains("已选 updateNewMessage"), "{screen}");
        assert!(screen.contains("复制完整 JSON"), "{screen}");
    }

    #[test]
    fn long_log_lines_wrap_instead_of_being_clipped() {
        let mut app = app();
        app.page = Page::Logs;
        app.focus = Focus::Workspace;
        app.log_lines = vec![format!(
            "2026-10-31 07:05:13 host INFO  {}",
            "x".repeat(200)
        )];
        let screen = text(&render(&mut app, 80, 24));
        assert!(
            screen.matches('x').count() >= 200,
            "折行后 200 个字符都应在屏幕上：\n{screen}"
        );
    }

    #[test]
    fn narrow_logs_drop_the_date_and_shorten_plugin_names() {
        let mut app = app();
        app.page = Page::Logs;
        app.focus = Focus::Workspace;
        app.log_lines =
            vec!["2026-10-31 07:05:13 [plugins - fuyubot.manage] INFO  hello".to_owned()];
        let screen = text(&render(&mut app, 40, 24));
        assert!(screen.contains("07:05:13"), "{screen}");
        assert!(screen.contains("[manage]"), "{screen}");
        assert!(!screen.contains("2026-10-31"), "窄屏不显示日期：\n{screen}");
        assert!(
            !screen.contains("[plugins -"),
            "窄屏不显示长前缀：\n{screen}"
        );
    }

    #[test]
    fn log_fields_are_not_padded_to_fixed_columns() {
        let mut app = app();
        app.page = Page::Logs;
        app.focus = Focus::Workspace;
        app.log_lines = vec!["2026-10-31 07:05:13 host INFO  hello".to_owned()];
        let screen = text(&render(&mut app, 100, 24));
        assert!(screen.contains("host INFO  hello"), "{screen}");
    }

    #[test]
    fn logs_page_shows_the_current_level_and_how_to_change_it() {
        let mut app = app();
        app.page = Page::Logs;
        app.focus = Focus::Workspace;
        app.state = running();
        if let State::Running(Some(info)) = &mut app.state {
            info.log_level = "debug".to_owned();
        }
        let screen = text(&render(&mut app, 100, 24));
        assert!(screen.contains("日志级别"), "{screen}");
        assert!(screen.contains("（l）"), "{screen}");
        assert!(screen.contains("过滤"), "{screen}");
        assert!(screen.contains("debug"), "{screen}");
    }

    #[test]
    fn filtered_logs_and_filter_modal_render() {
        let mut app = app();
        app.page = Page::Logs;
        app.focus = Focus::Workspace;
        app.log_lines = vec![
            "05:41:02.118  INFO   host          started".to_owned(),
            "05:41:03.000  DEBUG  fuyubot.demo  debug only".to_owned(),
            "[ 2][t 1][client.cpp:1] tdlib warn".to_owned(),
        ];
        app.log_filter = logfmt::LogFilter {
            level: logfmt::LevelFilter::Debug,
            source: logfmt::SourceFilter::Plugin,
        };

        let screen = text(&render(&mut app, 110, 24));
        assert!(screen.contains("debug only"), "{screen}");
        assert!(!screen.contains("host          started"), "{screen}");
        assert!(!screen.contains("tdlib warn"), "{screen}");
        assert!(!screen.contains("1/3 行"), "底栏不再显示行数：\n{screen}");

        app.filter_picker = Some(crate::panel::app::FilterDraft::new(app.log_filter));
        let screen = text(&render(&mut app, 110, 30));
        assert!(screen.contains("日志过滤"), "{screen}");
        assert!(screen.contains("过滤只影响显示，不改日志文件"), "{screen}");
        assert!(!screen.contains("将显示"), "弹框里不报行数：\n{screen}");
        assert!(screen.contains("来源"), "{screen}");
        assert!(screen.contains("插件"), "{screen}");
    }

    #[test]
    fn log_counts_are_not_shown_anywhere() {
        let mut app = app();
        app.log_lines = (0..42).map(|index| format!("line {index}")).collect();
        app.page = Page::Logs;
        app.focus = Focus::Workspace;
        let screen = text(&render(&mut app, 110, 24));
        assert!(!screen.contains("日志 42"), "导航不带行数：\n{screen}");
        assert!(!screen.contains("42 行"), "底栏不带行数：\n{screen}");

        // 概览页也不再写「最近多少行可看」。
        app.page = Page::Overview;
        app.focus = Focus::Navigation;
        let screen = text(&render(&mut app, 120, 30));
        assert!(!screen.contains("42 行"), "{screen}");
        assert!(!screen.contains("行可看"), "{screen}");
    }

    #[test]
    fn logs_footer_hints_that_earlier_lines_exist() {
        let mut app = app();
        app.page = Page::Logs;
        app.focus = Focus::Workspace;
        app.log_lines = vec!["hello".to_owned()];
        let screen = text(&render(&mut app, 110, 24));
        assert!(!screen.contains("↑ 更早"), "窗口已经在文件开头：\n{screen}");

        app.log_window_start = 128;
        let screen = text(&render(&mut app, 110, 24));
        assert!(screen.contains("↑ 更早"), "{screen}");
    }

    #[test]
    fn log_level_picker_lists_all_five_levels() {
        let mut app = app();
        app.page = Page::Logs;
        app.level_picker = Some(2);
        let screen = text(&render(&mut app, 110, 30));
        for level in ["error", "warn", "info", "debug", "trace"] {
            assert!(screen.contains(level), "缺少 {level}：\n{screen}");
        }
        // 实例没跑时选的是「下次启动」的级别，提示必须说清楚。
        assert!(screen.contains("下次启动生效"), "{screen}");
    }

    #[test]
    fn wide_characters_are_not_dropped_from_the_overview() {
        let mut app = app();
        app.state = State::Stale(Some(Info {
            pid: 29888,
            started: crate::instance::unix_now().saturating_sub(470),
            version: "0.1.2".to_owned(),
            log_level: "info".to_owned(),
            plugins: 0,
            name: "fuyubot".to_owned(),
            running: Some(true),
            stopped_at: None,
            connection: None,
            authorization: None,
            account: None,
            detail: PluginDetail::default(),
        }));
        let screen = text(&render(&mut app, 100, 24));
        assert!(screen.contains("上次异常退出"), "{screen}");
        assert!(screen.contains("版本"), "{screen}");
    }

    /// 真终端的输出层：`CrosstermBackend` 写的字节里也得有中文。
    ///
    /// 这条是为了把「缓冲里对」和「发出去的字节对」分开——winpty 之类的中转会自己
    /// 处理宽字符，那里丢字不能怪面板。
    #[test]
    fn crossterm_backend_writes_wide_characters() {
        use ratatui::backend::CrosstermBackend;
        use std::io::Write as _;
        use std::rc::Rc;

        #[derive(Clone, Default)]
        struct Sink(Rc<std::cell::RefCell<Vec<u8>>>);
        impl std::io::Write for Sink {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.borrow_mut().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let mut app = app();
        app.state = State::Stale(Some(Info {
            pid: 29888,
            started: crate::instance::unix_now().saturating_sub(470),
            version: "0.1.2".to_owned(),
            log_level: "info".to_owned(),
            plugins: 0,
            name: "fuyubot".to_owned(),
            running: Some(true),
            stopped_at: None,
            connection: None,
            authorization: None,
            account: None,
            detail: PluginDetail::default(),
        }));

        let sink = Sink::default();
        let mut terminal = Terminal::new(CrosstermBackend::new(sink.clone())).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        terminal.backend_mut().flush().unwrap();
        let bytes = sink.0.borrow().clone();
        let text = strip_ansi(&String::from_utf8_lossy(&bytes));
        assert!(
            text.contains("上次异常退出"),
            "输出里丢了「上次异常退出」：{text:?}"
        );
        assert!(text.contains("版本"), "输出里丢了「版本」：{text:?}");
        assert!(text.contains("导航"), "状态栏快捷键没发出去：{text:?}");
    }

    /// 去掉 CSI 控制序列（`ESC [ … 终字符`）。
    ///
    /// 光标定位会把相邻汉字拆到两段输出里（`已` `ESC[3;44H` `停`），断言前必须拼回来。
    fn strip_ansi(text: &str) -> String {
        let mut out = String::new();
        let mut chars = text.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch != '\x1b' {
                out.push(ch);
                continue;
            }
            // CSI：ESC [ 参数 / 中间字节 终字符（0x40..=0x7e）
            if chars.peek() == Some(&'[') {
                chars.next();
                for next in chars.by_ref() {
                    if ('@'..='~').contains(&next) {
                        break;
                    }
                }
            }
        }
        out
    }

    #[test]
    fn plugins_page_lists_reported_and_on_disk_plugins() {
        let mut app = app();
        app.state = running();
        app.disk_plugins = vec![crate::panel::app::DiskPlugin {
            name: "fuyubot_plugin_start".to_owned(),
            kind: PluginKind::Native,
            path: app.plugin_dir.join("fuyubot_plugin_start.dll"),
        }];
        app.page = Page::Plugins;
        let screen = text(&render(&mut app, 120, 30));
        assert!(screen.contains("fuyubot.start"), "{screen}");
        assert!(screen.contains("fuyubot_plugin_start"), "{screen}");
        assert!(screen.contains("dylib"), "{screen}");
    }

    #[test]
    fn narrow_plugins_page_drops_wide_columns() {
        let mut app = app();
        app.state = running();
        app.disk_plugins = vec![crate::panel::app::DiskPlugin {
            name: "fuyubot_plugin_start".to_owned(),
            kind: PluginKind::Native,
            path: app.plugin_dir.join("fuyubot_plugin_start.dll"),
        }];
        app.page = Page::Plugins;
        let screen = text(&render(&mut app, 40, 24));
        assert!(screen.contains("插件 / 状态"), "{screen}");
        assert!(screen.contains("fuyubot.start"), "{screen}");
        assert!(screen.contains("fuyubot_plugin_start"), "{screen}");
        assert!(!screen.contains("文件"), "窄屏不显示文件列：\n{screen}");
        assert!(!screen.contains(".dll"), "窄屏不显示文件名：\n{screen}");
    }

    #[test]
    fn stop_confirmation_is_a_modal() {
        let mut app = app();
        app.state = running();
        app.confirm_stop = true;
        let screen = text(&render(&mut app, 100, 30));
        assert!(screen.contains("确认停止当前实例"), "{screen}");
        assert!(screen.contains("取消"), "{screen}");
    }

    #[test]
    fn clear_logs_confirmation_is_a_modal() {
        let mut app = app();
        app.page = Page::Logs;
        app.focus = Focus::Workspace;
        app.confirm_clear_logs = true;
        let screen = text(&render(&mut app, 100, 30));
        assert!(screen.contains("清空日志"), "{screen}");
        assert!(screen.contains("清空当前日志文件"), "{screen}");
        assert!(screen.contains("取消"), "{screen}");
    }

    #[test]
    fn narrow_terminal_moves_navigation_to_the_top() {
        let mut app = app();
        let buffer = render(&mut app, 40, 24);
        let screen = text(&buffer);
        // 40 列：导航变成工作区上方的一行标签。
        for expected in [
            "概览", "插件", "日志", "配置", "帮助", "CPU", "内存", "info",
        ] {
            assert!(screen.contains(expected), "缺少 {expected}：\n{screen}");
        }
        // 运行时长 / PID / 插件数量在窄屏不显示；资源行只留 CPU / 内存 / 级别。
        let metrics = screen.lines().last().unwrap_or_default();
        assert!(!metrics.contains("PID"), "窄屏不该显示 PID：\n{screen}");
        assert!(
            !metrics.contains("运行"),
            "窄屏不该显示运行时长：\n{screen}"
        );
        assert!(
            !metrics.contains("插件"),
            "窄屏不该显示插件数量：\n{screen}"
        );
        assert!(metrics.contains("info"), "级别只留 info：\n{screen}");
        assert!(!screen.contains("日志 info"), "级别只留 info：\n{screen}");
        assert!(
            screen.contains("fuyubot.toml"),
            "窄屏路径应是相对的：\n{screen}"
        );
        assert!(!screen.contains("[工作区]"), "窄栏用短标记：\n{screen}");
    }

    #[test]
    fn ultra_narrow_terminal_uses_single_char_top_tabs() {
        let mut app = app();
        app.page = Page::Config;
        app.focus = Focus::Workspace;
        app.config_items = vec![ConfigItem {
            section: "telegram".to_owned(),
            key: "api_id".to_owned(),
            value: "12345".to_owned(),
            display: "12345".to_owned(),
            note: "文件".to_owned(),
            kind: ConfigKind::Integer {
                min: 0,
                max: i32::MAX as i64,
            },
        }];
        let screen = text(&render(&mut app, 30, 20));
        assert!(screen.contains("概"), "顶部应有单字标签：\n{screen}");
        assert!(screen.contains("配置"), "{screen}");
        assert!(screen.contains("[telegram].api_id"), "{screen}");
        assert!(screen.contains("12345"), "{screen}");
    }

    #[test]
    fn tiny_terminals_do_not_panic() {
        let mut app = app();
        for (width, height) in [(30u16, 10u16), (20, 5), (10, 4)] {
            let _ = render(&mut app, width, height);
        }
    }

    #[test]
    fn overview_reports_paths_and_actions() {
        let mut app = app();
        let screen = text(&render(&mut app, 120, 30));
        assert!(screen.contains("项目根"), "{screen}");
        assert!(screen.contains("配置文件"), "{screen}");
        assert!(screen.contains("启动"), "{screen}");
    }

    /// 概览页也要回答「现在连上了吗 / 登录到哪一步」：顶部那一格在窄屏上根本不画。
    #[test]
    fn overview_shows_the_tdlib_connection_and_authorization_states() {
        let mut app = app();
        let screen = text(&render(&mut app, 120, 30));
        assert!(screen.contains("TDLib 连接"), "{screen}");
        assert!(screen.contains("TDLib 授权"), "{screen}");
        assert!(screen.contains("实例没有在跑"), "{screen}");

        // 运行中但没收到更新（--no-tdlib / 刚起来）：说「还没收到」，不编一个状态。
        app.state = running_with(None);
        let screen = text(&render(&mut app, 120, 30));
        assert!(screen.contains("还没收到连接更新"), "{screen}");
        assert!(screen.contains("还没收到授权更新"), "{screen}");

        // 宽屏带 TDLib 的原始状态名；窄屏只留人话标签。
        app.state = running_with_states(
            Some("connectionStateReady"),
            Some("authorizationStateReady"),
        );
        let screen = text(&render(&mut app, 120, 30));
        assert!(
            screen.contains("● 已连接（connectionStateReady）"),
            "{screen}"
        );
        assert!(
            screen.contains("● 已登录（authorizationStateReady）"),
            "{screen}"
        );

        let screen = text(&render(&mut app, 70, 30));
        assert!(screen.contains("已连接"), "{screen}");
        assert!(screen.contains("已登录"), "{screen}");
        assert!(!screen.contains("connectionStateReady"), "{screen}");
        assert!(!screen.contains("authorizationStateReady"), "{screen}");
    }

    /// 授权状态逐种给对人话：等待凭据是黄的「等待…」，登出/关闭是灰的，
    /// 认不出的状态不是「已登录」。
    #[test]
    fn overview_authorization_labels_follow_the_state() {
        let mut app = app();
        for (name, label) in [
            ("authorizationStateWaitTdlibParameters", "等待参数"),
            ("authorizationStateWaitPhoneNumber", "等待手机号"),
            ("authorizationStateWaitCode", "等待验证码"),
            ("authorizationStateWaitPassword", "等待两步密码"),
            (
                "authorizationStateWaitOtherDeviceConfirmation",
                "等待扫码确认",
            ),
            ("authorizationStateReady", "已登录"),
            ("authorizationStateLoggingOut", "正在登出"),
            ("authorizationStateClosing", "正在关闭"),
            ("authorizationStateClosed", "已关闭"),
        ] {
            app.state = running_with_states(None, Some(name));
            let screen = text(&render(&mut app, 120, 30));
            assert!(screen.contains(label), "{name} 应显示 {label}：\n{screen}");
        }

        // 认不出的状态：说「未知」，并把原始名字带上，别冒充已登录。
        app.state = running_with_states(None, Some("authorizationStateSomethingNew"));
        let screen = text(&render(&mut app, 120, 30));
        assert!(
            screen.contains("未知状态（authorizationStateSomethingNew）"),
            "{screen}"
        );
        assert!(!screen.contains("已登录"), "{screen}");
    }

    /// 概览页的「账户」行：没登录说没登录；登录后是名字 + @用户名，宽屏再补 id / bot。
    #[test]
    fn overview_shows_the_logged_in_account() {
        let mut app = app();
        let screen = text(&render(&mut app, 120, 30));
        assert!(screen.contains("账户"), "{screen}");
        assert!(screen.contains("实例没有在跑"), "{screen}");

        // 实例在跑但还没登录（`--no-tdlib` / 登录窗口里）：说「还没登录」，不编一个账户。
        app.state = running_with(None);
        let screen = text(&render(&mut app, 120, 30));
        assert!(screen.contains("还没登录"), "{screen}");

        app.state = running_with_states(
            Some("connectionStateReady"),
            Some("authorizationStateReady"),
        );
        if let State::Running(Some(info)) = &mut app.state {
            info.account = Some(crate::instance::Account {
                id: 7,
                name: "Fuyu Bot".to_owned(),
                username: Some("FuyuBot".to_owned()),
                is_bot: true,
            });
        }
        let screen = text(&render(&mut app, 120, 30));
        assert!(screen.contains("Fuyu Bot"), "{screen}");
        assert!(screen.contains("@FuyuBot"), "{screen}");
        assert!(screen.contains("（id=7，bot）"), "{screen}");

        // 窄屏只留名字 + @用户名：id / 类型是排查用的，挤掉正事不划算。
        let screen = text(&render(&mut app, 70, 30));
        assert!(screen.contains("@FuyuBot"), "{screen}");
        assert!(!screen.contains("id=7"), "{screen}");
    }
}
