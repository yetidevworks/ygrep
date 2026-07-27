//! The ygrep management TUI: index list, watch control, service control, live activity.
//!
//! Bare `ygrep` on a terminal lands here, and so does `ygrep dashboard`. The event loop
//! never blocks: watching and re-indexing go through the [`WatchManager`] channels, and
//! anything else slow (compaction, deletes, service control, the registry scan itself)
//! runs on a worker thread that reports back through [`OpMessage`]. Every result lands in
//! the one status line, so a failed action reads as `✗ …` instead of tearing the TUI down.

mod stats;
mod ui;

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{self, IsTerminal, Stdout};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use chrono::{DateTime, Local, Utc};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, style::Color, Terminal};

use ygrep_core::dashboard::{ManagerCommand, ManagerEvent, WatchState, WorkspaceRegistration};
use ygrep_core::registry::{self, IndexInfo};
use ygrep_core::Config;

use crate::service::{self, ServiceReport};

/// The one accent colour the whole TUI is built from.
pub const ACCENT: Color = Color::Cyan;

/// How many activity lines to keep in memory.
const MAX_ACTIVITY: usize = 500;

/// How often the registry and the service report are re-read, at most.
const REGISTRY_REFRESH: Duration = Duration::from_secs(2);

/// Poll timeout while something is moving (stats view open, an action in flight).
const TICK_BUSY: Duration = Duration::from_millis(250);

/// Poll timeout when the screen is idle.
const TICK_IDLE: Duration = Duration::from_millis(1000);

/// How long to let an aborted watcher release its writer lock before an op takes it.
const WATCHER_RELEASE_GRACE: Duration = Duration::from_millis(300);

/// Which panel has focus.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Panel {
    Indexes,
    Activity,
}

/// Sort column for the Indexes panel, bound to keys 1-4.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SortCol {
    Name,
    Size,
    Age,
    Files,
}

impl SortCol {
    pub fn label(self) -> &'static str {
        match self {
            SortCol::Name => "name",
            SortCol::Size => "size",
            SortCol::Age => "age",
            SortCol::Files => "files",
        }
    }
}

/// One row of the Indexes panel: registry facts plus whatever the session knows live.
pub struct IndexRow {
    pub hash: String,
    pub workspace: PathBuf,
    pub display: String,
    pub index_path: PathBuf,
    pub size_bytes: u64,
    pub files: u64,
    pub segments: Option<usize>,
    pub semantic: bool,
    pub indexed_at: Option<DateTime<Utc>>,
    pub orphaned: bool,
    /// Persisted watch flag — what the background service acts on.
    pub watch: bool,
    /// Watch state of this session's own watcher.
    pub state: WatchState,
    pub changes_per_min: f64,
}

impl IndexRow {
    fn from_info(info: IndexInfo) -> Self {
        let workspace = info.workspace.clone().unwrap_or_default();
        Self {
            hash: info.hash,
            workspace: PathBuf::from(&workspace),
            display: registry::shorten_path(&workspace),
            index_path: info.path,
            size_bytes: info.size_bytes,
            files: info.files_indexed.unwrap_or(0),
            segments: info.segments,
            semantic: info.semantic.unwrap_or(false),
            indexed_at: info.indexed_at,
            orphaned: info.orphaned,
            watch: info.watch,
            state: WatchState::Off,
            changes_per_min: 0.0,
        }
    }

    /// Short name for the activity log.
    fn name(&self) -> String {
        self.workspace
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or_else(|| self.display.clone())
    }
}

/// What an activity line came from, for colouring.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ActivityKind {
    Indexed,
    Deleted,
    State,
    Error,
    Reindex,
    Service,
}

/// One line in the Activity panel.
pub struct Activity {
    pub at: DateTime<Local>,
    pub who: String,
    pub text: String,
    pub kind: ActivityKind,
}

/// A slow action queued behind a watcher that has to stop first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Deferred {
    Reindex,
    Compact,
}

impl Deferred {
    fn verb(self) -> &'static str {
        match self {
            Deferred::Reindex => "re-index",
            Deferred::Compact => "compaction",
        }
    }
}

/// A message from a worker thread back to the event loop.
enum OpMessage {
    /// A registry + service scan finished.
    Scan {
        indexes: Vec<IndexInfo>,
        service: Option<ServiceReport>,
        running: bool,
    },
    /// A slow action finished.
    Done {
        label: String,
        result: std::result::Result<String, String>,
        /// The index it ran against, when it had one
        hash: Option<String>,
    },
}

/// Everything the TUI draws and everything it needs to act.
pub struct App {
    pub rows: Vec<IndexRow>,
    /// Indices into `rows` that pass the filter, in sort order.
    pub view: Vec<usize>,
    pub sel: usize,
    pub focus: Panel,
    pub sort_col: SortCol,
    pub sort_asc: bool,
    pub filter: String,
    pub filter_input: bool,
    pub activity: VecDeque<Activity>,
    /// Whether the activity panel sticks to the newest line.
    pub follow: bool,
    /// Lines scrolled back from the newest line.
    pub activity_scroll: usize,
    pub message: String,
    pub service: Option<ServiceReport>,
    /// Hashes the running service is watching right now.
    pub service_watched: HashSet<String>,
    pub service_running: bool,
    /// `(hash, label)` while a remove confirmation is showing.
    pub confirm_remove: Option<(String, String)>,
    /// Selected entry while the service menu is open.
    pub service_menu: Option<usize>,
    pub help: bool,
    pub stats: Option<stats::StatsView>,
    pub data_dir: PathBuf,
    /// Actions in flight, used to pick the tick rate.
    pub busy: usize,
    cmd_tx: Option<tokio::sync::mpsc::UnboundedSender<ManagerCommand>>,
    ops_tx: mpsc::Sender<OpMessage>,
    ops_rx: mpsc::Receiver<OpMessage>,
    /// When each watched index last reported a change, for the per-minute rate.
    change_times: HashMap<String, VecDeque<Instant>>,
    deferred: HashMap<String, Deferred>,
    resume_watch: HashSet<String>,
    refreshing: bool,
    last_refresh: Instant,
    log_offset: u64,
    should_quit: bool,
}

