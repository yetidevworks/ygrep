use anyhow::Result;
use std::collections::VecDeque;
use std::io;
use std::path::PathBuf;
use std::time::Instant;

use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState},
    Frame, Terminal,
};

use ygrep_core::dashboard::{
    ActivityEvent, ActivityKind, IndexEntry, ManagerCommand, ManagerEvent, WatchState,
};

use super::indexes::{collect_indexes, format_relative_time, format_size, shorten_path, IndexInfo};

/// Maximum activity log entries to keep
const MAX_ACTIVITY_LOG: usize = 500;

/// Focus panel
#[derive(Debug, Clone, PartialEq, Eq)]
enum Focus {
    Table,
    Log,
}

/// Confirmation dialog state
#[derive(Debug)]
enum Dialog {
    None,
    ConfirmDelete { hash: String, name: String },
}

/// Sort column for the dashboard table
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SortColumn {
    Name,
    Size,
    Files,
    Indexed,
    Watch,
}

impl SortColumn {
    fn next(self) -> Self {
        match self {
            SortColumn::Name => SortColumn::Size,
            SortColumn::Size => SortColumn::Files,
            SortColumn::Files => SortColumn::Indexed,
            SortColumn::Indexed => SortColumn::Watch,
            SortColumn::Watch => SortColumn::Name,
        }
    }
}

/// Sort order
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SortOrder {
    Asc,
    Desc,
}

/// Dashboard application state
struct App {
    /// Index entries displayed in the table
    entries: Vec<IndexEntry>,
    /// Table selection state
    table_state: TableState,
    /// Activity log
    activity_log: VecDeque<ActivityEvent>,
    /// Current focus panel
    focus: Focus,
    /// Activity log scroll offset (0 = bottom/most recent)
    log_scroll: usize,
    /// Show help overlay
    show_help: bool,
    /// Active dialog
    dialog: Dialog,
    /// Should quit
    should_quit: bool,
    /// Command sender to WatchManager
    cmd_tx: tokio::sync::mpsc::UnboundedSender<ManagerCommand>,
    /// Changes tracking for rate calculation: hash -> timestamps of recent changes
    change_timestamps: std::collections::HashMap<String, VecDeque<Instant>>,
    /// Current sort column
    sort_column: SortColumn,
    /// Current sort order
    sort_order: SortOrder,
    /// Filter mode active
    filter_active: bool,
    /// Filter input string
    filter_input: String,
    /// All entries (unfiltered)
    all_entries: Vec<IndexEntry>,
}

impl App {
    fn new(
        entries: Vec<IndexEntry>,
        cmd_tx: tokio::sync::mpsc::UnboundedSender<ManagerCommand>,
    ) -> Self {
        let mut table_state = TableState::default();
        if !entries.is_empty() {
            table_state.select(Some(0));
        }

        Self {
            all_entries: entries.clone(),
            entries,
            table_state,
            activity_log: VecDeque::with_capacity(MAX_ACTIVITY_LOG),
            focus: Focus::Table,
            log_scroll: 0,
            show_help: false,
            dialog: Dialog::None,
            should_quit: false,
            cmd_tx,
            change_timestamps: std::collections::HashMap::new(),
            sort_column: SortColumn::Watch,
            sort_order: SortOrder::Desc,
            filter_active: false,
            filter_input: String::new(),
        }
    }

    fn selected_index(&self) -> Option<usize> {
        self.table_state.selected()
    }

    fn selected_entry(&self) -> Option<&IndexEntry> {
        self.selected_index().and_then(|i| self.entries.get(i))
    }

    fn move_selection(&mut self, delta: i32) {
        if self.entries.is_empty() {
            return;
        }
        let current = self.table_state.selected().unwrap_or(0) as i32;
        let new = (current + delta).clamp(0, self.entries.len() as i32 - 1) as usize;
        self.table_state.select(Some(new));
    }

    fn push_activity(&mut self, event: ActivityEvent) {
        if self.activity_log.len() >= MAX_ACTIVITY_LOG {
            self.activity_log.pop_front();
        }
        self.activity_log.push_back(event);
        // Auto-scroll to bottom when new events arrive (if we're at or near the bottom)
        if self.log_scroll <= 1 {
            self.log_scroll = 0;
        }
    }

