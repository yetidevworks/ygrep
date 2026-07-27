//! The background watch service: platform install/start/stop plus the headless loop.
//!
//! Everything here is an ordinary function call, so the `service` subcommand and the TUI
//! drive the same code — there is no daemon protocol to keep in sync.

pub mod daemon;
pub mod lock;
pub mod log;
pub mod run;
pub mod state;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;
use ygrep_core::registry;
use ygrep_core::Config;

pub use daemon::ServiceSpec;
pub use state::ServiceState;

/// What the platform's service manager knows about the service.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ServiceStatus {
    /// No plist or unit file on disk
    NotInstalled,
    /// Installed, with its current run state
    Installed {
        running: bool,
        pid: Option<u32>,
        /// The last run exited with a failure
        failed: bool,
    },
}

impl ServiceStatus {
    pub fn installed(&self) -> bool {
        matches!(self, ServiceStatus::Installed { .. })
    }

    pub fn running(&self) -> bool {
        matches!(self, ServiceStatus::Installed { running: true, .. })
    }

    pub fn pid(&self) -> Option<u32> {
        match self {
            ServiceStatus::Installed { pid, .. } => *pid,
            ServiceStatus::NotInstalled => None,
        }
    }

    /// One word for display: "running", "stopped", "error" or "not installed".
    pub fn as_str(&self) -> &'static str {
        match self {
            ServiceStatus::NotInstalled => "not installed",
            ServiceStatus::Installed { running: true, .. } => "running",
            ServiceStatus::Installed { failed: true, .. } => "error",
            ServiceStatus::Installed { .. } => "stopped",
        }
    }
}

/// Everything `ygrep service status` and the TUI's service panel need.
#[derive(Debug, Clone, Serialize)]
pub struct ServiceReport {
    pub status: ServiceStatus,
    /// Label of the LaunchAgent or systemd unit
    pub label: String,
    /// Where the plist or unit file lives, when the platform has one
    pub unit_path: Option<PathBuf>,
    pub log_path: PathBuf,
    pub data_dir: PathBuf,
    /// Indexes in the registry
    pub indexes: usize,
    /// Indexes with the persisted watch flag on
    pub watch_enabled: usize,
    /// The running service's own heartbeat, when there is one
    pub heartbeat: Option<ServiceState>,
}

/// What an install actually did.
#[derive(Debug, Clone, Serialize)]
pub struct InstallReport {
    pub unit_path: PathBuf,
    pub log_path: PathBuf,
    pub program: PathBuf,
    /// The service was already installed and its definition was rewritten
    pub refreshed: bool,
}

/// Data directory the service uses for indexes, logs, lock and heartbeat.
pub fn data_dir(config: &Config) -> Result<PathBuf> {
    Ok(registry::data_dir(config)?)
}

/// Service log path inside a known data directory.
pub fn log_path_in(data_dir: &Path) -> PathBuf {
    log::log_path(data_dir)
}

/// Service log path for the current configuration.
pub fn log_path() -> Result<PathBuf> {
    Ok(log_path_in(&data_dir(&Config::load())?))
}

/// Install (or refresh) the service definition and start it.
///
/// Re-running this after the binary moves — a `cargo install`, a Homebrew upgrade —
/// rewrites the definition with the new path and restarts.
pub fn install() -> Result<InstallReport> {
    let program = std::env::current_exe().context("Could not determine the ygrep binary path")?;
    let program = std::fs::canonicalize(&program).unwrap_or(program);
    let data_dir = data_dir(&Config::load())?;
    let log_path = log_path_in(&data_dir);

    let unit_path = daemon::unit_path()?;
    let refreshed = unit_path.exists();

    let spec = ServiceSpec {
        program: program.clone(),
        args: vec!["service".to_string(), "run".to_string()],
        log: log_path.clone(),
        keep_alive: true,
        run_at_load: true,
    };

    daemon::install(&spec)?;
    daemon::restart()?;

    Ok(InstallReport {
        unit_path,
        log_path,
        program,
        refreshed,
    })
}

/// Stop the service and remove its definition.
pub fn uninstall() -> Result<()> {
    daemon::uninstall()
}

/// Start the installed service.
pub fn start() -> Result<()> {
    daemon::start()
}

/// Stop the running service.
pub fn stop() -> Result<()> {
    daemon::stop()
}

/// Restart the service, re-reading its definition.
pub fn restart() -> Result<()> {
    daemon::restart()
}

/// Ask the platform whether the service is installed and running.
pub fn status() -> ServiceStatus {
    match daemon::status() {
        daemon::Status::NotInstalled => ServiceStatus::NotInstalled,
        daemon::Status::Running => ServiceStatus::Installed {
            running: true,
            pid: daemon::pid(),
            failed: false,
        },
        daemon::Status::Stopped => ServiceStatus::Installed {
            running: false,
            pid: None,
            failed: false,
        },
        daemon::Status::Error => ServiceStatus::Installed {
            running: false,
            pid: None,
            failed: true,
        },
    }
}

/// Full status plus what the registry says there is to watch.
pub fn report() -> Result<ServiceReport> {
    let config = Config::load();
    let data_dir = data_dir(&config)?;
    let indexes = registry::collect_indexes_in(&data_dir.join("indexes")).unwrap_or_default();

    Ok(ServiceReport {
        status: status(),
        label: daemon::label().to_string(),
        unit_path: daemon::unit_path().ok(),
        log_path: log_path_in(&data_dir),
        indexes: indexes.len(),
        watch_enabled: indexes.iter().filter(|info| info.watch).count(),
        heartbeat: state::read(&data_dir),
        data_dir,
    })
}

/// True when a service process is up right now — either the platform says so, or a
/// foreground `ygrep service run` is holding the lock.
pub fn is_running() -> bool {
    if status().running() {
        return true;
    }

    let Ok(data_dir) = data_dir(&Config::load()) else {
        return false;
    };
    state::read(&data_dir).is_some_and(|state| pid_alive(state.pid))
}

/// Is a process with this pid still around?
fn pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // Signal 0 performs the permission and existence checks without delivering.
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_reports_what_it_is_asked_about() {
        let running = ServiceStatus::Installed {
            running: true,
            pid: Some(42),
            failed: false,
        };
        assert!(running.installed());
        assert!(running.running());
        assert_eq!(running.pid(), Some(42));
        assert_eq!(running.as_str(), "running");

        let stopped = ServiceStatus::Installed {
            running: false,
            pid: None,
            failed: false,
        };
        assert!(stopped.installed());
        assert!(!stopped.running());
        assert_eq!(stopped.as_str(), "stopped");

        let failed = ServiceStatus::Installed {
            running: false,
            pid: None,
            failed: true,
        };
        assert_eq!(failed.as_str(), "error");

        assert!(!ServiceStatus::NotInstalled.installed());
        assert!(!ServiceStatus::NotInstalled.running());
        assert_eq!(ServiceStatus::NotInstalled.pid(), None);
    }

    #[test]
    fn this_process_counts_as_alive() {
        assert!(pid_alive(std::process::id()));
    }
}