/// The service menu entries, in order.
pub const SERVICE_ACTIONS: [(&str, &str); 5] = [
    ("install", "write the service definition and start it"),
    ("start", "start the installed service"),
    ("stop", "stop the running service"),
    ("restart", "restart, re-reading the definition"),
    ("uninstall", "stop it and remove the definition"),
];

impl App {
    fn new(
        data_dir: PathBuf,
        cmd_tx: Option<tokio::sync::mpsc::UnboundedSender<ManagerCommand>>,
    ) -> Self {
        let (ops_tx, ops_rx) = mpsc::channel();
        Self {
            rows: Vec::new(),
            view: Vec::new(),
            sel: 0,
            focus: Panel::Indexes,
            sort_col: SortCol::Size,
            sort_asc: false,
            filter: String::new(),
            filter_input: false,
            activity: VecDeque::with_capacity(MAX_ACTIVITY),
            follow: true,
            activity_scroll: 0,
            message: String::new(),
            service: None,
            service_watched: HashSet::new(),
            service_running: false,
            confirm_remove: None,
            service_menu: None,
            help: false,
            stats: None,
            data_dir,
            busy: 0,
            cmd_tx,
            ops_tx,
            ops_rx,
            change_times: HashMap::new(),
            deferred: HashMap::new(),
            resume_watch: HashSet::new(),
            refreshing: false,
            last_refresh: Instant::now(),
            log_offset: 0,
            should_quit: false,
        }
    }

    /// Record the outcome of an action in the status line. Errors never quit the TUI.
    fn act(&mut self, label: &str, result: std::result::Result<String, String>) {
        self.message = match result {
            Ok(msg) => format!("✓ {msg}"),
            Err(e) => format!("✗ {label}: {e}"),
        };
    }

    fn note(&mut self, msg: impl Into<String>) {
        self.message = msg.into();
    }

    pub fn selected(&self) -> Option<&IndexRow> {
        self.view.get(self.sel).and_then(|i| self.rows.get(*i))
    }

    fn selected_hash(&self) -> Option<String> {
        self.selected().map(|row| row.hash.clone())
    }

    fn row(&self, hash: &str) -> Option<&IndexRow> {
        self.rows.iter().find(|row| row.hash == hash)
    }

    fn row_mut(&mut self, hash: &str) -> Option<&mut IndexRow> {
        self.rows.iter_mut().find(|row| row.hash == hash)
    }

    fn name_of(&self, hash: &str) -> String {
        self.row(hash)
            .map(|row| row.name())
            .unwrap_or_else(|| hash.chars().take(8).collect())
    }

    /// True when the background service — not this session — is watching `hash`.
    pub fn watched_by_service(&self, hash: &str) -> bool {
        self.service_running && self.service_watched.contains(hash)
    }

    fn send(&self, cmd: ManagerCommand) {
        if let Some(tx) = &self.cmd_tx {
            let _ = tx.send(cmd);
        }
    }

    fn push(&mut self, kind: ActivityKind, who: impl Into<String>, text: impl Into<String>) {
        if self.activity.len() >= MAX_ACTIVITY {
            self.activity.pop_front();
        }
        self.activity.push_back(Activity {
            at: Local::now(),
            who: who.into(),
            text: text.into(),
            kind,
        });
        if self.follow {
            self.activity_scroll = 0;
        }
    }

    /// Re-sort `rows` and rebuild the filtered view, keeping the selected index selected.
    fn resort(&mut self) {
        let selected = self.selected_hash();
        self.resort_keeping(selected.as_deref());
    }

    /// Re-sort and reselect `keep`, for when the rows were replaced wholesale.
    fn resort_keeping(&mut self, keep: Option<&str>) {
        let (col, asc) = (self.sort_col, self.sort_asc);
        self.rows.sort_by(|a, b| {
            let ord = match col {
                SortCol::Name => a.display.to_lowercase().cmp(&b.display.to_lowercase()),
                SortCol::Size => a.size_bytes.cmp(&b.size_bytes),
                SortCol::Age => a.indexed_at.cmp(&b.indexed_at),
                SortCol::Files => a.files.cmp(&b.files),
            };
            let ord = if asc { ord } else { ord.reverse() };
            ord.then_with(|| a.display.to_lowercase().cmp(&b.display.to_lowercase()))
        });
        self.rebuild_view(keep);
    }

    fn rebuild_view(&mut self, keep: Option<&str>) {
        let needle = self.filter.to_lowercase();
        self.view = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| needle.is_empty() || row.display.to_lowercase().contains(&needle))
            .map(|(i, _)| i)
            .collect();

