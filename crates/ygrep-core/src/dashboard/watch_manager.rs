//! Multi-workspace watch orchestrator with sleep/wake state machine

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use crate::watcher::WatchEvent;
use crate::Workspace;

use super::types::*;

/// How long to wait with no events before transitioning Active -> Sleeping
const INACTIVITY_TIMEOUT: Duration = Duration::from_secs(5 * 60); // 5 minutes

/// How often to poll each sleeping workspace for file changes
const SLEEP_POLL_INTERVAL: Duration = Duration::from_secs(30);

/// How recently an index must have been updated to auto-watch on startup
const AUTO_WATCH_THRESHOLD: Duration = Duration::from_secs(24 * 60 * 60); // 24 hours

/// Per-workspace watcher state tracked by the manager
struct WorkspaceState {
    #[allow(dead_code)]
    hash: String,
    workspace_path: PathBuf,
    semantic: bool,
    watch_state: WatchState,
    /// When we last received a file change event
    last_activity: Option<Instant>,
    /// Handle to the spawned watcher task (if Active)
    watcher_handle: Option<tokio::task::JoinHandle<()>>,
    /// Channel to tell the watcher task to stop
    watcher_stop_tx: Option<tokio::sync::oneshot::Sender<()>>,
    /// When the index was last updated (from metadata)
    indexed_at: Option<chrono::DateTime<chrono::Utc>>,
    /// When we last polled this workspace for changes while sleeping
    last_sleep_poll: Option<Instant>,
}

/// Multi-workspace watch manager
pub struct WatchManager {
    /// Per-workspace state keyed by hash
    workspaces: HashMap<String, WorkspaceState>,
    /// Channel to receive commands from the TUI
    cmd_rx: mpsc::UnboundedReceiver<ManagerCommand>,
    /// Channel to send events to the TUI
    event_tx: mpsc::UnboundedSender<ManagerEvent>,
    /// Receiver for file events from per-workspace watcher tasks
    file_event_rx: mpsc::UnboundedReceiver<(String, WatchEvent)>,
    /// Sender cloned into each watcher task
    file_event_tx: mpsc::UnboundedSender<(String, WatchEvent)>,
}

impl WatchManager {
    /// Create a new WatchManager. Returns (manager, cmd_tx, event_rx).
    pub fn new() -> (
        Self,
        mpsc::UnboundedSender<ManagerCommand>,
        mpsc::UnboundedReceiver<ManagerEvent>,
    ) {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (file_event_tx, file_event_rx) = mpsc::unbounded_channel();

        let manager = Self {
            workspaces: HashMap::new(),
            cmd_rx,
            event_tx,
            file_event_rx,
            file_event_tx,
        };

        (manager, cmd_tx, event_rx)
    }

    /// Register a workspace. If recently indexed and workspace exists, auto-start watching.
    pub fn register(
        &mut self,
        hash: String,
        workspace_path: PathBuf,
        semantic: bool,
        indexed_at: Option<chrono::DateTime<chrono::Utc>>,
    ) {
        let should_auto_watch = indexed_at
            .map(|dt| {
                let age = chrono::Utc::now().signed_duration_since(dt);
                age.num_seconds() < AUTO_WATCH_THRESHOLD.as_secs() as i64 && workspace_path.exists()
            })
            .unwrap_or(false);

        let state = WorkspaceState {
            hash: hash.clone(),
            workspace_path,
            semantic,
            watch_state: if should_auto_watch {
                WatchState::Active
            } else {
                WatchState::Off
            },
            last_activity: if should_auto_watch {
                Some(Instant::now())
            } else {
                None
            },
            watcher_handle: None,
            watcher_stop_tx: None,
            indexed_at,
            last_sleep_poll: None,
        };

        self.workspaces.insert(hash, state);
    }

