use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use ygrep_core::registry::{
    self, dir_size, format_relative_time, format_size, shorten_path, IndexInfo, IndexMatch,
};

/// Segment count above which `indexes list` reports an index as worth compacting.
///
/// Matches the default `auto_compact_segments`, so an index only appears here when
/// auto-compaction is disabled or hasn't run since the segments accumulated.
const COMPACTABLE_SEGMENTS: usize = 16;

/// Indexes directory for the current working directory.
pub fn get_indexes_dir() -> Result<PathBuf> {
    let config = ygrep_core::Config::load();
    Ok(registry::indexes_dir(&config)?)
}

/// Collect all valid indexes
pub fn collect_indexes() -> Result<Vec<IndexInfo>> {
    Ok(registry::collect_indexes()?)
}

/// Delete an index directory, but only after proving it lives inside the indexes directory.
pub(crate) fn remove_index_dir(indexes_dir: &Path, target: &Path) -> Result<()> {
    Ok(registry::remove_index_dir(indexes_dir, target)?)
}

/// Ask before deleting. Non-interactive callers proceed — the containment check in core
/// is what actually keeps the deletion safe, so scripts and agents are not blocked.
fn confirm(prompt: &str) -> Result<bool> {
    use std::io::{IsTerminal, Write};

    if !std::io::stdin().is_terminal() {
        return Ok(true);
    }

    print!("{} [y/N] ", prompt);
    std::io::stdout().flush()?;

    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;

    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// Report an ambiguous identifier and how to disambiguate it.
fn report_ambiguous(identifier: &str, matches: &[IndexInfo], action: &str) {
    println!(
        "Ambiguous identifier '{}' matches {} indexes:",
        identifier,
        matches.len()
    );
    for info in matches {
        println!("  {} ({})", shorten_path(info.label()), info.hash);
    }
    println!("\nUse the full hash to {} a specific index.", action);
}

/// Resolve an identifier to a single registered index, reporting why not when it fails.
fn find_index(identifier: Option<&str>) -> Result<Option<IndexInfo>> {
    match registry::find_index(identifier)? {
        IndexMatch::One(info) => Ok(Some(info)),
        IndexMatch::Ambiguous(matches) => {
            report_ambiguous(identifier.unwrap_or(""), &matches, "select");
            Ok(None)
        }
        IndexMatch::None => Ok(None),
    }
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
            Some(dt) => format!("updated {}", format_relative_time(dt)),
            None => "-".to_string(),
        };

        let orphan_marker = if info.orphaned { " [orphaned]" } else { "" };
        let watch_marker = if info.watch { " [watch]" } else { "" };

        // Accumulated segments mean reclaimable space, so say so rather than letting
        // the index quietly carry garbage.
        let segment_marker = match info.segments {
            Some(n) if n > COMPACTABLE_SEGMENTS => format!(" [{} segments, compactable]", n),
            _ => String::new(),
        };

        // Line 1: number, size, type, files, time
        println!(
            "  {:>2}. {:>width$}  {}  {}  {}{}{}{}",
            i + 1,
            size_str,
            index_type,
            files_str,
            time_str,
            watch_marker,
            orphan_marker,
            segment_marker,
            width = size_width,
        );
        // Line 2: workspace path and hash
        println!("      {}  ({})", display_path, info.hash);
        println!();
    }

    println!("Commands:");
    println!("  ygrep indexes remove <hash|path>  Remove a specific index");
    println!("  ygrep indexes compact [hash|path] Compact an index");
    println!("  ygrep indexes watch <id> on|off   Watch this index from the background service");
    println!("  ygrep indexes clean               Remove all orphaned indexes");
    println!("  (add --dry-run to preview, --yes to skip confirmation)");

    Ok(())
}

/// Turn the persisted watch flag on or off for an index.
pub fn watch(identifier: Option<&str>, enabled: bool) -> Result<()> {
    let Some(mut info) = find_index(identifier)? else {
        match identifier {
            Some(identifier) => println!("Index not found: {}", identifier),
            None => println!("No index found for the current workspace."),
        }
        return Ok(());
    };

    info.set_watch(enabled)
        .with_context(|| format!("Failed to update the watch flag for {}", info.hash))?;

    println!(
        "Watch {} for {} ({})",
        if enabled { "enabled" } else { "disabled" },
        shorten_path(info.label()),
        info.hash
    );

    if enabled && info.orphaned {
        println!("Note: this workspace no longer exists on disk, so nothing will be watched.");
    }

    Ok(())
}

