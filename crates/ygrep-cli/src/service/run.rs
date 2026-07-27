//! The headless watch loop behind `ygrep service run`.
//!
//! It holds the single-instance lock, watches every index whose persisted watch flag is
//! on, and re-reads the registry on a timer so a flag toggled from the CLI or the TUI
//! takes effect without any IPC.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use ygrep_core::dashboard::{
    ManagerCommand, ManagerEvent, WatchManager, WatchState, WorkspaceRegistration,
};
use ygrep_core::registry::{self, IndexInfo};
use ygrep_core::Config;

use super::lock::{lock_path, InstanceLock};
use super::log::ServiceLog;
use super::state::{self, ServiceState};

/// How often batched file events are summarised into the log.
const ACTIVITY_FLUSH: Duration = Duration::from_secs(5);

/// Floor for the registry rescan, so a misconfigured interval cannot spin.
const MIN_RESCAN_SECS: u64 = 5;

/// How long to wait for watchers to stop before giving up on a clean shutdown.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(15);

/// What a rescan of the registry has to change.
///
/// Split out from the loop so the decision is a pure comparison of two maps of
/// hash -> "should this be watched", and can be tested without a filesystem.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RescanPlan {
    /// Indexes the manager has never seen
    pub added: Vec<String>,
    /// Known indexes whose watch flag was turned on
    pub enabled: Vec<String>,
    /// Known indexes whose watch flag was turned off
    pub disabled: Vec<String>,
    /// Indexes that no longer exist in the registry
    pub removed: Vec<String>,
}

impl RescanPlan {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty()
            && self.enabled.is_empty()
            && self.disabled.is_empty()
            && self.removed.is_empty()
    }
}

/// Compare what the service is managing against what the registry now says.
///
/// `current` maps a registered index to whether the service asked for it to be watched;
/// `wanted` is the same question answered by the registry on this pass.
pub fn plan_rescan(
    current: &BTreeMap<String, bool>,
    wanted: &BTreeMap<String, bool>,
) -> RescanPlan {
    let mut plan = RescanPlan::default();

    for (hash, should_watch) in wanted {
        match current.get(hash) {
            None => plan.added.push(hash.clone()),
            Some(watching) if watching == should_watch => {}
            Some(_) if *should_watch => plan.enabled.push(hash.clone()),
            Some(_) => plan.disabled.push(hash.clone()),
        }
    }

    for hash in current.keys() {
        if !wanted.contains_key(hash) {
            plan.removed.push(hash.clone());
        }
    }

    plan
}

/// Run the watch service in the foreground until it is signalled to stop.
pub fn run() -> Result<()> {
    let config = Config::load();
    let data_dir = super::data_dir(&config)?;
    std::fs::create_dir_all(&data_dir)
        .with_context(|| format!("Failed to create {}", data_dir.display()))?;

    let lock = InstanceLock::acquire(&lock_path(&data_dir))?;

    let mut log = ServiceLog::open(
        super::log_path_in(&data_dir),
        config.service.log_max_size_mb,
    )
    .with_context(|| format!("Failed to open the service log in {}", data_dir.display()))?;

    let rescan_secs = config.service.registry_rescan_secs.max(MIN_RESCAN_SECS);
    log.write(&format!(
        "ygrep {} service starting (pid {}, data dir {}, rescan {}s, log cap {}MB)",
        env!("CARGO_PKG_VERSION"),
        std::process::id(),
        data_dir.display(),
        rescan_secs,
        config.service.log_max_size_mb
    ));

    let runtime = tokio::runtime::Runtime::new().context("Failed to start the async runtime")?;
    let result = runtime.block_on(serve(&config, &data_dir, rescan_secs, &mut log));

    if let Err(ref e) = result {
        log.write(&format!("service failed: {e:#}"));
    }

    state::clear(&data_dir);
    log.write("service stopped");
    drop(lock);

    result
}

/// A workspace worth handing to the manager: the index still points at a real directory.
fn registration(info: &IndexInfo) -> Option<WorkspaceRegistration> {
    let workspace = info.workspace.as_ref()?;
    let workspace_path = PathBuf::from(workspace);
    if info.orphaned || !workspace_path.exists() {
        return None;
    }

    Some(WorkspaceRegistration {
        hash: info.hash.clone(),
        workspace_path,
        semantic: info.semantic.unwrap_or(false),
        indexed_at: info.indexed_at,
        watch: info.watch,
    })
}

/// Count with the right noun for the log line.
fn plural(count: usize, one: &str, many: &str) -> String {
    format!("{} {}", count, if count == 1 { one } else { many })
}