        self.sel = keep
            .and_then(|hash| self.view.iter().position(|i| self.rows[*i].hash == hash))
            .unwrap_or(self.sel);
        self.clamp_selection();
    }

    fn clamp_selection(&mut self) {
        if self.view.is_empty() {
            self.sel = 0;
        } else if self.sel >= self.view.len() {
            self.sel = self.view.len() - 1;
        }
    }

    fn move_sel(&mut self, delta: isize) {
        match self.focus {
            Panel::Indexes => {
                if self.view.is_empty() {
                    return;
                }
                let next = (self.sel as isize + delta).clamp(0, self.view.len() as isize - 1);
                self.sel = next as usize;
            }
            Panel::Activity => {
                // Up scrolls back through history, down returns toward the newest line.
                let back = -delta;
                let next = self.activity_scroll as isize + back;
                self.activity_scroll = next.clamp(0, self.activity.len() as isize) as usize;
                self.follow = self.activity_scroll == 0;
            }
        }
    }

    /// Poll-timeout work: rescan the registry, tail the service log, poll telemetry.
    fn on_tick(&mut self) {
        if !self.refreshing && self.last_refresh.elapsed() >= REGISTRY_REFRESH {
            self.spawn_scan();
        }
        self.decay_rates();
        self.tail_service_log();
        let data_dir = self.data_dir.clone();
        if let Some(stats) = self.stats.as_mut() {
            stats.poll(&data_dir);
        }
    }

    /// Age out change timestamps so a workspace that went quiet stops reading as busy.
    fn decay_rates(&mut self) {
        let now = Instant::now();
        let mut rates = Vec::new();
        self.change_times.retain(|hash, times| {
            while times
                .front()
                .is_some_and(|t| now.duration_since(*t).as_secs() > 60)
            {
                times.pop_front();
            }
            rates.push((hash.clone(), times.len() as f64));
            !times.is_empty()
        });
        for (hash, rate) in rates {
            if let Some(row) = self.row_mut(&hash) {
                row.changes_per_min = rate;
            }
        }
    }

    /// Re-read the registry and the service state off the event loop.
    ///
    /// The registry walk sizes every index directory and the service report shells out to
    /// launchctl/systemctl, so neither belongs on the drawing thread.
    fn spawn_scan(&mut self) {
        self.refreshing = true;
        self.last_refresh = Instant::now();
        let tx = self.ops_tx.clone();
        std::thread::spawn(move || {
            let indexes = registry::collect_indexes().unwrap_or_default();
            let service = service::report().ok();
            let running = service
                .as_ref()
                .map(|report| report.status.running())
                .unwrap_or(false)
                || service::is_running();
            let _ = tx.send(OpMessage::Scan {
                indexes,
                service,
                running,
            });
        });
    }

    /// Fold a finished scan into the rows, keeping live session state.
    fn apply_scan(
        &mut self,
        indexes: Vec<IndexInfo>,
        service: Option<ServiceReport>,
        running: bool,
    ) {
        self.refreshing = false;
        self.service_watched = service
            .as_ref()
            .and_then(|report| report.heartbeat.as_ref())
            .map(|state| state.watched.iter().cloned().collect())
            .unwrap_or_default();
        self.service = service;
        self.service_running = running;

        let selected = self.selected_hash();
        let mut live: HashMap<String, (WatchState, f64)> = HashMap::new();
        for row in &self.rows {
            live.insert(row.hash.clone(), (row.state.clone(), row.changes_per_min));
        }

        let known: HashSet<String> = self.rows.iter().map(|row| row.hash.clone()).collect();
        let mut fresh = Vec::with_capacity(indexes.len());
        for info in indexes {
            let is_new = !known.contains(&info.hash);
            let mut row = IndexRow::from_info(info);
            if let Some((state, rate)) = live.get(&row.hash) {
                row.state = state.clone();
                row.changes_per_min = *rate;
            }
            if is_new && !row.orphaned {
                // An index built while the TUI was open still has to be watchable.
                self.send(ManagerCommand::Register(WorkspaceRegistration {
                    hash: row.hash.clone(),
                    workspace_path: row.workspace.clone(),
                    semantic: row.semantic,
                    indexed_at: row.indexed_at,
                    watch: row.watch && !self.watched_by_service(&row.hash),
                }));
            }
            fresh.push(row);
        }

        // The old view indexes into the rows that were just replaced, so it goes first.
        self.rows = fresh;
        self.view.clear();
        self.sel = 0;
        self.resort_keeping(selected.as_deref());
    }

    /// Pull new lines out of the service log into the activity panel.
    fn tail_service_log(&mut self) {
        use std::io::{Read, Seek, SeekFrom};

        let path = service::log_path_in(&self.data_dir);
        let Ok(mut file) = std::fs::File::open(&path) else {
            return;
        };
        let len = file.metadata().map(|m| m.len()).unwrap_or(0);
        if self.log_offset == 0 && len > 0 {
            // First look: start at the end so the panel isn't flooded with old history.
            self.log_offset = len;
            return;
        }
        if len < self.log_offset {
            self.log_offset = 0;
        }
        if len == self.log_offset || file.seek(SeekFrom::Start(self.log_offset)).is_err() {
            return;
        }
        let mut buf = Vec::new();
        if file.read_to_end(&mut buf).is_err() {
            return;
        }
        self.log_offset = len;
        let body = String::from_utf8_lossy(&buf).to_string();
        for line in body.lines() {
            let line = line.trim();
            if !line.is_empty() {
                self.push(ActivityKind::Service, "service", line.to_string());
            }
        }
    }

    /// Drain results from the worker threads.
    fn drain_ops(&mut self) {
        while let Ok(msg) = self.ops_rx.try_recv() {
            match msg {
                OpMessage::Scan {
                    indexes,
                    service,
                    running,
                } => self.apply_scan(indexes, service, running),
                OpMessage::Done {
                    label,
                    result,
                    hash,
                } => {
                    self.busy = self.busy.saturating_sub(1);
                    if let Ok(msg) = &result {
                        self.push(ActivityKind::Reindex, "ygrep", msg.clone());
                    }
                    if let Err(e) = &result {
                        self.push(ActivityKind::Error, "ygrep", format!("{label}: {e}"));
                    }
                    self.act(&label, result);
                    if let Some(hash) = hash {
                        self.resume_after_op(&hash);
                    }
                    self.last_refresh = Instant::now() - REGISTRY_REFRESH;
                }
            }
        }
    }

    /// Restart the session watcher an action had to pause.
    fn resume_after_op(&mut self, hash: &str) {
        if self.resume_watch.remove(hash) {
            self.send(ManagerCommand::SetWatch {
                hash: hash.to_string(),
                enabled: true,
            });
        }
    }

    /// Note a file change and refresh the index's changes-per-minute figure.
    fn record_change(&mut self, hash: &str) {
        let now = Instant::now();
        let times = self.change_times.entry(hash.to_string()).or_default();
        times.push_back(now);
        while times
            .front()
            .is_some_and(|t| now.duration_since(*t).as_secs() > 60)
        {
            times.pop_front();
        }
        let rate = times.len() as f64;
        if let Some(row) = self.row_mut(hash) {
            row.changes_per_min = rate;
        }
    }

    fn handle_manager_event(&mut self, event: ManagerEvent) {
        match event {
            ManagerEvent::WatchStateChanged { hash, new_state } => {
                let name = self.name_of(&hash);
                let text = match new_state {
                    WatchState::Active => "watching".to_string(),
                    WatchState::Sleeping => "sleeping (idle)".to_string(),
                    WatchState::Off => "watch off".to_string(),
                };
                if let Some(row) = self.row_mut(&hash) {
                    row.state = new_state.clone();
                }
                self.push(ActivityKind::State, name, text);
                // An action waiting for the watcher to let go of the index can start now.
                if new_state == WatchState::Off {
                    if let Some(op) = self.deferred.remove(&hash) {
                        self.launch(op, &hash);
                    }
                }
            }
            ManagerEvent::FileIndexed { hash, path } => {
                let name = self.name_of(&hash);
                self.record_change(&hash);
                self.push(ActivityKind::Indexed, name, format!("[+] {path}"));
            }
            ManagerEvent::FileDeleted { hash, path } => {
                let name = self.name_of(&hash);
                self.push(ActivityKind::Deleted, name, format!("[-] {path}"));
            }
            ManagerEvent::Error { hash, message } => {
                let name = self.name_of(&hash);
                self.push(ActivityKind::Error, name, message);
            }
            ManagerEvent::ReindexStarted { hash } => {
                let name = self.name_of(&hash);
                self.push(ActivityKind::Reindex, name, "re-indexing…");
            }
            ManagerEvent::ReindexCompleted {
                hash,
                files_indexed,
            } => {
                let name = self.name_of(&hash);
                let now = Utc::now();
                if let Some(row) = self.row_mut(&hash) {
                    row.files = files_indexed;
                    row.indexed_at = Some(now);
                }
                self.busy = self.busy.saturating_sub(1);
                self.push(
                    ActivityKind::Reindex,
                    name.clone(),
                    format!("re-index complete ({files_indexed} files)"),
                );
                self.note(format!("✓ re-indexed {name} ({files_indexed} files)"));
                self.resume_after_op(&hash);
                self.last_refresh = Instant::now() - REGISTRY_REFRESH;
            }
            ManagerEvent::IndexRemoved { hash } => {
                self.rows.retain(|row| row.hash != hash);
                self.resort();
            }
            ManagerEvent::Log { hash, message } => {
                let name = self.name_of(&hash);
                let text = message.trim_start_matches('\r').trim().to_string();
                if !text.is_empty() {
                    self.push(ActivityKind::Indexed, name, text);
                }
            }
        }
    }

    /// Toggle this session's watcher for the selected index.
    fn toggle_session_watch(&mut self) {
        let Some(row) = self.selected() else { return };
        let (hash, name, orphaned, watching) = (
            row.hash.clone(),
            row.name(),
            row.orphaned,
            row.state != WatchState::Off,
        );
        if orphaned {
            self.note("✗ the workspace is gone — remove the index instead");
            return;
        }
        if self.watched_by_service(&hash) {
            self.note(format!(
                "✗ {name} is watched by the service — press w to turn its watch flag off"
            ));
            return;
        }
        self.send(ManagerCommand::SetWatch {
            hash,
            enabled: !watching,
        });
        self.note(if watching {
            format!("stopping the watcher on {name}")
        } else {
            format!("watching {name} for this session")
        });
    }

    /// Flip the persisted watch flag for the selected index.
    fn toggle_watch_flag(&mut self) {
        let Some(row) = self.selected() else { return };
        let (hash, name, path, enable, semantic, workspace, indexed_at) = (
            row.hash.clone(),
            row.name(),
            row.index_path.clone(),
            !row.watch,
            row.semantic,
            row.workspace.clone(),
            row.indexed_at,
        );

        if let Err(e) = registry::set_watch_flag(&path, enable) {
            self.act("watch flag", Err(e.to_string()));
            return;
        }
        if let Some(row) = self.row_mut(&hash) {
            row.watch = enable;
        }

        if self.service_running {
            self.note(format!(
                "✓ watch {} for {name} — the service picks it up in ≤30s",
                if enable { "on" } else { "off" }
            ));
            return;
        }

        // With no service running, the session watcher is what makes the flag felt.
        self.send(ManagerCommand::Register(WorkspaceRegistration {
            hash,
            workspace_path: workspace,
            semantic,
            indexed_at,
            watch: enable,
        }));
        self.note(format!(
            "✓ watch {} for {name}",
            if enable { "on" } else { "off" }
        ));
    }

    /// Start a slow action, pausing the session watcher first when there is one.
    fn request(&mut self, op: Deferred) {
        let Some(row) = self.selected() else { return };
        let (hash, name, orphaned, watching) = (
            row.hash.clone(),
            row.name(),
            row.orphaned,
            row.state != WatchState::Off,
        );

        if orphaned {
            self.note("✗ the workspace is gone — remove the index instead");
            return;
        }
        if self.watched_by_service(&hash) {
            self.note("✗ watched by service — turn watch off first or let auto-refresh handle it");
            return;
        }
        if watching {
            // Phase 1 made a second writer fail fast, so the watcher stops before the
            // action starts and comes back once it is done.
            self.resume_watch.insert(hash.clone());
            self.deferred.insert(hash.clone(), op);
            self.send(ManagerCommand::SetWatch {
                hash,
                enabled: false,
            });
            self.note(format!("pausing the watcher on {name} for {}", op.verb()));
            return;
        }

        self.launch(op, &hash);
    }

    fn launch(&mut self, op: Deferred, hash: &str) {
        let name = self.name_of(hash);
        match op {
            Deferred::Reindex => {
                self.busy += 1;
                self.send(ManagerCommand::Reindex(hash.to_string()));
                self.note(format!("re-indexing {name}…"));
            }
            Deferred::Compact => {
                let Some(path) = self.row(hash).map(|row| row.index_path.clone()) else {
                    return;
                };
                self.busy += 1;
                self.note(format!("compacting {name}…"));
                self.push(ActivityKind::Reindex, name.clone(), "compacting…");
                let tx = self.ops_tx.clone();
                let hash = hash.to_string();
                std::thread::spawn(move || {
                    // Give an aborted watcher a moment to drop its writer.
                    std::thread::sleep(WATCHER_RELEASE_GRACE);
                    let result = match ygrep_core::index::compact_index(&path) {
                        Ok(stats) => Ok(format!(
                            "compacted {name}: {} segments into {}",
                            stats.segments_before, stats.segments_after
                        )),
                        Err(e) => Err(e.to_string()),
                    };
                    let _ = tx.send(OpMessage::Done {
                        label: "compact".to_string(),
                        result,
                        hash: Some(hash),
                    });
                });
            }
        }
    }

    /// Delete the selected index off the event loop, watcher first.
    fn remove_index(&mut self, hash: String) {
        let name = self.name_of(&hash);
        let Some(path) = self.row(&hash).map(|row| row.index_path.clone()) else {
            return;
        };
        self.resume_watch.remove(&hash);
        self.deferred.remove(&hash);
        self.send(ManagerCommand::RemoveIndex(hash.clone()));
        self.busy += 1;
        self.note(format!("removing {name}…"));

        let tx = self.ops_tx.clone();
        std::thread::spawn(move || {
            std::thread::sleep(WATCHER_RELEASE_GRACE);
            let result = match crate::commands::indexes::get_indexes_dir() {
                Ok(indexes_dir) => crate::commands::indexes::remove_index_dir(&indexes_dir, &path)
                    .map(|()| format!("removed the index for {name}"))
                    .map_err(|e| e.to_string()),
                Err(e) => Err(e.to_string()),
            };
            let _ = tx.send(OpMessage::Done {
                label: "remove".to_string(),
                result,
                hash: None,
            });
        });
    }

    /// Open the selected workspace in the desktop file manager.
    fn open_workspace(&mut self) {
        let Some(row) = self.selected() else { return };
        let (path, display) = (row.workspace.clone(), row.display.clone());
        if !path.exists() {
            self.note("✗ the workspace is gone");
            return;
        }
        let opener = if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        };
        match std::process::Command::new(opener).arg(&path).spawn() {
            Ok(_) => self.note(format!("✓ opened {display}")),
            Err(e) => self.act("open", Err(e.to_string())),
        }
    }

    /// Run a service action on a worker thread.
    fn run_service_action(&mut self, index: usize) {
        let Some((action, _)) = SERVICE_ACTIONS.get(index) else {
            return;
        };
        let action = *action;
        self.busy += 1;
        self.note(format!("{action}ing the service…"));
        let tx = self.ops_tx.clone();
        std::thread::spawn(move || {
            let result = match action {
                "install" => service::install().map(|report| {
                    format!(
                        "{} the service ({})",
                        if report.refreshed {
                            "refreshed"
                        } else {
                            "installed"
                        },
                        report.unit_path.display()
                    )
                }),
                "uninstall" => service::uninstall().map(|()| "removed the service".to_string()),
                "start" => service::start().map(|()| "started the service".to_string()),
                "stop" => service::stop().map(|()| "stopped the service".to_string()),
                "restart" => service::restart().map(|()| "restarted the service".to_string()),
                _ => Ok(String::new()),
            };
            let _ = tx.send(OpMessage::Done {
                label: format!("service {action}"),
                result: result.map_err(|e| e.to_string()),
                hash: None,
            });
        });
    }
}

