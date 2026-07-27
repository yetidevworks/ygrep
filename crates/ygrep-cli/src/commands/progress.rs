//! Tracking for in-flight and stale indexes.
//!
//! An index build can take minutes on a large tree, and the Claude Code hook starts one
//! in the background at session start. Without a marker, a search during that window
//! reports "Workspace not indexed", which reads as "ygrep doesn't work here" and sends
//! the caller off to grep. The marker lets search say what is actually happening.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Marker file written into the index directory while a build is running
const MARKER: &str = "indexing.json";

/// A build older than this is assumed dead, so a crashed run can't wedge the marker
const MAX_BUILD_AGE_MINUTES: i64 = 60;

/// How old an index gets before search mentions it
const STALE_AFTER_HOURS: i64 = 24;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexingProgress {
    pub pid: u32,
    pub started_at: DateTime<Utc>,
}

impl IndexingProgress {
    /// How long this build has been running
    pub fn elapsed(&self) -> Duration {
        Utc::now().signed_duration_since(self.started_at)
    }
}

/// Removes the in-progress marker when indexing finishes, including on error.
pub struct IndexingGuard {
    marker: PathBuf,
}

impl IndexingGuard {
    /// Mark an index build as started. Failing to write the marker is not fatal:
    /// it only costs nicer reporting, so indexing proceeds either way.
    pub fn start(index_path: &Path) -> Self {
        let marker = index_path.join(MARKER);

        if let Err(e) = std::fs::create_dir_all(index_path).and_then(|()| {
            let progress = IndexingProgress {
                pid: std::process::id(),
                started_at: Utc::now(),
            };
            let json = serde_json::to_string(&progress).unwrap_or_default();
            std::fs::write(&marker, json)
        }) {
            tracing::debug!("Could not write indexing marker: {e}");
        }

        Self { marker }
    }
}

impl Drop for IndexingGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.marker);
    }
}

/// Read the in-progress marker for an index, if a build is currently running.
///
/// Returns `None` when no build is running, or when the marker is old enough that the
/// process behind it must have died without cleaning up.
pub fn indexing_in_progress(index_path: &Path) -> Option<IndexingProgress> {
    let json = std::fs::read_to_string(index_path.join(MARKER)).ok()?;
    let progress: IndexingProgress = serde_json::from_str(&json).ok()?;

    if progress.elapsed() > Duration::minutes(MAX_BUILD_AGE_MINUTES) {
        return None;
    }

    Some(progress)
}

/// When the index was last written, from the workspace metadata.
pub fn indexed_at(index_path: &Path) -> Option<DateTime<Utc>> {
    let json = std::fs::read_to_string(index_path.join("workspace.json")).ok()?;
    let value: serde_json::Value = serde_json::from_str(&json).ok()?;

    value
        .get("indexed_at")
        .and_then(|v| v.as_str())
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc))
}

/// A human-readable note when the index is old enough to be worth mentioning.
///
/// This is a timestamp comparison only. Detecting actual drift would mean walking the
/// tree on every search, which would cost more than the search itself.
pub fn staleness_note(index_path: &Path) -> Option<String> {
    let indexed_at = indexed_at(index_path)?;
    let age = Utc::now().signed_duration_since(indexed_at);

    if age < Duration::hours(STALE_AFTER_HOURS) {
        return None;
    }

    let age_text = if age.num_days() >= 1 {
        format!("{}d", age.num_days())
    } else {
        format!("{}h", age.num_hours())
    };

    Some(format!(
        "note: index is {} old, run `ygrep index` to refresh",
        age_text
    ))
}

/// Whether the index directory can be written to, i.e. whether we may build an index here.
///
/// A readable but non-writable index directory is a supported setup (issue #12), so
/// auto-indexing has to check rather than assume.
pub fn index_dir_writable(index_path: &Path) -> bool {
    // Walk up to the first directory that exists; that is what we'd be creating into.
    let mut candidate = index_path;
    loop {
        if candidate.exists() {
            break;
        }
        match candidate.parent() {
            Some(parent) => candidate = parent,
            None => return false,
        }
    }

    let probe = candidate.join(".ygrep-write-probe");
    match std::fs::write(&probe, b"") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// Format a duration for progress reporting, e.g. "12s" or "3m 04s"
pub fn format_duration(d: Duration) -> String {
    let secs = d.num_seconds().max(0);
    if secs < 60 {
        format!("{}s", secs)
    } else {
        format!("{}m {:02}s", secs / 60, secs % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn guard_writes_and_removes_the_marker() {
        let temp = TempDir::new().unwrap();
        let index = temp.path().join("idx");

        {
            let _guard = IndexingGuard::start(&index);
            let progress = indexing_in_progress(&index).expect("marker should exist");
            assert_eq!(progress.pid, std::process::id());
        }

        assert!(
            indexing_in_progress(&index).is_none(),
            "marker must be cleared on drop"
        );
    }

    #[test]
    fn guard_removes_the_marker_when_indexing_fails() {
        let temp = TempDir::new().unwrap();
        let index = temp.path().join("idx");

        let result: anyhow::Result<()> = (|| {
            let _guard = IndexingGuard::start(&index);
            anyhow::bail!("indexing blew up")
        })();

        assert!(result.is_err());
        assert!(indexing_in_progress(&index).is_none());
    }

    #[test]
    fn an_abandoned_marker_is_ignored() {
        let temp = TempDir::new().unwrap();
        let index = temp.path().join("idx");
        std::fs::create_dir_all(&index).unwrap();

        let stale = IndexingProgress {
            pid: 1,
            started_at: Utc::now() - Duration::minutes(MAX_BUILD_AGE_MINUTES + 1),
        };
        std::fs::write(index.join(MARKER), serde_json::to_string(&stale).unwrap()).unwrap();

        assert!(indexing_in_progress(&index).is_none());
    }

    #[test]
    fn staleness_note_respects_the_threshold() {
        let temp = TempDir::new().unwrap();
        let index = temp.path().join("idx");
        std::fs::create_dir_all(&index).unwrap();

        let write_indexed_at = |when: DateTime<Utc>| {
            std::fs::write(
                index.join("workspace.json"),
                serde_json::json!({ "indexed_at": when.to_rfc3339() }).to_string(),
            )
            .unwrap();
        };

        write_indexed_at(Utc::now() - Duration::hours(1));
        assert!(staleness_note(&index).is_none(), "fresh index says nothing");

        write_indexed_at(Utc::now() - Duration::days(3));
        let note = staleness_note(&index).expect("stale index should report");
        assert!(note.contains("3d"), "unexpected note: {note}");
    }

    #[test]
    fn writability_is_detected_for_a_missing_index_dir() {
        let temp = TempDir::new().unwrap();
        // Nothing exists yet; the nearest existing ancestor is writable.
        assert!(index_dir_writable(&temp.path().join("a/b/c")));
    }

    #[test]
    fn format_duration_reads_naturally() {
        assert_eq!(format_duration(Duration::seconds(9)), "9s");
        assert_eq!(format_duration(Duration::seconds(184)), "3m 04s");
    }
}
