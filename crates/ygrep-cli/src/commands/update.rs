use anyhow::{Context, Result};
use std::io::Read;
use std::path::{Path, PathBuf};

const GITHUB_REPO_OWNER: &str = "yetidevworks";
const GITHUB_REPO_NAME: &str = "ygrep";
const CHECK_INTERVAL_SECS: u64 = 24 * 60 * 60;

// ---------- version helpers ----------

/// Parse "1.2.3" into (1, 2, 3)
fn parse_version(v: &str) -> (u64, u64, u64) {
    let parts: Vec<u64> = v.split('.').filter_map(|p| p.parse().ok()).collect();
    (
        parts.first().copied().unwrap_or(0),
        parts.get(1).copied().unwrap_or(0),
        parts.get(2).copied().unwrap_or(0),
    )
}

fn is_newer(current: &str, latest: &str) -> bool {
    parse_version(latest) > parse_version(current)
}

// ---------- cache ----------

#[derive(serde::Serialize, serde::Deserialize)]
struct UpdateCache {
    latest_version: String,
    checked_at: u64,
}

fn cache_path() -> Option<PathBuf> {
    let data_dir = if let Ok(home) = std::env::var("YGREP_HOME") {
        PathBuf::from(home)
    } else if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        PathBuf::from(xdg).join("ygrep")
    } else {
        dirs::data_dir()?.join("ygrep")
    };
    Some(data_dir.join("update-check.json"))
}

fn read_cache() -> Option<UpdateCache> {
    let content = std::fs::read_to_string(cache_path()?).ok()?;
    serde_json::from_str(&content).ok()
}

fn write_cache(cache: &UpdateCache) -> Result<()> {
    let path = cache_path().context("Could not determine cache path")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_string(cache)?)?;
    Ok(())
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// ---------- GitHub API ----------

fn fetch_latest_version() -> Result<String> {
    let url = format!(
        "https://api.github.com/repos/{}/{}/releases/latest",
        GITHUB_REPO_OWNER, GITHUB_REPO_NAME
    );

    let resp = ureq::get(&url)
        .set(
            "User-Agent",
            &format!("ygrep/{}", env!("CARGO_PKG_VERSION")),
        )
        .set("Accept", "application/vnd.github.v3+json")
        .call()
        .context("Failed to reach GitHub API")?;

    let body: serde_json::Value = resp
        .into_json()
        .context("Failed to parse GitHub response")?;

    let tag = body["tag_name"]
        .as_str()
        .context("No tag_name in release")?;

    Ok(tag.strip_prefix('v').unwrap_or(tag).to_string())
}

// ---------- platform ----------

fn platform_target() -> Option<&'static str> {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    return Some("darwin-arm64");
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    return Some("darwin-x86_64");
    #[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "musl"))]
    return Some("linux-x86_64-musl");
    #[cfg(all(target_os = "linux", target_arch = "aarch64", target_env = "musl"))]
    return Some("linux-aarch64-musl");
    #[cfg(all(target_os = "linux", target_arch = "x86_64", not(target_env = "musl")))]
    return Some("linux-x86_64");
    #[cfg(all(target_os = "linux", target_arch = "aarch64", not(target_env = "musl")))]
    return Some("linux-aarch64");
    #[cfg(all(target_os = "linux", target_arch = "arm"))]
    return Some("linux-armv7");
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    return Some("windows-x86_64");
    #[allow(unreachable_code)]
    None
}

// ---------- install method ----------

enum InstallMethod {
    Homebrew,
    Cargo,
    Binary(PathBuf),
}

fn detect_install_method() -> InstallMethod {
    let exe = std::env::current_exe().unwrap_or_default();
    let s = exe.to_string_lossy();

    if s.contains("/Cellar/") || s.contains("/homebrew/") {
        InstallMethod::Homebrew
    } else if s.contains("/.cargo/bin/") || s.contains("\\.cargo\\bin\\") {
        InstallMethod::Cargo
    } else {
        InstallMethod::Binary(exe)
    }
}

// ---------- public entry points ----------

/// `ygrep update [--check]`
pub fn run(check_only: bool) -> Result<()> {
    let current = env!("CARGO_PKG_VERSION");

    eprintln!("Checking for updates...");
    let latest = fetch_latest_version()?;

    // Always update cache
    let _ = write_cache(&UpdateCache {
        latest_version: latest.clone(),
        checked_at: now_secs(),
    });

    if !is_newer(current, &latest) {
        eprintln!("ygrep v{} is already the latest version.", current);
        return Ok(());
    }

    eprintln!("Update available: v{} -> v{}", current, latest);

    if check_only {
        eprintln!("\nRun `ygrep update` to install.");
        return Ok(());
    }

    match detect_install_method() {
        InstallMethod::Homebrew => {
            eprintln!("\nygrep was installed via Homebrew. Run:");
            eprintln!("  brew upgrade ygrep");
        }
        InstallMethod::Cargo => {
            eprintln!("\nygrep was installed via cargo. Run:");
            eprintln!("  cargo install ygrep-cli");
        }
        InstallMethod::Binary(exe_path) => {
            perform_update(&exe_path, &latest)?;
        }
    }

    Ok(())
}

