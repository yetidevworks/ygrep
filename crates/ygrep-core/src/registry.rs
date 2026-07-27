//! Registry of every index ygrep has built.
//!
//! One place resolves where indexes live, reads their metadata, and deletes them
//! safely. The CLI, the dashboard, and the background service all read the same
//! records so they never disagree about what is indexed or where it sits.

use std::fs;
use std::path::{Component, Path, PathBuf};

use chrono::{DateTime, Utc};

use crate::error::{Result, YgrepError};
use crate::Config;

/// Resolve the data directory for a workspace root.
///
/// 1. Auto-detect: a `.ygrep/` directory in the root
/// 2. Relative `data_dir` in config: resolved against the root
/// 3. Absolute `data_dir` from config: used as-is
pub fn data_dir_for(root: &Path, config: &Config) -> PathBuf {
    let local_ygrep = root.join(".ygrep");
    if local_ygrep.is_dir() {
        local_ygrep
    } else if config.indexer.data_dir.is_relative() {
        root.join(&config.indexer.data_dir)
    } else {
        config.indexer.data_dir.clone()
    }
}

/// Resolve the indexes directory for a workspace root.
pub fn indexes_dir_for(root: &Path, config: &Config) -> PathBuf {
    data_dir_for(root, config).join("indexes")
}

/// Data directory for the current working directory.
pub fn data_dir(config: &Config) -> Result<PathBuf> {
    Ok(data_dir_for(&std::env::current_dir()?, config))
}

