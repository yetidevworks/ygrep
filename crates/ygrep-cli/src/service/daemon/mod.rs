//! Platform backends for the background watch service.
//!
//! macOS runs it as a user LaunchAgent, Linux as a systemd **user** unit. The public
//! API is identical on both, so every caller (the `service` subcommand, the TUI) stays
//! platform-agnostic — the backend is picked at compile time here.

use std::path::PathBuf;

// Both renderers are compiled under `cfg(test)` so the plist and the unit file can be
// checked on whichever machine runs the tests.
#[cfg(any(target_os = "macos", test))]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub mod launchd;
#[cfg(any(target_os = "linux", test))]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub mod systemd;

#[cfg(target_os = "macos")]
pub use launchd::{install, label, pid, restart, start, status, stop, uninstall, unit_path};

#[cfg(target_os = "linux")]
pub use systemd::{install, label, pid, restart, start, status, stop, uninstall, unit_path};

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod unsupported;
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub use unsupported::{install, label, pid, restart, start, status, stop, uninstall, unit_path};

/// Declarative description of the managed process, rendered into a launchd plist on
/// macOS or a systemd unit on Linux.
pub struct ServiceSpec {
    /// Absolute path to the ygrep binary to run.
    pub program: PathBuf,
    /// Arguments after the program itself.
    pub args: Vec<String>,
    /// Combined stdout/stderr log path.
    pub log: PathBuf,
    /// Restart the service if it exits unexpectedly.
    pub keep_alive: bool,
    /// Start the service as soon as it is loaded, and again on every login.
    pub run_at_load: bool,
}

/// What the platform's service manager reports about the service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Loaded and running.
    Running,
    /// Loaded but not currently running.
    Stopped,
    /// Loaded, not running, and it exited with a failure.
    Error,
    /// No plist or unit file is installed.
    NotInstalled,
}
