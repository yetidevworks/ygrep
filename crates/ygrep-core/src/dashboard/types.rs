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

/// A single index entry displayed in the dashboard table
#[derive(Debug, Clone)]
pub struct IndexEntry {
    /// Index hash
    pub hash: String,
    /// Workspace root path
    pub workspace_path: PathBuf,
    /// Display path (shortened with ~)
    pub display_path: String,
    /// Index size in bytes
    pub size_bytes: u64,
    /// Number of files indexed
    pub files_indexed: u64,
    /// When the index was last updated
    pub indexed_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Whether semantic indexing is enabled
    pub semantic: bool,
    /// Current watch state
    pub watch_state: WatchState,
    /// Changes per minute (rolling average)
    pub changes_per_min: f64,
    /// Whether the workspace still exists on disk
    pub orphaned: bool,
}

/// Activity log event
#[derive(Debug, Clone)]
pub struct ActivityEvent {
    /// Timestamp of the event
    pub timestamp: chrono::DateTime<chrono::Utc>,
    /// Short workspace name (last path component)
    pub workspace_name: String,
    /// Description of what happened
    pub message: String,
    /// Event kind for coloring
    pub kind: ActivityKind,
}

/// Kind of activity event (for display styling)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActivityKind {
    /// File indexed successfully
    Indexed,
    /// File deleted from index
    Deleted,
    /// Watch state changed
    StateChange,
    /// Error occurred
    Error,
    /// Re-index started/completed
    Reindex,
}

/// Commands sent from the TUI to the WatchManager
#[derive(Debug)]
pub enum ManagerCommand {
    /// Toggle watch state for a workspace (Off <-> Active)
    ToggleWatch(String),
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
    /// Index was removed
    IndexRemoved { hash: String },
}
