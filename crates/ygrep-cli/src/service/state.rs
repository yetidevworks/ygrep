//! The service heartbeat file, `<data_dir>/service.json`.
//!
//! Anything that wants to know what the running service is doing — `ygrep service
//! status`, the TUI — reads this instead of asking the service, which has no socket.
//! It is rewritten on every registry rescan and deleted on a clean shutdown.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Heartbeat written by `ygrep service run`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceState {
    /// Process id of the running service
    pub pid: u32,
    /// When this service process started
    pub started_at: DateTime<Utc>,
    /// When it last re-read the index registry
    pub last_rescan: DateTime<Utc>,
    /// Hashes of the indexes it is currently watching
    pub watched: Vec<String>,
    /// How many indexes it knows about at all
    pub registered: usize,
    /// Where it is logging
    pub log: PathBuf,
    /// How often it re-reads the registry, in seconds
    pub rescan_secs: u64,
}

/// Start time of this process, fixed on first use so every heartbeat agrees.
fn started_at() -> DateTime<Utc> {
    static STARTED_AT: OnceLock<DateTime<Utc>> = OnceLock::new();
    *STARTED_AT.get_or_init(Utc::now)
}

impl ServiceState {
    /// Snapshot the current state of a running service.
    pub fn new(data_dir: &Path, registered: &BTreeMap<String, bool>, rescan_secs: u64) -> Self {
        Self {
            pid: std::process::id(),
            started_at: started_at(),
            last_rescan: Utc::now(),
            watched: registered
                .iter()
                .filter(|(_, watching)| **watching)
                .map(|(hash, _)| hash.clone())
                .collect(),
            registered: registered.len(),
            log: super::log_path_in(data_dir),
            rescan_secs,
        }
    }
}

/// Path of the heartbeat file.
pub fn state_path(data_dir: &Path) -> PathBuf {
    data_dir.join("service.json")
}

/// Write the heartbeat. Best-effort: a service that cannot write it keeps watching.
pub fn write(data_dir: &Path, state: &ServiceState) {
    let Ok(body) = serde_json::to_string_pretty(state) else {
        return;
    };

    let path = state_path(data_dir);
    let temp = path.with_extension("json.tmp");
    if fs::write(&temp, body).is_ok() {
        let _ = fs::rename(&temp, &path);
    }
}

/// Read the heartbeat, if a service left one behind.
pub fn read(data_dir: &Path) -> Option<ServiceState> {
    let body = fs::read_to_string(state_path(data_dir)).ok()?;
    serde_json::from_str(&body).ok()
}

/// Remove the heartbeat on a clean shutdown, so nothing reads a dead service's state.
pub fn clear(data_dir: &Path) {
    let _ = fs::remove_file(state_path(data_dir));
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn registered() -> BTreeMap<String, bool> {
        BTreeMap::from([
            ("aaa".to_string(), true),
            ("bbb".to_string(), false),
            ("ccc".to_string(), true),
        ])
    }

    #[test]
    fn the_heartbeat_round_trips() {
        let dir = TempDir::new().unwrap();
        let state = ServiceState::new(dir.path(), &registered(), 30);

        write(dir.path(), &state);
        let read_back = read(dir.path()).expect("heartbeat is readable");

        assert_eq!(read_back.pid, std::process::id());
        assert_eq!(
            read_back.watched,
            vec!["aaa".to_string(), "ccc".to_string()]
        );
        assert_eq!(read_back.registered, 3);
        assert_eq!(read_back.rescan_secs, 30);
        assert_eq!(read_back.log, super::super::log_path_in(dir.path()));
    }

    #[test]
    fn clearing_leaves_nothing_to_read() {
        let dir = TempDir::new().unwrap();
        write(
            dir.path(),
            &ServiceState::new(dir.path(), &registered(), 30),
        );

        clear(dir.path());

        assert!(read(dir.path()).is_none());
        assert!(!state_path(dir.path()).exists());
    }

    #[test]
    fn a_missing_heartbeat_reads_as_nothing() {
        let dir = TempDir::new().unwrap();
        assert!(read(dir.path()).is_none());
    }
}
