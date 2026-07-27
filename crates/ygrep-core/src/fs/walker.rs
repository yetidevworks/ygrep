use ignore::{DirEntry, WalkBuilder, WalkState};
use parking_lot::Mutex;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use super::classify;
use crate::config::IndexerConfig;
use crate::error::Result;
use crate::index::FileMeta;

/// Walks a directory tree, respecting gitignore and handling symlinks
pub struct FileWalker {
    root: PathBuf,
    filter: Arc<EntryFilter>,
}

impl FileWalker {
    pub fn new(root: PathBuf, config: IndexerConfig) -> Result<Self> {
        let prune_suffixes = prune_suffixes(&config.ignore_patterns);

        tracing::debug!(
            "FileWalker initialized with {} ignore patterns ({} prunable directories)",
            config.ignore_patterns.len(),
            prune_suffixes.len()
        );

        Ok(Self {
            filter: Arc::new(EntryFilter {
                canonical_root: std::fs::canonicalize(&root).unwrap_or_else(|_| root.clone()),
                root: root.clone(),
                prune_suffixes,
                config,
                followed_symlinks: Mutex::new(HashSet::new()),
                visited: AtomicUsize::new(0),
            }),
            root,
        })
    }

    /// Iterate over all indexable files in the directory tree
    pub fn walk(&mut self) -> impl Iterator<Item = WalkEntry> + '_ {
        let filter = Arc::clone(&self.filter);

        self.builder()
            .build()
            .filter_map(move |entry| filter.accept(entry.ok()?))
    }

    /// Walk the tree on `threads` worker threads, handing each entry to a visitor.
    ///
    /// `make_visitor` is called once per worker thread, on the calling thread, so a
    /// visitor can own per-thread state (a channel sender, a reusable buffer) without
    /// any synchronisation of its own.
    pub fn walk_parallel<M, V>(&self, mut make_visitor: M)
    where
        M: FnMut() -> V,
        V: FnMut(WalkEntry) + Send,
    {
        self.builder().build_parallel().run(|| {
            let filter = Arc::clone(&self.filter);
            let mut visit = make_visitor();

            Box::new(move |entry| {
                if let Some(walk_entry) = entry.ok().and_then(|e| filter.accept(e)) {
                    visit(walk_entry);
                }
                WalkState::Continue
            })
        });
    }

    fn builder(&self) -> WalkBuilder {
        let config = &self.filter.config;
        let respect_gitignore = config.respect_gitignore;
        let mut builder = WalkBuilder::new(&self.root);

        // Hidden entries are judged by our own rules below, not skipped wholesale:
        // `.github/workflows/*.yml` and `.gitignore` are source like any other.
        builder
            .hidden(false)
            .follow_links(config.follow_symlinks)
            .threads(config.threads)
            .git_ignore(respect_gitignore)
            .git_global(respect_gitignore)
            .git_exclude(respect_gitignore)
            .ignore(respect_gitignore)
            .parents(respect_gitignore)
            // Gitignore rules are worth honouring in a checkout that has no .git of its
            // own — a worktree, a vendored copy, an export.
            .require_git(false);

        let filter = Arc::clone(&self.filter);
        builder.filter_entry(move |entry| filter.descend(entry));

        builder
    }

    /// Get the root directory
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Get statistics about the walk
    pub fn stats(&self) -> WalkStats {
        WalkStats {
            visited_paths: self.filter.visited.load(Ordering::Relaxed),
        }
    }
}

/// The rules applied to every entry the walk produces.
///
/// Shared by the sequential and parallel walks so they can never drift apart.
struct EntryFilter {
    root: PathBuf,
    /// The root with every symlink resolved, to compare canonical targets against
    canonical_root: PathBuf,
    config: IndexerConfig,
    /// Directory path suffixes pruned during the walk, derived from `ignore_patterns`
    prune_suffixes: Vec<String>,
    /// Canonical targets of symlinks already followed, so a tree reachable through two
    /// links is walked once. Only symlinked entries are canonicalized: doing it for
    /// every file cost more than the rest of the walk put together.
    followed_symlinks: Mutex<HashSet<PathBuf>>,
    visited: AtomicUsize,
}

