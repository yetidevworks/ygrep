use anyhow::{Context, Result};
use std::path::Path;
use ygrep_core::Workspace;

use crate::OutputFormat;

pub fn run(
    workspace_path: &Path,
    query: &str,
    limit: usize,
    extensions: Vec<String>,
    paths: Vec<String>,
    _show_scores: bool,
    text_only: bool,
    format: OutputFormat,
) -> Result<()> {
    // Open workspace
    let workspace = Workspace::open(workspace_path)
        .context("Failed to open workspace")?;

    // Check if indexed
    if !workspace.is_indexed() {
        eprintln!("Workspace not indexed. Run `ygrep index` first.");
        eprintln!("Indexing now...");

        let stats = workspace.index_all()
            .context("Failed to index workspace")?;

        eprintln!("Indexed {} files ({} skipped, {} errors)", stats.indexed, stats.skipped, stats.errors);
    }

    // Search: use hybrid search by default if semantic index is available
    let use_hybrid = !text_only && workspace.has_semantic_index();

    let result = if use_hybrid {
        // Hybrid search (BM25 + vector with RRF)
        workspace.search_hybrid(query, Some(limit))
            .context("Hybrid search failed")?
    } else {
        // Build filters for text-only search
        let ext_filter = if extensions.is_empty() { None } else { Some(extensions) };
        let path_filter = if paths.is_empty() { None } else { Some(paths) };

        workspace.search_filtered(query, Some(limit), ext_filter, path_filter)
            .context("Search failed")?
    };

    // Output results
    let output = match format {
        OutputFormat::Ai => result.format_ai(),
        OutputFormat::Json => result.format_json(),
        OutputFormat::Pretty => result.format_pretty(),
    };

    print!("{}", output);

    Ok(())
}