    /// Run the manager event loop. This should be spawned as a tokio task.
    pub async fn run(mut self) {
        // Start watchers for any workspaces that should auto-watch
        let auto_watch: Vec<String> = self
            .workspaces
            .iter()
            .filter(|(_, ws)| ws.watch_state == WatchState::Active)
            .map(|(hash, _)| hash.clone())
            .collect();

        for hash in auto_watch {
            self.start_watcher(&hash);
        }

        // Tick interval for checking inactivity and sleep polling
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                // Commands from TUI
                Some(cmd) = self.cmd_rx.recv() => {
                    match cmd {
                        ManagerCommand::ToggleWatch(hash) => {
                            self.handle_toggle(&hash);
                        }
                        ManagerCommand::Reindex(hash) => {
                            self.handle_reindex(&hash);
                        }
                        ManagerCommand::RemoveIndex(hash) => {
                            self.handle_remove(&hash);
                        }
                        ManagerCommand::Shutdown => {
                            self.shutdown_all();
                            break;
                        }
                    }
                }

                // File events from watcher tasks
                Some((hash, event)) = self.file_event_rx.recv() => {
                    self.handle_file_event(&hash, event);
                }

                // Periodic tick for inactivity checks and sleep polling
                _ = tick.tick() => {
                    self.check_inactivity();
                    self.poll_sleeping().await;
                }
            }
        }
    }

    /// Start a file watcher task for a workspace
    fn start_watcher(&mut self, hash: &str) {
        let ws = match self.workspaces.get_mut(hash) {
            Some(ws) => ws,
            None => return,
        };

        // Don't start if already has a watcher
        if ws.watcher_handle.is_some() {
            return;
        }

        let workspace_path = ws.workspace_path.clone();
        let semantic = ws.semantic;
        let hash_clone = hash.to_string();
        let file_event_tx = self.file_event_tx.clone();
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();

        let handle = tokio::spawn(async move {
            watcher_task(workspace_path, semantic, hash_clone, file_event_tx, stop_rx).await;
        });

        ws.watcher_handle = Some(handle);
        ws.watcher_stop_tx = Some(stop_tx);
        ws.last_activity = Some(Instant::now());
    }

    /// Stop a watcher task for a workspace
    fn stop_watcher(&mut self, hash: &str) {
        let ws = match self.workspaces.get_mut(hash) {
            Some(ws) => ws,
            None => return,
        };

        // Signal the watcher task to stop
        if let Some(stop_tx) = ws.watcher_stop_tx.take() {
            let _ = stop_tx.send(());
        }

        // Abort the task if it doesn't stop cleanly
        if let Some(handle) = ws.watcher_handle.take() {
            handle.abort();
        }
    }

    fn handle_toggle(&mut self, hash: &str) {
        let current_state = match self.workspaces.get(hash) {
            Some(ws) => ws.watch_state.clone(),
            None => return,
        };

        match current_state {
            WatchState::Off => {
                // Off -> Active
                if let Some(ws) = self.workspaces.get_mut(hash) {
                    ws.watch_state = WatchState::Active;
                    ws.last_activity = Some(Instant::now());
                }
                self.start_watcher(hash);
                let _ = self.event_tx.send(ManagerEvent::WatchStateChanged {
                    hash: hash.to_string(),
                    new_state: WatchState::Active,
                });
            }
            WatchState::Active | WatchState::Sleeping => {
                // Active/Sleeping -> Off
                self.stop_watcher(hash);
                if let Some(ws) = self.workspaces.get_mut(hash) {
                    ws.watch_state = WatchState::Off;
                    ws.last_activity = None;
                    ws.last_sleep_poll = None;
                }
                let _ = self.event_tx.send(ManagerEvent::WatchStateChanged {
                    hash: hash.to_string(),
                    new_state: WatchState::Off,
                });
            }
        }
    }

    fn handle_reindex(&mut self, hash: &str) {
        let ws = match self.workspaces.get(hash) {
            Some(ws) => ws,
            None => return,
        };

        let workspace_path = ws.workspace_path.clone();
        let semantic = ws.semantic;
        let hash_clone = hash.to_string();
        let event_tx = self.event_tx.clone();

        let _ = self.event_tx.send(ManagerEvent::ReindexStarted {
            hash: hash.to_string(),
        });

        // Spawn re-index as a blocking task
        tokio::spawn(async move {
            let result = tokio::task::spawn_blocking(move || {
                let workspace = Workspace::open(&workspace_path)?;
                let stats = workspace.index_incremental_quiet(semantic)?;
                Ok::<_, crate::error::YgrepError>(stats.indexed as u64 + stats.unchanged as u64)
            })
            .await;

            match result {
                Ok(Ok(files)) => {
                    let _ = event_tx.send(ManagerEvent::ReindexCompleted {
                        hash: hash_clone,
                        files_indexed: files,
                    });
                }
                Ok(Err(e)) => {
                    let _ = event_tx.send(ManagerEvent::Error {
                        hash: hash_clone,
                        message: format!("Re-index failed: {}", e),
                    });
                }
                Err(e) => {
                    let _ = event_tx.send(ManagerEvent::Error {
                        hash: hash_clone,
                        message: format!("Re-index task panicked: {}", e),
                    });
                }
            }
        });
    }

    fn handle_remove(&mut self, hash: &str) {
        // Stop watcher first
        self.stop_watcher(hash);
        self.workspaces.remove(hash);
        let _ = self.event_tx.send(ManagerEvent::IndexRemoved {
            hash: hash.to_string(),
        });
    }

    fn handle_file_event(&mut self, hash: &str, event: WatchEvent) {
        let ws = match self.workspaces.get_mut(hash) {
            Some(ws) => ws,
            None => return,
        };

        ws.last_activity = Some(Instant::now());

        match event {
            WatchEvent::Changed(path) => {
                let rel = path
                    .file_name()
                    .map(|f| f.to_string_lossy().to_string())
                    .unwrap_or_else(|| path.to_string_lossy().to_string());
                let _ = self.event_tx.send(ManagerEvent::FileIndexed {
                    hash: hash.to_string(),
                    path: rel,
                });
            }
            WatchEvent::Deleted(path) => {
                let rel = path
                    .file_name()
                    .map(|f| f.to_string_lossy().to_string())
                    .unwrap_or_else(|| path.to_string_lossy().to_string());
                let _ = self.event_tx.send(ManagerEvent::FileDeleted {
                    hash: hash.to_string(),
                    path: rel,
                });
            }
            WatchEvent::Error(msg) => {
                let _ = self.event_tx.send(ManagerEvent::Error {
                    hash: hash.to_string(),
                    message: msg,
                });
            }
            _ => {}
        }
    }

    /// Check for inactivity and transition Active -> Sleeping
    fn check_inactivity(&mut self) {
        let now = Instant::now();
        let mut to_sleep = Vec::new();

        for (hash, ws) in &self.workspaces {
            if ws.watch_state == WatchState::Active {
                if let Some(last) = ws.last_activity {
                    if now.duration_since(last) > INACTIVITY_TIMEOUT {
                        to_sleep.push(hash.clone());
                    }
                }
            }
        }

        for (i, hash) in to_sleep.iter().enumerate() {
            self.stop_watcher(hash);
            if let Some(ws) = self.workspaces.get_mut(hash) {
                ws.watch_state = WatchState::Sleeping;
                // Stagger initial polls: each workspace gets an offset so they don't all
                // poll on the same tick. Pretend we polled (interval - offset) ago.
                let stagger = Duration::from_secs((i as u64 * 5) % SLEEP_POLL_INTERVAL.as_secs());
                ws.last_sleep_poll = Some(Instant::now() - SLEEP_POLL_INTERVAL + stagger);
            }
            let _ = self.event_tx.send(ManagerEvent::WatchStateChanged {
                hash: hash.clone(),
                new_state: WatchState::Sleeping,
            });
        }
    }

    /// Poll sleeping workspaces for changes, respecting per-workspace poll interval
    async fn poll_sleeping(&mut self) {
        let now = Instant::now();
        let mut to_wake = Vec::new();
        let mut polled = Vec::new();

        for (hash, ws) in &self.workspaces {
            if ws.watch_state != WatchState::Sleeping {
                continue;
            }

            // Only poll if enough time has passed since last poll
            let should_poll = match ws.last_sleep_poll {
                Some(last) => now.duration_since(last) >= SLEEP_POLL_INTERVAL,
                None => true, // Never polled yet, poll now
            };

            if !should_poll {
                continue;
            }

            polled.push(hash.clone());

            if let Some(indexed_at) = ws.indexed_at {
                if has_recent_changes(&ws.workspace_path, indexed_at) {
                    to_wake.push(hash.clone());
                }
            }
        }

        // Update last_sleep_poll for all workspaces we checked
        for hash in &polled {
            if let Some(ws) = self.workspaces.get_mut(hash) {
                ws.last_sleep_poll = Some(now);
            }
        }

        for hash in to_wake {
            if let Some(ws) = self.workspaces.get_mut(&hash) {
                ws.watch_state = WatchState::Active;
                ws.last_activity = Some(Instant::now());
                ws.last_sleep_poll = None;
            }
            self.start_watcher(&hash);
            let _ = self.event_tx.send(ManagerEvent::WatchStateChanged {
                hash: hash.clone(),
                new_state: WatchState::Active,
            });
        }
    }

    fn shutdown_all(&mut self) {
        let hashes: Vec<String> = self.workspaces.keys().cloned().collect();
        for hash in hashes {
            self.stop_watcher(&hash);
        }
    }
}

