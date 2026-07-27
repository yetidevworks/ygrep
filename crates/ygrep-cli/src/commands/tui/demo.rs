//! Demo mode: the whole TUI, every key, over fabricated data.
//!
//! `ygrep dashboard --demo` runs the real event loop on the same dataset the snapshot
//! renderer uses, with nothing behind it — no registry, no watchers, no service, no
//! telemetry, no writes. Every action answers with a plausible result instead of doing
//! the work, and each tick nudges the data along so a screenshot or a recording reads
//! like a session in progress.

use std::time::{Duration, Instant};

use chrono::Utc;

use ygrep_core::dashboard::WatchState;

use crate::service::{ServiceState, ServiceStatus};

use super::{synthetic_app, ActivityKind, App, Deferred, SERVICE_ACTIONS};

/// How long each simulated action pretends to take.
const REINDEX_DELAY: Duration = Duration::from_millis(900);
const COMPACT_DELAY: Duration = Duration::from_millis(650);
const REMOVE_DELAY: Duration = Duration::from_millis(400);
const SERVICE_DELAY: Duration = Duration::from_millis(700);

/// How often the fake service re-reads its fake registry.
const DEMO_RESCAN_SECS: u64 = 30;

/// A simulated action, folded into the fake state once its delay has passed.
pub enum DemoOp {
    Reindex {
        hash: String,
        files: u64,
    },
    Compact {
        hash: String,
        before: usize,
        after: usize,
    },
    Remove {
        hash: String,
    },
    Service {
        action: &'static str,
    },
}

/// The dashboard demo mode starts from, live from the first tick on.
pub fn demo_app() -> App {
    let mut app = synthetic_app();
    app.demo = true;
    let count = app.rows.len();
    app.note(format!("{count} indexes · press ? for keys"));
    app
}