    fn record_change(&mut self, hash: &str) {
        let timestamps = self
            .change_timestamps
            .entry(hash.to_string())
            .or_insert_with(VecDeque::new);
        let now = Instant::now();
        timestamps.push_back(now);
        // Keep only last 5 minutes of timestamps
        while let Some(front) = timestamps.front() {
            if now.duration_since(*front).as_secs() > 300 {
                timestamps.pop_front();
            } else {
                break;
            }
        }
    }

    fn changes_per_min(&self, hash: &str) -> f64 {
        if let Some(timestamps) = self.change_timestamps.get(hash) {
            if timestamps.len() < 2 {
                return 0.0;
            }
            let now = Instant::now();
            let window_secs = 60.0_f64; // Look at last 60s
            let recent = timestamps
                .iter()
                .filter(|t| now.duration_since(**t).as_secs_f64() < window_secs)
                .count();
            recent as f64
        } else {
            0.0
        }
    }

    fn sort_entries(&mut self) {
        // Save selected hash
        let selected_hash = self.selected_entry().map(|e| e.hash.clone());

        let order = self.sort_order;
        let sort_col = self.sort_column;
        self.entries.sort_by(|a, b| {
            let cmp = match sort_col {
                SortColumn::Name => a
                    .display_path
                    .to_lowercase()
                    .cmp(&b.display_path.to_lowercase()),
                SortColumn::Size => a.size_bytes.cmp(&b.size_bytes),
                SortColumn::Files => a.files_indexed.cmp(&b.files_indexed),
                SortColumn::Indexed => a.indexed_at.cmp(&b.indexed_at),
                SortColumn::Watch => {
                    let rank = |w: &WatchState| match w {
                        WatchState::Active => 2,
                        WatchState::Sleeping => 1,
                        WatchState::Off => 0,
                    };
                    rank(&a.watch_state).cmp(&rank(&b.watch_state))
                }
            };
            let primary = match order {
                SortOrder::Asc => cmp,
                SortOrder::Desc => cmp.reverse(),
            };
            // Tiebreak by name ascending
            primary.then_with(|| {
                a.display_path
                    .to_lowercase()
                    .cmp(&b.display_path.to_lowercase())
            })
        });

        // Restore selection by hash
        if let Some(hash) = selected_hash {
            if let Some(pos) = self.entries.iter().position(|e| e.hash == hash) {
                self.table_state.select(Some(pos));
            }
        }
    }

    fn apply_filter(&mut self) {
        let selected_hash = self.selected_entry().map(|e| e.hash.clone());

        if self.filter_input.is_empty() {
            self.entries = self.all_entries.clone();
        } else {
            let query = self.filter_input.to_lowercase();
            self.entries = self
                .all_entries
                .iter()
                .filter(|e| e.display_path.to_lowercase().contains(&query))
                .cloned()
                .collect();
        }

        self.sort_entries();

        // Restore selection by hash or reset
        if let Some(hash) = selected_hash {
            if let Some(pos) = self.entries.iter().position(|e| e.hash == hash) {
                self.table_state.select(Some(pos));
                return;
            }
        }
        if !self.entries.is_empty() {
            self.table_state.select(Some(0));
        } else {
            self.table_state.select(None);
        }
    }

    fn workspace_name(&self, hash: &str) -> String {
        self.all_entries
            .iter()
            .find(|e| e.hash == hash)
            .or_else(|| self.entries.iter().find(|e| e.hash == hash))
            .map(|e| {
                e.workspace_path
                    .file_name()
                    .map(|f| f.to_string_lossy().to_string())
                    .unwrap_or_else(|| e.display_path.clone())
            })
            .unwrap_or_else(|| hash[..8].to_string())
    }