/// Check if a workspace has changes newer than the indexed_at timestamp.
/// Walks the directory tree (up to a cap) looking for any file modified after indexed_at.
fn has_recent_changes(workspace_path: &Path, indexed_at: chrono::DateTime<chrono::Utc>) -> bool {
    let indexed_secs = indexed_at.timestamp() as u64;

    let check_mtime = |path: &Path| -> bool {
        if let Ok(meta) = std::fs::metadata(path) {
            if let Ok(mtime) = meta.modified() {
                if let Ok(dur) = mtime.duration_since(std::time::UNIX_EPOCH) {
                    return dur.as_secs() > indexed_secs;
                }
            }
        }
        false
    };

    // Walk directory tree, checking file mtimes. Cap at 2000 entries to stay fast.
    let mut checked = 0;
    let mut dirs = vec![workspace_path.to_path_buf()];

    while let Some(dir) = dirs.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };

        for entry in entries.flatten() {
            checked += 1;
            if checked > 2000 {
                return false;
            }

            let path = entry.path();
            let name = entry.file_name();
            let name_str = name.to_string_lossy();

            // Skip hidden dirs and common non-source dirs
            if name_str.starts_with('.') || name_str == "node_modules" || name_str == "target" {
                continue;
            }

            if path.is_dir() {
                dirs.push(path);
            } else if is_indexable(&path) && check_mtime(&path) {
                return true;
            }
        }
    }

    false
}