/// Compact an index by merging segments and garbage-collecting stale files.
pub fn compact(identifier: Option<&str>) -> Result<()> {
    let Some(info) = find_index(identifier)? else {
        match identifier {
            Some(identifier) => println!("Index not found: {}", identifier),
            None => println!("No index found for the current workspace."),
        }
        return Ok(());
    };

    if info.orphaned {
        println!(
            "Refusing to compact orphaned index: {} ({})",
            shorten_path(info.label()),
            info.hash
        );
        println!("Use `ygrep indexes remove {}` to delete it.", info.hash);
        return Ok(());
    }

    let before = info.size_bytes;
    println!(
        "Compacting {} ({})...",
        shorten_path(info.label()),
        format_size(before)
    );

    let stats = ygrep_core::index::compact_index(&info.path)?;
    let after = dir_size(&info.path).unwrap_or(before);

    println!(
        "Compacted: {} -> {}",
        format_size(before),
        format_size(after)
    );
    println!(
        "Segments: {} -> {}",
        stats.segments_before, stats.segments_after
    );
    if before > after {
        println!("Freed {}", format_size(before - after));
    }

    Ok(())
}

/// Remove orphaned indexes (workspaces that no longer exist)
pub fn clean(dry_run: bool, assume_yes: bool) -> Result<()> {
    let indexes_dir = get_indexes_dir()?;
    let indexes = collect_indexes()?;

    if indexes.is_empty() {
        println!("No indexes found.");
        return Ok(());
    }

    let orphaned: Vec<&IndexInfo> = indexes.iter().filter(|info| info.orphaned).collect();

    if orphaned.is_empty() {
        println!("No orphaned indexes found.");
        return Ok(());
    }

    let total: u64 = orphaned.iter().map(|info| info.size_bytes).sum();

    for info in &orphaned {
        println!("  {} ({})", shorten_path(info.label()), info.path.display());
    }

    if dry_run {
        println!(
            "\nWould remove {} indexes, freeing {}",
            orphaned.len(),
            format_size(total)
        );
        return Ok(());
    }

    if !assume_yes
        && !confirm(&format!(
            "\nRemove {} orphaned indexes ({})?",
            orphaned.len(),
            format_size(total)
        ))?
    {
        println!("Aborted.");
        return Ok(());
    }

    for info in &orphaned {
        remove_index_dir(&indexes_dir, &info.path)?;
        println!(
            "Removed: {} ({})",
            shorten_path(info.label()),
            format_size(info.size_bytes)
        );
    }

    println!(
        "\nRemoved {} indexes, freed {}",
        orphaned.len(),
        format_size(total)
    );

    Ok(())
}

/// Remove a specific index by hash or workspace path
pub fn remove(identifier: &str, dry_run: bool, assume_yes: bool) -> Result<()> {
    let indexes_dir = get_indexes_dir()?;

    if !indexes_dir.exists() {
        println!("No indexes found.");
        return Ok(());
    }

    let info = match registry::resolve_index_target(&indexes_dir, identifier)? {
        IndexMatch::One(info) => info,
        IndexMatch::Ambiguous(matches) => {
            report_ambiguous(identifier, &matches, "remove");
            return Ok(());
        }
        IndexMatch::None => {
            println!("Index not found: {}", identifier);
            return Ok(());
        }
    };

    let label = shorten_path(info.label());
    let size = format_size(info.size_bytes);

    if dry_run {
        println!("Would remove index: {} ({})", label, size);
        println!("  index directory: {}", info.path.display());
        return Ok(());
    }

    if !assume_yes {
        println!("Index: {} ({})", label, size);
        println!("  index directory: {}", info.path.display());
        if !confirm("Remove this index?")? {
            println!("Aborted.");
            return Ok(());
        }
    }

    remove_index_dir(&indexes_dir, &info.path)?;
    println!("Removed index: {} ({})", label, size);

    Ok(())
}