impl EntryFilter {
    /// Whether the walk should look at this entry at all (and descend, for directories)
    fn descend(&self, entry: &DirEntry) -> bool {
        // The workspace root itself is always walked. Testing it would skip the whole
        // tree whenever the root is hidden or happens to be named like a build
        // directory, which is the caller's explicit choice to index.
        if entry.depth() == 0 {
            return true;
        }

        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);

        if let Some(name) = entry.file_name().to_str() {
            if name.starts_with('.') && is_dir && !classify::TEXT_DOT_DIRS.contains(&name) {
                return false;
            }
        }

        if is_dir {
            // Prune whole subtrees that the ignore patterns already exclude, so we never
            // descend into node_modules/ or target/ just to discard each file.
            if is_pruned_dir(&self.root, entry.path(), &self.prune_suffixes) {
                return false;
            }

            if entry.path_is_symlink() && self.skip_symlink(entry.path()) {
                return false;
            }
        }

        true
    }

    /// Whether this entry should be indexed, and the metadata the indexer will need
    fn accept(&self, entry: DirEntry) -> Option<WalkEntry> {
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(true) {
            return None;
        }

        let path = entry.path();

        if self.matches_ignore_pattern(path) {
            return None;
        }

        if !classify::is_indexable(path, &self.config) {
            return None;
        }

        if entry.path_is_symlink() && (!self.config.follow_symlinks || self.skip_symlink(path)) {
            return None;
        }

        // The one stat per file: the indexer needs the size to enforce the file-size
        // limit and the mtime to decide whether the file changed, and re-reading either
        // later would mean walking the tree's metadata twice.
        let metadata = entry.metadata().ok()?;
        let mtime = metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);

        self.visited.fetch_add(1, Ordering::Relaxed);

        Some(WalkEntry {
            path: path.to_path_buf(),
            meta: FileMeta {
                size: metadata.len(),
                mtime,
            },
        })
    }

    /// Whether a symlink leads somewhere the walk covers by another route.
    ///
    /// A link into the workspace itself is always redundant — the walk reaches the real
    /// path anyway — and two links to the same tree outside it are followed once. This
    /// is the only place anything is canonicalized: doing it for every file, which is
    /// how duplicates used to be found, cost more than the rest of the walk together.
    fn skip_symlink(&self, path: &Path) -> bool {
        let Ok(canonical) = std::fs::canonicalize(path) else {
            return true;
        };

        if canonical.starts_with(&self.canonical_root) {
            return true;
        }

        !self.followed_symlinks.lock().insert(canonical)
    }

    /// Check if path matches custom ignore patterns
    ///
    /// Patterns are matched against the path relative to the workspace root. Matching
    /// the absolute path would let directories *above* the workspace exclude everything
    /// inside it, so a project stored under `~/build/myapp` would index nothing.
    fn matches_ignore_pattern(&self, path: &Path) -> bool {
        let relative = path.strip_prefix(&self.root).unwrap_or(path);
        let path_str = relative.to_string_lossy();

        self.config
            .ignore_patterns
            .iter()
            .any(|pattern| glob_match(pattern, &path_str))
    }
}

/// An entry from walking the directory tree
#[derive(Debug, Clone)]
pub struct WalkEntry {
    /// The path to the file
    pub path: PathBuf,
    /// Size and mtime read during the walk
    pub meta: FileMeta,
}

/// Statistics about the walk
#[derive(Debug, Clone, Default)]
pub struct WalkStats {
    pub visited_paths: usize,
}

