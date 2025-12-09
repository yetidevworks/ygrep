use anyhow::{Result, Context};
use std::fs;
use std::path::PathBuf;

/// Get the indexes directory
fn get_indexes_dir() -> Result<PathBuf> {
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
}

/// Read index info from a directory
fn read_index_info(hash: &str, index_path: &PathBuf) -> Result<IndexInfo> {
    // Try to read workspace path from workspace.json (our metadata file)
    let workspace_meta_path = index_path.join("workspace.json");
    let workspace = if workspace_meta_path.exists() {
        fs::read_to_string(&workspace_meta_path)
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .and_then(|v| v.get("workspace").and_then(|w| w.as_str()).map(String::from))
    } else {
        None
    };

    // Calculate total size
    let size_bytes = dir_size(index_path).unwrap_or(0);

    Ok(IndexInfo {
        hash: hash.to_string(),
        path: index_path.clone(),
        workspace,
        size_bytes,
    })
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

/// Format bytes as human readable
fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

/// List all indexes
pub fn list() -> Result<()> {
    let indexes_dir = get_indexes_dir()?;

    if !indexes_dir.exists() {
        println!("No indexes found.");
        return Ok(());
    }

    let mut indexes = Vec::new();
    let mut total_size = 0u64;

    for entry in fs::read_dir(&indexes_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            if let Some(hash) = path.file_name().and_then(|n| n.to_str()) {
                if let Ok(info) = read_index_info(hash, &path) {
                    total_size += info.size_bytes;
                    indexes.push(info);
                }
            }
        }
    }

    if indexes.is_empty() {
        println!("No indexes found.");
        return Ok(());
    }

    println!("# {} indexes ({})\n", indexes.len(), format_size(total_size));

    for info in &indexes {
        let workspace = info.workspace.as_deref().unwrap_or("(unknown)");
        println!("{}  {}", info.hash, format_size(info.size_bytes));
        println!("  {}\n", workspace);
    }

    Ok(())
}

/// Remove orphaned indexes (workspaces that no longer exist)
pub fn clean() -> Result<()> {
    let indexes_dir = get_indexes_dir()?;

    if !indexes_dir.exists() {
        println!("No indexes found.");
        return Ok(());
    }

    let mut removed = 0;
    let mut freed = 0u64;

    for entry in fs::read_dir(&indexes_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            if let Some(hash) = path.file_name().and_then(|n| n.to_str()) {
                if let Ok(info) = read_index_info(hash, &path) {
                    // Check if workspace still exists
                    let should_remove = match &info.workspace {
                        Some(ws) => !PathBuf::from(ws).exists(),
                        None => true, // Remove indexes with unknown workspace
                    };

                    if should_remove {
                        let size = info.size_bytes;
                        fs::remove_dir_all(&path)?;
                        println!("Removed: {} ({})", info.workspace.as_deref().unwrap_or(&info.hash), format_size(size));
                        removed += 1;
                        freed += size;
                    }
                }
            }
        }
    }

    if removed == 0 {
        println!("No orphaned indexes found.");
    } else {
        println!("\nRemoved {} indexes, freed {}", removed, format_size(freed));
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
        println!("Removed index: {} ({})", identifier, format_size(info.size_bytes));
        return Ok(());
    }

    // Try to find by workspace path
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
                        println!("Removed index for: {} ({})", info.workspace.as_deref().unwrap_or(&info.hash), format_size(info.size_bytes));
                        return Ok(());
                    }
                }
            }
        }
    }

    println!("Index not found: {}", identifier);
    Ok(())
}
