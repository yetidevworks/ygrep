//! Shared types for the dashboard TUI and watch manager

use std::path::PathBuf;

/// Watch state for a single workspace
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchState {
    /// Not watching - no file watcher active
    Off,
    /// Actively watching with a live FileWatcher
    Active,
    /// Sleeping - no FileWatcher, polling mtime every 30s
    Sleeping,
}

impl std::fmt::Display for WatchState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WatchState::Off => write!(f, "off"),
            WatchState::Active => write!(f, "active"),
            WatchState::Sleeping => write!(f, "sleeping"),
        }
    }
}

/// A workspace handed to the WatchManager after it is already running
#[derive(Debug, Clone)]
pub struct WorkspaceRegistration {
    /// Index hash
    pub hash: String,
    /// Workspace root path
    pub workspace_path: PathBuf,
    /// Whether the index carries embeddings
    pub semantic: bool,
    /// When the index was last updated
    pub indexed_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Persisted watch flag
    pub watch: bool,
}

/// Commands sent from the TUI to the WatchManager
#[derive(Debug)]
pub enum ManagerCommand {
    /// Toggle watch state for a workspace (Off <-> Active)
    ToggleWatch(String),
    /// Set the watch state for a workspace explicitly
    SetWatch { hash: String, enabled: bool },
    /// Add a workspace the manager has not seen yet
    Register(WorkspaceRegistration),
    /// Trigger full re-index for a workspace
    Reindex(String),
    /// Remove an index entirely
    RemoveIndex(String),
    /// Shutdown the manager
    Shutdown,
}

/// Events sent from the WatchManager to the TUI
#[derive(Debug)]
pub enum ManagerEvent {
    /// Watch state changed for a workspace
    WatchStateChanged { hash: String, new_state: WatchState },
    /// A file was indexed
    FileIndexed { hash: String, path: String },
    /// A file was deleted from the index
    FileDeleted { hash: String, path: String },
    /// An error occurred
    Error { hash: String, message: String },
    /// Re-index started
    ReindexStarted { hash: String },
    /// Re-index completed
    ReindexCompleted { hash: String, files_indexed: u64 },
    /// Re-index finished without indexing anything
    ReindexFailed { hash: String, message: String },
    /// Index was removed
    IndexRemoved { hash: String },
    /// Log message from a workspace operation
    Log { hash: String, message: String },
}
