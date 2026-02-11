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
    use_regex: bool,
    _show_scores: bool,
    text_only: bool,
    case_sensitive: bool,
    context_before: Option<usize>,
    context_after: Option<usize>,
    format: OutputFormat,
) -> Result<()> {
    // Open existing workspace (fails if not indexed)
    let workspace = match Workspace::open(workspace_path) {
        Ok(ws) => ws,
        Err(_) => {
            eprintln!("Workspace not indexed: {}", workspace_path.display());
            eprintln!();
            eprintln!("To index this workspace, run:");
            eprintln!("  ygrep index              # Text-only (fast)");
            eprintln!("  ygrep index --semantic   # With semantic search (slower, better results)");
            std::process::exit(1);
        }
    };

    // Search: use hybrid search by default if semantic index is available
    #[cfg(feature = "embeddings")]
    let use_hybrid = !text_only && workspace.has_semantic_index();
    #[cfg(not(feature = "embeddings"))]
    let use_hybrid = false;
    let _ = text_only; // Suppress unused warning when embeddings disabled

    let result = if use_hybrid && !use_regex {
        // Hybrid search (BM25 + vector with RRF) - not supported with regex
        #[cfg(feature = "embeddings")]
        {
            workspace
                .search_hybrid(query, Some(limit))
                .context("Hybrid search failed")?
        }
        #[cfg(not(feature = "embeddings"))]
        unreachable!()
    } else {
        // Build filters for text-only search
        let ext_filter = if extensions.is_empty() {
            None
        } else {
            Some(extensions)
        };
        let path_filter = if paths.is_empty() { None } else { Some(paths) };

        workspace
            .search_filtered(
                query,
                Some(limit),
                ext_filter,
                path_filter,
                use_regex,
                case_sensitive,
                context_before,
                context_after,
            )
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
