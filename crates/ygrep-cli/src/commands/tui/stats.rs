//! The full-screen query-stats view: queries/sec sparkline, totals, top queries and
//! workspaces as bars, and a live tail — all fed by polling the telemetry log.
//!
//! Opening the view reads the log once for history, then every tick asks for whatever
//! was appended since. The reader handles rotation and truncation, so nothing here has
//! to care that a search in another terminal is writing to the same file.

use std::collections::{HashMap, VecDeque};
use std::path::Path;

use chrono::{Local, TimeZone, Utc};
use crossterm::event::KeyCode;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Sparkline},
    Frame,
};

use ygrep_core::telemetry::{self, QueryEvent};

use super::{App, ACCENT};

/// How many recorded queries to keep in memory.
const MAX_EVENTS: usize = 5000;

/// A query counts as "live" when the newest one is this recent.
const LIVE_WINDOW_SECS: i64 = 30;

/// How many entries the top-N panels show.
const TOP_N: usize = 8;

/// The open stats view: recorded queries plus the filter narrowing them.
pub struct StatsView {
    events: VecDeque<QueryEvent>,
    offset: u64,
    pub filter: String,
    pub filter_input: bool,
    /// Newest second demo mode has already fabricated traffic for.
    demo_second: i64,
    demo_seed: u64,
}

/// The query mix both the snapshot and demo mode draw from: `(query, mode, ms, hits)`.
const DEMO_QUERIES: [(&str, &str, u64, usize); 8] = [
    ("fn main", "literal", 12, 8),
    ("WatchManager", "literal", 41, 22),
    ("->get(", "regex", 96, 3),
    ("how does watching work", "hybrid", 220, 14),
    ("IndexLocked", "literal", 7, 2),
    ("segment_count", "literal", 19, 6),
    ("telemetry", "literal", 33, 11),
    ("::compact_index", "literal", 61, 0),
];

impl StatsView {
    /// Open the view, seeded with whatever the log already holds.
    pub fn open(data_dir: &Path) -> Self {
        let tail = telemetry::tail_from(data_dir, 0);
        let mut view = Self {
            events: VecDeque::new(),
            offset: tail.offset,
            filter: String::new(),
            filter_input: false,
            demo_second: 0,
            demo_seed: DEMO_SEED,
        };
        view.ingest(tail.events);
        view
    }

    /// Pull in whatever was appended since the last poll.
    pub fn poll(&mut self, data_dir: &Path) {
        let tail = telemetry::tail_from(data_dir, self.offset);
        self.offset = tail.offset;
        self.ingest(tail.events);
    }

    fn ingest(&mut self, events: Vec<QueryEvent>) {
        for event in events {
            if self.events.len() >= MAX_EVENTS {
                self.events.pop_front();
            }
            self.events.push_back(event);
        }
    }

    /// True when a query landed recently enough to call the feed live.
    fn live(&self) -> bool {
        let now = Utc::now().timestamp();
        self.events
            .back()
            .map(|event| now - event.ts <= LIVE_WINDOW_SECS)
            .unwrap_or(false)
    }

    /// A fabricated few minutes of traffic, so the snapshot exercises real scaling.
    pub fn synthetic() -> Self {
        let now = Utc::now().timestamp();
        let workspaces = demo_workspaces();

        // Repeats give the top-N bars something to rank; the wave keeps the sparkline
        // from rendering as one flat block.
        let picks = [0usize, 1, 0, 2, 0, 1, 3, 0, 4, 1, 0, 5, 1, 6, 0, 7, 2, 1];
        let ws_picks = [0usize, 0, 1, 0, 2, 0, 1, 0, 1, 2];

        let mut events = VecDeque::new();
        let mut n = 0usize;
        for second in 0..300i64 {
            let wave = (second as f64 / 23.0).sin() * 4.0 + 4.5;
            let count = (wave.max(0.0) as u64) + (second as u64 % 3);
            for _ in 0..count {
                let (q, mode, ms, hits) = DEMO_QUERIES[picks[n % picks.len()]];
                events.push_back(QueryEvent {
                    ts: now - (299 - second),
                    ws: workspaces[ws_picks[n % ws_picks.len()]].clone(),
                    q: q.to_string(),
                    ms: ms + (n as u64 % 7) * 3,
                    hits,
                    mode: mode.to_string(),
                });
                n += 1;
            }
        }

        Self {
            events,
            offset: 0,
            filter: String::new(),
            filter_input: false,
            demo_second: now,
            demo_seed: DEMO_SEED,
        }
    }

