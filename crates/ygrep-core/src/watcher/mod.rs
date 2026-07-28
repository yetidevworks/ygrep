//! File system watcher for incremental index updates

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use notify_debouncer_full::{new_debouncer, notify::RecursiveMode, DebounceEventResult};
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

type PlatformDebouncer = notify_debouncer_full::Debouncer<
    notify_debouncer_full::notify::RecommendedWatcher,
    notify_debouncer_full::RecommendedCache,
>;

/// File system watcher with debouncing
pub struct FileWatcher {
    root: PathBuf,
    #[allow(dead_code)]
    config: IndexerConfig,
    debouncer: PlatformDebouncer,
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
            find_symlink_targets(&root, &config)
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

    /// Stop watching all paths (root + symlink targets)
    pub fn stop(&mut self) -> Result<()> {
        for path in &self.watched_paths {
            match self.debouncer.unwatch(path) {
                Ok(()) => {
                    tracing::info!("Stopped watching: {}", path.display());
                }
                Err(e) => {
                    tracing::warn!("Failed to unwatch {}: {}", path.display(), e);
                }
            }
        }
        Ok(())
    }

    /// Get the next watch event (async)
    pub async fn next_event(&mut self) -> Option<WatchEvent> {
        self.event_rx.recv().await
    }

    /// Try to get the next watch event without waiting (returns None if no event queued)
    pub fn try_next_event(&mut self) -> Option<WatchEvent> {
        self.event_rx.try_recv().ok()
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
        if !is_watched_path(path, watched_paths, config) {
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

/// Whether a changed path is one we index.
///
/// Every check runs against the path relative to the watch root it sits under. Matching
/// the absolute path would let directories *above* the workspace exclude everything
/// inside it, so a project stored under `~/build` or `~/.local/src` would see no events
/// at all — the same trap the walker already avoids.
fn is_watched_path(path: &Path, watched_paths: &[PathBuf], config: &IndexerConfig) -> bool {
    let Some(relative) = watched_paths
        .iter()
        .find_map(|watched| path.strip_prefix(watched).ok())
    else {
        return false;
    };

    !is_hidden(relative) && !is_ignored_dir(relative) && !matches_ignore_pattern(relative, config)
}

/// Whether a path lives in a hidden directory the walk never descends into.
///
/// The walk indexes `.github/workflows/*.yml`, `.gitignore` and the other dotfiles
/// listed in [`crate::fs::classify`], so rejecting every dotted path here would leave
/// them indexed once and never updated again. Directories follow the walk's allowlist;
/// the final component is a file name, which the classifier judges for itself.
fn is_hidden(path: &Path) -> bool {
    let mut components = path.components().peekable();

    while let Some(component) = components.next() {
        let name = component.as_os_str();
        if !is_hidden_name(name) {
            continue;
        }
        if components.peek().is_none() {
            return false;
        }
        if !name
            .to_str()
            .is_some_and(|name| crate::fs::classify::TEXT_DOT_DIRS.contains(&name))
        {
            return true;
        }
    }

    false
}

/// Check if a single path component is hidden (starts with .)
fn is_hidden_name(name: &std::ffi::OsStr) -> bool {
    name.to_str().map(|s| s.starts_with('.')).unwrap_or(false)
}

/// Find all symlink targets in a directory tree
/// Returns the canonical paths of directories that are symlinked
///
/// Prunes the same subtrees the indexing walk prunes. Without that this lstats every
/// file in node_modules/, .git/ and target/ on watcher startup and on every wake from
/// sleep, to find symlinks in directories whose contents are never indexed anyway.
fn find_symlink_targets(root: &Path, config: &IndexerConfig) -> Vec<PathBuf> {
    use std::collections::HashSet;
    use walkdir::WalkDir;

    let prune = crate::fs::walker::prune_suffixes(&config.ignore_patterns);
    let canonical_root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let mut targets = HashSet::new();

    for entry in WalkDir::new(root)
        .follow_links(false) // Don't follow links during walk
        .into_iter()
        .filter_entry(|e| {
            // The root itself is always walked: it's the tree the caller asked to watch.
            if e.depth() == 0 {
                return true;
            }
            if is_hidden_name(e.file_name()) {
                return false;
            }
            !(e.file_type().is_dir() && crate::fs::walker::is_pruned_dir(root, e.path(), &prune))
        })
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
                    path.parent().map(|p| p.join(&target)).unwrap_or(target)
                };

                // Canonicalize to resolve any .. or . components
                if let Ok(canonical) = std::fs::canonicalize(&absolute_target) {
                    if canonical.is_dir() && !ignored_symlink_target(&canonical_root, &canonical) {
                        targets.insert(canonical);
                    }
                }
            }
        }
    }

    targets.into_iter().collect()
}

