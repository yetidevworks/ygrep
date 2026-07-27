use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Instant;
use ygrep_core::fs::FileWalker;
use ygrep_core::index::SCHEMA_VERSION;
use ygrep_core::{Config, Workspace};

/// Report what would be indexed, without building anything.
///
/// Useful for checking whether generated assets are being picked up, and for tuning
/// `ignore_patterns` / `max_avg_line_length` against a real tree.
pub fn dry_run(workspace_path: &Path) -> Result<()> {
    let root = std::fs::canonicalize(workspace_path)
        .with_context(|| format!("Cannot read {}", workspace_path.display()))?;
    let config = Config::load();

    let mut walker = FileWalker::new(root.clone(), config.indexer.clone())?;

    let mut total_files = 0u64;
    let mut total_bytes = 0u64;
    let mut by_ext: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    let mut largest: Vec<(u64, PathBuf)> = Vec::new();

    for entry in walker.walk() {
        let size = std::fs::metadata(&entry.path).map(|m| m.len()).unwrap_or(0);
        total_files += 1;
        total_bytes += size;

        let ext = entry
            .path
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_else(|| "(none)".into());
        let counter = by_ext.entry(ext).or_insert((0, 0));
        counter.0 += 1;
        counter.1 += size;

        largest.push((size, entry.path));
    }

    println!(
        "Would index {} ({} files, {})",
        root.display(),
        total_files,
        format_size(total_bytes)
    );

    if total_files == 0 {
        println!();
        println!("Nothing matched. Check `ignore_patterns` in your config.");
        return Ok(());
    }

    let mut exts: Vec<_> = by_ext.into_iter().collect();
    exts.sort_by(|a, b| b.1 .1.cmp(&a.1 .1));
    println!();
    println!("By extension:");
    for (ext, (count, bytes)) in exts.iter().take(10) {
        println!(
            "  {:<12} {:>6} files  {:>10}",
            ext,
            count,
            format_size(*bytes)
        );
    }

    largest.sort_by(|a, b| b.0.cmp(&a.0));
    println!();
    println!("Largest files:");
    for (size, path) in largest.iter().take(10) {
        let relative = path.strip_prefix(&root).unwrap_or(path);
        println!("  {:>10}  {}", format_size(*size), relative.display());
    }

    Ok(())
}

pub fn run(
    workspace_path: &Path,
    rebuild: bool,
    semantic_flag: bool,
    text_flag: bool,
) -> Result<()> {
    let start = Instant::now();

    eprintln!("Indexing {}...", workspace_path.display());

    // Open workspace first to read stored flags (before potential rebuild)
    // Use create() here since we may need to create the index
    let (stored_semantic, needs_schema_rebuild) = if !rebuild {
        match Workspace::create(workspace_path) {
            Ok(ws) => {
                let sem = ws.stored_semantic_flag();
                let schema_outdated = if ws.is_indexed() {
                    // Existing index: missing version means pre-v2 schema
                    ws.stored_schema_version()
                        .map(|v| v != SCHEMA_VERSION)
                        .unwrap_or(true)
                } else {
                    false // No existing index, nothing to rebuild
                };
                (sem, schema_outdated)
            }
            Err(_) => (None, false),
        }
    } else {
        (None, false)
    };

    let do_rebuild = rebuild || needs_schema_rebuild;

    if do_rebuild {
        if needs_schema_rebuild && !rebuild {
            eprintln!("Schema version changed, rebuilding index...");
        } else {
            eprintln!("Rebuilding index from scratch...");
        }
        // Delete existing index directory
        if let Ok(workspace) = Workspace::create(workspace_path) {
            let index_path = workspace.index_path().to_path_buf();
            drop(workspace); // Release the workspace before deleting
            if index_path.exists() {
                std::fs::remove_dir_all(&index_path).context("Failed to remove existing index")?;
                eprintln!("  Cleared old index at {}", index_path.display());
            }
        }
    }

    // Determine whether to use embeddings:
    // 1. Explicit --semantic flag always enables
    // 2. Explicit --text flag always disables
    // 3. Otherwise, use stored flag from workspace.json
    // 4. Default to false if no stored flag
    let with_embeddings = if semantic_flag {
        true
    } else if text_flag {
        false
    } else {
        stored_semantic.unwrap_or(false)
    };

    // Show what mode we're using
    if with_embeddings {
        if semantic_flag {
            eprintln!("(building semantic index - this may take a while)");
        } else {
            eprintln!("(using stored semantic mode - this may take a while)");
        }
    } else if text_flag && stored_semantic == Some(true) {
        eprintln!("(converting to text-only index)");
    }

    // Create or open workspace for indexing
    let workspace = Workspace::create(workspace_path).context("Failed to create workspace")?;

    // Mark the build as in flight so a concurrent search can report progress instead of
    // "not indexed". Dropped on every exit path, including errors.
    let _progress = crate::commands::progress::IndexingGuard::start(workspace.index_path());

    // Determine indexing strategy:
    // - --rebuild: full re-index
    // - existing index: incremental
    // - no existing index: full
    let use_incremental = !do_rebuild && workspace.is_indexed();

    let stats = if use_incremental {
        eprintln!("(incremental update)");
        workspace
            .index_incremental_with_options(with_embeddings)
            .context("Failed to incrementally index workspace")?
    } else {
        workspace
            .index_all_with_options(with_embeddings)
            .context("Failed to index workspace")?
    };

    let elapsed = start.elapsed();
    let index_size = dir_size(workspace.index_path());

    let index_type = if with_embeddings { "semantic" } else { "text" };

    eprintln!();
    eprintln!("Indexing complete in {:.2}s", elapsed.as_secs_f64());
    eprintln!("  Index type: {}", index_type);
    eprintln!("  Files indexed: {}", stats.indexed);
    if stats.unchanged > 0 {
        eprintln!("  Files unchanged: {}", stats.unchanged);
    }
    if stats.removed > 0 {
        eprintln!("  Files removed: {}", stats.removed);
    }
    if stats.embedded > 0 {
        eprintln!("  Semantic indexed: {}", stats.embedded);
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