    fn handle_manager_event(&mut self, event: ManagerEvent) {
        match event {
            ManagerEvent::WatchStateChanged { hash, new_state } => {
                let ws_name = self.workspace_name(&hash);
                let msg = match &new_state {
                    WatchState::Active => format!("{} watching", ws_name),
                    WatchState::Sleeping => format!("{} sleeping (idle)", ws_name),
                    WatchState::Off => format!("{} stopped", ws_name),
                };
                if let Some(entry) = self.all_entries.iter_mut().find(|e| e.hash == hash) {
                    entry.watch_state = new_state.clone();
                }
                if let Some(entry) = self.entries.iter_mut().find(|e| e.hash == hash) {
                    entry.watch_state = new_state;
                }
                self.push_activity(ActivityEvent {
                    timestamp: chrono::Utc::now(),
                    workspace_name: ws_name,
                    message: msg,
                    kind: ActivityKind::StateChange,
                });
            }
            ManagerEvent::FileIndexed { hash, path } => {
                self.record_change(&hash);
                let ws_name = self.workspace_name(&hash);
                // Update changes_per_min on the entry
                let cpm = self.changes_per_min(&hash);
                if let Some(entry) = self.all_entries.iter_mut().find(|e| e.hash == hash) {
                    entry.changes_per_min = cpm;
                }
                if let Some(entry) = self.entries.iter_mut().find(|e| e.hash == hash) {
                    entry.changes_per_min = cpm;
                }
                self.push_activity(ActivityEvent {
                    timestamp: chrono::Utc::now(),
                    workspace_name: ws_name,
                    message: format!("{:<40} [+] indexed", path),
                    kind: ActivityKind::Indexed,
                });
            }
            ManagerEvent::FileDeleted { hash, path } => {
                let ws_name = self.workspace_name(&hash);
                self.push_activity(ActivityEvent {
                    timestamp: chrono::Utc::now(),
                    workspace_name: ws_name,
                    message: format!("{:<40} [-] deleted", path),
                    kind: ActivityKind::Deleted,
                });
            }
            ManagerEvent::Error { hash, message } => {
                let ws_name = self.workspace_name(&hash);
                self.push_activity(ActivityEvent {
                    timestamp: chrono::Utc::now(),
                    workspace_name: ws_name,
                    message: format!("[!] {}", message),
                    kind: ActivityKind::Error,
                });
            }
            ManagerEvent::ReindexStarted { hash } => {
                let ws_name = self.workspace_name(&hash);
                self.push_activity(ActivityEvent {
                    timestamp: chrono::Utc::now(),
                    workspace_name: ws_name.clone(),
                    message: format!("{} re-indexing...", ws_name),
                    kind: ActivityKind::Reindex,
                });
            }
            ManagerEvent::ReindexCompleted {
                hash,
                files_indexed,
            } => {
                let ws_name = self.workspace_name(&hash);
                let now = chrono::Utc::now();
                if let Some(entry) = self.all_entries.iter_mut().find(|e| e.hash == hash) {
                    entry.files_indexed = files_indexed;
                    entry.indexed_at = Some(now);
                }
                if let Some(entry) = self.entries.iter_mut().find(|e| e.hash == hash) {
                    entry.files_indexed = files_indexed;
                    entry.indexed_at = Some(now);
                }
                self.push_activity(ActivityEvent {
                    timestamp: chrono::Utc::now(),
                    workspace_name: ws_name.clone(),
                    message: format!("{} re-index complete ({} files)", ws_name, files_indexed),
                    kind: ActivityKind::Reindex,
                });
            }
            ManagerEvent::IndexRemoved { hash } => {
                self.all_entries.retain(|e| e.hash != hash);
                self.entries.retain(|e| e.hash != hash);
                // Fix selection if needed
                if let Some(sel) = self.table_state.selected() {
                    if sel >= self.entries.len() && !self.entries.is_empty() {
                        self.table_state.select(Some(self.entries.len() - 1));
                    } else if self.entries.is_empty() {
                        self.table_state.select(None);
                    }
                }
            }
            ManagerEvent::Log { hash, message } => {
                let name = self.workspace_name(&hash);
                // Strip \r progress prefixes for clean display
                let msg = message.trim_start_matches('\r').trim();
                if !msg.is_empty() {
                    self.activity_log.push_back(ActivityEvent {
                        timestamp: chrono::Utc::now(),
                        workspace_name: name,
                        message: msg.to_string(),
                        kind: ActivityKind::Indexed,
                    });
                }
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        // Handle dialog first
        if let Dialog::ConfirmDelete { ref hash, .. } = self.dialog {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    let hash = hash.clone();
                    // Remove the index data directory
                    if let Ok(indexes_dir) = super::indexes::get_indexes_dir() {
                        let index_path = indexes_dir.join(&hash);
                        if index_path.exists() {
                            let _ = std::fs::remove_dir_all(&index_path);
                        }
                    }
                    // Tell manager to clean up watchers and remove from tracking
                    let _ = self.cmd_tx.send(ManagerCommand::RemoveIndex(hash));
                    self.dialog = Dialog::None;
                }
                _ => {
                    self.dialog = Dialog::None;
                }
            }
            return;
        }

        // Handle help overlay
        if self.show_help {
            self.show_help = false;
            return;
        }

        // Handle filter input mode
        if self.filter_active {
            match key.code {
                KeyCode::Esc => {
                    self.filter_active = false;
                    self.filter_input.clear();
                    self.apply_filter();
                }
                KeyCode::Enter => {
                    self.filter_active = false;
                    // Keep filter applied
                }
                KeyCode::Backspace => {
                    self.filter_input.pop();
                    self.apply_filter();
                }
                KeyCode::Char(c) => {
                    self.filter_input.push(c);
                    self.apply_filter();
                }
                _ => {}
            }
            return;
        }

        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => {
                // If filter is applied, clear it first
                if !self.filter_input.is_empty() {
                    self.filter_input.clear();
                    self.apply_filter();
                } else {
                    self.should_quit = true;
                }
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.should_quit = true;
            }
            KeyCode::Char('?') => {
                self.show_help = true;
            }
            KeyCode::Tab => {
                self.focus = match self.focus {
                    Focus::Table => Focus::Log,
                    Focus::Log => Focus::Table,
                };
            }
            // Sorting
            KeyCode::Char('s') => {
                self.sort_column = self.sort_column.next();
                self.sort_entries();
            }
            KeyCode::Char('S') => {
                self.sort_order = match self.sort_order {
                    SortOrder::Asc => SortOrder::Desc,
                    SortOrder::Desc => SortOrder::Asc,
                };
                self.sort_entries();
            }
            // Filter
            KeyCode::Char('/') => {
                self.filter_active = true;
            }
            // Navigation
            KeyCode::Char('j') | KeyCode::Down => match self.focus {
                Focus::Table => self.move_selection(1),
                Focus::Log => {
                    if self.log_scroll > 0 {
                        self.log_scroll -= 1;
                    }
                }
            },
            KeyCode::Char('k') | KeyCode::Up => match self.focus {
                Focus::Table => self.move_selection(-1),
                Focus::Log => {
                    self.log_scroll += 1;
                }
            },
            KeyCode::Char('g') => {
                if self.focus == Focus::Table && !self.entries.is_empty() {
                    self.table_state.select(Some(0));
                }
            }
            KeyCode::Char('G') => {
                if self.focus == Focus::Table && !self.entries.is_empty() {
                    self.table_state.select(Some(self.entries.len() - 1));
                }
            }
            // Actions
            KeyCode::Char('w') => {
                if let Some(entry) = self.selected_entry() {
                    let hash = entry.hash.clone();
                    let _ = self.cmd_tx.send(ManagerCommand::ToggleWatch(hash));
                }
            }
            KeyCode::Char('r') => {
                if let Some(entry) = self.selected_entry() {
                    if !entry.orphaned {
                        let hash = entry.hash.clone();
                        let _ = self.cmd_tx.send(ManagerCommand::Reindex(hash));
                    }
                }
            }
            KeyCode::Char('d') => {
                if let Some(entry) = self.selected_entry() {
                    self.dialog = Dialog::ConfirmDelete {
                        hash: entry.hash.clone(),
                        name: entry.display_path.clone(),
                    };
                }
            }
            _ => {}
        }
    }
}

