//! launchd (macOS) backend. The service runs as a user LaunchAgent at
//! `~/Library/LaunchAgents/com.yetidevworks.ygrep.plist`, loaded into the per-user GUI
//! domain so it starts at login and keeps running for the session.

use anyhow::{Context, Result};
use std::fs;
use std::path::PathBuf;

#[cfg(target_os = "macos")]
use anyhow::bail;
#[cfg(target_os = "macos")]
use std::process::{Command, Stdio};

use super::ServiceSpec;
#[cfg(target_os = "macos")]
use super::Status;

/// Reverse-DNS label of the LaunchAgent.
const LABEL: &str = "com.yetidevworks.ygrep";

pub fn label() -> &'static str {
    LABEL
}

fn launch_agents_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().context("Could not determine home directory")?;
    Ok(home.join("Library/LaunchAgents"))
}

/// Path of the plist that defines the service.
pub fn unit_path() -> Result<PathBuf> {
    Ok(launch_agents_dir()?.join(format!("{LABEL}.plist")))
}

/// The per-user GUI launchd domain, e.g. `gui/501`.
#[cfg(target_os = "macos")]
fn domain() -> String {
    // getuid() has no failure mode and no memory safety concerns.
    format!("gui/{}", unsafe { libc::getuid() })
}

/// Full service target within the GUI domain, e.g. `gui/501/com.yetidevworks.ygrep`.
#[cfg(target_os = "macos")]
fn service_target() -> String {
    format!("{}/{}", domain(), LABEL)
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Render the LaunchAgent plist for a spec.
pub fn render_plist(spec: &ServiceSpec) -> String {
    let mut args_xml = format!(
        "        <string>{}</string>\n",
        xml_escape(&spec.program.display().to_string())
    );
    for arg in &spec.args {
        args_xml.push_str(&format!("        <string>{}</string>\n", xml_escape(arg)));
    }

    let keep_alive = if spec.keep_alive {
        "    <key>KeepAlive</key>\n    <dict>\n        <key>SuccessfulExit</key>\n        <false/>\n    </dict>\n"
    } else {
        ""
    };
    let run_at_load = if spec.run_at_load { "true" } else { "false" };
    let log = xml_escape(&spec.log.display().to_string());

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
{args_xml}    </array>
    <key>RunAtLoad</key>
    <{run_at_load}/>
{keep_alive}    <key>StandardOutPath</key>
    <string>{log}</string>
    <key>StandardErrorPath</key>
    <string>{log}</string>
    <key>ProcessType</key>
    <string>Background</string>
</dict>
</plist>
"#
    )
}

/// Write the plist. Does not load it.
pub fn install(spec: &ServiceSpec) -> Result<()> {
    let dir = launch_agents_dir()?;
    fs::create_dir_all(&dir).with_context(|| format!("Failed to create {}", dir.display()))?;
    if let Some(parent) = spec.log.parent() {
        fs::create_dir_all(parent).ok();
    }
    let path = unit_path()?;
    fs::write(&path, render_plist(spec))
        .with_context(|| format!("Failed to write plist {}", path.display()))?;
    Ok(())
}

/// Load and start the service with the modern `launchctl bootstrap` API.
///
/// The legacy `launchctl load -w` silently refuses to start a label launchd has marked
/// disabled (e.g. after an earlier crash loop) while reporting success, so `enable`
/// clears that sticky flag first.
#[cfg(target_os = "macos")]
pub fn start() -> Result<()> {
    let path = unit_path()?;
    if !path.exists() {
        bail!("The ygrep service is not installed. Run `ygrep service install` first.");
    }

    // Best-effort: a never-disabled label makes this a no-op.
    let _ = Command::new("launchctl")
        .args(["enable", &service_target()])
        .output();

    // bootstrap can transiently fail with "Input/output error" while a just-booted-out
    // job in the same domain is still tearing down. That is a retry signal.
    for attempt in 0..6 {
        let out = Command::new("launchctl")
            .arg("bootstrap")
            .arg(domain())
            .arg(&path)
            .output()
            .context("Failed to run launchctl bootstrap")?;
        if out.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&out.stderr);
        if stderr.contains("already bootstrapped")
            || stderr.contains("already loaded")
            || stderr.contains("service already loaded")
            || stderr.contains("Operation already in progress")
        {
            return Ok(());
        }
        let retryable = stderr.contains("Input/output error") || stderr.contains(": 5:");
        if retryable && attempt < 5 {
            std::thread::sleep(std::time::Duration::from_millis(200));
            continue;
        }
        if !stderr.trim().is_empty() {
            bail!("launchctl bootstrap failed: {}", stderr.trim());
        }
        return Ok(());
    }
    Ok(())
}

/// Stop and unload the service. Tolerates "not loaded".
#[cfg(target_os = "macos")]
pub fn stop() -> Result<()> {
    let path = unit_path()?;
    if !path.exists() {
        return Ok(());
    }
    let out = Command::new("launchctl")
        .arg("bootout")
        .arg(service_target())
        .output()
        .context("Failed to run launchctl bootout")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let benign = stderr.contains("No such process")
            || stderr.contains("not loaded")
            || stderr.contains("Could not find")
            || stderr.contains("could not find");
        if !benign && !stderr.trim().is_empty() {
            bail!("launchctl bootout failed: {}", stderr.trim());
        }
    }
    Ok(())
}

