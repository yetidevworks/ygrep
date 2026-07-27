use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use std::fs;
use std::path::{Component, Path, PathBuf};

/// Segment count above which `indexes list` reports an index as worth compacting.
///
/// Matches the default `auto_compact_segments`, so an index only appears here when
/// auto-compaction is disabled or hasn't run since the segments accumulated.
const COMPACTABLE_SEGMENTS: usize = 16;

/// Get the indexes directory with the same resolution as Workspace::open_internal():
/// 1. Auto-detect: .ygrep/ in CWD
/// 2. Relative data_dir in config: resolve against CWD
/// 3. Absolute data_dir from config: use as-is
pub fn get_indexes_dir() -> Result<PathBuf> {
    let config = ygrep_core::Config::load();
    let cwd = std::env::current_dir()?;
    let local_ygrep = cwd.join(".ygrep");
    let data_dir = if local_ygrep.is_dir() {
        local_ygrep
    } else if config.indexer.data_dir.is_relative() {
        cwd.join(&config.indexer.data_dir)
    } else {
        config.indexer.data_dir.clone()
    };
    Ok(data_dir.join("indexes"))
}

/// True when `identifier` is a single ordinary path component, e.g. an index hash.
///
/// Anything else — an absolute path, `..`, or a nested path — must never be joined
/// onto the indexes directory: `Path::join` silently discards its base when handed an
/// absolute path, and `..` walks straight back out of it.
fn is_bare_component(identifier: &str) -> bool {
    let mut components = Path::new(identifier).components();
    matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none()
}

/// Refuse any delete target that is not strictly inside the indexes directory.
///
/// Both sides are canonicalized so symlinks and `..` cannot be used to step outside,
/// and the indexes directory itself is rejected so a bad resolution cannot wipe every
/// index at once.
fn ensure_within_indexes_dir(indexes_dir: &Path, target: &Path) -> Result<()> {
    let root = fs::canonicalize(indexes_dir).with_context(|| {
        format!(
            "Failed to resolve indexes directory {}",
            indexes_dir.display()
        )
    })?;
    let resolved = fs::canonicalize(target)
        .with_context(|| format!("Failed to resolve {}", target.display()))?;

    if resolved == root || !resolved.starts_with(&root) {
        bail!(
            "Refusing to delete {}: it is not inside the ygrep index directory ({}).\n\
             This is a bug — please report it at https://github.com/yetidevworks/ygrep/issues",
            resolved.display(),
            root.display()
        );
    }

    Ok(())
}

/// Delete an index directory, but only after proving it lives inside the indexes directory.
///
/// Every deletion of an index goes through here.
pub(crate) fn remove_index_dir(indexes_dir: &Path, target: &Path) -> Result<()> {
    ensure_within_indexes_dir(indexes_dir, target)?;
    fs::remove_dir_all(target)
        .with_context(|| format!("Failed to remove index at {}", target.display()))
}

/// Ask before deleting. Non-interactive callers proceed — the containment check above
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

/// Index metadata stored in each index directory
#[derive(Debug, Clone)]
pub struct IndexInfo {
    pub hash: String,
    pub path: PathBuf,
    pub workspace: Option<String>,
    pub size_bytes: u64,
    pub semantic: Option<bool>,
    pub files_indexed: Option<u64>,
    pub indexed_at: Option<DateTime<Utc>>,
    pub orphaned: bool,
}

