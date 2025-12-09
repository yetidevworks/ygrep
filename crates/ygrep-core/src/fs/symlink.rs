use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{Result, YgrepError};

/// Resolves symlinks and detects circular references
pub struct SymlinkResolver {
    /// Set of visited canonical paths (for circular detection)
    visited_canonical: HashSet<PathBuf>,

    /// Maximum symlink depth to follow
    max_depth: usize,

    /// Whether to follow symlinks
    follow_symlinks: bool,
}

impl SymlinkResolver {
    pub fn new(follow_symlinks: bool, max_depth: usize) -> Self {
        Self {
            visited_canonical: HashSet::new(),
            max_depth,
            follow_symlinks,
        }
    }

    /// Resolve a path, handling symlinks and detecting cycles
    pub fn resolve(&mut self, path: &Path) -> Result<ResolvedPath> {
        self.resolve_inner(path, 0)
    }

    fn resolve_inner(&mut self, path: &Path, depth: usize) -> Result<ResolvedPath> {
        if depth > self.max_depth {
            return Err(YgrepError::SymlinkDepthExceeded(path.to_path_buf()));
        }

        let metadata = match fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ResolvedPath::Skipped(SkipReason::NotFound));
            }
            Err(e) => return Err(e.into()),
        };

        if metadata.is_symlink() {
            if !self.follow_symlinks {
                return Ok(ResolvedPath::Skipped(SkipReason::SymlinkNotFollowed));
            }

            // Read the symlink target
            let target = match fs::read_link(path) {
                Ok(t) => t,
                Err(_) => {
                    return Ok(ResolvedPath::Skipped(SkipReason::BrokenSymlink));
                }
            };

            // Resolve to absolute path
            let resolved = if target.is_absolute() {
                target
            } else {
                path.parent()
                    .ok_or_else(|| YgrepError::InvalidPath(path.to_path_buf()))?
                    .join(&target)
            };

            // Get canonical path for cycle detection
            let canonical = match fs::canonicalize(&resolved) {
                Ok(c) => c,
                Err(_) => {
                    return Ok(ResolvedPath::Skipped(SkipReason::BrokenSymlink));
                }
            };

            // Check for circular symlink
            if self.visited_canonical.contains(&canonical) {
                return Ok(ResolvedPath::Skipped(SkipReason::CircularSymlink));
            }

            self.visited_canonical.insert(canonical.clone());

            return Ok(ResolvedPath::Resolved {
                original: path.to_path_buf(),
                canonical,
                is_symlink: true,
            });
        }

        // Not a symlink - get canonical path for deduplication
        let canonical = match fs::canonicalize(path) {
            Ok(c) => c,
            Err(_) => path.to_path_buf(),
        };

        // Check if we've already visited this canonical path
        if self.visited_canonical.contains(&canonical) {
            return Ok(ResolvedPath::Skipped(SkipReason::Duplicate));
        }

        self.visited_canonical.insert(canonical.clone());

        Ok(ResolvedPath::Resolved {
            original: path.to_path_buf(),
            canonical,
            is_symlink: false,
        })
    }

    /// Check if a canonical path has been visited
    pub fn is_visited(&self, canonical: &Path) -> bool {
        self.visited_canonical.contains(canonical)
    }

    /// Mark a canonical path as visited
    pub fn mark_visited(&mut self, canonical: PathBuf) {
        self.visited_canonical.insert(canonical);
    }

    /// Reset visited set (for new indexing run)
    pub fn reset(&mut self) {
        self.visited_canonical.clear();
    }

    /// Get count of visited paths
    pub fn visited_count(&self) -> usize {
        self.visited_canonical.len()
    }
}

/// Result of resolving a path
#[derive(Debug, Clone)]
pub enum ResolvedPath {
    Resolved {
        original: PathBuf,
        canonical: PathBuf,
        is_symlink: bool,
    },
    Skipped(SkipReason),
}

impl ResolvedPath {
    pub fn canonical(&self) -> Option<&Path> {
        match self {
            ResolvedPath::Resolved { canonical, .. } => Some(canonical),
            ResolvedPath::Skipped(_) => None,
        }
    }

    pub fn is_skipped(&self) -> bool {
        matches!(self, ResolvedPath::Skipped(_))
    }
}

/// Reason why a path was skipped
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    CircularSymlink,
    SymlinkNotFollowed,
    BrokenSymlink,
    Duplicate,
    NotFound,
}

impl std::fmt::Display for SkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SkipReason::CircularSymlink => write!(f, "circular symlink"),
            SkipReason::SymlinkNotFollowed => write!(f, "symlink not followed"),
            SkipReason::BrokenSymlink => write!(f, "broken symlink"),
            SkipReason::Duplicate => write!(f, "duplicate path"),
            SkipReason::NotFound => write!(f, "not found"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_regular_file() {
        let temp_dir = tempdir().unwrap();
        let file_path = temp_dir.path().join("test.txt");
        fs::write(&file_path, "content").unwrap();

        let mut resolver = SymlinkResolver::new(true, 10);
        let result = resolver.resolve(&file_path).unwrap();

        match result {
            ResolvedPath::Resolved { is_symlink, .. } => {
                assert!(!is_symlink);
            }
            _ => panic!("Expected Resolved"),
        }
    }

    #[test]
    fn test_symlink_detection() {
        let temp_dir = tempdir().unwrap();
        let file_path = temp_dir.path().join("target.txt");
        let link_path = temp_dir.path().join("link.txt");

        fs::write(&file_path, "content").unwrap();

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&file_path, &link_path).unwrap();

            let mut resolver = SymlinkResolver::new(true, 10);
            let result = resolver.resolve(&link_path).unwrap();

            match result {
                ResolvedPath::Resolved { is_symlink, .. } => {
                    assert!(is_symlink);
                }
                _ => panic!("Expected Resolved"),
            }
        }
    }

    #[test]
    fn test_duplicate_detection() {
        let temp_dir = tempdir().unwrap();
        let file_path = temp_dir.path().join("test.txt");
        fs::write(&file_path, "content").unwrap();

        let mut resolver = SymlinkResolver::new(true, 10);

        // First resolution should succeed
        let result1 = resolver.resolve(&file_path).unwrap();
        assert!(!result1.is_skipped());

        // Second resolution should be skipped as duplicate
        let result2 = resolver.resolve(&file_path).unwrap();
        match result2 {
            ResolvedPath::Skipped(SkipReason::Duplicate) => {}
            _ => panic!("Expected Skipped(Duplicate)"),
        }
    }
}
