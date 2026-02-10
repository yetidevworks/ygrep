use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use std::fs;
use std::path::PathBuf;

/// Get the indexes directory
fn get_indexes_dir() -> Result<PathBuf> {
    // Honor XDG_DATA_HOME if set (even on macOS)
    if let Ok(xdg_data) = std::env::var("XDG_DATA_HOME") {
        if !xdg_data.is_empty() {
            return Ok(PathBuf::from(xdg_data).join("ygrep").join("indexes"));
        }
    }
    let data_dir = dirs::data_dir()
        .or_else(|| dirs::home_dir().map(|h| h.join(".local/share")))
        .context("Could not determine data directory")?;
    Ok(data_dir.join("ygrep").join("indexes"))
}

/// Index metadata stored in each index directory
#[derive(Debug)]
struct IndexInfo {
    hash: String,
    path: PathBuf,
    workspace: Option<String>,
    size_bytes: u64,
    semantic: Option<bool>,
    files_indexed: Option<u64>,
    indexed_at: Option<DateTime<Utc>>,
    orphaned: bool,
}

/// Read index info from a directory
fn read_index_info(hash: &str, index_path: &PathBuf) -> Result<IndexInfo> {
    let workspace_meta_path = index_path.join("workspace.json");
    let (workspace, semantic, files_indexed, indexed_at) = if workspace_meta_path.exists() {
        let json = fs::read_to_string(&workspace_meta_path)
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());

        let workspace = json.as_ref().and_then(|v| {
            v.get("workspace")
                .and_then(|w| w.as_str())
                .map(String::from)
        });
        let semantic = json
            .as_ref()
            .and_then(|v| v.get("semantic").and_then(|s| s.as_bool()));
        let files_indexed = json
            .as_ref()
            .and_then(|v| v.get("files_indexed").and_then(|f| f.as_u64()));
        let indexed_at = json
            .as_ref()
            .and_then(|v| v.get("indexed_at").and_then(|t| t.as_str()))
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Utc));

        (workspace, semantic, files_indexed, indexed_at)
    } else {
        (None, None, None, None)
    };

    let orphaned = match &workspace {
        Some(ws) => !PathBuf::from(ws).exists(),
        None => true,
    };

    let size_bytes = dir_size(index_path).unwrap_or(0);

    Ok(IndexInfo {
        hash: hash.to_string(),
        path: index_path.clone(),
        workspace,
        size_bytes,
        semantic,
        files_indexed,
        indexed_at,
        orphaned,
    })
}

/// Collect all valid indexes
fn collect_indexes() -> Result<Vec<IndexInfo>> {
    let indexes_dir = get_indexes_dir()?;

    if !indexes_dir.exists() {
        return Ok(Vec::new());
    }

    let mut indexes = Vec::new();

    for entry in fs::read_dir(&indexes_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            if !path.join("workspace.json").exists() {
                continue;
            }
            if let Some(hash) = path.file_name().and_then(|n| n.to_str()) {
                if let Ok(info) = read_index_info(hash, &path) {
                    indexes.push(info);
                }
            }
        }
    }

    Ok(indexes)
}

/// Calculate directory size recursively
fn dir_size(path: &PathBuf) -> Result<u64> {
    let mut size = 0;
    if path.is_dir() {
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                size += dir_size(&path)?;
            } else {
                size += entry.metadata()?.len();
            }
        }
    }
    Ok(size)
}

/// Format bytes as human readable (compact: "1.9G", "147M", "690K")
fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if bytes >= GB {
        format!("{:.1}G", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.0}M", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.0}K", bytes as f64 / KB as f64)
    } else {
        format!("{}B", bytes)
    }
}

/// Format a relative time string like "2h ago", "3d ago", "5mo ago"
fn format_relative_time(dt: &DateTime<Utc>) -> String {
    let now = Utc::now();
    let duration = now.signed_duration_since(dt);

    let minutes = duration.num_minutes();
    let hours = duration.num_hours();
    let days = duration.num_days();

    if minutes < 1 {
        "just now".to_string()
    } else if minutes < 60 {
        format!("{}m ago", minutes)
    } else if hours < 24 {
        format!("{}h ago", hours)
    } else if days < 30 {
        format!("{}d ago", days)
    } else if days < 365 {
        format!("{}mo ago", days / 30)
    } else {
        format!("{}y ago", days / 365)
    }
}

/// Shorten path by replacing home dir with ~
fn shorten_path(path: &str) -> String {
    if let Some(home) = dirs::home_dir() {
        if let Some(home_str) = home.to_str() {
            if path.starts_with(home_str) {
                return format!("~{}", &path[home_str.len()..]);
            }
        }
    }
    path.to_string()
}