/// Decide whether the manager starts watching a workspace as soon as it is registered.
///
/// Mirrors the manager's own rule so the first frame agrees with what it did.
fn starts_watching(row: &IndexRow, auto_recent: bool) -> bool {
    if !row.workspace.exists() {
        return false;
    }
    if row.watch {
        return true;
    }
    auto_recent
        && row
            .indexed_at
            .map(|at| Utc::now().signed_duration_since(at).num_seconds() < 4 * 3600)
            .unwrap_or(false)
}

/// Launch the TUI. Requires a terminal on both ends.
pub fn run() -> Result<()> {
    if !io::stdout().is_terminal() || !io::stdin().is_terminal() {
        return Err(anyhow!(
            "The ygrep TUI needs an interactive terminal.\n\
             Try `ygrep indexes list`, `ygrep status` or `ygrep service status` instead."
        ));
    }

    let config = Config::load();
    let data_dir = registry::data_dir(&config)?;
    let indexes = registry::collect_indexes()?;
    let report = service::report().ok();
    let service_running = report
        .as_ref()
        .map(|report| report.status.running())
        .unwrap_or(false)
        || service::is_running();
    let service_watched: HashSet<String> = report
        .as_ref()
        .and_then(|report| report.heartbeat.as_ref())
        .map(|state| state.watched.iter().cloned().collect())
        .unwrap_or_default();

    let (mut manager, cmd_tx, mut event_rx) = ygrep_core::dashboard::WatchManager::new();
    // With the service up it already owns the watching; this session only covers what
    // the service is not, and never starts a watcher nobody asked for.
    let auto_recent = !service_running;
    manager.set_auto_watch_recent(auto_recent);

    let mut app = App::new(data_dir, Some(cmd_tx.clone()));
    app.service = report;
    app.service_running = service_running;
    app.service_watched = service_watched;
    app.rows = indexes.into_iter().map(IndexRow::from_info).collect();

    for row in &mut app.rows {
        if row.orphaned {
            continue;
        }
        if app.service_running && app.service_watched.contains(&row.hash) {
            continue;
        }
        manager.register(
            row.hash.clone(),
            row.workspace.clone(),
            row.semantic,
            row.indexed_at,
            row.watch,
        );
        if starts_watching(row, auto_recent) {
            row.state = WatchState::Active;
        }
    }
    app.resort();
    app.note(format!("{} indexes · press ? for keys", app.rows.len()));

    let rt = tokio::runtime::Runtime::new()?;
    rt.spawn(async move {
        manager.run().await;
    });

    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        original_hook(info);
    }));

    let mut terminal = setup_terminal()?;
    let result = run_loop(&mut terminal, &mut app, &mut event_rx);
    restore_terminal(&mut terminal)?;
    let _ = cmd_tx.send(ManagerCommand::Shutdown);
    // Give the watchers a moment to commit, then leave rather than block the shell on a
    // blocking task that is mid-index.
    rt.shutdown_timeout(Duration::from_millis(500));
    result
}

fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    event_rx: &mut tokio::sync::mpsc::UnboundedReceiver<ManagerEvent>,
) -> Result<()> {
    loop {
        terminal.draw(|f| ui::render(f, app))?;

        let tick = if app.stats.is_some() || app.busy > 0 {
            TICK_BUSY
        } else {
            TICK_IDLE
        };
        if event::poll(tick)? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    handle_key(app, key.code, key.modifiers);
                }
            }
        } else {
            app.on_tick();
        }

        while let Ok(event) = event_rx.try_recv() {
            app.handle_manager_event(event);
        }
        app.drain_ops();

        if app.should_quit {
            return Ok(());
        }
    }
}

fn handle_key(app: &mut App, code: KeyCode, mods: KeyModifiers) {
    if code == KeyCode::Char('c') && mods.contains(KeyModifiers::CONTROL) {
        app.should_quit = true;
        return;
    }

    // The stats view and the modals capture everything while they are showing.
    if app.stats.is_some() {
        stats::handle_key(app, code);
        return;
    }
    if app.help {
        app.help = false;
        return;
    }
    if app.confirm_remove.is_some() {
        handle_confirm_key(app, code);
        return;
    }
    if app.service_menu.is_some() {
        handle_service_menu_key(app, code);
        return;
    }
    if app.filter_input {
        handle_filter_key(app, code);
        return;
    }

    match code {
        KeyCode::Char('q') => app.should_quit = true,
        KeyCode::Esc => {
            if app.filter.is_empty() {
                app.should_quit = true;
            } else {
                app.filter.clear();
                let keep = app.selected_hash();
                app.rebuild_view(keep.as_deref());
            }
        }
        KeyCode::Char('?') => app.help = true,
        KeyCode::Tab | KeyCode::BackTab => {
            app.focus = match app.focus {
                Panel::Indexes => Panel::Activity,
                Panel::Activity => Panel::Indexes,
            };
        }
        KeyCode::Down | KeyCode::Char('j') => app.move_sel(1),
        KeyCode::Up | KeyCode::Char('k') => app.move_sel(-1),
        KeyCode::PageDown => app.move_sel(10),
        KeyCode::PageUp => app.move_sel(-10),
        KeyCode::Home => match app.focus {
            Panel::Indexes => app.sel = 0,
            Panel::Activity => {
                app.activity_scroll = app.activity.len();
                app.follow = false;
            }
        },
        KeyCode::End => match app.focus {
            Panel::Indexes => {
                app.sel = app.view.len().saturating_sub(1);
            }
            Panel::Activity => {
                app.activity_scroll = 0;
                app.follow = true;
            }
        },
        KeyCode::Enter => app.toggle_session_watch(),
        KeyCode::Char('w') => app.toggle_watch_flag(),
        KeyCode::Char('i') => app.request(Deferred::Reindex),
        KeyCode::Char('c') => app.request(Deferred::Compact),
        KeyCode::Char('R') | KeyCode::Delete => {
            if let Some(row) = app.selected() {
                app.confirm_remove = Some((row.hash.clone(), row.display.clone()));
            }
        }
        KeyCode::Char('o') => app.open_workspace(),
        KeyCode::Char('g') => {
            app.follow = !app.follow;
            if app.follow {
                app.activity_scroll = 0;
            }
            let state = if app.follow { "following" } else { "paused" };
            app.note(format!("activity {state}"));
        }
        KeyCode::Char('t') => {
            app.stats = Some(stats::StatsView::open(&app.data_dir));
        }
        KeyCode::Char('S') => app.service_menu = Some(0),
        KeyCode::Char('/') => app.filter_input = true,
        KeyCode::Char(c @ '1'..='4') => {
            let col = match c {
                '1' => SortCol::Name,
                '2' => SortCol::Size,
                '3' => SortCol::Age,
                _ => SortCol::Files,
            };
            if app.sort_col == col {
                app.sort_asc = !app.sort_asc;
            } else {
                app.sort_col = col;
                app.sort_asc = matches!(col, SortCol::Name);
            }
            app.resort();
        }
        _ => {}
    }
}