/// Check if path is in an ignored directory
/// A symlink target is judged the way the walk judges directories: by its path inside
/// the workspace when it lives there, and by its own name when it does not — never by
/// ancestors like /tmp or ~/build that merely contain the workspace.
fn ignored_symlink_target(root: &Path, target: &Path) -> bool {
    match target.strip_prefix(root) {
        Ok(relative) => is_hidden(relative) || is_ignored_dir(relative),
        Err(_) => target
            .file_name()
            .map(|name| is_hidden_name(name) || is_ignored_dir(Path::new(name)))
            .unwrap_or(false),
    }
}

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
        if crate::fs::walker::glob_match(pattern, &path_str) {
            return true;
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_hidden() {
        assert!(is_hidden(Path::new("foo/.git/config")));
        assert!(!is_hidden(Path::new("foo/bar/baz.rs")));
    }

    #[test]
    fn test_is_hidden_root_not_hidden() {
        // A path with no hidden components
        assert!(!is_hidden(Path::new("usr/local/bin")));
    }

    #[test]
    fn test_is_hidden_nested_in_hidden() {
        // File nested inside a hidden directory
        assert!(is_hidden(Path::new("project/.cache/data/file.txt")));
    }

    #[test]
    fn the_dotfiles_the_walk_indexes_are_also_watched() {
        let config = IndexerConfig::default();
        let watched = vec![PathBuf::from("/home/andy/src/myapp")];

        // Indexed by the walk since 4.0, so edits to them have to reach the index too.
        for path in [
            "/home/andy/src/myapp/.gitignore",
            "/home/andy/src/myapp/.github/workflows/ci.yml",
        ] {
            assert!(
                is_watched_path(Path::new(path), &watched, &config),
                "{path} must produce watch events"
            );
        }

        // A dotfile the walk never indexes still reaches the classifier, which drops it,
        // but a hidden directory of machine state is filtered out here.
        assert!(!is_watched_path(
            Path::new("/home/andy/src/myapp/.cache/blob"),
            &watched,
            &config
        ));
    }

    #[test]
    fn test_is_ignored_dir() {
        assert!(is_ignored_dir(Path::new("/foo/node_modules/bar")));
        assert!(is_ignored_dir(Path::new("/foo/vendor/package")));
        assert!(!is_ignored_dir(Path::new("/foo/src/main.rs")));
    }

    #[test]
    fn test_matches_ignore_pattern_with_config() {
        let mut config = IndexerConfig::default();
        config.ignore_patterns = vec!["**/*.log".to_string(), "**/temp/**".to_string()];

        assert!(matches_ignore_pattern(Path::new("debug.log"), &config));
        assert!(matches_ignore_pattern(Path::new("temp/cache.txt"), &config));
        assert!(!matches_ignore_pattern(Path::new("src/main.rs"), &config));
    }

    #[test]
    fn events_inside_the_workspace_are_judged_relative_to_it() {
        let config = IndexerConfig::default();
        let watched = vec![PathBuf::from("/home/andy/build/myapp")];

        // A workspace stored under a directory named like a build output still gets
        // events for its own files.
        assert!(is_watched_path(
            Path::new("/home/andy/build/myapp/src/main.rs"),
            &watched,
            &config
        ));

        // Its own build output is still excluded.
        assert!(!is_watched_path(
            Path::new("/home/andy/build/myapp/dist/bundle.js"),
            &watched,
            &config
        ));
    }

    #[test]
    fn a_dotted_ancestor_does_not_silence_the_watch() {
        let config = IndexerConfig::default();
        let watched = vec![PathBuf::from("/home/andy/.local/src/myapp")];

        assert!(is_watched_path(
            Path::new("/home/andy/.local/src/myapp/src/main.rs"),
            &watched,
            &config
        ));

        // Hidden directories inside the workspace are still skipped.
        assert!(!is_watched_path(
            Path::new("/home/andy/.local/src/myapp/.git/index"),
            &watched,
            &config
        ));
    }

    #[test]
    fn paths_outside_every_watch_root_are_ignored() {
        let config = IndexerConfig::default();
        let watched = vec![PathBuf::from("/home/andy/src/myapp")];

        assert!(!is_watched_path(
            Path::new("/home/andy/src/other/main.rs"),
            &watched,
            &config
        ));
    }

    #[test]
    fn symlink_discovery_skips_pruned_and_hidden_directories() {
        let temp = tempfile::tempdir().unwrap();
        // A workspace under an ignored-name ancestor (like /tmp on the Linux CI
        // runners): the target check must judge paths inside the workspace, not the
        // directories that merely contain it.
        let root = &temp.path().join("tmp/project");
        std::fs::create_dir_all(root).unwrap();

        // The target a real symlink points at
        let shared = root.join("shared");
        std::fs::create_dir_all(shared.join("lib")).unwrap();

        // A symlink we should find, and two we should never walk far enough to see
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
        std::fs::create_dir_all(root.join(".git/objects")).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            symlink(shared.join("lib"), root.join("src/lib")).unwrap();
            symlink(shared.join("lib"), root.join("node_modules/pkg/lib")).unwrap();
            symlink(shared.join("lib"), root.join(".git/objects/lib")).unwrap();
        }

        let targets = find_symlink_targets(root, &IndexerConfig::default());

        #[cfg(unix)]
        {
            let expected = std::fs::canonicalize(shared.join("lib")).unwrap();
            assert_eq!(
                targets,
                vec![expected],
                "only symlinks in walked directories should be watched"
            );
        }
        #[cfg(not(unix))]
        let _ = targets;
    }
}
