//! Rendering for the stacked management dashboard: title bar, Indexes, Service,
//! Activity, a context key bar and the status line, plus the modals on top.

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};

use ygrep_core::dashboard::WatchState;
use ygrep_core::registry::{format_relative_time, format_size};

use super::{App, IndexRow, Panel, ACCENT, SERVICE_ACTIONS};
use crate::service::ServiceStatus;

pub fn render(f: &mut Frame, app: &App) {
    // The stats view replaces the whole dashboard while it is open.
    if app.stats.is_some() {
        super::stats::render(f, app);
        return;
    }

    let area = f.area();
    // Elastic split: the fixed rows come off the top and bottom first, the service panel
    // takes its four lines, the index list sizes to its content, and the activity feed
    // gets everything left over — so a short list doesn't leave the screen half empty.
    let rest = area.height.saturating_sub(3 + 4);
    let indexes_h = (app.view.len() as u16 + 2).clamp(5, rest.saturating_sub(5).max(5));

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),         // title
            Constraint::Length(indexes_h), // indexes
            Constraint::Length(4),         // service
            Constraint::Min(3),            // activity
            Constraint::Length(1),         // key bar
            Constraint::Length(1),         // status
        ])
        .split(area);

    render_title(f, app, chunks[0]);
    render_indexes(f, app, chunks[1]);
    render_service(f, app, chunks[2]);
    render_activity(f, app, chunks[3]);
    render_keys(f, app, chunks[4]);
    render_status(f, app, chunks[5]);

    if app.help {
        render_help(f, area);
    }
    if let Some(sel) = app.service_menu {
        render_service_menu(f, area, sel);
    }
    if let Some((_, label)) = &app.confirm_remove {
        render_confirm(f, area, label);
    }
}

fn panel_block(title: &str, focused: bool) -> Block<'_> {
    let color = if focused { ACCENT } else { Color::DarkGray };
    let title_style = if focused {
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(color))
        .title(Span::styled(format!(" {title} "), title_style))
}

fn row_style(selected: bool, focused: bool) -> Style {
    if selected && focused {
        Style::default().add_modifier(Modifier::REVERSED)
    } else if selected {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    }
}

/// First visible row for a list of `len` rows in a `view_h`-tall panel.
fn window_offset(sel: usize, len: usize, view_h: usize) -> usize {
    let view_h = view_h.max(1);
    if len <= view_h {
        return 0;
    }
    sel.saturating_sub(view_h / 2).min(len - view_h)
}

/// The status dot for one index row.
fn status_dot(app: &App, row: &IndexRow) -> Span<'static> {
    if row.orphaned {
        return Span::styled("✗", Style::default().fg(Color::Red));
    }
    if app.watched_by_service(&row.hash) {
        return Span::styled("●", Style::default().fg(Color::Green));
    }
    match row.state {
        WatchState::Active => Span::styled("●", Style::default().fg(Color::Green)),
        WatchState::Sleeping => Span::styled("◐", Style::default().fg(Color::Yellow)),
        WatchState::Off => Span::styled("○", Style::default().fg(Color::DarkGray)),
    }
}

/// Clamp a string to `max` display columns, keeping the tail of a long path.
fn clamp_path(text: &str, max: usize) -> String {
    let len = text.chars().count();
    if len <= max {
        return text.to_string();
    }
    let tail: String = text
        .chars()
        .skip(len - max.saturating_sub(1))
        .collect::<String>();
    format!("…{tail}")
}