/// Check if a file should be indexed (simple extension check)
fn is_indexable(path: &Path) -> bool {
    const TEXT_EXTENSIONS: &[&str] = &[
        "rs",
        "py",
        "js",
        "ts",
        "jsx",
        "tsx",
        "mjs",
        "mts",
        "cjs",
        "cts",
        "go",
        "rb",
        "php",
        "java",
        "c",
        "cpp",
        "cc",
        "h",
        "hpp",
        "hh",
        "cs",
        "swift",
        "kt",
        "scala",
        "clj",
        "ex",
        "exs",
        "erl",
        "hs",
        "ml",
        "fs",
        "r",
        "jl",
        "lua",
        "pl",
        "pm",
        "sh",
        "bash",
        "zsh",
        "fish",
        "ps1",
        "bat",
        "cmd",
        "html",
        "htm",
        "css",
        "scss",
        "sass",
        "less",
        "xml",
        "json",
        "yaml",
        "yml",
        "toml",
        "twig",
        "blade",
        "ejs",
        "hbs",
        "handlebars",
        "mustache",
        "pug",
        "jade",
        "erb",
        "haml",
        "njk",
        "nunjucks",
        "jinja",
        "jinja2",
        "liquid",
        "eta",
        "md",
        "markdown",
        "rst",
        "txt",
        "csv",
        "sql",
        "graphql",
        "gql",
        "dockerfile",
        "makefile",
        "cmake",
        "gradle",
        "pom",
        "ini",
        "conf",
        "cfg",
        "vue",
        "svelte",
        "astro",
        "tf",
        "hcl",
        "nix",
        "proto",
        "thrift",
        "avsc",
        "gitignore",
        "gitattributes",
        "editorconfig",
        "env",
    ];

    if let Some(ext) = path.extension() {
        let ext_lower = ext.to_string_lossy().to_lowercase();
        TEXT_EXTENSIONS.contains(&ext_lower.as_str())
    } else {
        false
    }
}

