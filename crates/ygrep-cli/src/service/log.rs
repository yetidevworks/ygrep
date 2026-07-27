//! The service log: one timestamped line per event, size-capped with one old generation.
//!
//! launchd and systemd point the service's stdout and stderr at the same file, so this
//! writer appends rather than truncating, and rotation renames instead of rewriting.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

/// Log file for the background service.
pub fn log_path(data_dir: &Path) -> PathBuf {
    data_dir.join("logs").join("service.log")
}

/// Append-only writer for the service log.
pub struct ServiceLog {
    path: PathBuf,
    rotated: PathBuf,
    max_bytes: u64,
    file: File,
    size: u64,
}

impl ServiceLog {
    /// Open (or create) the log, rotating once it passes `max_size_mb`.
    pub fn open(path: PathBuf, max_size_mb: u64) -> std::io::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let rotated = path.with_extension("log.1");
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let size = file.metadata().map(|m| m.len()).unwrap_or(0);

        Ok(Self {
            path,
            rotated,
            // A zero cap would rotate on every line, so treat it as "no rotation".
            max_bytes: max_size_mb.saturating_mul(1024 * 1024),
            file,
            size,
        })
    }

    /// Append one timestamped line. Logging failures are swallowed — the service keeps
    /// watching even when its log cannot be written.
    pub fn write(&mut self, message: &str) {
        let line = format!(
            "{} {}\n",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
            message.trim()
        );

        if self.max_bytes > 0 && self.size + line.len() as u64 > self.max_bytes {
            self.rotate();
        }

        if self.file.write_all(line.as_bytes()).is_ok() {
            self.size += line.len() as u64;
            let _ = self.file.flush();
        }
    }

    /// Move the active log aside and start a fresh one. One generation is kept.
    fn rotate(&mut self) {
        if fs::rename(&self.path, &self.rotated).is_err() {
            return;
        }
        if let Ok(file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            self.file = file;
            self.size = 0;
        }
    }
}

/// The last `lines` lines of the log, oldest first. Missing logs read as empty.
pub fn tail(path: &Path, lines: usize) -> Vec<String> {
    let Ok(file) = File::open(path) else {
        return Vec::new();
    };

    let mut kept: std::collections::VecDeque<String> = std::collections::VecDeque::new();
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        if kept.len() == lines {
            kept.pop_front();
        }
        kept.push_back(line);
    }

    kept.into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn every_line_is_timestamped() {
        let dir = TempDir::new().unwrap();
        let path = log_path(dir.path());

        let mut log = ServiceLog::open(path.clone(), 5).unwrap();
        log.write("service starting");

        let body = fs::read_to_string(&path).unwrap();
        assert!(body.ends_with("service starting\n"));
        assert!(
            body.starts_with(&chrono::Local::now().format("%Y-%m-%d").to_string()),
            "unexpected line: {body}"
        );
    }

    #[test]
    fn reopening_appends_instead_of_truncating() {
        let dir = TempDir::new().unwrap();
        let path = log_path(dir.path());

        ServiceLog::open(path.clone(), 5).unwrap().write("first");
        ServiceLog::open(path.clone(), 5).unwrap().write("second");

        let body = fs::read_to_string(&path).unwrap();
        assert!(body.contains("first"));
        assert!(body.contains("second"));
    }

    #[test]
    fn the_log_rotates_once_it_passes_the_cap() {
        let dir = TempDir::new().unwrap();
        let path = log_path(dir.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();

        // One megabyte of history, with a one-megabyte cap: the next line rotates.
        fs::write(&path, "x".repeat(1024 * 1024 + 1)).unwrap();

        let mut log = ServiceLog::open(path.clone(), 1).unwrap();
        log.write("after rotation");

        let rotated = dir.path().join("logs/service.log.1");
        assert!(rotated.exists(), "the old generation is kept");
        assert!(fs::read_to_string(&rotated).unwrap().starts_with('x'));

        let body = fs::read_to_string(&path).unwrap();
        assert!(body.contains("after rotation"));
        assert!(body.len() < 1024, "the active log restarted");

        // A second rotation replaces .1 rather than piling up generations.
        fs::write(&path, "y".repeat(1024 * 1024 + 1)).unwrap();
        let mut log = ServiceLog::open(path.clone(), 1).unwrap();
        log.write("again");
        assert!(!dir.path().join("logs/service.log.2").exists());
        assert!(fs::read_to_string(&rotated).unwrap().starts_with('y'));
    }

    #[test]
    fn a_zero_cap_disables_rotation() {
        let dir = TempDir::new().unwrap();
        let path = log_path(dir.path());

        let mut log = ServiceLog::open(path.clone(), 0).unwrap();
        log.write("one");
        log.write("two");

        assert!(!dir.path().join("logs/service.log.1").exists());
        assert_eq!(fs::read_to_string(&path).unwrap().lines().count(), 2);
    }

    #[test]
    fn tail_returns_the_last_lines_oldest_first() {
        let dir = TempDir::new().unwrap();
        let path = log_path(dir.path());

        let mut log = ServiceLog::open(path.clone(), 5).unwrap();
        for i in 0..10 {
            log.write(&format!("line {i}"));
        }

        let last = tail(&path, 3);
        assert_eq!(last.len(), 3);
        assert!(last[0].ends_with("line 7"));
        assert!(last[2].ends_with("line 9"));

        assert_eq!(tail(&path, 100).len(), 10);
        assert!(tail(&dir.path().join("missing.log"), 5).is_empty());
    }
}