fn render_title(f: &mut Frame, app: &App, area: Rect) {
    let total: u64 = app.rows.iter().map(|row| row.size_bytes).sum();
    let watching = app
        .rows
        .iter()
        .filter(|row| row.state != WatchState::Off || app.watched_by_service(&row.hash))
        .count();

    let (chip, color) = match (&app.service, app.service_running) {
        (_, true) => ("[service ✓ running]", Color::Green),
        (Some(report), false) => match report.status {
            ServiceStatus::NotInstalled => ("[service — not installed]", Color::DarkGray),
            ServiceStatus::Installed { failed: true, .. } => ("[service ✗ error]", Color::Red),
            ServiceStatus::Installed { .. } => ("[service ○ stopped]", Color::Gray),
        },
        (None, false) => ("[service — not installed]", Color::DarkGray),
    };

    let mut spans = vec![
        Span::styled(
            " ygrep",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" v{}  ", env!("CARGO_PKG_VERSION")),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(chip, Style::default().fg(color)),
        Span::raw(format!(
            "  {} indexes · {} · {watching} watching",
            app.rows.len(),
            format_size(total)
        )),
    ];
    if area.width >= 92 {
        spans.push(Span::styled(
            "   t stats · S service · ? keys",
            Style::default().fg(Color::DarkGray),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_indexes(f: &mut Frame, app: &App, area: Rect) {
    let focused = app.focus == Panel::Indexes;
    let view_h = area.height.saturating_sub(2) as usize;
    let inner_w = area.width.saturating_sub(2) as usize;
    // Everything to the right of the path is fixed-width, so the path takes the slack.
    // A narrow terminal drops the file/segment/semantic columns rather than truncating
    // the columns that matter.
    let wide = inner_w >= 88;
    let fixed = if wide { 58 } else { 32 };
    let path_w = inner_w.saturating_sub(fixed).clamp(10, 60);
    let off = window_offset(app.sel, app.view.len(), view_h);

    let mut lines: Vec<Line> = Vec::new();
    if app.view.is_empty() {
        let hint = if app.rows.is_empty() {
            "  no indexes yet — run `ygrep index` in a workspace"
        } else {
            "  nothing matches the filter"
        };
        lines.push(Line::from(Span::styled(
            hint,
            Style::default().fg(Color::DarkGray),
        )));
    }

    for (i, idx) in app.view.iter().enumerate().skip(off).take(view_h) {
        let row = &app.rows[*idx];
        let selected = i == app.sel;
        let marker = if selected && focused { "›" } else { " " };
        let indexed = row
            .indexed_at
            .as_ref()
            .map(format_relative_time)
            .unwrap_or_else(|| "-".to_string());
        let rate = if row.changes_per_min > 0.0 {
            format!("{:>5.0}/m", row.changes_per_min)
        } else {
            "      ".to_string()
        };

        let mut spans = vec![
            Span::raw(format!(" {marker} ")),
            status_dot(app, row),
            Span::raw(format!(
                " {:<path_w$} {:>6} ",
                clamp_path(&row.display, path_w),
                format_size(row.size_bytes),
            )),
        ];
        if wide {
            spans.push(Span::raw(format!(
                "{:>7} {:>4}seg ",
                row.files,
                row.segments.unwrap_or(0)
            )));
            spans.push(if row.semantic {
                Span::styled("sem ", Style::default().fg(Color::Magenta))
            } else {
                Span::raw("    ")
            });
        }
        spans.push(Span::styled(
            format!("{indexed:>9} "),
            Style::default().fg(Color::Gray),
        ));
        if wide {
            spans.push(Span::styled(rate, Style::default().fg(Color::Green)));
        }
        spans.push(if row.watch {
            Span::styled(" [w]", Style::default().fg(ACCENT))
        } else {
            Span::raw("    ")
        });
        if app.watched_by_service(&row.hash) {
            spans.push(Span::styled(" svc", Style::default().fg(Color::Green)));
        } else if row.orphaned {
            spans.push(Span::styled(" gone", Style::default().fg(Color::Red)));
        }

        lines.push(Line::from(spans).style(row_style(selected, focused)));
    }

    let arrow = if app.sort_asc { "▲" } else { "▼" };
    let mut title = format!(
        "Indexes ({})  [1-4 sort: {}{arrow}]",
        app.view.len(),
        app.sort_col.label()
    );
    if !app.filter.is_empty() || app.filter_input {
        title.push_str(&format!("  /{}", app.filter));
        if app.filter_input {
            title.push('▏');
        }
    }
    f.render_widget(
        Paragraph::new(lines).block(panel_block(&title, focused)),
        area,
    );
}

fn render_service(f: &mut Frame, app: &App, area: Rect) {
    let mut lines: Vec<Line> = Vec::new();

    match &app.service {
        None => lines.push(Line::from(Span::styled(
            "  service state unavailable",
            Style::default().fg(Color::DarkGray),
        ))),
        Some(report) => {
            let (dot, color, label) = if app.service_running {
                ("●", Color::Green, "running")
            } else {
                match report.status {
                    ServiceStatus::NotInstalled => ("○", Color::DarkGray, "not installed"),
                    ServiceStatus::Installed { failed: true, .. } => ("✗", Color::Red, "error"),
                    ServiceStatus::Installed { .. } => ("○", Color::Gray, "stopped"),
                }
            };

            let mut head = vec![
                Span::raw(" "),
                Span::styled(format!("{dot} {label:<13}"), Style::default().fg(color)),
            ];
            if let Some(state) = &report.heartbeat {
                let up = format_relative_time(&state.started_at).replace(" ago", "");
                head.push(Span::raw(format!(
                    "pid {}  up {up}  watching {} of {}",
                    state.pid,
                    state.watched.len(),
                    state.registered,
                )));
                if area.width >= 80 {
                    head.push(Span::styled(
                        format!(
                            "  rescan {} (every {}s)",
                            format_relative_time(&state.last_rescan),
                            state.rescan_secs
                        ),
                        Style::default().fg(Color::DarkGray),
                    ));
                }
            } else {
                head.push(Span::styled(
                    format!(
                        "{} of {} indexes have the watch flag on   S to install or start",
                        report.watch_enabled, report.indexes
                    ),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            lines.push(Line::from(head));
            lines.push(Line::from(Span::styled(
                format!(
                    " log {}",
                    ygrep_core::registry::shorten_path(&report.log_path.display().to_string())
                ),
                Style::default().fg(Color::DarkGray),
            )));
        }
    }

    f.render_widget(
        Paragraph::new(lines).block(panel_block("Service", false)),
        area,
    );
}

fn render_activity(f: &mut Frame, app: &App, area: Rect) {
    let focused = app.focus == Panel::Activity;
    let view_h = area.height.saturating_sub(2) as usize;
    let total = app.activity.len();
    // `activity_scroll` counts lines back from the newest, so the window walks up.
    let end = total.saturating_sub(app.activity_scroll);
    let start = end.saturating_sub(view_h);

    let mut lines: Vec<Line> = Vec::new();
    if total == 0 {
        lines.push(Line::from(Span::styled(
            "  no activity yet — watched workspaces report here",
            Style::default().fg(Color::DarkGray),
        )));
    }
    for entry in app.activity.iter().take(end).skip(start) {
        let color = match entry.kind {
            super::ActivityKind::Indexed => Color::Green,
            super::ActivityKind::Deleted => Color::Red,
            super::ActivityKind::State => Color::Yellow,
            super::ActivityKind::Error => Color::Red,
            super::ActivityKind::Reindex => ACCENT,
            super::ActivityKind::Service => Color::Blue,
        };
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {} ", entry.at.format("%H:%M:%S")),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(
                format!("{:<12} ", entry.who),
                Style::default().fg(Color::Blue),
            ),
            Span::styled(entry.text.clone(), Style::default().fg(color)),
        ]));
    }

    let title = if app.follow {
        format!("Activity ({total})  following")
    } else {
        format!("Activity ({total})  paused")
    };
    f.render_widget(
        Paragraph::new(lines).block(panel_block(&title, focused)),
        area,
    );
}

fn render_keys(f: &mut Frame, app: &App, area: Rect) {
    let keys: &[(&str, &str)] = if app.filter_input {
        &[("type", "filter"), ("enter", "keep"), ("esc", "clear")]
    } else {
        match app.focus {
            Panel::Indexes => &[
                ("enter", "watch"),
                ("w", "watch flag"),
                ("i", "reindex"),
                ("c", "compact"),
                ("R", "remove"),
                ("o", "open"),
                ("t", "stats"),
                ("S", "service"),
                ("/", "filter"),
                ("tab", "focus"),
                ("q", "quit"),
            ],
            Panel::Activity => &[
                ("↑↓", "scroll"),
                ("g", "follow"),
                ("home/end", "jump"),
                ("t", "stats"),
                ("S", "service"),
                ("tab", "focus"),
                ("q", "quit"),
            ],
        }
    };

    let mut spans = vec![Span::raw(" ")];
    for (key, label) in keys {
        spans.push(Span::styled(
            (*key).to_string(),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            format!(" {label}   "),
            Style::default().fg(Color::DarkGray),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_status(f: &mut Frame, app: &App, area: Rect) {
    let color = if app.message.starts_with('✗') {
        Color::Red
    } else {
        Color::Gray
    };
    f.render_widget(
        Paragraph::new(Span::styled(
            format!(" {}", app.message),
            Style::default().fg(color),
        )),
        area,
    );
}

pub fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width: width.min(area.width),
        height: height.min(area.height),
    }
}

fn render_confirm(f: &mut Frame, area: Rect, label: &str) {
    let rect = centered_rect(64, 7, area);
    f.render_widget(Clear, rect);
    let lines = vec![
        Line::raw(""),
        Line::from(format!("  Remove the index for {label}?")),
        Line::from(Span::styled(
            "  The workspace is untouched; `ygrep index` rebuilds it.",
            Style::default().fg(Color::DarkGray),
        )),
        Line::raw(""),
        Line::from(Span::styled(
            "  y confirm · n/esc cancel",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Red))
        .title(Span::styled(
            " Remove index ",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ));
    f.render_widget(Paragraph::new(lines).block(block), rect);
}

fn render_service_menu(f: &mut Frame, area: Rect, sel: usize) {
    let rect = centered_rect(62, SERVICE_ACTIONS.len() as u16 + 4, area);
    f.render_widget(Clear, rect);

    let mut lines = vec![Line::raw("")];
    for (i, (action, blurb)) in SERVICE_ACTIONS.iter().enumerate() {
        let active = i == sel;
        let marker = if active { "› " } else { "  " };
        let style = if active {
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{marker}{action:<11}"), style),
            Span::styled((*blurb).to_string(), Style::default().fg(Color::DarkGray)),
        ]));
    }
    lines.push(Line::from(Span::styled(
        "  ↑↓ select · enter run · esc cancel",
        Style::default().fg(Color::DarkGray),
    )));

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT))
        .title(Span::styled(
            " Watch service ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
    f.render_widget(Paragraph::new(lines).block(block), rect);
}

fn render_help(f: &mut Frame, area: Rect) {
    let entries: [(&str, &str); 16] = [
        ("↑↓ / j k", "move · scroll the focused panel"),
        ("tab", "switch focus between Indexes and Activity"),
        ("1-4", "sort by name / size / age / files (again reverses)"),
        ("/", "filter indexes by path"),
        ("enter", "start or stop watching for this session"),
        (
            "w",
            "toggle the persisted watch flag (the service reads it)",
        ),
        ("i", "re-index the selected workspace"),
        ("c", "compact the selected index"),
        ("R / del", "remove the selected index"),
        ("o", "open the workspace in the file manager"),
        ("g", "follow or pause the activity panel"),
        ("home/end", "top / bottom of the focused panel"),
        ("t", "query stats: rate, top queries, live tail"),
        ("S", "service menu: install, start, stop, restart"),
        ("?", "this help"),
        ("q / esc", "quit"),
    ];

    let rect = centered_rect(74, entries.len() as u16 + 4, area);
    f.render_widget(Clear, rect);

    let mut lines = vec![Line::raw("")];
    for (key, label) in entries {
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {key:<10}"),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::raw(label),
        ]));
    }
    lines.push(Line::from(Span::styled(
        "  any key to close",
        Style::default().fg(Color::DarkGray),
    )));

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT))
        .title(Span::styled(
            " Keys ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
    f.render_widget(Paragraph::new(lines).block(block), rect);
}
