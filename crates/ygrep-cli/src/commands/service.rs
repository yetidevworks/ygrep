//! `ygrep service` — install, control and inspect the background watch service.

use anyhow::Result;
use std::io::Write;
use std::path::Path;

use ygrep_core::registry::{format_relative_time, shorten_path};

use crate::service;
use crate::OutputFormat;

/// Install the service definition and start it.
pub fn install() -> Result<()> {
    let report = service::install()?;

    println!(
        "{} the ygrep service",
        if report.refreshed {
            "Refreshed"
        } else {
            "Installed"
        }
    );
    println!("  binary:  {}", report.program.display());
    println!("  service: {}", report.unit_path.display());
    println!("  log:     {}", report.log_path.display());
    println!("\nIt starts at login and watches every index with the watch flag on.");
    println!("Turn one on with `ygrep indexes watch <hash|path> on`.");

    Ok(())
}

/// Stop the service and remove its definition.
pub fn uninstall() -> Result<()> {
    let unit_path = service::daemon::unit_path().ok();
    service::uninstall()?;

    println!("Removed the ygrep service");
    if let Some(path) = unit_path {
        println!("  service: {}", path.display());
    }
    println!("Indexes and their watch flags are untouched.");

    Ok(())
}

/// Start the installed service.
pub fn start() -> Result<()> {
    if !service::status().installed() {
        return not_installed();
    }
    service::start()?;
    println!("Started the ygrep service");
    Ok(())
}

/// Stop the running service.
pub fn stop() -> Result<()> {
    if !service::status().installed() {
        return not_installed();
    }
    service::stop()?;
    println!("Stopped the ygrep service");
    Ok(())
}

/// Restart the service, re-reading its definition.
pub fn restart() -> Result<()> {
    if !service::status().installed() {
        return not_installed();
    }
    service::restart()?;
    println!("Restarted the ygrep service");
    Ok(())
}

fn not_installed() -> Result<()> {
    println!("The ygrep service is not installed.");
    println!("Install it with `ygrep service install`.");
    Ok(())
}

/// Report whether the service is installed, running, and what it is watching.
pub fn status(format: OutputFormat) -> Result<()> {
    let report = service::report()?;

    if format == OutputFormat::Json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    println!("Service: {}", report.status.as_str());
    if let Some(pid) = report.status.pid() {
        println!("  pid:     {}", pid);
    }
    if let Some(path) = &report.unit_path {
        println!("  service: {}", shorten_path(&path.display().to_string()));
    }

    if let Some(state) = &report.heartbeat {
        println!(
            "  started: {} ({})",
            state
                .started_at
                .with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M"),
            format_relative_time(&state.started_at)
        );
        println!(
            "  watching {} of {} registered indexes, last rescan {} (every {}s)",
            state.watched.len(),
            state.registered,
            format_relative_time(&state.last_rescan),
            state.rescan_secs
        );
    }

    println!(
        "  {} of {} indexes have the watch flag on",
        report.watch_enabled, report.indexes
    );
    println!(
        "  log:     {}",
        shorten_path(&report.log_path.display().to_string())
    );
    println!(
        "  data:    {}",
        shorten_path(&report.data_dir.display().to_string())
    );

    if !report.status.installed() {
        println!("\nInstall it with `ygrep service install` to watch indexes on login.");
    } else if !report.status.running() {
        println!("\nStart it with `ygrep service start`.");
    } else if report.watch_enabled == 0 {
        println!("\nNothing is watch-enabled yet: `ygrep indexes watch <hash|path> on`.");
    }

    Ok(())
}

/// Run the watch loop in the foreground.
pub fn run() -> Result<()> {
    service::run::run()
}

/// Print the tail of the service log, optionally following it.
pub fn log(lines: usize, follow: bool) -> Result<()> {
    let path = service::log_path()?;

    if !path.exists() {
        println!("No service log yet: {}", path.display());
        println!("Start the service with `ygrep service start`.");
        return Ok(());
    }

    for line in service::log::tail(&path, lines) {
        println!("{}", line);
    }

    if follow {
        follow_log(&path)?;
    }

    Ok(())
}

/// Print lines as they are appended, until interrupted.
fn follow_log(path: &Path) -> Result<()> {
    use std::io::{Read, Seek, SeekFrom};

    let mut offset = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let stdout = std::io::stdout();

    loop {
        std::thread::sleep(std::time::Duration::from_millis(500));

        let Ok(mut file) = std::fs::File::open(path) else {
            continue;
        };
        let len = file.metadata().map(|m| m.len()).unwrap_or(0);
        // A shorter file means the log rotated under us, so read it from the start.
        if len < offset {
            offset = 0;
        }
        if len == offset || file.seek(SeekFrom::Start(offset)).is_err() {
            continue;
        }

        let mut appended = Vec::new();
        let Ok(read) = file.read_to_end(&mut appended) else {
            continue;
        };

        let mut handle = stdout.lock();
        let _ = handle.write_all(&appended);
        let _ = handle.flush();
        offset += read as u64;
    }
}
