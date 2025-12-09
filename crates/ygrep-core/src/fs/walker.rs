use std::path::{Path, PathBuf};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use walkdir::WalkDir;

use crate::config::IndexerConfig;
use crate::error::Result;
use super::symlink::{SymlinkResolver, ResolvedPath};

/// Walks a directory tree, respecting gitignore and handling symlinks
pub struct FileWalker {
    root: PathBuf,
    config: IndexerConfig,
    gitignore: Option<Gitignore>,
    symlink_resolver: SymlinkResolver,
}

impl FileWalker {
    pub fn new(root: PathBuf, config: IndexerConfig) -> Result<Self> {
        let gitignore = if config.respect_gitignore {
            load_gitignore(&root)
        } else {
            None
        };
        let symlink_resolver = SymlinkResolver::new(config.follow_symlinks, 20);

        tracing::debug!("FileWalker initialized with {} ignore patterns", config.ignore_patterns.len());
        for pattern in &config.ignore_patterns {
            tracing::debug!("  ignore pattern: {}", pattern);
        }

        Ok(Self {
            root,
            config,
            gitignore,
            symlink_resolver,
        })
    }

    /// Iterate over all indexable files in the directory tree
    pub fn walk(&mut self) -> impl Iterator<Item = WalkEntry> + '_ {
        let follow_links = self.config.follow_symlinks;
        let ignore_patterns = self.config.ignore_patterns.clone();

        WalkDir::new(&self.root)
            .follow_links(follow_links)
            .into_iter()
            .filter_entry(move |e| {
                // Skip hidden files/directories
                if is_hidden(e) {
                    return false;
                }

                // Skip directories matching ignore patterns
                if e.file_type().is_dir() {
                    let dir_name = e.file_name().to_string_lossy();

                    // Quick check for common ignored directories
                    let dominated = matches!(
                        dir_name.as_ref(),
                        "cache" | "node_modules" | "vendor" | "target" | "dist" |
                        "build" | "logs" | "log" | "tmp" | "temp" | "var" |
                        "__pycache__" | ".git" | ".svn" | "coverage" | "htmlcov"
                    );

                    if dominated {
                        return false;
                    }
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
                    Ok(ResolvedPath::Resolved { original, canonical, is_symlink }) => {
                        Some(WalkEntry {
                            path: original,
                            canonical,
                            is_symlink,
                        })
                    }
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
    fn matches_ignore_pattern(&self, path: &Path) -> bool {
        let path_str = path.to_string_lossy();

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
                if !self.config.include_extensions.iter().any(|e| e.to_lowercase() == ext_str) {
                    return false;
                }
            } else {
                return false;
            }
        }

        // Check if it's a text file
        is_text_file(path)
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

/// Check if a directory entry is hidden (starts with .)
fn is_hidden(entry: &walkdir::DirEntry) -> bool {
    entry
        .file_name()
        .to_str()
        .map(|s| s.starts_with('.'))
        .unwrap_or(false)
}

/// Glob matching for directory patterns - used in filter_entry to skip entire directories
fn glob_match_dir(pattern: &str, path: &str) -> bool {
    // Handle **/dir/** patterns - directory name anywhere
    if pattern.starts_with("**/") && pattern.ends_with("/**") {
        let dir_name = &pattern[3..pattern.len()-3];
        // Check if this is the directory or path ends with the directory
        return path.ends_with(&format!("/{}", dir_name))
            || path.ends_with(dir_name)
            || path.contains(&format!("/{}/", dir_name));
    }

    // Handle **/something patterns - directory name at end
    if pattern.starts_with("**/") {
        let suffix = &pattern[3..];
        // Remove trailing /** if present
        let suffix = suffix.strip_suffix("/**").unwrap_or(suffix);
        return path.ends_with(suffix) || path.ends_with(&format!("/{}", suffix));
    }

    // Handle something/** patterns - prefix match
    if pattern.ends_with("/**") {
        let prefix = &pattern[..pattern.len()-3];
        return path.ends_with(prefix) || path.ends_with(&format!("/{}", prefix));
    }

    // Exact directory name match
    path.ends_with(pattern) || path.ends_with(&format!("/{}", pattern))
}

/// Simple glob matching for ignore patterns (for files)
fn glob_match(pattern: &str, path: &str) -> bool {
    // Handle **/dir/** patterns (match dir anywhere in path)
    if pattern.starts_with("**/") && pattern.ends_with("/**") {
        let dir_name = &pattern[3..pattern.len()-3];
        // Check if this directory name appears as a complete path component
        return path.contains(&format!("/{}/", dir_name))
            || path.starts_with(&format!("{}/", dir_name))
            || path.ends_with(&format!("/{}", dir_name));  // At end of path (exact match)
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
        let prefix = &pattern[..pattern.len()-3];
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
        "rs", "py", "js", "ts", "jsx", "tsx", "mjs", "mts", "cjs", "cts",
        "go", "rb", "php", "java", "c", "cpp", "cc", "h", "hpp", "hh",
        "cs", "swift", "kt", "scala", "clj", "ex", "exs", "erl", "hs", "ml", "fs", "r", "jl",
        "lua", "pl", "pm", "sh", "bash", "zsh", "fish", "ps1", "bat", "cmd",
        // Web/markup
        "html", "htm", "css", "scss", "sass", "less", "xml", "json", "yaml", "yml", "toml",
        // Templates
        "twig", "blade", "ejs", "hbs", "handlebars", "mustache", "pug", "jade", "erb", "haml",
        "njk", "nunjucks", "jinja", "jinja2", "liquid", "eta",
        // Documentation
        "md", "markdown", "rst", "txt", "csv", "sql", "graphql", "gql",
        // Config/build
        "dockerfile", "makefile", "cmake", "gradle", "pom", "ini", "conf", "cfg",
        // Frontend frameworks
        "vue", "svelte", "astro",
        // Infrastructure
        "tf", "hcl", "nix",
        // Data formats
        "proto", "thrift", "avsc",
        // Git/editor config
        "gitignore", "gitattributes", "editorconfig", "env",
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
            "dockerfile", "makefile", "rakefile", "gemfile", "procfile",
            "readme", "license", "copying", "authors", "changelog",
            "todo", "contributing",
        ];
        if TEXT_FILENAMES.contains(&name_lower.as_str()) {
            return true;
        }
    }

    // Fall back to checking first bytes for binary content
    if let Ok(bytes) = std::fs::read(path) {
        // Check first 8KB for null bytes
        let check_len = bytes.len().min(8192);
        !bytes[..check_len].contains(&0)
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_walk_directory() {
        let temp_dir = tempdir().unwrap();

        // Create some files
        std::fs::write(temp_dir.path().join("test.rs"), "fn main() {}").unwrap();
        std::fs::write(temp_dir.path().join("readme.md"), "# Hello").unwrap();
        std::fs::create_dir(temp_dir.path().join("src")).unwrap();
        std::fs::write(temp_dir.path().join("src/lib.rs"), "pub mod lib;").unwrap();

        let config = IndexerConfig::default();
        let mut walker = FileWalker::new(temp_dir.path().to_path_buf(), config).unwrap();

        let entries: Vec<_> = walker.walk().collect();
        assert!(entries.len() >= 3);
    }

    #[test]
    fn test_glob_match() {
        assert!(glob_match("**/node_modules/**", "foo/node_modules/bar/baz.js"));
        assert!(glob_match("**/.git/**", ".git/config"));
        assert!(glob_match("*.log", "debug.log"));
        assert!(!glob_match("*.log", "debug.txt"));
    }
}