impl App {
    /// xorshift64*, so the drift varies without pulling in an rng crate.
    fn demo_rand(&mut self) -> u64 {
        let mut x = self.demo_seed;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.demo_seed = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn demo_pick(&mut self, len: usize) -> usize {
        if len == 0 {
            return 0;
        }
        (self.demo_rand() % len as u64) as usize
    }

    fn demo_chance(&mut self, percent: u64) -> bool {
        self.demo_rand() % 100 < percent
    }

    /// Hashes of the indexes something is watching right now.
    fn demo_watched(&self) -> Vec<String> {
        self.rows
            .iter()
            .filter(|row| {
                !row.orphaned
                    && (row.state == WatchState::Active || self.watched_by_service(&row.hash))
            })
            .map(|row| row.hash.clone())
            .collect()
    }

    /// Poll-timeout work in demo mode: move the data instead of reading the world.
    pub(super) fn demo_tick(&mut self) {
        self.demo_ticks = self.demo_ticks.wrapping_add(1);
        let tick = self.demo_ticks;

        if tick.is_multiple_of(8) {
            self.demo_drift_rates();
        }
        if tick.is_multiple_of(5) {
            self.demo_file_event();
        }
        if tick.is_multiple_of(41) {
            self.demo_heartbeat();
        }
        if tick.is_multiple_of(67) {
            self.demo_sleep_wake();
        }
        if tick.is_multiple_of(97) {
            self.demo_compaction();
        }
        if let Some(stats) = self.stats.as_mut() {
            stats.demo_tick();
        }
    }

    /// Walk the changes-per-minute figures of the watched indexes.
    fn demo_drift_rates(&mut self) {
        for hash in self.demo_watched() {
            let step = (self.demo_rand() % 800) as f64 / 100.0 - 3.5;
            if let Some(row) = self.row_mut(&hash) {
                row.changes_per_min = (row.changes_per_min + step).clamp(0.0, 48.0);
            }
        }
    }

    /// Report a file landing in — or leaving — one of the watched workspaces.
    fn demo_file_event(&mut self) {
        let watched = self.demo_watched();
        if watched.is_empty() || !self.demo_chance(70) {
            return;
        }
        let hash = watched[self.demo_pick(watched.len())].clone();
        let name = self.name_of(&hash);
        let n = self.demo_rand() as usize;
        let path = demo_file(&name, n);

        if self.demo_chance(8) {
            self.push(ActivityKind::Deleted, name, format!("[-] {path}"));
            return;
        }
        let now = Utc::now();
        if let Some(row) = self.row_mut(&hash) {
            row.files += 1;
            row.size_bytes += 4_096 + (n as u64 % 9_000);
            row.indexed_at = Some(now);
        }
        self.push(ActivityKind::Indexed, name, format!("[+] {path}"));
    }

    /// Freshen the fake service heartbeat and log its rescan.
    fn demo_heartbeat(&mut self) {
        if !self.service_running {
            return;
        }
        let indexes = self.rows.len();
        let watched = self.service_watched.len();
        let now = Utc::now();
        if let Some(state) = self
            .service
            .as_mut()
            .and_then(|report| report.heartbeat.as_mut())
        {
            state.last_rescan = now;
            state.registered = indexes;
        }
        self.push(
            ActivityKind::Service,
            "service",
            format!("rescan: {indexes} indexes, {watched} watched"),
        );
    }

    /// Move one index between watching and sleeping, the way the idle timer would.
    fn demo_sleep_wake(&mut self) {
        let candidates: Vec<(String, WatchState)> = self
            .rows
            .iter()
            .filter(|row| {
                !row.orphaned && row.state != WatchState::Off && !self.watched_by_service(&row.hash)
            })
            .map(|row| (row.hash.clone(), row.state.clone()))
            .collect();
        if candidates.is_empty() {
            return;
        }
        let (hash, state) = candidates[self.demo_pick(candidates.len())].clone();
        let name = self.name_of(&hash);
        let (next, text) = if state == WatchState::Sleeping {
            (WatchState::Active, "watching")
        } else {
            (WatchState::Sleeping, "sleeping (idle)")
        };
        let sleeping = next == WatchState::Sleeping;
        if let Some(row) = self.row_mut(&hash) {
            row.state = next;
            if sleeping {
                row.changes_per_min = 0.0;
            }
        }
        self.push(ActivityKind::State, name, text);
    }

    /// Fold a segment-heavy index down, the way auto-compaction would.
    fn demo_compaction(&mut self) {
        let candidates: Vec<(String, usize)> = self
            .rows
            .iter()
            .filter_map(|row| {
                row.segments
                    .filter(|count| *count >= 6)
                    .map(|count| (row.hash.clone(), count))
            })
            .collect();
        if candidates.is_empty() {
            return;
        }
        let (hash, before) = candidates[self.demo_pick(candidates.len())].clone();
        let after = (before / 4).max(1);
        let name = self.name_of(&hash);
        if let Some(row) = self.row_mut(&hash) {
            row.segments = Some(after);
        }
        self.push(
            ActivityKind::Indexed,
            name,
            format!("compacted {before} segments into {after}"),
        );
    }

    /// Flip this session's fake watcher for one index.
    pub(super) fn demo_toggle_watch(&mut self, hash: &str, watching: bool) {
        let name = self.name_of(hash);
        let next = if watching {
            WatchState::Off
        } else {
            WatchState::Active
        };
        if let Some(row) = self.row_mut(hash) {
            row.state = next;
            if watching {
                row.changes_per_min = 0.0;
            }
        }
        self.push(
            ActivityKind::State,
            name.clone(),
            if watching { "watch off" } else { "watching" },
        );
        self.note(if watching {
            format!("stopped watching {name}")
        } else {
            format!("watching {name} for this session")
        });
    }

    /// Start a simulated re-index or compaction.
    pub(super) fn demo_launch(&mut self, op: Deferred, hash: &str) {
        let name = self.name_of(hash);
        let hash = hash.to_string();
        self.busy += 1;
        match op {
            Deferred::Reindex => {
                let grown = self.demo_rand() % 40 + 1;
                let files = self.row(&hash).map(|row| row.files).unwrap_or(0) + grown;
                self.push(ActivityKind::Reindex, name.clone(), "re-indexing…");
                self.note(format!("re-indexing {name}…"));
                self.defer_demo(REINDEX_DELAY, DemoOp::Reindex { hash, files });
            }
            Deferred::Compact => {
                let before = self
                    .row(&hash)
                    .and_then(|row| row.segments)
                    .unwrap_or(1)
                    .max(2);
                let after = (before / 4).max(1);
                self.push(ActivityKind::Reindex, name.clone(), "compacting…");
                self.note(format!("compacting {name}…"));
                self.defer_demo(
                    COMPACT_DELAY,
                    DemoOp::Compact {
                        hash,
                        before,
                        after,
                    },
                );
            }
        }
    }

    /// Drop one fake row, once the pretend delete has "run".
    pub(super) fn demo_remove(&mut self, hash: String) {
        let name = self.name_of(&hash);
        self.busy += 1;
        self.note(format!("removing {name}…"));
        self.defer_demo(REMOVE_DELAY, DemoOp::Remove { hash });
    }

    /// Run a service action against the fake service state.
    pub(super) fn demo_service_action(&mut self, index: usize) {
        let Some((action, _)) = SERVICE_ACTIONS.get(index) else {
            return;
        };
        let action = *action;
        self.busy += 1;
        self.note(format!("{action}ing the service…"));
        self.defer_demo(SERVICE_DELAY, DemoOp::Service { action });
    }

    fn defer_demo(&mut self, delay: Duration, op: DemoOp) {
        self.demo_pending.push((Instant::now() + delay, op));
    }

    /// Apply the simulated actions whose delay has passed. `force` ignores the delays.
    pub(super) fn drain_demo(&mut self, force: bool) {
        if self.demo_pending.is_empty() {
            return;
        }
        let now = Instant::now();
        let (ready, waiting): (Vec<_>, Vec<_>) = std::mem::take(&mut self.demo_pending)
            .into_iter()
            .partition(|(at, _)| force || *at <= now);
        self.demo_pending = waiting;
        for (_, op) in ready {
            self.apply_demo_op(op);
        }
    }

    fn apply_demo_op(&mut self, op: DemoOp) {
        self.busy = self.busy.saturating_sub(1);
        match op {
            DemoOp::Reindex { hash, files } => {
                let name = self.name_of(&hash);
                let now = Utc::now();
                if let Some(row) = self.row_mut(&hash) {
                    row.files = files;
                    row.indexed_at = Some(now);
                    row.segments = Some(1);
                }
                self.push(
                    ActivityKind::Reindex,
                    name.clone(),
                    format!("re-index complete ({files} files)"),
                );
                self.note(format!("✓ re-indexed {name} ({files} files)"));
            }
            DemoOp::Compact {
                hash,
                before,
                after,
            } => {
                let name = self.name_of(&hash);
                if let Some(row) = self.row_mut(&hash) {
                    row.segments = Some(after);
                    row.size_bytes = (row.size_bytes / 10) * 9;
                }
                let message = format!("compacted {name}: {before} segments into {after}");
                self.push(ActivityKind::Reindex, name, message.clone());
                self.act("compact", Ok(message));
            }
            DemoOp::Remove { hash } => {
                let name = self.name_of(&hash);
                self.rows.retain(|row| row.hash != hash);
                self.service_watched.remove(&hash);
                self.resort();
                let message = format!("removed the index for {name}");
                self.push(ActivityKind::Deleted, name, message.clone());
                self.act("remove", Ok(message));
            }
            DemoOp::Service { action } => {
                let message = self.demo_apply_service(action);
                self.push(ActivityKind::Service, "service", message.clone());
                self.act(&format!("service {action}"), Ok(message));
            }
        }
    }

    /// Rewrite the fake service state for one menu action, and describe what happened.
    fn demo_apply_service(&mut self, action: &str) -> String {
        let now = Utc::now();
        let flagged: Vec<String> = self
            .rows
            .iter()
            .filter(|row| row.watch && !row.orphaned)
            .map(|row| row.hash.clone())
            .collect();
        let registered = self.rows.iter().filter(|row| !row.orphaned).count();
        let pid = 4_000 + (self.demo_rand() % 900) as u32;

        match action {
            "install" | "start" | "restart" => {
                self.service_running = true;
                self.service_watched = flagged.iter().cloned().collect();
                let mut unit = String::new();
                if let Some(report) = self.service.as_mut() {
                    report.status = ServiceStatus::Installed {
                        running: true,
                        pid: Some(pid),
                        failed: false,
                    };
                    report.watch_enabled = flagged.len();
                    report.indexes = registered;
                    report.heartbeat = Some(ServiceState {
                        pid,
                        started_at: now,
                        last_rescan: now,
                        watched: flagged,
                        registered,
                        log: report.log_path.clone(),
                        rescan_secs: DEMO_RESCAN_SECS,
                    });
                    unit = report
                        .unit_path
                        .as_ref()
                        .map(|path| path.display().to_string())
                        .unwrap_or_default();
                }
                match action {
                    "install" => format!("installed the service ({unit})"),
                    "start" => "started the service".to_string(),
                    _ => "restarted the service".to_string(),
                }
            }
            "stop" => {
                self.service_running = false;
                self.service_watched.clear();
                if let Some(report) = self.service.as_mut() {
                    report.status = ServiceStatus::Installed {
                        running: false,
                        pid: None,
                        failed: false,
                    };
                    report.heartbeat = None;
                }
                "stopped the service".to_string()
            }
            "uninstall" => {
                self.service_running = false;
                self.service_watched.clear();
                if let Some(report) = self.service.as_mut() {
                    report.status = ServiceStatus::NotInstalled;
                    report.heartbeat = None;
                }
                "removed the service".to_string()
            }
            _ => String::new(),
        }
    }
}

/// A plausible file path inside one of the demo workspaces.
fn demo_file(name: &str, n: usize) -> &'static str {
    let files: &[&str] = match name {
        "ygrep" => &[
            "crates/ygrep-core/src/search/searcher.rs",
            "crates/ygrep-core/src/index/writer.rs",
            "crates/ygrep-cli/src/commands/tui/ui.rs",
            "crates/ygrep-core/src/registry.rs",
            "crates/ygrep-cli/src/commands/search.rs",
            "CHANGELOG.md",
        ],
        "grav" => &[
            "system/src/Grav/Common/Page/Page.php",
            "system/src/Grav/Framework/Flex/FlexDirectory.php",
            "system/templates/partials/base.html.twig",
            "system/blueprints/config/system.yaml",
        ],
        "reeve" => &[
            "crates/reeve/src/daemon/launchd.rs",
            "crates/reeve/src/site/resolver.rs",
            "docs/getting-started.md",
        ],
        "acme-monorepo-frontend" => &[
            "apps/web/src/routes/dashboard.tsx",
            "packages/ui/src/components/Button.tsx",
            "packages/api-client/src/generated/types.ts",
            "apps/web/src/lib/query-cache.ts",
        ],
        _ => &["notes.md", "index.html", "todo.txt"],
    };
    files[n % files.len()]
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyModifiers};

    use super::super::{handle_key, SortCol};
    use super::*;

    fn press(app: &mut App, code: KeyCode) {
        handle_key(app, code, KeyModifiers::NONE);
    }

    #[test]
    fn demo_mode_fakes_every_action_it_is_asked_for() {
        let mut app = demo_app();
        assert!(app.demo);

        // Work on the one index this session watches itself.
        app.sort_col = SortCol::Name;
        app.sort_asc = true;
        app.resort();
        let pos = app
            .view
            .iter()
            .position(|i| {
                app.rows[*i].state == WatchState::Active
                    && !app.watched_by_service(&app.rows[*i].hash)
            })
            .expect("the demo dashboard watches one index itself");
        app.sel = pos;
        let hash = app.rows[app.view[pos]].hash.clone();

        // Enter flips the fake watch state in place, both ways.
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.row(&hash).unwrap().state, WatchState::Off);
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.row(&hash).unwrap().state, WatchState::Active);

        // w flips the persisted flag with no registry behind it.
        let flag = app.row(&hash).unwrap().watch;
        press(&mut app, KeyCode::Char('w'));
        assert_eq!(app.row(&hash).unwrap().watch, !flag);

        // i answers like a finished re-index once its delay is skipped.
        let files = app.row(&hash).unwrap().files;
        press(&mut app, KeyCode::Char('i'));
        assert_eq!(app.busy, 1, "the fake re-index counts as in flight");
        app.drain_demo(true);
        assert_eq!(app.busy, 0);
        assert!(app.row(&hash).unwrap().files > files);
        assert!(app.message.contains("re-indexed"), "{}", app.message);

        // R opens the real modal and y drops the fake row.
        let rows = app.rows.len();
        press(&mut app, KeyCode::Char('R'));
        assert!(app.confirm_remove.is_some());
        press(&mut app, KeyCode::Char('y'));
        app.drain_demo(true);
        assert_eq!(app.rows.len(), rows - 1);
        assert!(app.rows.iter().all(|row| row.hash != hash));

        // The stats view opens on fabricated queries, ticks, and closes again.
        press(&mut app, KeyCode::Char('t'));
        assert!(app.stats.is_some());
        app.on_tick();
        press(&mut app, KeyCode::Char('t'));
        assert!(app.stats.is_none());

        // The service menu rewrites the fake service state.
        press(&mut app, KeyCode::Char('S'));
        assert_eq!(app.service_menu, Some(0));
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Enter);
        app.drain_demo(true);
        assert!(!app.service_running, "stop turned the fake service off");
        assert!(app.service_watched.is_empty());

        // The help overlay and the filter still behave.
        press(&mut app, KeyCode::Char('?'));
        assert!(app.help);
        press(&mut app, KeyCode::Esc);
        assert!(!app.help);
        press(&mut app, KeyCode::Char('/'));
        for c in "grav".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.view.len(), 1);

        assert!(!app.should_quit, "none of that may have quit the TUI");
    }

    #[test]
    fn a_demo_tick_keeps_the_data_moving() {
        let mut app = demo_app();
        let activity = app.activity.len();
        for _ in 0..200 {
            app.on_tick();
        }
        assert!(
            app.activity.len() > activity,
            "the demo has to keep reporting activity"
        );
        assert!(
            app.activity
                .iter()
                .rev()
                .take(5)
                .all(|line| line.at >= app.activity[0].at),
            "new lines carry current timestamps"
        );
        assert!(app.demo_pending.is_empty(), "ticks start no fake actions");
    }
}
