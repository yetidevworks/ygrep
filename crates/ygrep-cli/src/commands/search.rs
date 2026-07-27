use anyhow::{Context, Result};
use std::path::Path;
use ygrep_core::index::SCHEMA_VERSION;
use ygrep_core::search::SearchResult;
use ygrep_core::telemetry::{self, QueryMode};
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

    let workspace = refresh_if_needed(workspace, workspace_path, no_auto_index)?;

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

/// Bring an existing index up to date before searching it.
///
/// An index in an outdated format can return wrong results rather than merely dated
/// ones, so telling the user about it and searching anyway is the worst of both worlds:
/// they get an answer that looks authoritative and isn't. Refresh, then search.
///
/// Falls back to reporting the problem when a rebuild isn't possible or is disabled,
/// so a read-only index directory still searches rather than failing.
fn refresh_if_needed(
    workspace: Workspace,
    workspace_path: &Path,
    no_auto_index: bool,
) -> Result<Workspace> {
    let config = Config::load();
    let index_path = workspace.index_path().to_path_buf();

    let schema_outdated = workspace
        .stored_schema_version()
        .map(|v| v != SCHEMA_VERSION)
        .unwrap_or(true);
    let stale_note = progress::staleness_note(&index_path);

    if !schema_outdated && stale_note.is_none() {
        return Ok(workspace);
    }

    // Another process is already building this index. Starting a second build would put
    // two writers on one index, so search what we have and say why.
    if let Some(running) = progress::indexing_in_progress(&index_path) {
        eprintln!(
            "Index is being rebuilt for {} (running {}); searching the current index.",
            workspace_path.display(),
            progress::format_duration(running.elapsed())
        );
        return Ok(workspace);
    }

    let mut may_rebuild =
        !no_auto_index && config.search.auto_index && progress::index_dir_writable(&index_path);

    // A schema rebuild of a semantic index re-embeds every file, which takes minutes.
    // That is too long to spend implicitly behind a search, so ask instead. Incremental
    // refreshes stay automatic: they only embed the files that actually changed.
    if schema_outdated && workspace.stored_semantic_flag() == Some(true) {
        may_rebuild = false;
    }

    if !may_rebuild {
        // Can't fix it, so at least say what's wrong.
        if schema_outdated {
            eprintln!(
                "note: index was built by an older ygrep and may return wrong results, \
                 run `ygrep index` to rebuild it"
            );
        } else if let Some(note) = stale_note {
            eprintln!("{}", note);
        }
        return Ok(workspace);
    }

    if schema_outdated {
        eprintln!("Index format changed, rebuilding...");
    } else {
        eprintln!("Index is out of date, refreshing...");
    }

    // Claim the build before releasing the index, so a search starting in the same
    // moment sees it and searches instead of opening a second writer.
    let _progress = progress::IndexingGuard::start(&index_path);

    // Release the index before reindexing so the writer isn't blocked by our reader.
    drop(workspace);

    // A schema change needs a full rebuild; staleness only needs an incremental pass,
    // which skips every file whose mtime is unchanged.
    super::index::run_quiet(workspace_path, schema_outdated, false, false)
        .context("Refreshing the index failed")?;

    Workspace::open_readonly(workspace_path).context("Index refreshed but could not be reopened")
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
    super::index::run_quiet(workspace_path, false, false, true).context("Auto-indexing failed")?;

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

    let mode = if use_hybrid && !use_regex {
        QueryMode::Hybrid
    } else if use_regex {
        QueryMode::Regex
    } else {
        QueryMode::Literal
    };

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

    record_telemetry(&workspace, query, &result, mode);

    // Output results
    let output = match format {
        OutputFormat::Ai => result.format_ai(),
        OutputFormat::Json => result.format_json(),
        OutputFormat::Pretty => result.format_pretty(),
    };

    print!("{}", output);

    Ok(())
}

/// Log the query for the dashboard's stats view.
///
/// Best-effort throughout: a search that already produced its answer must not fail
/// because the log line couldn't be worked out or written.
fn record_telemetry(workspace: &Workspace, query: &str, result: &SearchResult, mode: QueryMode) {
    let index_path = workspace.index_path();
    let Some(hash) = index_path.file_name().and_then(|name| name.to_str()) else {
        return;
    };
    // <data_dir>/indexes/<hash> — the telemetry log lives beside the indexes directory.
    let Some(data_dir) = index_path.parent().and_then(|p| p.parent()) else {
        return;
    };

    telemetry::record_query(
        &Config::load(),
        data_dir,
        hash,
        query,
        result.query_time_ms,
        result.hits.len(),
        mode,
    );
}