/// Build IndexEntry list from IndexInfo
fn build_entries(indexes: Vec<IndexInfo>) -> Vec<IndexEntry> {
    indexes
        .into_iter()
        .filter(|info| !info.orphaned)
        .map(|info| {
            let workspace_str = info.workspace.as_deref().unwrap_or("");
            IndexEntry {
                hash: info.hash,
                workspace_path: PathBuf::from(workspace_str),
                display_path: shorten_path(workspace_str),
                size_bytes: info.size_bytes,
                files_indexed: info.files_indexed.unwrap_or(0),
                indexed_at: info.indexed_at,
                semantic: info.semantic.unwrap_or(false),
                watch_state: WatchState::Off,
                changes_per_min: 0.0,
                orphaned: info.orphaned,
            }
        })
        .collect()
}

/// Render the UI
fn render(frame: &mut Frame, app: &mut App) {
    let size = frame.area();

    // Main layout: header stats (1), table (60%), log (rest), footer (1)
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),                                 // title bar
            Constraint::Min(5),                                    // table
            Constraint::Length(1),                                 // divider
            Constraint::Length(size.height.saturating_sub(5) / 3), // activity log
            Constraint::Length(1),                                 // footer
        ])
        .split(size);

    render_title_bar(frame, chunks[0], app);
    render_table(frame, chunks[1], app);
    render_activity_log(frame, chunks[3], app);
    render_footer(frame, chunks[4], app);

    // Overlay dialogs
    if app.show_help {
        render_help_overlay(frame, size);
    }

    if let Dialog::ConfirmDelete { ref name, .. } = app.dialog {
        render_confirm_dialog(frame, size, name);
    }
}