/// Short label for the log: the last component of the workspace path.
fn short_label(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| path.display().to_string())
}

/// Read the registry and reduce it to "hash -> should the service watch this".
fn scan(indexes_dir: &Path) -> Result<(Vec<IndexInfo>, BTreeMap<String, bool>)> {
    let indexes = registry::collect_indexes_in(indexes_dir)?;
    let wanted = indexes
        .iter()
        .filter(|info| registration(info).is_some())
        .map(|info| (info.hash.clone(), info.watch))
        .collect();
    Ok((indexes, wanted))
}

async fn serve(
    config: &Config,
    data_dir: &Path,
    rescan_secs: u64,
    log: &mut ServiceLog,
) -> Result<()> {
    let indexes_dir = data_dir.join("indexes");
    let (mut manager, cmd_tx, mut event_rx) = WatchManager::new();
    // The service watches exactly what the persisted flag says — a workspace indexed an
    // hour ago is not a request to keep watching it forever.
    manager.set_auto_watch_recent(false);

    let (indexes, mut current) = scan(&indexes_dir)?;
    let mut labels: HashMap<String, String> = HashMap::new();

    for info in &indexes {
        let Some(reg) = registration(info) else {
            continue;
        };
        labels.insert(reg.hash.clone(), short_label(&reg.workspace_path));
        if reg.watch {
            log.write(&format!(
                "watching {} ({})",
                reg.workspace_path.display(),
                reg.hash
            ));
        }
        manager.register(
            reg.hash,
            reg.workspace_path,
            reg.semantic,
            reg.indexed_at,
            reg.watch,
        );
    }

    let watched = current.values().filter(|watch| **watch).count();
    log.write(&format!(
        "registered {}, watching {}",
        plural(current.len(), "index", "indexes"),
        watched
    ));
    if watched == 0 {
        log.write("nothing to watch — enable one with `ygrep indexes watch <id> on`");
    }

    let manager_task = tokio::spawn(manager.run());
    state::write(
        data_dir,
        &ServiceState::new(data_dir, &current, rescan_secs),
    );

    let mut rescan = tokio::time::interval(Duration::from_secs(rescan_secs));
    rescan.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    rescan.tick().await; // the first tick is immediate

    let mut flush = tokio::time::interval(ACTIVITY_FLUSH);
    flush.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut activity = Activity::default();
    let mut shutdown = Shutdown::new();

    let signal = loop {
        tokio::select! {
            Some(event) = event_rx.recv() => {
                activity.record(event, &labels, log);
            }
            _ = flush.tick() => {
                activity.flush(&labels, log);
            }
            _ = rescan.tick() => {
                let (indexes, next) = match scan(&indexes_dir) {
                    Ok(scanned) => scanned,
                    Err(e) => {
                        log.write(&format!("registry rescan failed: {e}"));
                        continue;
                    }
                };

                let plan = plan_rescan(&current, &next);
                if !plan.is_empty() {
                    apply(&plan, &indexes, &cmd_tx, &mut current, &mut labels, log);
                }
                state::write(data_dir, &ServiceState::new(data_dir, &current, rescan_secs));
            }
            signal = shutdown.recv() => break signal,
        }
    };

    log.write(&format!("{signal} received, stopping watchers"));
    activity.flush(&labels, log);

    let _ = cmd_tx.send(ManagerCommand::Shutdown);
    if tokio::time::timeout(SHUTDOWN_TIMEOUT, manager_task)
        .await
        .is_err()
    {
        log.write("watchers did not stop in time");
    }

    compact_watched(config, &indexes_dir, &current, &labels, log).await;

    Ok(())
}

/// Send the manager everything a rescan turned up, and record what it now manages.
fn apply(
    plan: &RescanPlan,
    indexes: &[IndexInfo],
    cmd_tx: &tokio::sync::mpsc::UnboundedSender<ManagerCommand>,
    current: &mut BTreeMap<String, bool>,
    labels: &mut HashMap<String, String>,
    log: &mut ServiceLog,
) {
    for hash in &plan.added {
        let Some(reg) = indexes
            .iter()
            .find(|info| &info.hash == hash)
            .and_then(registration)
        else {
            continue;
        };
        labels.insert(hash.clone(), short_label(&reg.workspace_path));
        log.write(&format!(
            "new index {} ({}), watch {}",
            reg.workspace_path.display(),
            hash,
            if reg.watch { "on" } else { "off" }
        ));
        current.insert(hash.clone(), reg.watch);
        let _ = cmd_tx.send(ManagerCommand::Register(reg));
    }

    for hash in &plan.enabled {
        log.write(&format!("watch enabled for {}", label_of(labels, hash)));
        current.insert(hash.clone(), true);
        let _ = cmd_tx.send(ManagerCommand::SetWatch {
            hash: hash.clone(),
            enabled: true,
        });
    }

    for hash in &plan.disabled {
        log.write(&format!("watch disabled for {}", label_of(labels, hash)));
        current.insert(hash.clone(), false);
        let _ = cmd_tx.send(ManagerCommand::SetWatch {
            hash: hash.clone(),
            enabled: false,
        });
    }

    for hash in &plan.removed {
        log.write(&format!("index gone: {}", label_of(labels, hash)));
        current.remove(hash);
        labels.remove(hash);
        let _ = cmd_tx.send(ManagerCommand::RemoveIndex(hash.clone()));
    }
}

