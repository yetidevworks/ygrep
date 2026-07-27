use ignore::gitignore::{Gitignore, GitignoreBuilder};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

use super::symlink::{ResolvedPath, SymlinkResolver};
use crate::config::IndexerConfig;
use crate::error::Result;

/// Walks a directory tree, respecting gitignore and handling symlinks
pub struct FileWalker {
    root: PathBuf,
    config: IndexerConfig,
    gitignore: Option<Gitignore>,
    symlink_resolver: SymlinkResolver,
    /// Directory path suffixes pruned during the walk, derived from `ignore_patterns`
    prune_suffixes: Vec<String>,
}

impl FileWalker {
    pub fn new(root: PathBuf, config: IndexerConfig) -> Result<Self> {
        let gitignore = if config.respect_gitignore {
            load_gitignore(&root)
        } else {
            None
        };
        let symlink_resolver = SymlinkResolver::new(config.follow_symlinks, 20);
        let prune_suffixes = prune_suffixes(&config.ignore_patterns);

        tracing::debug!(
            "FileWalker initialized with {} ignore patterns ({} prunable directories)",
            config.ignore_patterns.len(),
            prune_suffixes.len()
        );
        for pattern in &config.ignore_patterns {
            tracing::debug!("  ignore pattern: {}", pattern);
        }

        Ok(Self {
            root,
            config,
            gitignore,
            symlink_resolver,
            prune_suffixes,
        })
    }

    /// Iterate over all indexable files in the directory tree
    pub fn walk(&mut self) -> impl Iterator<Item = WalkEntry> + '_ {
        let follow_links = self.config.follow_symlinks;
        let root = self.root.clone();
        let prune = self.prune_suffixes.clone();

        WalkDir::new(&self.root)
            .follow_links(follow_links)
            .into_iter()
            .filter_entry(move |e| {
                // The workspace root itself is always walked. Testing it would skip the
                // whole tree whenever the root is hidden or happens to be named like a
                // build directory, which is the caller's explicit choice to index.
                if e.depth() == 0 {
                    return true;
                }

                // Skip hidden files/directories
                if is_hidden(e) {
                    return false;
                }

                // Prune whole subtrees that the ignore patterns already exclude, so we
                // never descend into node_modules/ or target/ just to discard each file.
                if e.file_type().is_dir() && is_pruned_dir(&root, e.path(), &prune) {
                    return false;
                }

                true
            })
            .filter_map(|entry| entry.ok())
            .filter_map(move |entry| {
                let path = entry.path();

                // Skip directories
                if entry.file_type().is_dir() {
                    return None;
                }

                // Check gitignore
                if self.is_ignored(path) {
                    return None;
                }

                // Check custom ignore patterns
                if self.matches_ignore_pattern(path) {
                    return None;
                }

                // Check if file is indexable (text file, right extension)
                if !self.is_indexable(path) {
                    return None;
                }

                // Resolve symlinks and check for cycles/duplicates
                match self.symlink_resolver.resolve(path) {
                    Ok(ResolvedPath::Resolved {
                        original,
                        canonical,
                        is_symlink,
                    }) => Some(WalkEntry {
                        path: original,
                        canonical,
                        is_symlink,
                    }),
                    Ok(ResolvedPath::Skipped(reason)) => {
                        tracing::debug!("Skipping {}: {}", path.display(), reason);
                        None
                    }
                    Err(e) => {
                        tracing::warn!("Error resolving {}: {}", path.display(), e);
                        None
                    }
                }
            })
    }

    /// Check if a path should be ignored by gitignore
    fn is_ignored(&self, path: &Path) -> bool {
        if let Some(ref gitignore) = self.gitignore {
            let is_dir = path.is_dir();
            gitignore.matched(path, is_dir).is_ignore()
        } else {
            false
        }
    }

    /// Check if path matches custom ignore patterns
    ///
    /// Patterns are matched against the path relative to the workspace root. Matching the
    /// absolute path would let directories *above* the workspace exclude everything inside
    /// it, so a project stored under `~/build/myapp` would index nothing.
    fn matches_ignore_pattern(&self, path: &Path) -> bool {
        let relative = path.strip_prefix(&self.root).unwrap_or(path);
        let path_str = relative.to_string_lossy();

        for pattern in &self.config.ignore_patterns {
            if glob_match(pattern, &path_str) {
                return true;
            }
        }

        false
    }

    /// Check if a file should be indexed
    fn is_indexable(&self, path: &Path) -> bool {
        // Check extension filter if set
        if !self.config.include_extensions.is_empty() {
            if let Some(ext) = path.extension() {
                let ext_str = ext.to_string_lossy().to_lowercase();
                if !self
                    .config
                    .include_extensions
                    .iter()
                    .any(|e| e.to_lowercase() == ext_str)
                {
                    return false;
                }
            } else {
                return false;
            }
        }

        // Check if it's a text file
        if !is_text_file(path) {
            return false;
        }

        // Skip generated assets: bundled JS, minified CSS, compact data blobs
        if is_minified(path, self.config.max_avg_line_length) {
            tracing::debug!("Skipping minified/generated file: {}", path.display());
            return false;
        }

        true
    }

    /// Get the root directory
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Get statistics about the walk
    pub fn stats(&self) -> WalkStats {
        WalkStats {
            visited_paths: self.symlink_resolver.visited_count(),
        }
    }
}