fn handle_filter_key(app: &mut App, code: KeyCode) {
    match code {
        KeyCode::Enter => app.filter_input = false,
        KeyCode::Esc => {
            app.filter_input = false;
            app.filter.clear();
            let keep = app.selected_hash();
            app.rebuild_view(keep.as_deref());
        }
        KeyCode::Backspace => {
            app.filter.pop();
            let keep = app.selected_hash();
            app.rebuild_view(keep.as_deref());
        }
        KeyCode::Char(c) => {
            app.filter.push(c);
            let keep = app.selected_hash();
            app.rebuild_view(keep.as_deref());
        }
        _ => {}
    }
}

fn handle_confirm_key(app: &mut App, code: KeyCode) {
    match code {
        KeyCode::Char('y') | KeyCode::Char('Y') => {
            if let Some((hash, _)) = app.confirm_remove.take() {
                app.remove_index(hash);
            }
        }
        _ => app.confirm_remove = None,
    }
}

fn handle_service_menu_key(app: &mut App, code: KeyCode) {
    let sel = app.service_menu.unwrap_or(0);
    match code {
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('S') => app.service_menu = None,
        KeyCode::Down | KeyCode::Char('j') => {
            app.service_menu = Some((sel + 1) % SERVICE_ACTIONS.len());
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.service_menu = Some((sel + SERVICE_ACTIONS.len() - 1) % SERVICE_ACTIONS.len());
        }
        KeyCode::Enter => {
            app.service_menu = None;
            app.run_service_action(sel);
        }
        _ => {}
    }
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    Ok(Terminal::new(CrosstermBackend::new(stdout))?)
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

/// Render one frame with synthetic data and return it as plain text.
///
/// This is how the layout gets checked without a terminal: `ygrep tui-snapshot`.
pub fn snapshot(width: u16, height: u16, view: &str) -> Result<String> {
    let mut app = synthetic_app();
    if view == "stats" {
        app.stats = Some(stats::StatsView::synthetic());
    }

    let backend = ratatui::backend::TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend)?;
    terminal.draw(|f| ui::render(f, &app))?;

    let buffer = terminal.backend().buffer();
    let mut out = String::new();
    for y in 0..height {
        for x in 0..width {
            if let Some(cell) = buffer.cell((x, y)) {
                out.push_str(cell.symbol());
            }
        }
        out.push('\n');
    }
    Ok(out)
}