fn label_of(labels: &HashMap<String, String>, hash: &str) -> String {
    match labels.get(hash) {
        Some(label) => format!("{label} ({hash})"),
        None => hash.to_string(),
    }
}

/// Merge segments on the way out for any watched index that accumulated enough of them.
/// Watch commits never merge, so this is where a long-running service gives the space back.
async fn compact_watched(
    config: &Config,
    indexes_dir: &Path,
    current: &BTreeMap<String, bool>,
    labels: &HashMap<String, String>,
    log: &mut ServiceLog,
) {
    let threshold = config.indexer.auto_compact_segments;
    if threshold == 0 {
        return;
    }

    for hash in current
        .iter()
        .filter(|(_, watching)| **watching)
        .map(|(hash, _)| hash)
    {
        let index_path = indexes_dir.join(hash);
        if !ygrep_core::index::compaction_due(&index_path, threshold) {
            continue;
        }

        let compacted = tokio::task::spawn_blocking(move || {
            ygrep_core::index::auto_compact(&index_path, threshold)
        })
        .await
        .ok()
        .flatten();

        if let Some(stats) = compacted {
            log.write(&format!(
                "compacted {}: {} segments into {}",
                label_of(labels, hash),
                stats.segments_before,
                stats.segments_after
            ));
        }
    }
}

/// Per-workspace counters, summarised into one line rather than one line per file.
#[derive(Default)]
struct Activity {
    indexed: BTreeMap<String, usize>,
    deleted: BTreeMap<String, usize>,
}

impl Activity {
    fn record(
        &mut self,
        event: ManagerEvent,
        labels: &HashMap<String, String>,
        log: &mut ServiceLog,
    ) {
        match event {
            ManagerEvent::FileIndexed { hash, .. } => {
                *self.indexed.entry(hash).or_default() += 1;
            }
            ManagerEvent::FileDeleted { hash, .. } => {
                *self.deleted.entry(hash).or_default() += 1;
            }
            ManagerEvent::WatchStateChanged { hash, new_state } => {
                // Sleeping is the manager's own idle handling, not something a reader of
                // the log needs a line for every five minutes.
                if new_state != WatchState::Sleeping {
                    log.write(&format!("{} is now {}", label_of(labels, &hash), new_state));
                }
            }
            ManagerEvent::ReindexStarted { hash } => {
                log.write(&format!("re-indexing {}", label_of(labels, &hash)));
            }
            ManagerEvent::ReindexCompleted {
                hash,
                files_indexed,
            } => {
                log.write(&format!(
                    "re-indexed {} ({} files)",
                    label_of(labels, &hash),
                    files_indexed
                ));
            }
            ManagerEvent::ReindexFailed { hash, message } => {
                log.write(&format!(
                    "re-index of {} failed: {}",
                    label_of(labels, &hash),
                    message
                ));
            }
            ManagerEvent::IndexRemoved { hash } => {
                log.write(&format!("stopped watching {}", label_of(labels, &hash)));
            }
            ManagerEvent::Error { hash, message } => {
                log.write(&format!("error {}: {}", label_of(labels, &hash), message));
            }
            ManagerEvent::Log { hash, message } => {
                let message = message.trim();
                if !message.is_empty() {
                    log.write(&format!("{}: {}", label_of(labels, &hash), message));
                }
            }
        }
    }

    /// Write one summary line per workspace that saw changes, then reset.
    fn flush(&mut self, labels: &HashMap<String, String>, log: &mut ServiceLog) {
        let hashes: Vec<String> = self
            .indexed
            .keys()
            .chain(self.deleted.keys())
            .cloned()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();

        for hash in hashes {
            let indexed = self.indexed.get(&hash).copied().unwrap_or(0);
            let deleted = self.deleted.get(&hash).copied().unwrap_or(0);
            let mut parts = Vec::new();
            if indexed > 0 {
                parts.push(format!("{indexed} indexed"));
            }
            if deleted > 0 {
                parts.push(format!("{deleted} removed"));
            }
            log.write(&format!(
                "{}: {}",
                label_of(labels, &hash),
                parts.join(", ")
            ));
        }

        self.indexed.clear();
        self.deleted.clear();
    }
}