fn render_title_bar(frame: &mut Frame, area: Rect, app: &App) {
    let total_indexes = app.entries.len();
    let watching = app
        .entries
        .iter()
        .filter(|e| e.watch_state != WatchState::Off)
        .count();
    let total_files: u64 = app.entries.iter().map(|e| e.files_indexed).sum();

    let title = Line::from(vec![
        Span::styled(
            " ygrep dashboard ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("─── "),
        Span::styled(
            format!("{} indexes", total_indexes),
            Style::default().fg(Color::White),
        ),
        Span::raw(" │ "),
        Span::styled(
            format!("{} watching", watching),
            Style::default().fg(if watching > 0 {
                Color::Green
            } else {
                Color::DarkGray
            }),
        ),
        Span::raw(" │ "),
        Span::styled(
            format!("{} files", total_files),
            Style::default().fg(Color::White),
        ),
    ]);

    frame.render_widget(Paragraph::new(title), area);
}

fn render_table(frame: &mut Frame, area: Rect, app: &mut App) {
    let indicator = match app.sort_order {
        SortOrder::Asc => " \u{25b2}",
        SortOrder::Desc => " \u{25bc}",
    };
    let col_header = |name: &str, col: SortColumn| -> String {
        if app.sort_column == col {
            format!("{}{}", name, indicator)
        } else {
            name.to_string()
        }
    };

    let header = Row::new(vec![
        Cell::from(" #"),
        Cell::from(col_header("Workspace", SortColumn::Name)),
        Cell::from(col_header("Size", SortColumn::Size)),
        Cell::from(col_header("Files", SortColumn::Files)),
        Cell::from(col_header("Indexed", SortColumn::Indexed)),
        Cell::from(col_header("Watch", SortColumn::Watch)),
    ])
    .style(
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    );

    let rows: Vec<Row> = app
        .entries
        .iter()
        .enumerate()
        .map(|(i, entry)| {
            let watch_span = match entry.watch_state {
                WatchState::Active => {
                    let rate = if entry.changes_per_min > 0.0 {
                        format!(" ({:.0}/m)", entry.changes_per_min)
                    } else {
                        String::new()
                    };
                    Span::styled(
                        format!("● active{}", rate),
                        Style::default().fg(Color::Green),
                    )
                }
                WatchState::Sleeping => {
                    Span::styled("○ sleeping", Style::default().fg(Color::Yellow))
                }
                WatchState::Off => Span::styled("  off", Style::default().fg(Color::DarkGray)),
            };

            let indexed_str = entry
                .indexed_at
                .as_ref()
                .map(|dt| format_relative_time(dt))
                .unwrap_or_else(|| "-".to_string());

            Row::new(vec![
                Cell::from(format!("{:>2}", i + 1)),
                Cell::from(entry.display_path.clone()),
                Cell::from(format_size(entry.size_bytes)),
                Cell::from(format!("{}", entry.files_indexed)),
                Cell::from(indexed_str),
                Cell::from(watch_span),
            ])
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Length(3),
            Constraint::Min(20),
            Constraint::Length(6),
            Constraint::Length(7),
            Constraint::Length(10),
            Constraint::Length(18),
        ],
    )
    .header(header)
    .block(Block::default().borders(Borders::NONE))
    .row_highlight_style(
        Style::default()
            .add_modifier(Modifier::BOLD)
            .bg(Color::DarkGray),
    )
    .highlight_symbol("> ");

    frame.render_stateful_widget(table, area, &mut app.table_state);
}

fn render_activity_log(frame: &mut Frame, area: Rect, app: &App) {
    let focus_style = if app.focus == Focus::Log {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let block = Block::default()
        .title(Span::styled(" Activity Log ", focus_style))
        .borders(Borders::TOP);

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if app.activity_log.is_empty() {
        let empty = Paragraph::new(Span::styled(
            "  No activity yet. Watching workspaces will show events here.",
            Style::default().fg(Color::DarkGray),
        ));
        frame.render_widget(empty, inner);
        return;
    }

    let visible_height = inner.height as usize;
    let total = app.activity_log.len();
    let skip = if total > visible_height + app.log_scroll {
        total - visible_height - app.log_scroll
    } else {
        0
    };

    let lines: Vec<Line> = app
        .activity_log
        .iter()
        .skip(skip)
        .take(visible_height)
        .map(|event| {
            let time_str = event.timestamp.format("%H:%M:%S").to_string();
            let kind_color = match event.kind {
                ActivityKind::Indexed => Color::Green,
                ActivityKind::Deleted => Color::Red,
                ActivityKind::StateChange => Color::Yellow,
                ActivityKind::Error => Color::Red,
                ActivityKind::Reindex => Color::Cyan,
            };

            Line::from(vec![
                Span::styled(
                    format!(" {} ", time_str),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(
                    format!("{:<14}", event.workspace_name),
                    Style::default().fg(Color::Blue),
                ),
                Span::styled(event.message.clone(), Style::default().fg(kind_color)),
            ])
        })
        .collect();

    let paragraph = Paragraph::new(lines);
    frame.render_widget(paragraph, inner);
}

fn render_footer(frame: &mut Frame, area: Rect, app: &App) {
    if app.filter_active {
        let footer = Line::from(vec![
            Span::styled(" /", Style::default().fg(Color::Cyan)),
            Span::raw(&app.filter_input),
            Span::styled("_", Style::default().fg(Color::Cyan)),
            Span::raw("  "),
            Span::styled("Enter", Style::default().fg(Color::DarkGray)),
            Span::raw(":apply  "),
            Span::styled("Esc", Style::default().fg(Color::DarkGray)),
            Span::raw(":clear"),
        ]);
        frame.render_widget(Paragraph::new(footer), area);
        return;
    }

    let mut spans = vec![
        Span::styled(" w", Style::default().fg(Color::Cyan)),
        Span::raw(":watch  "),
        Span::styled("r", Style::default().fg(Color::Cyan)),
        Span::raw(":reindex  "),
        Span::styled("d", Style::default().fg(Color::Cyan)),
        Span::raw(":delete  "),
        Span::styled("s", Style::default().fg(Color::Cyan)),
        Span::raw(":sort  "),
        Span::styled("/", Style::default().fg(Color::Cyan)),
        Span::raw(":filter  "),
        Span::styled("?", Style::default().fg(Color::Cyan)),
        Span::raw(":help  "),
        Span::styled("q", Style::default().fg(Color::Cyan)),
        Span::raw(":quit"),
    ];

    // Show active filter indicator
    if !app.filter_input.is_empty() {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            format!("[filter: {}]", app.filter_input),
            Style::default().fg(Color::Yellow),
        ));
    }

    let footer = Line::from(spans);
    frame.render_widget(Paragraph::new(footer), area);
}

fn render_help_overlay(frame: &mut Frame, area: Rect) {
    let help_area = centered_rect(50, 60, area);

    frame.render_widget(Clear, help_area);

    let help_text = vec![
        Line::from(Span::styled(
            "  ygrep dashboard help",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(vec![Span::styled(
            "  Navigation",
            Style::default().add_modifier(Modifier::BOLD),
        )]),
        Line::from("  j/↓       Move down"),
        Line::from("  k/↑       Move up"),
        Line::from("  g         Go to top"),
        Line::from("  G         Go to bottom"),
        Line::from("  Tab       Switch focus (table/log)"),
        Line::from(""),
        Line::from(vec![Span::styled(
            "  Sorting & Filter",
            Style::default().add_modifier(Modifier::BOLD),
        )]),
        Line::from("  s         Cycle sort column"),
        Line::from("  S         Toggle sort order (asc/desc)"),
        Line::from("  /         Filter by name"),
        Line::from(""),
        Line::from(vec![Span::styled(
            "  Actions",
            Style::default().add_modifier(Modifier::BOLD),
        )]),
        Line::from("  w         Toggle watch (off ↔ active)"),
        Line::from("  r         Re-index workspace"),
        Line::from("  d         Delete index (with confirm)"),
        Line::from(""),
        Line::from(vec![Span::styled(
            "  Watch States",
            Style::default().add_modifier(Modifier::BOLD),
        )]),
        Line::from(vec![
            Span::raw("  "),
            Span::styled("● active", Style::default().fg(Color::Green)),
            Span::raw("    File watcher running"),
        ]),
        Line::from(vec![
            Span::raw("  "),
            Span::styled("○ sleeping", Style::default().fg(Color::Yellow)),
            Span::raw("  Idle 5m, polling every 30s"),
        ]),
        Line::from(vec![
            Span::raw("  "),
            Span::styled("  off", Style::default().fg(Color::DarkGray)),
            Span::raw("       Not watching"),
        ]),
        Line::from(""),
        Line::from("  Press any key to close"),
    ];

    let block = Block::default()
        .title(" Help ")
        .borders(Borders::ALL)
        .style(Style::default().fg(Color::White).bg(Color::Black));

    let paragraph = Paragraph::new(help_text).block(block);
    frame.render_widget(paragraph, help_area);
}

fn render_confirm_dialog(frame: &mut Frame, area: Rect, name: &str) {
    let dialog_area = centered_rect(45, 20, area);

    frame.render_widget(Clear, dialog_area);

    let text = vec![
        Line::from(""),
        Line::from(format!("  Delete index for {}?", name)),
        Line::from(""),
        Line::from("  This cannot be undone."),
        Line::from("  Re-run `ygrep index` to rebuild."),
        Line::from(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled("y", Style::default().fg(Color::Red)),
            Span::raw(" = confirm, any other key = cancel"),
        ]),
    ];

    let block = Block::default()
        .title(Span::styled(
            " Confirm Delete ",
            Style::default().fg(Color::Red),
        ))
        .borders(Borders::ALL)
        .style(Style::default().fg(Color::White).bg(Color::Black));

    let paragraph = Paragraph::new(text).block(block);
    frame.render_widget(paragraph, dialog_area);
}

/// Create a centered rectangle
fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

pub fn run() -> Result<()> {
    // Collect all indexes
    let indexes = collect_indexes()?;

    if indexes.is_empty() {
        eprintln!("No indexes found. Index a workspace first:");
        eprintln!("  ygrep index");
        return Ok(());
    }

    let entries = build_entries(indexes);

    if entries.is_empty() {
        eprintln!("No valid (non-orphaned) indexes found.");
        eprintln!("Run `ygrep indexes clean` to remove orphaned indexes.");
        return Ok(());
    }

    // Create the WatchManager
    let (mut manager, cmd_tx, mut event_rx) = ygrep_core::dashboard::WatchManager::new();

    // Register all entries with the manager
    for entry in &entries {
        manager.register(
            entry.hash.clone(),
            entry.workspace_path.clone(),
            entry.semantic,
            entry.indexed_at,
        );
    }

    // Build the app
    let mut app = App::new(entries, cmd_tx.clone());

    // Set up initial watch states from manager registration (auto-watch recently indexed)
    // We need to check which entries the manager decided to auto-watch
    for entry in &mut app.entries {
        if let Some(indexed_at) = entry.indexed_at {
            let age = chrono::Utc::now().signed_duration_since(indexed_at);
            if age.num_seconds() < 4 * 3600 && entry.workspace_path.exists() {
                entry.watch_state = WatchState::Active;
            }
        }
    }
    for entry in &mut app.all_entries {
        if let Some(indexed_at) = entry.indexed_at {
            let age = chrono::Utc::now().signed_duration_since(indexed_at);
            if age.num_seconds() < 4 * 3600 && entry.workspace_path.exists() {
                entry.watch_state = WatchState::Active;
            }
        }
    }

    // Sort after watch states are set
    app.sort_entries();

    // Create tokio runtime
    let rt = tokio::runtime::Runtime::new()?;

    // Spawn the WatchManager
    rt.spawn(async move {
        manager.run().await;
    });

    // Set up terminal with panic hook for clean restoration
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        original_hook(panic_info);
    }));

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Main event loop
    loop {
        // Draw
        terminal.draw(|frame| render(frame, &mut app))?;

        // Poll for keyboard input (250ms timeout)
        if event::poll(std::time::Duration::from_millis(250))? {
            if let Event::Key(key) = event::read()? {
                app.handle_key(key);
                if app.should_quit {
                    break;
                }
            }
        }

        // Drain manager events
        while let Ok(event) = event_rx.try_recv() {
            app.handle_manager_event(event);
        }
    }

    // Shutdown
    let _ = cmd_tx.send(ManagerCommand::Shutdown);

    // Restore terminal
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    Ok(())
}