/// List all indexes sorted by size (largest first)
pub fn list() -> Result<()> {
    let mut indexes = collect_indexes()?;

    if indexes.is_empty() {
        println!("No indexes found.");
        return Ok(());
    }

    // Sort by size descending
    indexes.sort_by(|a, b| b.size_bytes.cmp(&a.size_bytes));

    let total_size: u64 = indexes.iter().map(|i| i.size_bytes).sum();
    let orphan_count = indexes.iter().filter(|i| i.orphaned).count();

    println!(
        "{} indexes, {} total{}",
        indexes.len(),
        format_size(total_size),
        if orphan_count > 0 {
            format!(" ({} orphaned)", orphan_count)
        } else {
            String::new()
        }
    );
    println!();

    // Calculate column widths
    let size_width = indexes
        .iter()
        .map(|i| format_size(i.size_bytes).len())
        .max()
        .unwrap_or(4);

    for (i, info) in indexes.iter().enumerate() {
        let workspace = info.workspace.as_deref().unwrap_or("(unknown)");
        let display_path = shorten_path(workspace);

        let size_str = format_size(info.size_bytes);
        let index_type = match info.semantic {
            Some(true) => "semantic",
            _ => "text",
        };

        let files_str = match info.files_indexed {
            Some(n) => format!("{} files", n),
            None => "-".to_string(),
        };

        let time_str = match &info.indexed_at {
            Some(dt) => format_relative_time(dt),
            None => "-".to_string(),
        };

        let orphan_marker = if info.orphaned { " [orphaned]" } else { "" };

        // Line 1: number, size, type, files, time
        println!(
            "  {:>2}. {:>width$}  {}  {}  {}{}",
            i + 1,
            size_str,
            index_type,
            files_str,
            time_str,
            orphan_marker,
            width = size_width,
        );
        // Line 2: workspace path and hash
        println!("      {}  ({})", display_path, info.hash);
        println!();
    }

    println!("Commands:");
    println!("  ygrep indexes remove <hash|path>  Remove a specific index");
    println!("  ygrep indexes clean               Remove all orphaned indexes");

    Ok(())
}

/// Remove orphaned indexes (workspaces that no longer exist)
pub fn clean() -> Result<()> {
    let indexes = collect_indexes()?;

    if indexes.is_empty() {
        println!("No indexes found.");
        return Ok(());
    }

    let mut removed = 0;
    let mut freed = 0u64;

    for info in &indexes {
        if info.orphaned {
            let size = info.size_bytes;
            fs::remove_dir_all(&info.path)?;
            println!(
                "Removed: {} ({})",
                shorten_path(info.workspace.as_deref().unwrap_or(&info.hash)),
                format_size(size)
            );
            removed += 1;
            freed += size;
        }
    }

    if removed == 0 {
        println!("No orphaned indexes found.");
    } else {
        println!(
            "\nRemoved {} indexes, freed {}",
            removed,
            format_size(freed)
        );
    }

    Ok(())
}

/// Remove a specific index by hash or workspace path
pub fn remove(identifier: &str) -> Result<()> {
    let indexes_dir = get_indexes_dir()?;

    if !indexes_dir.exists() {
        println!("No indexes found.");
        return Ok(());
    }

    // First try as hash
    let index_path = indexes_dir.join(identifier);
    if index_path.exists() && index_path.is_dir() {
        let info = read_index_info(identifier, &index_path)?;
        fs::remove_dir_all(&index_path)?;
        println!(
            "Removed index: {} ({})",
            shorten_path(info.workspace.as_deref().unwrap_or(identifier)),
            format_size(info.size_bytes)
        );
        return Ok(());
    }

    // Try to find by workspace path (exact match or substring)
    let target_path = std::fs::canonicalize(identifier).ok();

    for entry in fs::read_dir(&indexes_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            if let Some(hash) = path.file_name().and_then(|n| n.to_str()) {
                if let Ok(info) = read_index_info(hash, &path) {
                    let matches = match (&info.workspace, &target_path) {
                        (Some(ws), Some(target)) => PathBuf::from(ws) == *target,
                        (Some(ws), None) => ws.contains(identifier),
                        _ => false,
                    };

                    if matches {
                        fs::remove_dir_all(&path)?;
                        println!(
                            "Removed index: {} ({})",
                            shorten_path(info.workspace.as_deref().unwrap_or(&info.hash)),
                            format_size(info.size_bytes)
                        );
                        return Ok(());
                    }
                }
            }
        }
    }

    println!("Index not found: {}", identifier);
    Ok(())
}
