use anyhow::{Context, Result};
use std::path::Path;
use std::time::Instant;
use ygrep_core::Workspace;

pub fn run(workspace_path: &Path, rebuild: bool, with_embeddings: bool) -> Result<()> {
    let start = Instant::now();

    eprintln!("Indexing {}...", workspace_path.display());

    if rebuild {
        eprintln!("Rebuilding index from scratch...");
        // Delete existing index directory
        if let Ok(workspace) = Workspace::open(workspace_path) {
            let index_path = workspace.index_path().to_path_buf();
            drop(workspace); // Release the workspace before deleting
            if index_path.exists() {
                std::fs::remove_dir_all(&index_path)
                    .context("Failed to remove existing index")?;
                eprintln!("  Cleared old index at {}", index_path.display());
            }
        }
    }

    if with_embeddings {
        eprintln!("(with semantic embeddings - this may take a while)");
    }

    // Open workspace (creates fresh index if rebuilt)
    let workspace = Workspace::open(workspace_path)
        .context("Failed to open workspace")?;

    // Index all files
    let stats = workspace.index_all_with_options(with_embeddings)
        .context("Failed to index workspace")?;

    let elapsed = start.elapsed();
    let index_size = dir_size(workspace.index_path());

    eprintln!();
    eprintln!("Indexing complete in {:.2}s", elapsed.as_secs_f64());
    eprintln!("  Files indexed: {}", stats.indexed);
    if stats.embedded > 0 {
        eprintln!("  Files embedded: {}", stats.embedded);
    }
    eprintln!("  Files skipped: {}", stats.skipped);
    eprintln!("  Errors: {}", stats.errors);
    eprintln!("  Index size: {}", format_size(index_size));
    eprintln!();
    eprintln!("Index stored at: {}", workspace.index_path().display());

    Ok(())
}

fn dir_size(path: &Path) -> u64 {
    walkdir::WalkDir::new(path)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.2} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} bytes", bytes)
    }
}
