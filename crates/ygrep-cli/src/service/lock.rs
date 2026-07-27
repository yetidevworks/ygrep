//! Single-instance lock for the background service.
//!
//! Two services watching the same indexes would fight over tantivy's writer lock, so
//! only one process may hold `<data_dir>/service.lock` at a time. The lock is advisory
//! and tied to the open file, so it is released even if the process is killed.

use anyhow::{Context, Result};
use fs2::FileExt;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Lock file guarding a single running service.
pub fn lock_path(data_dir: &Path) -> PathBuf {
    data_dir.join("service.lock")
}

/// An exclusive hold on the service lock. Dropping it releases the lock.
#[derive(Debug)]
pub struct InstanceLock {
    file: File,
}

impl InstanceLock {
    /// Take the lock, failing when another service already holds it.
    ///
    /// The pid of the holder is written into the file so the failure can name it.
    pub fn acquire(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create {}", parent.display()))?;
        }

        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .truncate(false)
            .write(true)
            .open(path)
            .with_context(|| format!("Failed to open {}", path.display()))?;

        if file.try_lock_exclusive().is_err() {
            let holder = fs::read_to_string(path)
                .ok()
                .and_then(|body| body.trim().parse::<u32>().ok());
            return Err(match holder {
                Some(pid) => {
                    anyhow::anyhow!("Another ygrep service is already running (pid {pid})")
                }
                None => anyhow::anyhow!("Another ygrep service is already running"),
            });
        }

        let mut file = file;
        let _ = file.set_len(0);
        let _ = write!(file, "{}", std::process::id());
        let _ = file.flush();

        Ok(Self { file })
    }
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.file);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn the_second_attempt_is_refused_while_the_first_holds_it() {
        let dir = TempDir::new().unwrap();
        let path = lock_path(dir.path());

        let held = InstanceLock::acquire(&path).unwrap();

        let err = InstanceLock::acquire(&path).unwrap_err();
        assert!(
            err.to_string().contains("already running"),
            "unexpected error: {err}"
        );
        assert!(err.to_string().contains(&std::process::id().to_string()));

        drop(held);

        // Releasing it lets the next service start.
        InstanceLock::acquire(&path).unwrap();
    }

    #[test]
    fn the_lock_file_records_the_holders_pid() {
        let dir = TempDir::new().unwrap();
        let path = lock_path(dir.path());

        let _held = InstanceLock::acquire(&path).unwrap();

        let body = fs::read_to_string(&path).unwrap();
        assert_eq!(body.trim(), std::process::id().to_string());
    }
}
