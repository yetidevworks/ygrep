//! Query telemetry: one JSON line per search, appended to the data directory.
//!
//! The writer is best-effort and never reports failure to the search path — a search
//! that found what you asked for must not fail because a log line couldn't be written.
//! The reader is built for polling: it hands back new events plus the offset to resume
//! from, and starts over when the file shrinks under it.

use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::Config;

/// Rotate the log once it passes this size. One old generation is kept.
const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;

/// Queries longer than this are stored truncated; nothing reads past it.
const MAX_QUERY_CHARS: usize = 200;

/// How a query was executed
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryMode {
    /// Plain term search
    Literal,
    /// Regex search
    Regex,
    /// BM25 + vector search fused together
    Hybrid,
}

impl QueryMode {
    pub fn as_str(self) -> &'static str {
        match self {
            QueryMode::Literal => "literal",
            QueryMode::Regex => "regex",
            QueryMode::Hybrid => "hybrid",
        }
    }
}

impl std::fmt::Display for QueryMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One recorded query
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryEvent {
    /// Unix timestamp in seconds
    pub ts: i64,
    /// Index hash of the searched workspace
    pub ws: String,
    /// The query text, truncated
    pub q: String,
    /// Query time in milliseconds
    pub ms: u64,
    /// Number of hits returned
    pub hits: usize,
    /// Query mode: literal, regex, or hybrid
    pub mode: String,
}

/// New events plus the offset to resume from
#[derive(Debug, Clone, Default)]
pub struct QueryTail {
    pub events: Vec<QueryEvent>,
    pub offset: u64,
}

/// Path of the active telemetry log
pub fn queries_path(data_dir: &Path) -> PathBuf {
    data_dir.join("telemetry").join("queries.jsonl")
}

/// Path of the rotated telemetry log
fn rotated_path(data_dir: &Path) -> PathBuf {
    data_dir.join("telemetry").join("queries.jsonl.1")
}

/// Record one query, if telemetry is enabled.
///
/// Failures are swallowed: telemetry is a convenience for the dashboard, not something
/// a search result depends on.
pub fn record_query(
    config: &Config,
    data_dir: &Path,
    ws_hash: &str,
    query: &str,
    ms: u64,
    hits: usize,
    mode: QueryMode,
) {
    if !config.output.telemetry {
        return;
    }

    let event = QueryEvent {
        ts: chrono::Utc::now().timestamp(),
        ws: ws_hash.to_string(),
        q: truncate(query, MAX_QUERY_CHARS),
        ms,
        hits,
        mode: mode.as_str().to_string(),
    };

    let _ = append(data_dir, &event);
}

fn truncate(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((byte, _)) => text[..byte].to_string(),
        None => text.to_string(),
    }
}

fn append(data_dir: &Path, event: &QueryEvent) -> std::io::Result<()> {
    let path = queries_path(data_dir);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    rotate_if_full(data_dir, &path);

    let mut line = serde_json::to_string(event)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    line.push('\n');

    // One O_APPEND write, so concurrent ygrep processes interleave whole lines.
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?
        .write_all(line.as_bytes())
}

fn rotate_if_full(data_dir: &Path, path: &Path) {
    let too_big = fs::metadata(path)
        .map(|m| m.len() > MAX_LOG_BYTES)
        .unwrap_or(false);
    if too_big {
        let _ = fs::rename(path, rotated_path(data_dir));
    }
}

/// Read every recorded query at or after `since_unix_ts`, oldest first.
///
/// The rotated generation is read too, so a rotation mid-window doesn't lose history.
pub fn read_recent(data_dir: &Path, since_unix_ts: i64) -> Vec<QueryEvent> {
    let mut events = Vec::new();

    for path in [rotated_path(data_dir), queries_path(data_dir)] {
        let Ok(body) = fs::read_to_string(&path) else {
            continue;
        };
        events.extend(
            body.lines()
                .filter_map(|line| serde_json::from_str::<QueryEvent>(line).ok())
                .filter(|event| event.ts >= since_unix_ts),
        );
    }

    events
}

