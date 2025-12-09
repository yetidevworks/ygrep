//! File system watcher for incremental index updates

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use notify_debouncer_full::{
    new_debouncer,
    notify::{self, RecommendedWatcher, RecursiveMode},
    DebounceEventResult, Debouncer, FileIdMap,
};
use parking_lot::Mutex;
use tokio::sync::mpsc;

use crate::config::IndexerConfig;
use crate::error::{Result, YgrepError};

/// Events emitted by the file watcher
#[derive(Debug, Clone)]
pub enum WatchEvent {
    /// File was created or modified
    Changed(PathBuf),
    /// File was deleted
    Deleted(PathBuf),
    /// Directory was created
    DirCreated(PathBuf),
    /// Directory was deleted
    DirDeleted(PathBuf),
    /// Error occurred while watching
    Error(String),
}

/// File system watcher with debouncing
pub struct FileWatcher {
    root: PathBuf,
    config: IndexerConfig,
    debouncer: Debouncer<RecommendedWatcher, FileIdMap>,
    event_rx: mpsc::UnboundedReceiver<WatchEvent>,
    /// All paths being watched (root + symlink targets)
    watched_paths: Vec<PathBuf>,
}

impl FileWatcher {
    /// Create a new file watcher for the given directory
    pub fn new(root: PathBuf, config: IndexerConfig) -> Result<Self> {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let event_tx = Arc::new(Mutex::new(event_tx));

        // Find symlink targets upfront so we can pass them to the event handler
        let symlink_targets = if config.follow_symlinks {
            find_symlink_targets(&root)
        } else {
            vec![]
        };

        // Build list of all watched paths
        let mut watched_paths = vec![root.clone()];
        watched_paths.extend(symlink_targets.clone());
        let watched_paths_for_closure = watched_paths.clone();

        // Clone for the closure
        let config_clone = config.clone();

        // Create debouncer with 500ms delay
        let debouncer = new_debouncer(
            Duration::from_millis(500),
            None,
            move |result: DebounceEventResult| {
                use std::collections::HashSet;

                let tx = event_tx.lock();
                match result {
                    Ok(events) => {
                        // Deduplicate events by path to avoid processing same file twice
                        let mut seen_changed: HashSet<PathBuf> = HashSet::new();
                        let mut seen_deleted: HashSet<PathBuf> = HashSet::new();

                        for event in events {
                            let watch_events = process_notify_event(
                                &event,
                                &watched_paths_for_closure,
                                &config_clone,
                            );
                            for e in watch_events {
                                match &e {
                                    WatchEvent::Changed(p) => {
                                        if seen_changed.insert(p.clone()) {
                                            let _ = tx.send(e);
                                        }
                                    }
                                    WatchEvent::Deleted(p) => {
                                        if seen_deleted.insert(p.clone()) {
                                            let _ = tx.send(e);
                                        }
                                    }
                                    _ => {
                                        let _ = tx.send(e);
                                    }
                                }
                            }
                        }
                    }
                    Err(errors) => {
                        for e in errors {
                            let _ = tx.send(WatchEvent::Error(e.to_string()));
                        }
                    }
                }
            },
        )
        .map_err(|e| YgrepError::WatchError(e.to_string()))?;

        Ok(Self {
            root,
            config,
            debouncer,
            event_rx,
            watched_paths,
        })
    }

    /// Start watching the directory
    pub fn start(&mut self) -> Result<()> {
        // Watch all paths (root + symlink targets found during construction)
        for path in &self.watched_paths {
            match self.debouncer.watch(path, RecursiveMode::Recursive) {
                Ok(()) => {
                    if path == &self.root {
                        tracing::info!("Started watching: {}", path.display());
                    } else {
                        tracing::info!("Also watching symlink target: {}", path.display());
                    }
                }
                Err(e) => {
                    if path == &self.root {
                        return Err(YgrepError::WatchError(e.to_string()));
                    } else {
                        tracing::warn!("Failed to watch symlink target {}: {}", path.display(), e);
                    }
                }
            }
        }

        Ok(())
    }

    /// Stop watching
    pub fn stop(&mut self) -> Result<()> {
        self.debouncer
            .unwatch(&self.root)
            .map_err(|e| YgrepError::WatchError(e.to_string()))?;

        tracing::info!("Stopped watching: {}", self.root.display());
        Ok(())
    }

    /// Get the next watch event (async)
    pub async fn next_event(&mut self) -> Option<WatchEvent> {
        self.event_rx.recv().await
    }

    /// Get the root directory being watched
    pub fn root(&self) -> &Path {
        &self.root
    }
}

