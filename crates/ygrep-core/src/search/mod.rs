#[cfg(feature = "embeddings")]
mod hybrid;
mod results;
mod searcher;

#[cfg(feature = "embeddings")]
pub use hybrid::HybridSearcher;
pub use results::{MatchType, SearchHit, SearchResult};
pub use searcher::{SearchFilters, Searcher};

use tantivy::{Index, IndexReader};

/// Open an IndexReader with retry-and-backoff for META_LOCK contention (issue #7).
///
/// Tantivy's `index.reader()` acquires an exclusive flock on `.tantivy-meta.lock`.
/// On macOS this can transiently fail with EPERM when `ygrep watch` is committing
/// concurrently.  We retry up to 3 times with exponential backoff (100/200/400 ms).
pub(crate) fn open_reader_with_retry(index: &Index) -> crate::error::Result<IndexReader> {
    let mut last_err = None;
    for attempt in 0..4 {
        match index.reader() {
            Ok(reader) => return Ok(reader),
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("Lockfile") && attempt < 3 {
                    let wait_ms = 100 * (1u64 << attempt);
                    tracing::debug!(
                        "META_LOCK contention (attempt {}/3), retrying in {}ms…",
                        attempt + 1,
                        wait_ms
                    );
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