    /// Fabricate the traffic for every second that has passed since the last call, so
    /// the sparkline and the tail keep moving with no telemetry file behind them.
    pub fn demo_tick(&mut self) {
        let now = Utc::now().timestamp();
        if self.demo_second == 0 || self.demo_second > now {
            self.demo_second = now;
            return;
        }
        let workspaces = demo_workspaces();
        while self.demo_second < now {
            self.demo_second += 1;
            let wave = (self.demo_second as f64 / 19.0).sin() * 4.0 + 4.5;
            let count = wave.max(0.0) as u64 + demo_rand(&mut self.demo_seed) % 3;
            for _ in 0..count {
                let n = demo_rand(&mut self.demo_seed) as usize;
                let (q, mode, ms, hits) = DEMO_QUERIES[n % DEMO_QUERIES.len()];
                if self.events.len() >= MAX_EVENTS {
                    self.events.pop_front();
                }
                self.events.push_back(QueryEvent {
                    ts: self.demo_second,
                    ws: workspaces[n % workspaces.len()].clone(),
                    q: q.to_string(),
                    ms: ms + (n as u64 % 7) * 3,
                    hits,
                    mode: mode.to_string(),
                });
            }
        }
    }
}

/// Seed for the fabricated query mix.
const DEMO_SEED: u64 = 0x2545_f491_4f6c_dd1d;

/// The synthetic workspaces queries are attributed to.
fn demo_workspaces() -> [String; 3] {
    [
        super::synthetic_hash(0),
        super::synthetic_hash(1),
        super::synthetic_hash(3),
    ]
}

/// xorshift64*, matching the dashboard's own drift generator.
fn demo_rand(seed: &mut u64) -> u64 {
    let mut x = *seed;
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    *seed = x;
    x.wrapping_mul(0x2545_f491_4f6c_dd1d)
}

/// Everything the panels need, computed from the filtered events in one pass.
struct Summary {
    series: Vec<u64>,
    /// Every recorded query the view is holding
    total: u64,
    /// Only the ones inside the sparkline's window
    windowed: u64,
    peak: u64,
    avg_ms: f64,
    max_ms: u64,
    zero_hits: u64,
    top_queries: Vec<(String, u64)>,
    top_workspaces: Vec<(String, u64)>,
}

/// Map an index hash to the shortened workspace path, so bars read as paths.
fn labels(app: &App) -> HashMap<String, String> {
    app.rows
        .iter()
        .map(|row| (row.hash.clone(), row.display.clone()))
        .collect()
}

fn workspace_label(labels: &HashMap<String, String>, hash: &str) -> String {
    labels
        .get(hash)
        .cloned()
        .unwrap_or_else(|| hash.chars().take(12).collect())
}

fn matches(event: &QueryEvent, needle: &str, label: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    event.q.to_lowercase().contains(needle)
        || event.mode.contains(needle)
        || label.to_lowercase().contains(needle)
}

fn summarize(view: &StatsView, labels: &HashMap<String, String>, window: usize) -> Summary {
    let now = Utc::now().timestamp();
    let needle = view.filter.to_lowercase();
    let start = now - window as i64 + 1;

    let mut series = vec![0u64; window];
    let mut total = 0u64;
    let mut windowed = 0u64;
    let mut ms_sum = 0u64;
    let mut max_ms = 0u64;
    let mut zero_hits = 0u64;
    let mut by_query: HashMap<&str, u64> = HashMap::new();
    let mut by_ws: HashMap<String, u64> = HashMap::new();

    for event in &view.events {
        let label = workspace_label(labels, &event.ws);
        if !matches(event, &needle, &label) {
            continue;
        }
        total += 1;
        ms_sum += event.ms;
        max_ms = max_ms.max(event.ms);
        if event.hits == 0 {
            zero_hits += 1;
        }
        *by_query.entry(event.q.as_str()).or_insert(0) += 1;
        *by_ws.entry(label).or_insert(0) += 1;

        if event.ts >= start && event.ts <= now {
            series[(event.ts - start) as usize] += 1;
            windowed += 1;
        }
    }

    let mut top_queries: Vec<(String, u64)> = by_query
        .into_iter()
        .map(|(q, n)| (q.to_string(), n))
        .collect();
    top_queries.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    top_queries.truncate(TOP_N);

    let mut top_workspaces: Vec<(String, u64)> = by_ws.into_iter().collect();
    top_workspaces.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    top_workspaces.truncate(TOP_N);

    Summary {
        peak: series.iter().copied().max().unwrap_or(0),
        series,
        avg_ms: if total > 0 {
            ms_sum as f64 / total as f64
        } else {
            0.0
        },
        total,
        windowed,
        max_ms,
        zero_hits,
        top_queries,
        top_workspaces,
    }
}