/// Process a notify event and convert to WatchEvent(s)
fn process_notify_event(
    event: &notify_debouncer_full::DebouncedEvent,
    watched_paths: &[PathBuf],
    config: &IndexerConfig,
) -> Vec<WatchEvent> {
    use notify::EventKind;

    let mut events = Vec::new();

    for path in &event.paths {
        // Skip if path is not under any watched path
        let is_under_watched = watched_paths.iter().any(|wp| path.starts_with(wp));
        if !is_under_watched {
            continue;
        }

        // Skip hidden files/directories
        if is_hidden(path) {
            continue;
        }

        // Skip ignored directories
        if is_ignored_dir(path) {
            continue;
        }

        // Skip files matching ignore patterns
        if matches_ignore_pattern(path, config) {
            continue;
        }

        match event.kind {
            EventKind::Create(_) => {
                if path.is_dir() {
                    events.push(WatchEvent::DirCreated(path.clone()));
                } else if path.is_file() {
                    events.push(WatchEvent::Changed(path.clone()));
                }
            }
            EventKind::Modify(_) => {
                if path.is_file() {
                    events.push(WatchEvent::Changed(path.clone()));
                }
            }
            EventKind::Remove(_) => {
                // Can't check if it was a file or dir since it's deleted
                // We'll handle both cases in the indexer
                events.push(WatchEvent::Deleted(path.clone()));
            }
            _ => {}
        }
    }

    events
}

/// Check if a path is hidden (starts with .)
fn is_hidden(path: &Path) -> bool {
    path.components().any(|c| {
        c.as_os_str()
            .to_str()
            .map(|s| s.starts_with('.'))
            .unwrap_or(false)
    })
}

/// Find all symlink targets in a directory tree
/// Returns the canonical paths of directories that are symlinked
fn find_symlink_targets(root: &Path) -> Vec<PathBuf> {
    use std::collections::HashSet;
    use walkdir::WalkDir;

    let mut targets = HashSet::new();

    for entry in WalkDir::new(root)
        .follow_links(false) // Don't follow links during walk
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let path = entry.path();

        // Check if this is a symlink to a directory
        if path.is_symlink() {
            if let Ok(target) = std::fs::read_link(path) {
                // Resolve to absolute path
                let absolute_target = if target.is_absolute() {
                    target
                } else {
                    path.parent()
                        .map(|p| p.join(&target))
                        .unwrap_or(target)
                };

                // Canonicalize to resolve any .. or . components
                if let Ok(canonical) = std::fs::canonicalize(&absolute_target) {
                    if canonical.is_dir() && !is_ignored_dir(&canonical) {
                        targets.insert(canonical);
                    }
                }
            }
        }
    }

    targets.into_iter().collect()
}

/// Check if path is in an ignored directory
fn is_ignored_dir(path: &Path) -> bool {
    const IGNORED_DIRS: &[&str] = &[
        "node_modules",
        "vendor",
        "target",
        "dist",
        "build",
        "cache",
        ".git",
        "__pycache__",
        "logs",
        "tmp",
    ];

    path.components().any(|c| {
        c.as_os_str()
            .to_str()
            .map(|s| IGNORED_DIRS.contains(&s))
            .unwrap_or(false)
    })
}

/// Check if path matches custom ignore patterns
fn matches_ignore_pattern(path: &Path, config: &IndexerConfig) -> bool {
    let path_str = path.to_string_lossy();

    for pattern in &config.ignore_patterns {
        if glob_match(pattern, &path_str) {
            return true;
        }
    }

    false
}

/// Simple glob matching (copied from walker.rs for consistency)
fn glob_match(pattern: &str, path: &str) -> bool {
    // Handle **/dir/** patterns (match dir anywhere in path)
    if pattern.starts_with("**/") && pattern.ends_with("/**") {
        let dir_name = &pattern[3..pattern.len() - 3];
        return path.contains(&format!("/{}/", dir_name))
            || path.starts_with(&format!("{}/", dir_name))
            || path.ends_with(&format!("/{}", dir_name));
    }

    // Handle **/*.ext patterns (match extension anywhere)
    if pattern.starts_with("**/*.") {
        let ext = &pattern[5..];
        return path.ends_with(&format!(".{}", ext));
    }

    // Handle **/something patterns (match at end)
    if pattern.starts_with("**/") {
        let suffix = &pattern[3..];
        return path.ends_with(suffix) || path.ends_with(&format!("/{}", suffix));
    }

    // Handle something/** patterns (match at start)
    if pattern.ends_with("/**") {
        let prefix = &pattern[..pattern.len() - 3];
        return path.starts_with(prefix) || path.contains(&format!("/{}", prefix));
    }

    // Handle simple * patterns (*.ext)
    if pattern.starts_with("*.") {
        let ext = &pattern[2..];
        return path.ends_with(&format!(".{}", ext));
    }

    // Exact match or path component match
    path == pattern
        || path.ends_with(&format!("/{}", pattern))
        || path.contains(&format!("/{}/", pattern))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_hidden() {
        assert!(is_hidden(Path::new("/foo/.git/config")));
        assert!(is_hidden(Path::new("/foo/.hidden")));
        assert!(!is_hidden(Path::new("/foo/bar/baz.rs")));
    }

    #[test]
    fn test_is_ignored_dir() {
        assert!(is_ignored_dir(Path::new("/foo/node_modules/bar")));
        assert!(is_ignored_dir(Path::new("/foo/vendor/package")));
        assert!(!is_ignored_dir(Path::new("/foo/src/main.rs")));
    }
}