/// An entry from walking the directory tree
#[derive(Debug, Clone)]
pub struct WalkEntry {
    /// The original path (may be a symlink)
    pub path: PathBuf,
    /// The canonical (resolved) path
    pub canonical: PathBuf,
    /// Whether this was a symlink
    pub is_symlink: bool,
}

/// Statistics about the walk
#[derive(Debug, Clone, Default)]
pub struct WalkStats {
    pub visited_paths: usize,
}

/// Load .gitignore from a directory
fn load_gitignore(root: &Path) -> Option<Gitignore> {
    let gitignore_path = root.join(".gitignore");
    if gitignore_path.exists() {
        let mut builder = GitignoreBuilder::new(root);
        if builder.add(&gitignore_path).is_none() {
            if let Ok(gi) = builder.build() {
                return Some(gi);
            }
        }
    }
    None
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

/// Check if a directory entry is hidden (starts with .)
fn is_hidden(entry: &walkdir::DirEntry) -> bool {
    entry
        .file_name()
        .to_str()
        .map(|s| s.starts_with('.'))
        .unwrap_or(false)
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

/// Check if a file is likely a text file
fn is_text_file(path: &Path) -> bool {
    // Known text extensions
    const TEXT_EXTENSIONS: &[&str] = &[
        // Programming languages
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
        // Web/markup
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
        // Templates
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
        // Documentation
        "md",
        "markdown",
        "rst",
        "txt",
        "csv",
        "sql",
        "graphql",
        "gql",
        // Config/build
        "dockerfile",
        "makefile",
        "cmake",
        "gradle",
        "pom",
        "ini",
        "conf",
        "cfg",
        // Frontend frameworks
        "vue",
        "svelte",
        "astro",
        // Infrastructure
        "tf",
        "hcl",
        "nix",
        // Data formats
        "proto",
        "thrift",
        "avsc",
        // Git/editor config
        "gitignore",
        "gitattributes",
        "editorconfig",
        "env",
    ];

    // Check extension
    if let Some(ext) = path.extension() {
        let ext_lower = ext.to_string_lossy().to_lowercase();
        if TEXT_EXTENSIONS.contains(&ext_lower.as_str()) {
            return true;
        }
    }

    // Check filename for extensionless text files
    if let Some(name) = path.file_name() {
        let name_lower = name.to_string_lossy().to_lowercase();
        const TEXT_FILENAMES: &[&str] = &[
            "dockerfile",
            "makefile",
            "rakefile",
            "gemfile",
            "procfile",
            "readme",
            "license",
            "copying",
            "authors",
            "changelog",
            "todo",
            "contributing",
        ];
        if TEXT_FILENAMES.contains(&name_lower.as_str()) {
            return true;
        }
    }

    // Fall back to checking the first bytes for binary content. Read only the head:
    // reading the whole file here would pull a multi-gigabyte blob into memory just to
    // inspect its first few kilobytes.
    match read_head(path, BINARY_SNIFF_BYTES) {
        Ok(head) => !head.contains(&0),
        Err(_) => false,
    }
}

/// Bytes sampled from the head of a file to classify it
const BINARY_SNIFF_BYTES: usize = 8192;

/// Bytes sampled when deciding whether a file is minified or bundled
const MINIFIED_SNIFF_BYTES: usize = 65536;

/// Read at most `limit` bytes from the start of a file
fn read_head(path: &Path, limit: usize) -> std::io::Result<Vec<u8>> {
    use std::io::Read;

    let file = std::fs::File::open(path)?;
    let mut buf = Vec::with_capacity(limit.min(8192));
    file.take(limit as u64).read_to_end(&mut buf)?;
    Ok(buf)
}

/// Check whether a file looks minified, bundled, or otherwise machine-generated.
///
/// Generated assets (bundled JS, minified CSS, compact JSON data, TextMate grammars)
/// are enormous relative to their usefulness in a code search, and they pack many more
/// bytes per line than hand-written source. Average line length over the head of the
/// file separates the two cleanly: measured across real projects this excludes 12-30%
/// of indexed bytes in web projects while matching nothing in a plain Rust project.
///
/// A threshold of 0 disables the check.
fn is_minified(path: &Path, max_avg_line_len: usize) -> bool {
    if max_avg_line_len == 0 {
        return false;
    }

    let Ok(head) = read_head(path, MINIFIED_SNIFF_BYTES) else {
        return false;
    };
    if head.is_empty() {
        return false;
    }

    // A file whose head has no newline at all is only suspicious once it is big enough
    // that a single line is implausible for hand-written source.
    let newlines = head.iter().filter(|b| **b == b'\n').count();
    let lines = newlines.max(1);

    head.len() / lines > max_avg_line_len
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

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

        let mut config = IndexerConfig::default();
        config.ignore_patterns = vec![];

        let mut walker = FileWalker::new(workspace.clone(), config).unwrap();

        let entries: Vec<_> = walker.walk().collect();
        assert!(
            entries.len() >= 3,
            "Expected at least 3 entries, got {}",
            entries.len()
        );
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

        let mut walker = FileWalker::new(workspace, IndexerConfig::default()).unwrap();
        let entries: Vec<_> = walker.walk().collect();

        assert_eq!(entries.len(), 1, "workspace under build/ must still index");
    }

    #[test]
    fn ignore_patterns_still_match_inside_the_workspace() {
        let (_temp, workspace) = workspace_with_source("myapp");
        std::fs::create_dir_all(workspace.join("build")).unwrap();
        std::fs::write(workspace.join("build/generated.rs"), "fn gen() {}\n").unwrap();

        let mut walker = FileWalker::new(workspace, IndexerConfig::default()).unwrap();
        let paths: Vec<_> = walker
            .walk()
            .map(|e| e.path.to_string_lossy().into_owned())
            .collect();

        assert_eq!(paths.len(), 1);
        assert!(paths[0].ends_with("src/main.rs"));
    }

    #[test]
    fn hidden_workspace_root_is_still_walked() {
        // `ygrep index ~/.config/something` used to return zero files silently.
        let (_temp, workspace) = workspace_with_source(".dotroot");

        let mut walker = FileWalker::new(workspace, IndexerConfig::default()).unwrap();
        let entries: Vec<_> = walker.walk().collect();

        assert_eq!(entries.len(), 1, "hidden root must still be indexed");
    }

    #[test]
    fn hidden_directories_inside_the_workspace_are_still_skipped() {
        let (_temp, workspace) = workspace_with_source("myapp");
        std::fs::create_dir_all(workspace.join(".hidden")).unwrap();
        std::fs::write(workspace.join(".hidden/secret.rs"), "fn s() {}\n").unwrap();

        let mut walker = FileWalker::new(workspace, IndexerConfig::default()).unwrap();
        let entries: Vec<_> = walker.walk().collect();

        assert_eq!(entries.len(), 1);
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
    fn minified_files_are_detected_and_source_is_not() {
        let temp = tempdir().unwrap();

        let bundle = temp.path().join("app.bundle.js");
        std::fs::write(&bundle, format!("var a=1;{}\n", "x".repeat(5000))).unwrap();
        assert!(is_minified(&bundle, 400));

        let source = temp.path().join("main.rs");
        let normal: String = (0..200)
            .map(|i| format!("    let x{} = {};\n", i, i))
            .collect();
        std::fs::write(&source, &normal).unwrap();
        assert!(!is_minified(&source, 400));

        // A threshold of zero disables the check.
        assert!(!is_minified(&bundle, 0));
    }

    #[test]
    fn minified_files_are_excluded_from_the_walk() {
        let (_temp, workspace) = workspace_with_source("myapp");
        std::fs::write(
            workspace.join("src/vendor.js"),
            format!("var a=1;{}\n", "y".repeat(20000)),
        )
        .unwrap();

        let mut walker = FileWalker::new(workspace, IndexerConfig::default()).unwrap();
        let paths: Vec<_> = walker
            .walk()
            .map(|e| e.path.to_string_lossy().into_owned())
            .collect();

        assert_eq!(paths.len(), 1);
        assert!(paths[0].ends_with("src/main.rs"));
    }

    #[test]
    fn read_head_stops_at_the_limit() {
        let temp = tempdir().unwrap();
        let big = temp.path().join("big.bin");
        std::fs::write(&big, vec![b'a'; 100_000]).unwrap();

        let head = read_head(&big, 8192).unwrap();

        assert_eq!(head.len(), 8192, "must not read the whole file");
    }
}