/// Keys while the stats view is open.
pub fn handle_key(app: &mut App, code: KeyCode) {
    let Some(view) = app.stats.as_mut() else {
        return;
    };

    if view.filter_input {
        match code {
            KeyCode::Enter => view.filter_input = false,
            KeyCode::Esc => {
                view.filter.clear();
                view.filter_input = false;
            }
            KeyCode::Backspace => {
                view.filter.pop();
            }
            KeyCode::Char(c) => view.filter.push(c),
            _ => {}
        }
        return;
    }

    match code {
        KeyCode::Char('/') => view.filter_input = true,
        // With a filter kept, the first Esc clears it and the next one leaves.
        KeyCode::Esc if !view.filter.is_empty() => view.filter.clear(),
        KeyCode::Char('0') | KeyCode::Home => view.filter.clear(),
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('t') => app.stats = None,
        _ => {}
    }
}

pub fn render(f: &mut Frame, app: &App) {
    let Some(view) = app.stats.as_ref() else {
        return;
    };
    let area = f.area();

    // Give the chart and the bar panels what the terminal can spare, in that order.
    let chart_h = if area.height >= 26 {
        10
    } else if area.height >= 20 {
        7
    } else {
        5
    };
    let mid_h = if area.height >= 24 { 9 } else { 6 };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),       // title
            Constraint::Length(chart_h), // queries/sec
            Constraint::Length(mid_h),   // totals + top lists
            Constraint::Min(3),          // live tail
            Constraint::Length(1),       // key bar
        ])
        .split(area);

    let window = (chunks[1].width.saturating_sub(2) as usize).clamp(30, 300);
    let labels = labels(app);
    let summary = summarize(view, &labels, window);

    render_title(f, view, chunks[0]);
    render_chart(f, chunks[1], &summary, window);

    // Drop the least useful column first when the terminal can't hold three.
    let columns: Vec<Constraint> = if area.width >= 96 {
        vec![
            Constraint::Length(32),
            Constraint::Percentage(36),
            Constraint::Min(20),
        ]
    } else if area.width >= 64 {
        vec![Constraint::Length(32), Constraint::Min(24)]
    } else {
        vec![Constraint::Min(24)]
    };
    let mid = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(columns)
        .split(chunks[2]);
    render_totals(f, mid[0], &summary);
    if let Some(rect) = mid.get(1) {
        render_top(f, *rect, "Top queries", &summary.top_queries);
    }
    if let Some(rect) = mid.get(2) {
        render_top(f, *rect, "Top workspaces", &summary.top_workspaces);
    }

    render_tail(f, chunks[3], view, &labels);
    render_keys(f, view, chunks[4]);
}