/// Stable fake index hash, so the synthetic telemetry names the synthetic workspaces.
fn synthetic_hash(i: usize) -> String {
    format!("{:016x}", 0x51a7_0000_u64 + i as u64 * 0x1111)
}

/// A believable dashboard with no filesystem behind it: watched, sleeping, semantic,
/// service-watched and orphaned rows, a running service, and a busy activity log.
fn synthetic_app() -> App {
    use crate::service::{ServiceState, ServiceStatus};

    let mut app = App::new(PathBuf::from("/tmp/ygrep-snapshot"), None);
    let now = Utc::now();

    struct Spec {
        path: &'static str,
        size: u64,
        files: u64,
        segments: usize,
        semantic: bool,
        age_min: i64,
        watch: bool,
        state: WatchState,
        rate: f64,
    }

    let spec = |path, size, files, segments, semantic, age_min, watch, state, rate| Spec {
        path,
        size,
        files,
        segments,
        semantic,
        age_min,
        watch,
        state,
        rate,
    };

    let specs = [
        spec(
            "~/Projects/yetidevworks/ygrep",
            412_140_236,
            8_412,
            6,
            true,
            2,
            true,
            WatchState::Active,
            14.0,
        ),
        spec(
            "~/Projects/getgrav/grav",
            97_517_568,
            3_190,
            11,
            false,
            41,
            true,
            WatchState::Active,
            0.0,
        ),
        spec(
            "~/Projects/yetidevworks/reeve",
            18_874_368,
            742,
            3,
            false,
            190,
            false,
            WatchState::Sleeping,
            0.0,
        ),
        spec(
            "~/work/acme-monorepo-frontend",
            1_932_735_283,
            41_338,
            28,
            true,
            1_460,
            true,
            WatchState::Off,
            0.0,
        ),
        spec(
            "~/Sites/scratch",
            694_272,
            38,
            1,
            false,
            8_600,
            false,
            WatchState::Off,
            0.0,
        ),
        spec(
            "~/Projects/deleted-experiment",
            2_097_152,
            120,
            2,
            false,
            26_000,
            false,
            WatchState::Off,
            0.0,
        ),
    ];

    for (i, spec) in specs.into_iter().enumerate() {
        app.rows.push(IndexRow {
            hash: synthetic_hash(i),
            workspace: PathBuf::from(spec.path),
            display: spec.path.to_string(),
            index_path: PathBuf::from("/tmp/ygrep-snapshot/indexes").join(format!("idx{i}")),
            size_bytes: spec.size,
            files: spec.files,
            segments: Some(spec.segments),
            semantic: spec.semantic,
            indexed_at: Some(now - chrono::Duration::minutes(spec.age_min)),
            orphaned: i == 5,
            watch: spec.watch,
            state: spec.state,
            changes_per_min: spec.rate,
        });
    }

    // The second row stands in for an index the background service owns.
    let service_hash = app.rows[1].hash.clone();
    app.service_watched.insert(service_hash.clone());
    app.service_running = true;
    app.service = Some(ServiceReport {
        status: ServiceStatus::Installed {
            running: true,
            pid: Some(4821),
            failed: false,
        },
        label: "com.yetidevworks.ygrep".to_string(),
        unit_path: Some(PathBuf::from(
            "~/Library/LaunchAgents/com.yetidevworks.ygrep.plist",
        )),
        log_path: PathBuf::from("~/.ygrep/logs/service.log"),
        data_dir: PathBuf::from("~/.ygrep"),
        indexes: 6,
        watch_enabled: 3,
        heartbeat: Some(ServiceState {
            pid: 4821,
            started_at: now - chrono::Duration::hours(6) - chrono::Duration::minutes(12),
            last_rescan: now - chrono::Duration::seconds(11),
            watched: vec![service_hash],
            registered: 5,
            log: PathBuf::from("~/.ygrep/logs/service.log"),
            rescan_secs: 30,
        }),
    });

    let lines: [(ActivityKind, &str, &str); 9] = [
        (ActivityKind::State, "ygrep", "watching"),
        (
            ActivityKind::Indexed,
            "ygrep",
            "[+] crates/ygrep-cli/src/commands/tui/ui.rs",
        ),
        (
            ActivityKind::Indexed,
            "ygrep",
            "[+] crates/ygrep-cli/src/commands/tui/mod.rs",
        ),
        (
            ActivityKind::Service,
            "service",
            "rescan: 6 indexes, 3 watched",
        ),
        (
            ActivityKind::Deleted,
            "grav",
            "[-] system/src/Grav/Common/Old.php",
        ),
        (
            ActivityKind::Indexed,
            "grav",
            "compacted 19 segments into 4",
        ),
        (
            ActivityKind::Reindex,
            "reeve",
            "re-index complete (742 files)",
        ),
        (ActivityKind::Error, "scratch", "watcher: permission denied"),
        (ActivityKind::State, "reeve", "sleeping (idle)"),
    ];
    for (kind, who, text) in lines {
        app.push(kind, who, text);
    }

    app.resort();
    app.note("✓ watch on for grav — the service picks it up in ≤30s");
    app
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_dashboard_renders_its_panels() {
        let out = snapshot(100, 30, "dashboard").unwrap();

        assert!(out.contains("ygrep"), "title bar missing:\n{out}");
        assert!(out.contains("service"), "service chip missing:\n{out}");
        assert!(out.contains("Indexes"), "indexes panel missing:\n{out}");
        assert!(out.contains("Service"), "service panel missing:\n{out}");
        assert!(out.contains("Activity"), "activity panel missing:\n{out}");
        assert!(out.contains("[w]"), "watch-flag marker missing:\n{out}");
        assert!(out.contains("svc"), "service-watched tag missing:\n{out}");
        assert!(out.contains("●"), "status dot missing:\n{out}");
        assert!(out.contains("sort: size"), "sort indicator missing:\n{out}");
        assert!(out.contains("reindex"), "key bar missing:\n{out}");
    }

    #[test]
    fn the_stats_view_renders_its_panels() {
        let out = snapshot(100, 30, "stats").unwrap();

        assert!(out.contains("query stats"), "title missing:\n{out}");
        assert!(
            out.contains("Queries/sec"),
            "sparkline title missing:\n{out}"
        );
        assert!(out.contains("Totals"), "totals panel missing:\n{out}");
        assert!(out.contains("Top queries"), "top queries missing:\n{out}");
        assert!(
            out.contains("Top workspaces"),
            "top workspaces missing:\n{out}"
        );
        assert!(out.contains("▐"), "bars missing:\n{out}");
    }

    #[test]
    fn a_narrow_terminal_still_renders_every_panel() {
        let out = snapshot(60, 20, "dashboard").unwrap();
        assert!(out.contains("Indexes"), "indexes panel missing:\n{out}");
        assert!(out.contains("Service"), "service panel missing:\n{out}");
        assert!(out.contains("Activity"), "activity panel missing:\n{out}");
        for line in out.lines() {
            assert!(
                line.chars().count() <= 60,
                "line overflows the terminal: {line}"
            );
        }

        let out = snapshot(60, 20, "stats").unwrap();
        assert!(out.contains("query stats"), "stats title missing:\n{out}");
    }

    #[test]
    fn sorting_reorders_the_view_and_keeps_the_selection() {
        let mut app = synthetic_app();
        app.sort_col = SortCol::Size;
        app.sort_asc = false;
        app.resort();
        let biggest = app.selected().map(|row| row.hash.clone());
        assert_eq!(
            app.rows[app.view[0]].display,
            "~/work/acme-monorepo-frontend"
        );

        app.sort_asc = true;
        app.resort();
        assert_eq!(app.rows[app.view[0]].display, "~/Sites/scratch");
        assert_eq!(app.selected().map(|row| row.hash.clone()), biggest);
    }

    #[test]
    fn the_filter_narrows_the_view() {
        let mut app = synthetic_app();
        app.filter = "grav".to_string();
        app.rebuild_view(None);
        assert_eq!(app.view.len(), 1);
        assert!(app.rows[app.view[0]].display.contains("grav"));

        app.filter.clear();
        app.rebuild_view(None);
        assert_eq!(app.view.len(), 6);
    }

    #[test]
    fn a_service_watched_index_refuses_tui_side_work() {
        let mut app = synthetic_app();
        app.sort_col = SortCol::Name;
        app.sort_asc = true;
        app.resort();
        let pos = app
            .view
            .iter()
            .position(|i| app.watched_by_service(&app.rows[*i].hash))
            .expect("the synthetic service watches one index");
        app.sel = pos;

        app.request(Deferred::Reindex);
        assert!(
            app.message.contains("watched by service"),
            "unexpected message: {}",
            app.message
        );
        assert_eq!(app.busy, 0, "nothing may have started");

        app.toggle_session_watch();
        assert!(app.message.contains("watched by the service"));
    }

    #[test]
    fn an_action_on_a_watched_index_waits_for_the_watcher_to_stop() {
        let mut app = synthetic_app();
        app.sort_col = SortCol::Name;
        app.sort_asc = true;
        app.resort();
        let pos = app
            .view
            .iter()
            .position(|i| {
                app.rows[*i].state == WatchState::Active
                    && !app.watched_by_service(&app.rows[*i].hash)
            })
            .expect("the synthetic dashboard watches one index itself");
        app.sel = pos;
        let hash = app.rows[app.view[pos]].hash.clone();

        app.request(Deferred::Reindex);
        assert_eq!(app.deferred.get(&hash), Some(&Deferred::Reindex));
        assert!(app.resume_watch.contains(&hash));
        assert_eq!(app.busy, 0, "the re-index waits for the watcher");

        app.handle_manager_event(ManagerEvent::WatchStateChanged {
            hash: hash.clone(),
            new_state: WatchState::Off,
        });
        assert!(app.deferred.is_empty());
        assert_eq!(app.busy, 1, "the re-index started once the watcher stopped");

        app.handle_manager_event(ManagerEvent::ReindexCompleted {
            hash: hash.clone(),
            files_indexed: 42,
        });
        assert_eq!(app.busy, 0);
        assert!(
            !app.resume_watch.contains(&hash),
            "watching resumes after the re-index"
        );
    }
}