/// Read events appended since `offset` and return the offset to resume from.
///
/// A file shorter than `offset` means it was rotated or truncated, so reading restarts
/// from the beginning rather than handing back garbage. A trailing partial line is left
/// unconsumed and picked up on the next poll.
pub fn tail_from(data_dir: &Path, offset: u64) -> QueryTail {
    let path = queries_path(data_dir);

    let Ok(mut file) = fs::File::open(&path) else {
        return QueryTail::default();
    };

    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let start = if len < offset { 0 } else { offset };

    if file.seek(SeekFrom::Start(start)).is_err() {
        return QueryTail {
            events: Vec::new(),
            offset: start,
        };
    }

    let mut buf = Vec::new();
    if file.read_to_end(&mut buf).is_err() {
        return QueryTail {
            events: Vec::new(),
            offset: start,
        };
    }

    let Some(last_newline) = buf.iter().rposition(|b| *b == b'\n') else {
        return QueryTail {
            events: Vec::new(),
            offset: start,
        };
    };

    let complete = String::from_utf8_lossy(&buf[..=last_newline]);
    let events = complete
        .lines()
        .filter_map(|line| serde_json::from_str::<QueryEvent>(line).ok())
        .collect();

    QueryTail {
        events,
        offset: start + last_newline as u64 + 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn config(telemetry: bool) -> Config {
        let mut config = Config::default();
        config.output.telemetry = telemetry;
        config
    }

    #[test]
    fn records_one_line_per_query() {
        let dir = TempDir::new().unwrap();
        record_query(
            &config(true),
            dir.path(),
            "abc123",
            "fn main",
            12,
            3,
            QueryMode::Literal,
        );
        record_query(
            &config(true),
            dir.path(),
            "abc123",
            "->get(",
            4,
            0,
            QueryMode::Regex,
        );

        let events = read_recent(dir.path(), 0);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].q, "fn main");
        assert_eq!(events[0].ms, 12);
        assert_eq!(events[0].hits, 3);
        assert_eq!(events[0].mode, "literal");
        assert_eq!(events[1].mode, "regex");
    }

    #[test]
    fn the_config_gate_stops_the_writer() {
        let dir = TempDir::new().unwrap();
        record_query(
            &config(false),
            dir.path(),
            "abc123",
            "fn main",
            12,
            3,
            QueryMode::Literal,
        );

        assert!(!queries_path(dir.path()).exists());
        assert!(read_recent(dir.path(), 0).is_empty());
    }

    #[test]
    fn long_queries_are_truncated() {
        let dir = TempDir::new().unwrap();
        let long = "x".repeat(500);
        record_query(
            &config(true),
            dir.path(),
            "abc123",
            &long,
            1,
            0,
            QueryMode::Literal,
        );

        let events = read_recent(dir.path(), 0);
        assert_eq!(events[0].q.chars().count(), MAX_QUERY_CHARS);
    }

    #[test]
    fn truncation_never_splits_a_character() {
        let multibyte = "é".repeat(500);
        assert_eq!(
            truncate(&multibyte, MAX_QUERY_CHARS).chars().count(),
            MAX_QUERY_CHARS
        );
        assert_eq!(truncate("short", MAX_QUERY_CHARS), "short");
    }

    #[test]
    fn read_recent_filters_by_timestamp() {
        let dir = TempDir::new().unwrap();
        record_query(
            &config(true),
            dir.path(),
            "abc123",
            "old",
            1,
            0,
            QueryMode::Literal,
        );

        let future = chrono::Utc::now().timestamp() + 60;
        assert!(read_recent(dir.path(), future).is_empty());
        assert_eq!(read_recent(dir.path(), 0).len(), 1);
    }

    #[test]
    fn rotation_moves_the_log_aside_and_keeps_one_generation() {
        let dir = TempDir::new().unwrap();
        let path = queries_path(dir.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();

        // A single oversized line, so the next append has to rotate.
        let mut filler = String::from("{\"junk\":\"");
        filler.push_str(&"z".repeat(MAX_LOG_BYTES as usize + 1));
        filler.push_str("\"}\n");
        fs::write(&path, &filler).unwrap();

        record_query(
            &config(true),
            dir.path(),
            "abc123",
            "after",
            1,
            1,
            QueryMode::Literal,
        );

        assert!(rotated_path(dir.path()).exists(), "old generation kept");
        assert!(
            fs::metadata(&path).unwrap().len() < MAX_LOG_BYTES,
            "the active log restarted"
        );

        let events = read_recent(dir.path(), 0);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].q, "after");

        // A second rotation replaces the previous .1 rather than piling up generations.
        fs::write(&path, &filler).unwrap();
        record_query(
            &config(true),
            dir.path(),
            "abc123",
            "later",
            1,
            1,
            QueryMode::Literal,
        );
        assert!(!dir.path().join("telemetry/queries.jsonl.2").exists());
    }

    #[test]
    fn tail_returns_only_new_events() {
        let dir = TempDir::new().unwrap();
        record_query(
            &config(true),
            dir.path(),
            "ws",
            "first",
            1,
            1,
            QueryMode::Literal,
        );

        let first = tail_from(dir.path(), 0);
        assert_eq!(first.events.len(), 1);
        assert!(first.offset > 0);

        let idle = tail_from(dir.path(), first.offset);
        assert!(idle.events.is_empty());
        assert_eq!(idle.offset, first.offset);

        record_query(
            &config(true),
            dir.path(),
            "ws",
            "second",
            2,
            2,
            QueryMode::Hybrid,
        );
        let next = tail_from(dir.path(), first.offset);
        assert_eq!(next.events.len(), 1);
        assert_eq!(next.events[0].q, "second");
        assert!(next.offset > first.offset);
    }

    #[test]
    fn tail_restarts_when_the_file_shrinks() {
        let dir = TempDir::new().unwrap();
        for i in 0..5 {
            record_query(
                &config(true),
                dir.path(),
                "ws",
                &format!("q{i}"),
                1,
                1,
                QueryMode::Literal,
            );
        }
        let seen = tail_from(dir.path(), 0);
        assert_eq!(seen.events.len(), 5);

        // Rotation replaces the active log with a short one; the stale offset is past
        // its end, so the reader has to start over instead of reading nothing forever.
        fs::rename(queries_path(dir.path()), rotated_path(dir.path())).unwrap();
        record_query(
            &config(true),
            dir.path(),
            "ws",
            "fresh",
            1,
            1,
            QueryMode::Literal,
        );

        let after = tail_from(dir.path(), seen.offset);
        assert_eq!(after.events.len(), 1);
        assert_eq!(after.events[0].q, "fresh");
    }

    #[test]
    fn tail_leaves_a_partial_line_for_the_next_poll() {
        let dir = TempDir::new().unwrap();
        record_query(
            &config(true),
            dir.path(),
            "ws",
            "complete",
            1,
            1,
            QueryMode::Literal,
        );

        let path = queries_path(dir.path());
        let mut body = fs::read_to_string(&path).unwrap();
        body.push_str("{\"ts\":1,\"ws\":\"ws\",\"q\":\"part");
        fs::write(&path, &body).unwrap();

        let tail = tail_from(dir.path(), 0);
        assert_eq!(tail.events.len(), 1);
        assert_eq!(tail.events[0].q, "complete");

        // Finishing the line makes it readable from the offset the reader stopped at.
        let mut body = fs::read_to_string(&path).unwrap();
        body.push_str("ial\",\"ms\":1,\"hits\":0,\"mode\":\"literal\"}\n");
        fs::write(&path, &body).unwrap();

        let tail = tail_from(dir.path(), tail.offset);
        assert_eq!(tail.events.len(), 1);
        assert_eq!(tail.events[0].q, "partial");
    }

    #[test]
    fn tail_of_a_missing_log_is_empty() {
        let dir = TempDir::new().unwrap();
        let tail = tail_from(dir.path(), 0);
        assert!(tail.events.is_empty());
        assert_eq!(tail.offset, 0);
    }
}
