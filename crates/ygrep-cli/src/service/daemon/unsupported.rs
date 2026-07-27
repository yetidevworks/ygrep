//! Fallback backend for platforms with no service manager ygrep knows how to drive.
//! `ygrep service run` still works there; only install/start/stop are unavailable.

use anyhow::{bail, Result};
use std::path::PathBuf;

use super::{ServiceSpec, Status};

pub fn label() -> &'static str {
    "ygrep"
}

pub fn unit_path() -> Result<PathBuf> {
    unsupported()
}

pub fn install(_spec: &ServiceSpec) -> Result<()> {
    unsupported()
}

pub fn uninstall() -> Result<()> {
    unsupported()
}

pub fn start() -> Result<()> {
    unsupported()
}

pub fn stop() -> Result<()> {
    unsupported()
}

pub fn restart() -> Result<()> {
    unsupported()
}

pub fn status() -> Status {
    Status::NotInstalled
}

pub fn pid() -> Option<u32> {
    None
}

fn unsupported<T>() -> Result<T> {
    bail!(
        "Installing the ygrep service is only supported on macOS (launchd) and Linux (systemd). \
         Run `ygrep service run` yourself to watch in the foreground."
    )
}