/// Resolves when the service is asked to stop.
///
/// The handlers are registered once and kept, so a signal that lands while the loop is
/// busy elsewhere is still delivered on the next poll rather than being missed.
struct Shutdown {
    #[cfg(unix)]
    terminate: Option<tokio::signal::unix::Signal>,
    #[cfg(unix)]
    interrupt: Option<tokio::signal::unix::Signal>,
}

impl Shutdown {
    fn new() -> Self {
        #[cfg(unix)]
        use tokio::signal::unix::{signal, SignalKind};

        Self {
            #[cfg(unix)]
            terminate: signal(SignalKind::terminate()).ok(),
            #[cfg(unix)]
            interrupt: signal(SignalKind::interrupt()).ok(),
        }
    }

    async fn recv(&mut self) -> &'static str {
        #[cfg(unix)]
        if let (Some(terminate), Some(interrupt)) =
            (self.terminate.as_mut(), self.interrupt.as_mut())
        {
            return tokio::select! {
                _ = terminate.recv() => "SIGTERM",
                _ = interrupt.recv() => "SIGINT",
            };
        }

        let _ = tokio::signal::ctrl_c().await;
        "SIGINT"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(entries: &[(&str, bool)]) -> BTreeMap<String, bool> {
        entries
            .iter()
            .map(|(hash, watch)| (hash.to_string(), *watch))
            .collect()
    }

    #[test]
    fn an_unchanged_registry_plans_nothing() {
        let current = map(&[("a", true), ("b", false)]);
        let plan = plan_rescan(&current, &current.clone());

        assert!(plan.is_empty());
        assert_eq!(plan, RescanPlan::default());
    }

    #[test]
    fn a_new_index_is_added_whether_or_not_it_is_watched() {
        let current = map(&[("a", true)]);
        let wanted = map(&[("a", true), ("b", true), ("c", false)]);

        let plan = plan_rescan(&current, &wanted);

        assert_eq!(plan.added, vec!["b".to_string(), "c".to_string()]);
        assert!(plan.enabled.is_empty());
        assert!(plan.removed.is_empty());
    }

    #[test]
    fn toggling_the_flag_enables_and_disables_a_known_index() {
        let current = map(&[("a", false), ("b", true)]);
        let wanted = map(&[("a", true), ("b", false)]);

        let plan = plan_rescan(&current, &wanted);

        assert_eq!(plan.enabled, vec!["a".to_string()]);
        assert_eq!(plan.disabled, vec!["b".to_string()]);
        assert!(plan.added.is_empty());
        assert!(plan.removed.is_empty());
    }

    #[test]
    fn an_index_that_left_the_registry_is_removed() {
        let current = map(&[("a", true), ("b", false)]);
        let wanted = map(&[("a", true)]);

        let plan = plan_rescan(&current, &wanted);

        assert_eq!(plan.removed, vec!["b".to_string()]);
        assert!(plan.added.is_empty());
        assert!(plan.disabled.is_empty());
    }

    #[test]
    fn a_deleted_workspace_stops_being_watched() {
        // An orphaned index drops out of `wanted` entirely, which reads as removal.
        let current = map(&[("a", true)]);
        let plan = plan_rescan(&current, &BTreeMap::new());

        assert_eq!(plan.removed, vec!["a".to_string()]);
    }

    #[test]
    fn an_index_with_no_workspace_is_never_registered() {
        let info = IndexInfo {
            hash: "abc".into(),
            path: PathBuf::from("/data/indexes/abc"),
            workspace: None,
            size_bytes: 0,
            semantic: None,
            files_indexed: None,
            indexed_at: None,
            orphaned: true,
            watch: true,
            segments: None,
        };

        assert!(registration(&info).is_none());
    }

    #[test]
    fn an_orphaned_index_is_never_registered() {
        let info = IndexInfo {
            hash: "abc".into(),
            path: PathBuf::from("/data/indexes/abc"),
            workspace: Some("/nowhere/at/all".into()),
            size_bytes: 0,
            semantic: Some(false),
            files_indexed: Some(1),
            indexed_at: None,
            orphaned: true,
            watch: true,
            segments: None,
        };

        assert!(registration(&info).is_none());
    }
}