/// Restart with a full unload/load cycle so a rewritten plist's `ProgramArguments` are
/// re-read — `kickstart -k` would re-run the job definition launchd already has.
#[cfg(target_os = "macos")]
pub fn restart() -> Result<()> {
    let path = unit_path()?;
    if !path.exists() {
        bail!("The ygrep service is not installed. Run `ygrep service install` first.");
    }
    stop().ok();
    start()
}

/// Unload the service and delete its plist.
#[cfg(target_os = "macos")]
pub fn uninstall() -> Result<()> {
    stop().ok();
    let path = unit_path()?;
    if path.exists() {
        fs::remove_file(&path)
            .with_context(|| format!("Failed to remove plist {}", path.display()))?;
    }
    Ok(())
}

/// Report the service's state from `launchctl print`, falling back to `launchctl list`.
#[cfg(target_os = "macos")]
pub fn status() -> Status {
    if !unit_path().map(|p| p.exists()).unwrap_or(false) {
        return Status::NotInstalled;
    }

    let out = Command::new("launchctl")
        .args(["print", &service_target()])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output();

    match out {
        Ok(o) if o.status.success() => {
            let text = String::from_utf8_lossy(&o.stdout);
            if parse_pid(&text).is_some() || text.contains("state = running") {
                Status::Running
            } else if last_exit_failed(&text) {
                Status::Error
            } else {
                Status::Stopped
            }
        }
        // A plist on disk that launchd doesn't know about is installed but not loaded.
        _ => Status::Stopped,
    }
}

/// The pid launchd reports for the service, if it is running.
#[cfg(target_os = "macos")]
pub fn pid() -> Option<u32> {
    let out = Command::new("launchctl")
        .args(["print", &service_target()])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_pid(&String::from_utf8_lossy(&out.stdout))
}

/// Pull the `pid = N` line out of `launchctl print` output.
fn parse_pid(text: &str) -> Option<u32> {
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("pid = ") {
            if let Ok(pid) = rest.trim().parse::<u32>() {
                return Some(pid);
            }
        }
    }
    None
}

/// True when the last run exited with a non-zero status.
fn last_exit_failed(text: &str) -> bool {
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("last exit code = ") {
            return rest.trim() != "0";
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> ServiceSpec {
        ServiceSpec {
            program: PathBuf::from("/usr/local/bin/ygrep"),
            args: vec!["service".into(), "run".into()],
            log: PathBuf::from("/home/dev/.local/share/ygrep/logs/service.log"),
            keep_alive: true,
            run_at_load: true,
        }
    }

    #[test]
    fn plist_carries_the_label_program_and_log() {
        let plist = render_plist(&spec());

        assert!(plist.contains("<key>Label</key>\n    <string>com.yetidevworks.ygrep</string>"));
        assert!(plist.contains("<string>/usr/local/bin/ygrep</string>"));
        assert!(plist.contains("<string>service</string>"));
        assert!(plist.contains("<string>run</string>"));
        assert!(plist.contains("<key>RunAtLoad</key>\n    <true/>"));
        assert!(plist.contains("<key>KeepAlive</key>"));
        assert!(plist.contains(
            "<key>StandardOutPath</key>\n    <string>/home/dev/.local/share/ygrep/logs/service.log</string>"
        ));
        assert!(plist.contains(
            "<key>StandardErrorPath</key>\n    <string>/home/dev/.local/share/ygrep/logs/service.log</string>"
        ));
        assert!(plist.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>"));
    }

    #[test]
    fn plist_omits_keep_alive_when_it_is_off() {
        let mut spec = spec();
        spec.keep_alive = false;
        spec.run_at_load = false;

        let plist = render_plist(&spec);

        assert!(!plist.contains("KeepAlive"));
        assert!(plist.contains("<key>RunAtLoad</key>\n    <false/>"));
    }

    #[test]
    fn paths_with_xml_characters_are_escaped() {
        let mut spec = spec();
        spec.program = PathBuf::from("/opt/a&b/ygrep");
        spec.log = PathBuf::from("/tmp/<log>.log");

        let plist = render_plist(&spec);

        assert!(plist.contains("<string>/opt/a&amp;b/ygrep</string>"));
        assert!(plist.contains("<string>/tmp/&lt;log&gt;.log</string>"));
    }

    #[test]
    fn pid_is_read_from_launchctl_print() {
        let text =
            "com.yetidevworks.ygrep = {\n\tactive count = 1\n\tpid = 4823\n\tstate = running\n}";
        assert_eq!(parse_pid(text), Some(4823));
        assert!(!last_exit_failed(text));
    }

    #[test]
    fn a_failed_last_run_is_reported_as_an_error() {
        let text = "com.yetidevworks.ygrep = {\n\tlast exit code = 1\n}";
        assert_eq!(parse_pid(text), None);
        assert!(last_exit_failed(text));

        let clean = "com.yetidevworks.ygrep = {\n\tlast exit code = 0\n}";
        assert!(!last_exit_failed(clean));
    }
}