/// Read index info from a directory
pub fn read_index_info(hash: &str, index_path: &PathBuf) -> Result<IndexInfo> {
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
pub fn collect_indexes() -> Result<Vec<IndexInfo>> {
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
pub fn dir_size(path: &PathBuf) -> Result<u64> {
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
pub fn format_size(bytes: u64) -> String {
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
pub fn format_relative_time(dt: &DateTime<Utc>) -> String {
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
pub fn shorten_path(path: &str) -> String {
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
            Some(dt) => format!("updated {}", format_relative_time(dt)),
            None => "-".to_string(),
        };

        let orphan_marker = if info.orphaned { " [orphaned]" } else { "" };

        // Accumulated segments mean reclaimable space, so say so rather than letting
        // the index quietly carry garbage.
        let segment_marker = match ygrep_core::index::segment_count(&info.path) {
            Some(n) if n > COMPACTABLE_SEGMENTS => format!(" [{} segments, compactable]", n),
            _ => String::new(),
        };

        // Line 1: number, size, type, files, time
        println!(
            "  {:>2}. {:>width$}  {}  {}  {}{}{}",
            i + 1,
            size_str,
            index_type,
            files_str,
            time_str,
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
    println!("  ygrep indexes clean               Remove all orphaned indexes");
    println!("  (add --dry-run to preview, --yes to skip confirmation)");

    Ok(())
}

fn find_index(identifier: Option<&str>) -> Result<Option<IndexInfo>> {
    let indexes = collect_indexes()?;

    if indexes.is_empty() {
        return Ok(None);
    }

    if let Some(identifier) = identifier {
        if let Some(info) = indexes.iter().find(|info| info.hash == identifier) {
            return Ok(Some(info.clone()));
        }

        let target_path = std::fs::canonicalize(identifier).ok();
        let matches: Vec<_> = indexes
            .into_iter()
            .filter(|info| match (&info.workspace, &target_path) {
                (Some(ws), Some(target)) => PathBuf::from(ws) == *target,
                (Some(ws), None) => ws.contains(identifier),
                _ => false,
            })
            .collect();

        return match matches.len() {
            0 => Ok(None),
            1 => Ok(matches.into_iter().next()),
            _ => {
                println!(
                    "Ambiguous identifier '{}' matches {} indexes:",
                    identifier,
                    matches.len()
                );
                for info in &matches {
                    println!(
                        "  {} ({})",
                        shorten_path(info.workspace.as_deref().unwrap_or(&info.hash)),
                        info.hash
                    );
                }
                println!("\nUse the full hash to select a specific index.");
                Ok(None)
            }
        };
    }

    let cwd = std::env::current_dir()
        .ok()
        .and_then(|path| std::fs::canonicalize(path).ok());
    if let Some(cwd) = cwd {
        if let Some(info) = indexes.iter().find(|info| {
            info.workspace
                .as_ref()
                .map(|workspace| PathBuf::from(workspace) == cwd)
                .unwrap_or(false)
        }) {
            return Ok(Some(info.clone()));
        }
    }

    Ok(None)
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
            shorten_path(info.workspace.as_deref().unwrap_or(&info.hash)),
            info.hash
        );
        println!("Use `ygrep indexes remove {}` to delete it.", info.hash);
        return Ok(());
    }

    let before = info.size_bytes;
    println!(
        "Compacting {} ({})...",
        shorten_path(info.workspace.as_deref().unwrap_or(&info.hash)),
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
        println!(
            "  {} ({})",
            shorten_path(info.workspace.as_deref().unwrap_or(&info.hash)),
            info.path.display()
        );
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
            shorten_path(info.workspace.as_deref().unwrap_or(&info.hash)),
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

/// Resolve an identifier (index hash or workspace path) to the index directory it names.
///
/// Returns `None` when nothing matched or the identifier was ambiguous; the reason is
/// printed for the user. Nothing is deleted here — resolution happens before any
/// destructive step so the caller can report or confirm the real target first.
fn resolve_removal_target(
    indexes_dir: &Path,
    identifier: &str,
) -> Result<Option<(PathBuf, IndexInfo)>> {
    // Hash form. An index hash is always a single path component, so requiring one here
    // keeps the join from resolving anywhere but inside the indexes directory.
    if is_bare_component(identifier) {
        let index_path = indexes_dir.join(identifier);
        if index_path.is_dir() {
            let info = read_index_info(identifier, &index_path)?;
            return Ok(Some((index_path, info)));
        }
    }

    // Workspace-path form: match the recorded workspace of each index, never the
    // identifier as a filesystem location.
    let target_path = fs::canonicalize(identifier).ok();

    let mut matched: Vec<(PathBuf, IndexInfo)> = Vec::new();

    for entry in fs::read_dir(indexes_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            if let Some(hash) = path.file_name().and_then(|n| n.to_str()) {
                if let Ok(info) = read_index_info(hash, &path) {
                    let is_match = match (&info.workspace, &target_path) {
                        (Some(ws), Some(target)) => PathBuf::from(ws) == *target,
                        (Some(ws), None) => ws.contains(identifier),
                        _ => false,
                    };

                    if is_match {
                        matched.push((path, info));
                    }
                }
            }
        }
    }

    match matched.len() {
        0 => {
            println!("Index not found: {}", identifier);
            Ok(None)
        }
        1 => Ok(matched.into_iter().next()),
        _ => {
            println!(
                "Ambiguous identifier '{}' matches {} indexes:",
                identifier,
                matched.len()
            );
            for (_, info) in &matched {
                println!(
                    "  {} ({})",
                    shorten_path(info.workspace.as_deref().unwrap_or(&info.hash)),
                    info.hash
                );
            }
            println!("\nUse the full hash to remove a specific index.");
            Ok(None)
        }
    }
}

/// Remove a specific index by hash or workspace path
pub fn remove(identifier: &str, dry_run: bool, assume_yes: bool) -> Result<()> {
    let indexes_dir = get_indexes_dir()?;

    if !indexes_dir.exists() {
        println!("No indexes found.");
        return Ok(());
    }

    let Some((path, info)) = resolve_removal_target(&indexes_dir, identifier)? else {
        return Ok(());
    };

    let label = shorten_path(info.workspace.as_deref().unwrap_or(&info.hash));
    let size = format_size(info.size_bytes);

    if dry_run {
        println!("Would remove index: {} ({})", label, size);
        println!("  index directory: {}", path.display());
        return Ok(());
    }

    if !assume_yes {
        println!("Index: {} ({})", label, size);
        println!("  index directory: {}", path.display());
        if !confirm("Remove this index?")? {
            println!("Aborted.");
            return Ok(());
        }
    }

    remove_index_dir(&indexes_dir, &path)?;
    println!("Removed index: {} ({})", label, size);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Build an indexes dir containing one index for `workspace`, and return both paths.
    fn fixture(hash: &str) -> (TempDir, PathBuf, PathBuf) {
        let root = TempDir::new().unwrap();
        let indexes_dir = root.path().join("indexes");
        let workspace = root.path().join("workspace");
        fs::create_dir_all(&indexes_dir).unwrap();
        fs::create_dir_all(workspace.join("src")).unwrap();
        fs::write(workspace.join("src/main.rs"), "fn main() {}").unwrap();

        let index_path = indexes_dir.join(hash);
        fs::create_dir_all(&index_path).unwrap();
        fs::write(
            index_path.join("workspace.json"),
            serde_json::json!({
                "workspace": fs::canonicalize(&workspace).unwrap(),
                "semantic": false,
                "files_indexed": 1,
            })
            .to_string(),
        )
        .unwrap();

        (root, indexes_dir, workspace)
    }

    #[test]
    fn bare_component_accepts_a_hash() {
        assert!(is_bare_component("8583a10179ed36ba"));
        assert!(is_bare_component("some-index"));
    }

    #[test]
    // The assertion below joins an absolute path on purpose, to pin down the exact
    // std behaviour that caused issue #13.
    #[allow(clippy::join_absolute_paths)]
    fn bare_component_rejects_anything_that_escapes() {
        // The absolute-path case is issue #13: `Path::join` throws away its base.
        assert!(!is_bare_component("/Users/someone/Developer"));
        assert!(!is_bare_component(".."));
        assert!(!is_bare_component("../../etc"));
        assert!(!is_bare_component("./Developer"));
        assert!(!is_bare_component("a/b"));
        assert!(!is_bare_component(""));

        assert_eq!(
            Path::new("/tmp/indexes").join("/Users/someone/Developer"),
            Path::new("/Users/someone/Developer"),
            "join discards its base for absolute paths — the reason the guard exists"
        );
    }

    #[test]
    fn issue_13_absolute_workspace_path_never_resolves_to_the_workspace() {
        let (_root, indexes_dir, workspace) = fixture("8583a10179ed36ba");
        let absolute = fs::canonicalize(&workspace).unwrap();

        let (path, info) = resolve_removal_target(&indexes_dir, absolute.to_str().unwrap())
            .unwrap()
            .expect("the indexed workspace path should resolve to its index");

        assert_eq!(path, indexes_dir.join("8583a10179ed36ba"));
        assert!(path.starts_with(&indexes_dir));
        assert_ne!(path, absolute);
        assert_eq!(info.hash, "8583a10179ed36ba");
    }

    #[test]
    fn issue_13_unindexed_directory_resolves_to_nothing() {
        let root = TempDir::new().unwrap();
        let indexes_dir = root.path().join("indexes");
        let victim = root.path().join("Developer");
        fs::create_dir_all(&indexes_dir).unwrap();
        fs::create_dir_all(&victim).unwrap();
        fs::write(victim.join("precious.txt"), "uncommitted work").unwrap();

        let resolved = resolve_removal_target(&indexes_dir, victim.to_str().unwrap()).unwrap();

        assert!(resolved.is_none(), "a plain directory is not an index");
        assert!(
            victim.join("precious.txt").exists(),
            "workspace must survive"
        );
    }

    #[test]
    fn issue_13_parent_traversal_resolves_to_nothing() {
        let root = TempDir::new().unwrap();
        let indexes_dir = root.path().join("indexes");
        let sibling = root.path().join("sibling");
        fs::create_dir_all(&indexes_dir).unwrap();
        fs::create_dir_all(&sibling).unwrap();

        let resolved = resolve_removal_target(&indexes_dir, "../sibling").unwrap();

        assert!(resolved.is_none());
        assert!(sibling.exists());
    }

    #[test]
    fn hash_still_resolves_to_its_index() {
        let (_root, indexes_dir, _workspace) = fixture("8583a10179ed36ba");

        let (path, _) = resolve_removal_target(&indexes_dir, "8583a10179ed36ba")
            .unwrap()
            .expect("hash should resolve");

        assert_eq!(path, indexes_dir.join("8583a10179ed36ba"));
    }

    #[test]
    fn remove_index_dir_deletes_only_inside_the_indexes_dir() {
        let (_root, indexes_dir, _workspace) = fixture("8583a10179ed36ba");
        let index_path = indexes_dir.join("8583a10179ed36ba");

        remove_index_dir(&indexes_dir, &index_path).unwrap();

        assert!(!index_path.exists());
        assert!(indexes_dir.exists(), "the indexes dir itself must survive");
    }

    #[test]
    fn remove_index_dir_refuses_a_target_outside_the_indexes_dir() {
        let (_root, indexes_dir, workspace) = fixture("8583a10179ed36ba");

        let err = remove_index_dir(&indexes_dir, &workspace).unwrap_err();

        assert!(
            err.to_string().contains("Refusing to delete"),
            "unexpected error: {err}"
        );
        assert!(
            workspace.join("src/main.rs").exists(),
            "workspace must survive"
        );
    }

    #[test]
    fn remove_index_dir_refuses_the_indexes_dir_itself() {
        let (_root, indexes_dir, _workspace) = fixture("8583a10179ed36ba");

        let err = remove_index_dir(&indexes_dir, &indexes_dir).unwrap_err();

        assert!(
            err.to_string().contains("Refusing to delete"),
            "unexpected error: {err}"
        );
        assert!(indexes_dir.join("8583a10179ed36ba").exists());
    }

    #[test]
    fn remove_index_dir_refuses_a_symlink_that_points_outside() {
        let (_root, indexes_dir, workspace) = fixture("8583a10179ed36ba");
        let link = indexes_dir.join("sneaky");

        #[cfg(unix)]
        std::os::unix::fs::symlink(&workspace, &link).unwrap();
        #[cfg(not(unix))]
        return;

        let err = remove_index_dir(&indexes_dir, &link).unwrap_err();

        assert!(
            err.to_string().contains("Refusing to delete"),
            "unexpected error: {err}"
        );
        assert!(
            workspace.join("src/main.rs").exists(),
            "workspace must survive"
        );
    }
}
