#[cfg(feature = "embeddings")]
mod hybrid;
mod results;
mod searcher;

#[cfg(feature = "embeddings")]
pub use hybrid::HybridSearcher;
pub use results::{MatchType, SearchHit, SearchResult};
pub use searcher::{SearchFilters, Searcher};

use tantivy::{Directory, Index, IndexReader};

/// Open an IndexReader with retry-and-backoff for META_LOCK contention (issue #7).
///
/// Tantivy's `index.reader()` acquires an exclusive flock on `.tantivy-meta.lock`.
/// On macOS this can transiently fail with EPERM when `ygrep watch` is committing
/// concurrently.  We retry up to 3 times with exponential backoff (100/200/400 ms),
/// clearing the lockfile before the last attempt in case its inode is holding a flock
/// nobody owns any more.
///
/// If the error is a permission issue (e.g. sandboxed `~/Library/Application Support`),
/// we fail immediately with a helpful message suggesting `XDG_DATA_HOME`.
pub(crate) fn open_reader_with_retry(index: &Index) -> crate::error::Result<IndexReader> {
    let mut last_err = None;
    for attempt in 0..4 {
        match index.reader() {
            Ok(reader) => return Ok(reader),
            Err(e) => {
                let msg = e.to_string();
                // Permission errors are not transient — fail immediately with guidance
                if msg.contains("PermissionDenied") || msg.contains("Operation not permitted") {
                    return Err(crate::error::YgrepError::Search(format!(
                        "Index directory is not writable: {}\n\n\
                         Hint: Set XDG_DATA_HOME to a writable location, e.g.:\n  \
                         export XDG_DATA_HOME=\"$PWD/.ygrep-data\"",
                        msg
                    )));
                }
                if msg.contains("Lockfile") && attempt < 3 {
                    let wait_ms = 100 * (1u64 << attempt);
                    tracing::debug!(
                        "META_LOCK contention (attempt {}/3), retrying in {}ms…",
                        attempt + 1,
                        wait_ms
                    );
                    // Last chance: a lockfile inode left behind by a dead process can
                    // keep failing acquire forever. Only after every ordinary retry has
                    // failed, so a lock a live process is holding stays put.
                    if attempt == 2 {
                        let _ = index
                            .directory()
                            .delete(std::path::Path::new(".tantivy-meta.lock"));
                    }
                    std::thread::sleep(std::time::Duration::from_millis(wait_ms));
                    last_err = Some(e);
                } else {
                    return Err(e.into());
                }
            }
        }
    }
    Err(last_err
        .map(|e| e.into())
        .unwrap_or_else(|| crate::error::YgrepError::Search("Failed to open reader".into())))
}
