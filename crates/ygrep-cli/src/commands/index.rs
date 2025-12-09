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

    eprintln!();
    eprintln!("Indexing complete in {:.2}s", elapsed.as_secs_f64());
    eprintln!("  Files indexed: {}", stats.indexed);
    eprintln!("  Files skipped: {}", stats.skipped);
    eprintln!("  Errors: {}", stats.errors);
    eprintln!("  Unique paths: {}", stats.unique_paths);
    eprintln!();
    eprintln!("Index stored at: {}", workspace.index_path().display());

    Ok(())
}
