use anyhow::{Context, Result};
use std::path::Path;
use ygrep_core::{Config, Workspace, YgrepError};

use crate::commands::progress;
use crate::OutputFormat;

#[allow(clippy::too_many_arguments)]
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
    verbose: bool,
    no_auto_index: bool,
) -> Result<()> {
    // Open existing workspace read-only (fails if not indexed)
    let workspace = match Workspace::open_readonly(workspace_path) {
        Ok(ws) => ws,
        Err(YgrepError::WorkspaceNotIndexed(_)) => {
            open_after_auto_index(workspace_path, no_auto_index)?
        }
        Err(e) => {
            eprintln!(
                "Failed to open index for {}: {}",
                workspace_path.display(),
                e
            );
            std::process::exit(1);
        }
    };

    if let Ok(index_path) = Workspace::resolve_index_path(workspace_path, &Config::load()) {
        if let Some(note) = progress::staleness_note(&index_path) {
            eprintln!("{}", note);
        }
    }

    search_with(
        workspace,
        query,
        limit,
        extensions,
        paths,
        use_regex,
        text_only,
        case_sensitive,
        context_before,
        context_after,
        format,
        verbose,
    )
}

/// Handle a search against a workspace with no index yet.
///
/// Builds a text-only index and returns the opened workspace. Bails with the previous
/// guidance when auto-indexing is off, another build is already running, or the index
/// directory isn't writable.
fn open_after_auto_index(workspace_path: &Path, no_auto_index: bool) -> Result<Workspace> {
    let config = Config::load();
    let index_path = Workspace::resolve_index_path(workspace_path, &config).ok();

    // Another process is already building this index (e.g. the editor session hook).
    // Waiting would be worse than saying so: the caller can retry in a moment.
    if let Some(running) = index_path
        .as_deref()
        .and_then(progress::indexing_in_progress)
    {
        eprintln!(
            "Index is being built for {} (running {}).",
            workspace_path.display(),
            progress::format_duration(running.elapsed())
        );
        eprintln!("Retry the search shortly, or run `ygrep index` to build it in the foreground.");
        std::process::exit(1);
    }

    let writable = index_path
        .as_deref()
        .map(progress::index_dir_writable)
        .unwrap_or(false);

    if no_auto_index || !config.search.auto_index || !writable {
        eprintln!("Workspace not indexed: {}", workspace_path.display());
        if !writable {
            eprintln!("(the index directory is not writable, so it can't be built here)");
        }
        eprintln!();
        eprintln!("To index this workspace, run:");
        eprintln!("  ygrep index              # Text-only (fast)");
        eprintln!("  ygrep index --semantic   # With semantic search (slower, better results)");
        std::process::exit(1);
    }

    eprintln!(
        "No index for {}, building one (text-only)...",
        workspace_path.display()
    );

    // Text-only: a semantic build downloads a model and takes minutes, which is far too
    // much to do implicitly behind someone's search.
    super::index::run(workspace_path, false, false, true).context("Auto-indexing failed")?;

    Workspace::open_readonly(workspace_path).context("Index built but could not be opened")
}

#[allow(clippy::too_many_arguments)]
fn search_with(
    workspace: Workspace,
    query: &str,
    limit: usize,
    extensions: Vec<String>,
    paths: Vec<String>,
    use_regex: bool,
    text_only: bool,
    case_sensitive: bool,
    context_before: Option<usize>,
    context_after: Option<usize>,
    format: OutputFormat,
    verbose: bool,
) -> Result<()> {
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
                verbose,
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