/// Async task that runs a FileWatcher for a single workspace
async fn watcher_task(
    workspace_path: PathBuf,
    semantic: bool,
    hash: String,
    event_tx: mpsc::UnboundedSender<(String, WatchEvent)>,
    mut stop_rx: tokio::sync::oneshot::Receiver<()>,
) {
    // Open workspace and create watcher (blocking)
    let result = tokio::task::spawn_blocking({
        let workspace_path = workspace_path.clone();
        move || -> Result<(Workspace, crate::watcher::FileWatcher), String> {
            let workspace = Workspace::open(&workspace_path).map_err(|e| format!("open: {}", e))?;

            // Run incremental update first
            let _stats = workspace
                .index_incremental_quiet(semantic)
                .map_err(|e| format!("incremental: {}", e))?;

            let mut watcher = workspace
                .create_watcher()
                .map_err(|e| format!("watcher: {}", e))?;
            watcher.start().map_err(|e| format!("start: {}", e))?;

            Ok((workspace, watcher))
        }
    })
    .await;

    let (workspace, mut watcher) = match result {
        Ok(Ok((ws, w))) => (ws, w),
        Ok(Err(e)) => {
            let _ = event_tx.send((hash, WatchEvent::Error(e)));
            return;
        }
        Err(e) => {
            let _ = event_tx.send((hash, WatchEvent::Error(format!("task panicked: {}", e))));
            return;
        }
    };

    // Event loop: process file events until told to stop
    loop {
        tokio::select! {
            _ = &mut stop_rx => {
                let _ = watcher.stop();
                break;
            }
            event = watcher.next_event() => {
                match event {
                    Some(WatchEvent::Changed(ref path)) => {
                        if is_indexable(path) {
                            match workspace.index_file_with_options(path, semantic) {
                                Ok(()) => {
                                    let _ = event_tx.send((hash.clone(), WatchEvent::Changed(path.clone())));
                                }
                                Err(e) => {
                                    let _ = event_tx.send((hash.clone(), WatchEvent::Error(format!("{}: {}", path.display(), e))));
                                }
                            }
                        }
                    }
                    Some(WatchEvent::Deleted(ref path)) => {
                        let _ = workspace.delete_file(path);
                        let _ = event_tx.send((hash.clone(), WatchEvent::Deleted(path.clone())));
                    }
                    Some(WatchEvent::Error(ref msg)) => {
                        let _ = event_tx.send((hash.clone(), WatchEvent::Error(msg.clone())));
                    }
                    Some(_) => {} // DirCreated/DirDeleted - ignore
                    None => break, // Channel closed
                }
            }
        }
    }
}