/// Directory path suffixes that can be pruned wholesale during the walk.
///
/// Derived from `**/<segments>/**` ignore patterns so that pruning always follows the
/// configured patterns instead of a separate hardcoded list that could drift from them.
pub(crate) fn prune_suffixes(patterns: &[String]) -> Vec<String> {
    patterns
        .iter()
        .filter_map(|pattern| {
            let inner = pattern.strip_prefix("**/")?.strip_suffix("/**")?;
            // A wildcard inside the segment can't be reduced to a directory name.
            if inner.is_empty() || inner.contains('*') {
                return None;
            }
            Some(inner.to_string())
        })
        .collect()
}

/// Whether a directory is covered by one of the prunable ignore patterns.
///
/// Compared against the root-relative path so directories above the workspace can't
/// prune the tree the caller asked to index.
pub(crate) fn is_pruned_dir(root: &Path, path: &Path, suffixes: &[String]) -> bool {
    let relative = path.strip_prefix(root).unwrap_or(path);
    let path_str = relative.to_string_lossy().replace('\\', "/");

    suffixes
        .iter()
        .any(|suffix| path_str == *suffix || path_str.ends_with(&format!("/{}", suffix)))
}

/// Simple glob matching for ignore patterns (for files)
pub(crate) fn glob_match(pattern: &str, path: &str) -> bool {
    // Handle **/dir/** patterns (match dir anywhere in path)
    if pattern.starts_with("**/") && pattern.ends_with("/**") {
        let dir_name = &pattern[3..pattern.len() - 3];
        // Check if this directory name appears as a complete path component
        return path.contains(&format!("/{}/", dir_name))
            || path.starts_with(&format!("{}/", dir_name))
            || path.ends_with(&format!("/{}", dir_name)); // At end of path (exact match)
    }

    // Handle **/*.ext patterns (match extension anywhere)
    if pattern.starts_with("**/*.") {
        let ext = &pattern[5..]; // Get everything after "**/*." (index 5 skips the dot)
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
    use tempfile::tempdir;

    fn walked_paths(workspace: &Path, config: IndexerConfig) -> Vec<String> {
        let root = workspace.to_path_buf();
        let mut walker = FileWalker::new(root.clone(), config).unwrap();
        let mut paths: Vec<String> = walker
            .walk()
            .map(|e| {
                e.path
                    .strip_prefix(&root)
                    .unwrap_or(&e.path)
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        paths.sort();
        paths
    }

    #[test]
    fn test_walk_directory() {
        let temp_dir = tempdir().unwrap();

        // Ignore patterns are matched relative to the workspace root, so tempdir path
        // components like "tmp" and "var" above the root no longer affect the walk.
        let workspace = temp_dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();

        // Create some files
        std::fs::write(workspace.join("test.rs"), "fn main() {}").unwrap();
        std::fs::write(workspace.join("readme.md"), "# Hello").unwrap();
        std::fs::create_dir(workspace.join("src")).unwrap();
        std::fs::write(workspace.join("src/lib.rs"), "pub mod lib;").unwrap();

        let config = IndexerConfig {
            ignore_patterns: vec![],
            ..Default::default()
        };

        let paths = walked_paths(&workspace, config);
        assert_eq!(paths, vec!["readme.md", "src/lib.rs", "test.rs"]);
    }

    #[test]
    fn the_parallel_walk_sees_the_same_files() {
        let temp_dir = tempdir().unwrap();
        let workspace = temp_dir.path().join("workspace");
        std::fs::create_dir_all(workspace.join("src/deep")).unwrap();
        for i in 0..50 {
            std::fs::write(workspace.join(format!("src/f{i}.rs")), "fn f() {}\n").unwrap();
            std::fs::write(workspace.join(format!("src/deep/g{i}.rs")), "fn g() {}\n").unwrap();
        }

        let sequential = walked_paths(&workspace, IndexerConfig::default());

        let walker = FileWalker::new(workspace.clone(), IndexerConfig::default()).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        walker.walk_parallel(|| {
            let tx = tx.clone();
            move |entry: WalkEntry| {
                let _ = tx.send(entry.path);
            }
        });
        drop(tx);

        let mut parallel: Vec<String> = rx
            .into_iter()
            .map(|p| {
                p.strip_prefix(&workspace)
                    .unwrap_or(&p)
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        parallel.sort();

        assert_eq!(parallel, sequential);
        assert_eq!(parallel.len(), 100);
    }

    #[test]
    fn test_glob_match() {
        assert!(glob_match(
            "**/node_modules/**",
            "foo/node_modules/bar/baz.js"
        ));
        assert!(glob_match("**/.git/**", ".git/config"));
        assert!(glob_match("*.log", "debug.log"));
        assert!(!glob_match("*.log", "debug.txt"));
    }

    /// Build a workspace with one source file and return (tempdir, workspace root).
    fn workspace_with_source(relative_root: &str) -> (tempfile::TempDir, PathBuf) {
        let temp_dir = tempdir().unwrap();
        let workspace = temp_dir.path().join(relative_root);
        std::fs::create_dir_all(workspace.join("src")).unwrap();
        std::fs::write(workspace.join("src/main.rs"), "fn main() {}\n").unwrap();
        (temp_dir, workspace)
    }

    #[test]
    fn ignore_patterns_do_not_match_directories_above_the_workspace() {
        // A project stored under a directory named "build" must still index. Matching
        // the absolute path made the whole workspace vanish.
        let (_temp, workspace) = workspace_with_source("build/myapp");

        assert_eq!(
            walked_paths(&workspace, IndexerConfig::default()),
            vec!["src/main.rs"],
            "workspace under build/ must still index"
        );
    }

    #[test]
    fn ignore_patterns_still_match_inside_the_workspace() {
        let (_temp, workspace) = workspace_with_source("myapp");
        std::fs::create_dir_all(workspace.join("build")).unwrap();
        std::fs::write(workspace.join("build/generated.rs"), "fn gen() {}\n").unwrap();

        assert_eq!(
            walked_paths(&workspace, IndexerConfig::default()),
            vec!["src/main.rs"]
        );
    }

    #[test]
    fn hidden_workspace_root_is_still_walked() {
        // `ygrep index ~/.config/something` used to return zero files silently.
        let (_temp, workspace) = workspace_with_source(".dotroot");

        assert_eq!(
            walked_paths(&workspace, IndexerConfig::default()),
            vec!["src/main.rs"],
            "hidden root must still be indexed"
        );
    }

    #[test]
    fn hidden_directories_inside_the_workspace_are_still_skipped() {
        let (_temp, workspace) = workspace_with_source("myapp");
        std::fs::create_dir_all(workspace.join(".hidden")).unwrap();
        std::fs::write(workspace.join(".hidden/secret.rs"), "fn s() {}\n").unwrap();

        assert_eq!(
            walked_paths(&workspace, IndexerConfig::default()),
            vec!["src/main.rs"]
        );
    }

    #[test]
    fn workflow_files_and_useful_dotfiles_are_indexed() {
        let (_temp, workspace) = workspace_with_source("myapp");
        std::fs::create_dir_all(workspace.join(".github/workflows")).unwrap();
        std::fs::write(workspace.join(".github/workflows/ci.yml"), "on: push\n").unwrap();
        std::fs::write(workspace.join(".gitignore"), "target\n").unwrap();
        std::fs::write(workspace.join(".editorconfig"), "root = true\n").unwrap();
        std::fs::write(workspace.join(".DS_Store"), "junk").unwrap();

        assert_eq!(
            walked_paths(&workspace, IndexerConfig::default()),
            vec![
                ".editorconfig",
                ".github/workflows/ci.yml",
                ".gitignore",
                "src/main.rs",
            ]
        );
    }

    #[test]
    fn nested_gitignore_files_are_honoured() {
        let (_temp, workspace) = workspace_with_source("myapp");
        std::fs::create_dir_all(workspace.join("src/nested")).unwrap();
        std::fs::write(workspace.join("src/nested/keep.rs"), "fn keep() {}\n").unwrap();
        std::fs::write(workspace.join("src/nested/drop.rs"), "fn drop_me() {}\n").unwrap();
        std::fs::write(workspace.join("src/nested/.gitignore"), "drop.rs\n").unwrap();

        let config = IndexerConfig {
            respect_gitignore: true,
            ..Default::default()
        };

        let paths = walked_paths(&workspace, config);
        assert!(paths.contains(&"src/nested/keep.rs".to_string()));
        assert!(
            !paths.contains(&"src/nested/drop.rs".to_string()),
            "a nested .gitignore must exclude its own directory: {paths:?}"
        );

        // With gitignore disabled the same file is indexed again.
        let paths = walked_paths(&workspace, IndexerConfig::default());
        assert!(paths.contains(&"src/nested/drop.rs".to_string()));
    }

    #[cfg(unix)]
    #[test]
    fn a_tree_reachable_twice_is_walked_once() {
        use std::os::unix::fs::symlink;

        let (_temp, workspace) = workspace_with_source("myapp");
        std::fs::write(workspace.join("src/other.rs"), "fn other() {}\n").unwrap();

        // A link into the workspace: everything behind it is walked by its real path.
        symlink(workspace.join("src"), workspace.join("alias")).unwrap();

        // Two links to the same tree outside the workspace: followed once.
        let outside = _temp.path().join("shared");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("shared.rs"), "fn shared() {}\n").unwrap();
        symlink(&outside, workspace.join("first")).unwrap();
        symlink(&outside, workspace.join("second")).unwrap();

        let paths = walked_paths(&workspace, IndexerConfig::default());

        assert_eq!(paths.len(), 3, "{paths:?}");
        assert!(paths.contains(&"src/main.rs".to_string()));
        assert!(paths.contains(&"src/other.rs".to_string()));
        assert_eq!(
            paths
                .iter()
                .filter(|p| p.ends_with("shared.rs"))
                .collect::<Vec<_>>()
                .len(),
            1
        );
    }

    #[test]
    fn prune_suffixes_are_derived_from_ignore_patterns() {
        let patterns = vec![
            "**/node_modules/**".to_string(),
            "**/var/cache/**".to_string(),
            "**/*.log".to_string(),
            "Cargo.lock".to_string(),
            "**/*.dSYM/**".to_string(),
        ];

        let suffixes = prune_suffixes(&patterns);

        assert!(suffixes.contains(&"node_modules".to_string()));
        assert!(suffixes.contains(&"var/cache".to_string()));
        // Wildcards inside the segment can't be reduced to a directory name.
        assert!(!suffixes.iter().any(|s| s.contains('*')));
        assert!(!suffixes.contains(&"Cargo.lock".to_string()));
    }

    #[test]
    fn pruning_is_relative_to_the_workspace_root() {
        let root = Path::new("/home/me/build/myapp");
        let suffixes = vec!["build".to_string(), "var/cache".to_string()];

        // The "build" above the root must not prune the root's own contents.
        assert!(!is_pruned_dir(
            root,
            Path::new("/home/me/build/myapp/src"),
            &suffixes
        ));
        assert!(is_pruned_dir(
            root,
            Path::new("/home/me/build/myapp/build"),
            &suffixes
        ));
        assert!(is_pruned_dir(
            root,
            Path::new("/home/me/build/myapp/var/cache"),
            &suffixes
        ));
        // A bare "var" is not pruned when the pattern is "var/cache".
        assert!(!is_pruned_dir(
            root,
            Path::new("/home/me/build/myapp/var"),
            &suffixes
        ));
    }

    #[test]
    fn the_walk_yields_the_metadata_the_indexer_needs() {
        let (_temp, workspace) = workspace_with_source("myapp");

        let mut walker = FileWalker::new(workspace.clone(), IndexerConfig::default()).unwrap();
        let entries: Vec<_> = walker.walk().collect();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].meta.size, 13);
        assert!(entries[0].meta.mtime > 0);
        assert_eq!(walker.stats().visited_paths, 1);
    }
}