fn render_title(f: &mut Frame, view: &StatsView, area: Rect) {
    let mut spans = vec![Span::styled(
        " ygrep query stats",
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    )];
    if view.filter_input {
        spans.push(Span::styled(
            format!("   /{}▏", view.filter),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    } else if !view.filter.is_empty() {
        spans.push(Span::styled(
            format!("   /{}", view.filter),
            Style::default().fg(Color::Yellow),
        ));
    }
    spans.push(if view.live() {
        Span::styled("   ● live", Style::default().fg(Color::Green))
    } else {
        Span::styled("   ○ idle", Style::default().fg(Color::DarkGray))
    });
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_chart(f: &mut Frame, area: Rect, summary: &Summary, window: usize) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(Span::styled(
            format!(
                " Queries/sec — last {window}s · {} in window · peak {}/s ",
                summary.windowed, summary.peak
            ),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
    // Right-align the series so the newest second sits at the right edge.
    let width = area.width.saturating_sub(2) as usize;
    let data: Vec<u64> = summary.series[summary.series.len().saturating_sub(width)..].to_vec();
    f.render_widget(
        Sparkline::default()
            .block(block)
            .data(data)
            .style(Style::default().fg(ACCENT)),
        area,
    );
}

fn render_totals(f: &mut Frame, area: Rect, summary: &Summary) {
    let now_rate = summary.series.last().copied().unwrap_or(0);
    let lines = vec![
        Line::from(vec![
            Span::raw(" rate    "),
            Span::styled(
                format!("{now_rate}/s"),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  peak {}/s", summary.peak),
                Style::default().fg(Color::DarkGray),
            ),
        ]),
        Line::from(vec![
            Span::raw(" queries "),
            Span::styled(
                summary.total.to_string(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::raw(" time    "),
            Span::raw(format!(
                "avg {:.0}ms  max {}ms",
                summary.avg_ms, summary.max_ms
            )),
        ]),
        Line::from(vec![
            Span::raw(" misses  "),
            Span::styled(
                format!("{} with no hits", summary.zero_hits),
                Style::default().fg(if summary.zero_hits > 0 {
                    Color::Yellow
                } else {
                    Color::DarkGray
                }),
            ),
        ]),
    ];
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(Span::styled(
            " Totals ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn render_top(f: &mut Frame, area: Rect, title: &str, entries: &[(String, u64)]) {
    let view_h = area.height.saturating_sub(2) as usize;
    let max = entries.first().map(|e| e.1).unwrap_or(0).max(1);

    let mut lines: Vec<Line> = Vec::new();
    if entries.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (nothing recorded yet)",
            Style::default().fg(Color::DarkGray),
        )));
    }

    // Name column sized to the widest visible entry; the bar takes what's left.
    let name_max = (area.width.saturating_sub(12) as usize).max(6);
    let name_w = entries
        .iter()
        .take(view_h)
        .map(|e| e.0.chars().count())
        .max()
        .unwrap_or(8)
        .clamp(6, name_max);
    let bar_w = (area.width as usize).saturating_sub(name_w + 8).max(3);

    for (name, count) in entries.iter().take(view_h) {
        let shown = clamp_tail(name, name_w);
        let filled = ((*count as f64 / max as f64) * bar_w as f64).round() as usize;
        lines.push(Line::from(vec![
            Span::raw(format!(" {shown:<name_w$} ")),
            Span::styled("▐".repeat(filled.max(1)), Style::default().fg(ACCENT)),
            Span::styled(format!(" {count}"), Style::default().fg(Color::Gray)),
        ]));
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(Span::styled(
            format!(" {title} "),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
    f.render_widget(Paragraph::new(lines).block(block), area);
}

/// Keep the tail of a long name, which is the informative end of a path.
fn clamp_tail(text: &str, max: usize) -> String {
    let len = text.chars().count();
    if len <= max {
        return text.to_string();
    }
    let tail: String = text.chars().skip(len - max.saturating_sub(1)).collect();
    format!("…{tail}")
}

fn ms_color(ms: u64) -> Color {
    if ms < 50 {
        Color::Green
    } else if ms < 250 {
        Color::Yellow
    } else {
        Color::Red
    }
}

fn render_tail(f: &mut Frame, area: Rect, view: &StatsView, labels: &HashMap<String, String>) {
    let view_h = area.height.saturating_sub(2) as usize;
    let needle = view.filter.to_lowercase();

    // Newest at the bottom: walk backwards, then flip for display.
    let mut rows: Vec<&QueryEvent> = view
        .events
        .iter()
        .rev()
        .filter(|event| matches(event, &needle, &workspace_label(labels, &event.ws)))
        .take(view_h)
        .collect();
    rows.reverse();

    let mut lines: Vec<Line> = Vec::new();
    if rows.is_empty() {
        lines.push(Line::from(Span::styled(
            "  no queries recorded yet — run a search and they land here",
            Style::default().fg(Color::DarkGray),
        )));
    }
    for event in rows {
        let when = Local
            .timestamp_opt(event.ts, 0)
            .single()
            .map(|t| t.format("%H:%M:%S").to_string())
            .unwrap_or_else(|| "--:--:--".to_string());
        lines.push(Line::from(vec![
            Span::styled(format!(" {when} "), Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{:>6}ms ", event.ms),
                Style::default().fg(ms_color(event.ms)),
            ),
            Span::styled(
                format!("{:>5} hits ", event.hits),
                Style::default().fg(Color::Gray),
            ),
            Span::styled(
                format!("{:<7} ", event.mode),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(
                format!(
                    "{:<24}",
                    clamp_tail(&workspace_label(labels, &event.ws), 24)
                ),
                Style::default().fg(Color::Blue),
            ),
            Span::raw(format!("  {}", event.q)),
        ]));
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(Span::styled(
            " Recent queries ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn render_keys(f: &mut Frame, view: &StatsView, area: Rect) {
    let keys: &[(&str, &str)] = if view.filter_input {
        &[("type", "filter"), ("enter", "keep"), ("esc", "clear")]
    } else if !view.filter.is_empty() {
        &[("/", "edit filter"), ("0/esc", "clear"), ("t", "back")]
    } else {
        &[("/", "filter"), ("esc/t", "back"), ("q", "back")]
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