/// Non-blocking hint printed after search (reads cache, never does I/O to network).
/// Spawns a background refresh when the cache is stale.
pub fn maybe_print_update_hint() {
    let current = env!("CARGO_PKG_VERSION");
    let now = now_secs();

    if let Some(cache) = read_cache() {
        if is_newer(current, &cache.latest_version) {
            eprintln!(
                "\nygrep v{} available (current: v{}). Run `ygrep update` to upgrade.",
                cache.latest_version, current
            );
            return;
        }
        if now.saturating_sub(cache.checked_at) < CHECK_INTERVAL_SECS {
            return;
        }
    }

    // Cache is stale or missing — fire-and-forget background check
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::process::Command::new(exe)
            .args(["update", "--check"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
}

// ---------- download & replace ----------

fn perform_update(exe_path: &Path, version: &str) -> Result<()> {
    let target = platform_target().context("Unsupported platform for self-update")?;

    let is_windows = cfg!(target_os = "windows");
    let ext = if is_windows { "zip" } else { "tar.gz" };
    let asset_name = format!("ygrep-{}-{}.{}", version, target, ext);

    let download_url = format!(
        "https://github.com/{}/{}/releases/download/v{}/{}",
        GITHUB_REPO_OWNER, GITHUB_REPO_NAME, version, asset_name
    );

    eprintln!("Downloading {}...", asset_name);

    // Download to a temp directory
    let tmp = std::env::temp_dir().join(format!("ygrep-update-{}", std::process::id()));
    std::fs::create_dir_all(&tmp)?;

    // Ensure cleanup on all exit paths
    let _cleanup = TempDirGuard(tmp.clone());

    let archive_path = tmp.join(&asset_name);

    let resp = ureq::get(&download_url)
        .set(
            "User-Agent",
            &format!("ygrep/{}", env!("CARGO_PKG_VERSION")),
        )
        .call()
        .context("Failed to download release")?;

    // Stream response body to file
    let mut body = resp.into_reader();
    let mut file = std::fs::File::create(&archive_path)?;
    std::io::copy(&mut body, &mut file)?;
    drop(file);

    // Extract
    let bin_name = if is_windows { "ygrep.exe" } else { "ygrep" };

    if is_windows {
        extract_zip(&archive_path, &tmp)?;
    } else {
        extract_tar_gz(&archive_path, &tmp)?;
    }

    let new_bin = tmp.join(bin_name);
    if !new_bin.exists() {
        anyhow::bail!("Binary not found in archive");
    }

    // Replace
    replace_binary(&new_bin, exe_path)?;

    eprintln!("Updated ygrep to v{}", version);
    Ok(())
}

fn extract_tar_gz(archive: &Path, dest: &Path) -> Result<()> {
    let status = std::process::Command::new("tar")
        .args([
            "xzf",
            &archive.to_string_lossy(),
            "-C",
            &dest.to_string_lossy(),
        ])
        .status()
        .context("Failed to run tar")?;

    if !status.success() {
        anyhow::bail!("tar extraction failed");
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn extract_zip(archive: &Path, dest: &Path) -> Result<()> {
    let status = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            &format!(
                "Expand-Archive -Path '{}' -DestinationPath '{}' -Force",
                archive.display(),
                dest.display()
            ),
        ])
        .status()
        .context("Failed to run PowerShell")?;

    if !status.success() {
        anyhow::bail!("ZIP extraction failed");
    }
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn extract_zip(_archive: &Path, _dest: &Path) -> Result<()> {
    anyhow::bail!("ZIP extraction is only used on Windows")
}

fn replace_binary(new_bin: &Path, exe_path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(new_bin, std::fs::Permissions::from_mode(0o755))?;

        // Try atomic rename first; fall back to copy (e.g. cross-device)
        if std::fs::rename(new_bin, exe_path).is_err() {
            std::fs::copy(new_bin, exe_path)?;
        }
    }

    #[cfg(windows)]
    {
        let backup = exe_path.with_extension("exe.old");
        let _ = std::fs::remove_file(&backup);
        std::fs::rename(exe_path, &backup).context("Failed to move current binary")?;
        if let Err(e) = std::fs::copy(new_bin, exe_path) {
            // Attempt restore
            let _ = std::fs::rename(&backup, exe_path);
            return Err(e).context("Failed to install new binary");
        }
        let _ = std::fs::remove_file(&backup);
    }

    Ok(())
}

/// RAII guard that removes a temp directory on drop.
struct TempDirGuard(PathBuf);

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// ---------- helpers for download with progress ----------

/// Read the full response body, showing a simple byte counter on stderr.
#[allow(dead_code)]
fn read_body_with_progress(reader: &mut impl Read) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut total = 0usize;

    loop {
        let n = reader.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        total += n;
        eprint!("\r  downloaded {} KB", total / 1024);
    }
    eprintln!();
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_version() {
        assert_eq!(parse_version("3.1.6"), (3, 1, 6));
        assert_eq!(parse_version("0.1.0"), (0, 1, 0));
        assert_eq!(parse_version("10.20.30"), (10, 20, 30));
    }

    #[test]
    fn test_is_newer() {
        assert!(is_newer("3.1.5", "3.1.6"));
        assert!(is_newer("3.1.6", "3.2.0"));
        assert!(is_newer("3.1.6", "4.0.0"));
        assert!(!is_newer("3.1.6", "3.1.6"));
        assert!(!is_newer("3.1.6", "3.1.5"));
        assert!(!is_newer("4.0.0", "3.9.9"));
    }

    #[test]
    fn test_platform_target_is_some() {
        // Should always resolve on CI/dev machines
        assert!(platform_target().is_some());
    }
}