/// Indexes directory for the current working directory.
pub fn indexes_dir(config: &Config) -> Result<PathBuf> {
    Ok(indexes_dir_for(&std::env::current_dir()?, config))
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
pub fn ensure_within_indexes_dir(indexes_dir: &Path, target: &Path) -> Result<()> {
    let root = fs::canonicalize(indexes_dir).map_err(|e| {
        YgrepError::Registry(format!(
            "Failed to resolve indexes directory {}: {}",
            indexes_dir.display(),
            e
        ))
    })?;
    let resolved = fs::canonicalize(target).map_err(|e| {
        YgrepError::Registry(format!("Failed to resolve {}: {}", target.display(), e))
    })?;

    if resolved == root || !resolved.starts_with(&root) {
        return Err(YgrepError::Registry(format!(
            "Refusing to delete {}: it is not inside the ygrep index directory ({}).\n\
             This is a bug — please report it at https://github.com/yetidevworks/ygrep/issues",
            resolved.display(),
            root.display()
        )));
    }

    Ok(())
}

/// Delete an index directory, but only after proving it lives inside the indexes directory.
///
/// Every deletion of an index goes through here.
pub fn remove_index_dir(indexes_dir: &Path, target: &Path) -> Result<()> {
    ensure_within_indexes_dir(indexes_dir, target)?;
    fs::remove_dir_all(target).map_err(|e| {
        YgrepError::Registry(format!(
            "Failed to remove index at {}: {}",
            target.display(),
            e
        ))
    })
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
    /// Persisted watch flag: the background service watches this index on login
    pub watch: bool,
    /// Live segment count, `None` when the index has no readable metadata
    pub segments: Option<usize>,
}

impl IndexInfo {
    /// Display label for this index: the workspace path when known, else the hash.
    pub fn label(&self) -> &str {
        self.workspace.as_deref().unwrap_or(&self.hash)
    }

    /// Persist the watch flag for this index and update the in-memory copy.
    pub fn set_watch(&mut self, enabled: bool) -> Result<()> {
        set_watch_flag(&self.path, enabled)?;
        self.watch = enabled;
        Ok(())
    }
}

/// Read index info from a directory
pub fn read_index_info(hash: &str, index_path: &Path) -> Result<IndexInfo> {
    let json = read_metadata(index_path);

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
    let watch = json
        .as_ref()
        .and_then(|v| v.get("watch").and_then(|w| w.as_bool()))
        .unwrap_or(false);

    let orphaned = match &workspace {
        Some(ws) => !PathBuf::from(ws).exists(),
        None => true,
    };

    Ok(IndexInfo {
        hash: hash.to_string(),
        path: index_path.to_path_buf(),
        workspace,
        size_bytes: dir_size(index_path).unwrap_or(0),
        semantic,
        files_indexed,
        indexed_at,
        orphaned,
        watch,
        segments: crate::index::segment_count(index_path),
    })
}

/// Collect every index recorded under the current working directory's data dir.
pub fn collect_indexes() -> Result<Vec<IndexInfo>> {
    let config = Config::load();
    collect_indexes_in(&indexes_dir(&config)?)
}

/// Collect every index stored under `indexes_dir`.
///
/// Directories without a `workspace.json` are skipped: they are half-built indexes,
/// not registry entries.
pub fn collect_indexes_in(indexes_dir: &Path) -> Result<Vec<IndexInfo>> {
    if !indexes_dir.exists() {
        return Ok(Vec::new());
    }

    let mut indexes = Vec::new();

    for entry in fs::read_dir(indexes_dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() || !path.join("workspace.json").exists() {
            continue;
        }
        if let Some(hash) = path.file_name().and_then(|n| n.to_str()) {
            if let Ok(info) = read_index_info(hash, &path) {
                indexes.push(info);
            }
        }
    }

    Ok(indexes)
}

/// Calculate directory size recursively
pub fn dir_size(path: &Path) -> Result<u64> {
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

/// Outcome of resolving an identifier to an index.
///
/// Ambiguity is returned rather than reported, so the caller decides whether to print
/// a list, open a picker, or fail.
#[derive(Debug, Clone)]
pub enum IndexMatch {
    /// Nothing matched
    None,
    /// Exactly one index matched
    One(IndexInfo),
    /// More than one index matched
    Ambiguous(Vec<IndexInfo>),
}

/// Resolve an identifier to a registered index.
///
/// The identifier is an index hash or a workspace path; `None` means "the index for the
/// current directory".
pub fn find_index(identifier: Option<&str>) -> Result<IndexMatch> {
    let indexes = collect_indexes()?;
    Ok(match_index(indexes, identifier))
}

/// Resolve an identifier against an already-collected list of indexes.
pub fn match_index(indexes: Vec<IndexInfo>, identifier: Option<&str>) -> IndexMatch {
    if indexes.is_empty() {
        return IndexMatch::None;
    }

    if let Some(identifier) = identifier {
        if let Some(info) = indexes.iter().find(|info| info.hash == identifier) {
            return IndexMatch::One(info.clone());
        }

        let target_path = fs::canonicalize(identifier).ok();
        let mut matches: Vec<_> = indexes
            .into_iter()
            .filter(|info| workspace_matches(info, identifier, target_path.as_deref()))
            .collect();

        return match matches.len() {
            0 => IndexMatch::None,
            1 => IndexMatch::One(matches.remove(0)),
            _ => IndexMatch::Ambiguous(matches),
        };
    }

    let cwd = std::env::current_dir()
        .ok()
        .and_then(|path| fs::canonicalize(path).ok());
    if let Some(cwd) = cwd {
        if let Some(info) = indexes.iter().find(|info| {
            info.workspace
                .as_ref()
                .map(|workspace| PathBuf::from(workspace) == cwd)
                .unwrap_or(false)
        }) {
            return IndexMatch::One(info.clone());
        }
    }

    IndexMatch::None
}

fn workspace_matches(info: &IndexInfo, identifier: &str, target_path: Option<&Path>) -> bool {
    match (&info.workspace, target_path) {
        (Some(ws), Some(target)) => Path::new(ws) == target,
        (Some(ws), None) => ws.contains(identifier),
        _ => false,
    }
}

/// Resolve an identifier (index hash or workspace path) to the index directory it names.
///
/// Unlike [`find_index`] this also finds index directories with no readable metadata, so
/// a half-written index can still be removed. Nothing is deleted here — resolution
/// happens before any destructive step so the caller can report or confirm the real
/// target first.
pub fn resolve_index_target(indexes_dir: &Path, identifier: &str) -> Result<IndexMatch> {
    // Hash form. An index hash is always a single path component, so requiring one here
    // keeps the join from resolving anywhere but inside the indexes directory.
    if is_bare_component(identifier) {
        let index_path = indexes_dir.join(identifier);
        if index_path.is_dir() {
            return Ok(IndexMatch::One(read_index_info(identifier, &index_path)?));
        }
    }

    // Workspace-path form: match the recorded workspace of each index, never the
    // identifier as a filesystem location.
    let target_path = fs::canonicalize(identifier).ok();

    let mut matched: Vec<IndexInfo> = Vec::new();

    for entry in fs::read_dir(indexes_dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if let Some(hash) = path.file_name().and_then(|n| n.to_str()) {
            if let Ok(info) = read_index_info(hash, &path) {
                if workspace_matches(&info, identifier, target_path.as_deref()) {
                    matched.push(info);
                }
            }
        }
    }

    Ok(match matched.len() {
        0 => IndexMatch::None,
        1 => IndexMatch::One(matched.remove(0)),
        _ => IndexMatch::Ambiguous(matched),
    })
}

/// Read the `workspace.json` metadata for an index.
pub fn read_metadata(index_path: &Path) -> Option<serde_json::Value> {
    fs::read_to_string(index_path.join("workspace.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
}

/// Merge `updates` into an index's `workspace.json`, preserving every other field.
///
/// Indexing rewrites its own counters on every pass; anything else stored alongside them
/// (the watch flag, fields a newer ygrep wrote) has to survive that.
pub fn update_metadata(index_path: &Path, updates: serde_json::Value) -> Result<()> {
    let mut metadata = match read_metadata(index_path) {
        Some(serde_json::Value::Object(map)) => map,
        _ => serde_json::Map::new(),
    };

    if let serde_json::Value::Object(updates) = updates {
        for (key, value) in updates {
            metadata.insert(key, value);
        }
    }

    let path = index_path.join("workspace.json");
    let body = serde_json::to_string_pretty(&serde_json::Value::Object(metadata))
        .map_err(|e| YgrepError::Registry(format!("Failed to encode index metadata: {}", e)))?;
    fs::write(&path, body).map_err(|e| {
        YgrepError::Registry(format!(
            "Failed to write index metadata at {}: {}",
            path.display(),
            e
        ))
    })
}

/// Whether the background service should watch this index.
pub fn watch_enabled(index_path: &Path) -> bool {
    read_metadata(index_path)
        .and_then(|v| v.get("watch").and_then(|w| w.as_bool()))
        .unwrap_or(false)
}

/// Persist the watch flag for an index.
pub fn set_watch_flag(index_path: &Path, enabled: bool) -> Result<()> {
    if !index_path.join("workspace.json").exists() {
        return Err(YgrepError::Registry(format!(
            "No index metadata at {}",
            index_path.display()
        )));
    }

    update_metadata(index_path, serde_json::json!({ "watch": enabled }))
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
    let duration = Utc::now().signed_duration_since(dt);

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

    fn single(m: IndexMatch) -> IndexInfo {
        match m {
            IndexMatch::One(info) => info,
            other => panic!("expected exactly one match, got {:?}", other),
        }
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

        let info = single(resolve_index_target(&indexes_dir, absolute.to_str().unwrap()).unwrap());

        assert_eq!(info.path, indexes_dir.join("8583a10179ed36ba"));
        assert!(info.path.starts_with(&indexes_dir));
        assert_ne!(info.path, absolute);
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

        let resolved = resolve_index_target(&indexes_dir, victim.to_str().unwrap()).unwrap();

        assert!(
            matches!(resolved, IndexMatch::None),
            "a plain directory is not an index"
        );
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

        let resolved = resolve_index_target(&indexes_dir, "../sibling").unwrap();

        assert!(matches!(resolved, IndexMatch::None));
        assert!(sibling.exists());
    }

    #[test]
    fn hash_still_resolves_to_its_index() {
        let (_root, indexes_dir, _workspace) = fixture("8583a10179ed36ba");

        let info = single(resolve_index_target(&indexes_dir, "8583a10179ed36ba").unwrap());

        assert_eq!(info.path, indexes_dir.join("8583a10179ed36ba"));
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

    #[test]
    fn collect_indexes_reads_the_watch_flag() {
        let (_root, indexes_dir, _workspace) = fixture("8583a10179ed36ba");
        let index_path = indexes_dir.join("8583a10179ed36ba");

        let indexes = collect_indexes_in(&indexes_dir).unwrap();
        assert_eq!(indexes.len(), 1);
        assert!(!indexes[0].watch, "the flag defaults to off");

        set_watch_flag(&index_path, true).unwrap();
        assert!(watch_enabled(&index_path));

        let indexes = collect_indexes_in(&indexes_dir).unwrap();
        assert!(indexes[0].watch);
    }

    #[test]
    fn updating_metadata_keeps_the_watch_flag_and_unknown_fields() {
        let (_root, indexes_dir, _workspace) = fixture("8583a10179ed36ba");
        let index_path = indexes_dir.join("8583a10179ed36ba");

        set_watch_flag(&index_path, true).unwrap();
        update_metadata(&index_path, serde_json::json!({ "from_the_future": 42 })).unwrap();

        update_metadata(
            &index_path,
            serde_json::json!({ "files_indexed": 99, "semantic": true }),
        )
        .unwrap();

        let metadata = read_metadata(&index_path).unwrap();
        assert_eq!(metadata["watch"], serde_json::json!(true));
        assert_eq!(metadata["from_the_future"], serde_json::json!(42));
        assert_eq!(metadata["files_indexed"], serde_json::json!(99));
        assert_eq!(metadata["semantic"], serde_json::json!(true));
    }

    #[test]
    fn set_watch_flag_refuses_a_directory_that_is_not_an_index() {
        let root = TempDir::new().unwrap();
        assert!(set_watch_flag(root.path(), true).is_err());
    }

    #[test]
    fn format_size_scales() {
        assert_eq!(format_size(512), "512B");
        assert_eq!(format_size(2 * 1024), "2K");
        assert_eq!(format_size(3 * 1024 * 1024), "3M");
    }
}
