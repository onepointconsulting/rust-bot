//! Per-session tracking of background work (spawned sub-agents, later ACP child
//! agents), so a channel can show "Waiting for N background agents to finish".
//!
//! The tracker is UI-agnostic: work registers itself with [`BackgroundActivity::begin`]
//! and holds the returned [`ActivityGuard`]. Dropping the guard ends the entry,
//! whichever way the work ended, so a panic or an early return cannot leave a
//! stale entry behind. Every change is broadcast as an [`ActivityChange`].

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::broadcast;

/// Capacity of the change channel. A slow subscriber that lags re-reads the
/// current state with [`BackgroundActivity::snapshot`], so losing events is safe.
const CHANGE_CHANNEL_CAPACITY: usize = 64;

/// Prefix shown before the name of an ACP child agent, so it is clear that an
/// external agent is being called.
pub const ACP_LABEL_PREFIX: &str = "acp::";

/// What kind of background work an entry stands for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActivityKind {
    /// A sub-agent started by the `spawn` tool.
    Spawn,
    /// A run of the named ACP child agent.
    Acp { agent: String },
}

/// What is running in one session at one moment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ActivitySnapshot {
    /// Number of running spawned sub-agents.
    pub spawn_count: usize,
    /// Names of the ACP child agents currently running, oldest first.
    pub acp_agents: Vec<String>,
}

impl ActivitySnapshot {
    /// True when nothing is running.
    pub fn is_idle(&self) -> bool {
        self.spawn_count == 0 && self.acp_agents.is_empty()
    }

    /// The banner text for this snapshot, or `None` when nothing is running.
    ///
    /// Spawned sub-agents are a plain count; ACP agents are listed by name with
    /// the [`ACP_LABEL_PREFIX`], e.g. `Waiting for 2 background agents, acp::reviewer to finish`.
    pub fn banner(&self) -> Option<String> {
        if self.is_idle() {
            return None;
        }
        let mut parts: Vec<String> = Vec::new();
        match self.spawn_count {
            0 => {}
            1 => parts.push("1 background agent".to_string()),
            count => parts.push(format!("{count} background agents")),
        }
        parts.extend(
            self.acp_agents
                .iter()
                .map(|agent| format!("{ACP_LABEL_PREFIX}{agent}")),
        );
        Some(format!("Waiting for {} to finish", parts.join(", ")))
    }
}

/// Broadcast whenever the running work of a session changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivityChange {
    pub session_key: String,
    /// The session's state after the change.
    pub snapshot: ActivitySnapshot,
}

/// One running piece of work, identified so that ending it twice is a no-op.
#[derive(Debug)]
struct ActivityEntry {
    id: u64,
    kind: ActivityKind,
}

/// Tracks running background work per session and announces every change.
pub struct BackgroundActivity {
    entries: Mutex<HashMap<String, Vec<ActivityEntry>>>,
    next_entry_id: AtomicU64,
    changes: broadcast::Sender<ActivityChange>,
}

impl Default for BackgroundActivity {
    fn default() -> Self {
        let (changes, _) = broadcast::channel(CHANGE_CHANNEL_CAPACITY);
        Self {
            entries: Mutex::new(HashMap::new()),
            next_entry_id: AtomicU64::new(0),
            changes,
        }
    }
}

impl BackgroundActivity {
    /// A new, empty tracker.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Register one piece of work for `session_key`. The work counts until the
    /// returned guard is dropped.
    pub fn begin(self: &Arc<Self>, session_key: &str, kind: ActivityKind) -> ActivityGuard {
        let entry_id = self.next_entry_id.fetch_add(1, Ordering::Relaxed);
        let snapshot = {
            let mut entries = self.lock_entries();
            let session_entries = entries.entry(session_key.to_string()).or_default();
            session_entries.push(ActivityEntry { id: entry_id, kind });
            snapshot_of(session_entries)
        };
        self.announce(session_key, snapshot);
        ActivityGuard {
            tracker: Arc::clone(self),
            session_key: session_key.to_string(),
            entry_id,
        }
    }

    /// What is running in `session_key` right now.
    pub fn snapshot(&self, session_key: &str) -> ActivitySnapshot {
        self.lock_entries()
            .get(session_key)
            .map(|entries| snapshot_of(entries))
            .unwrap_or_default()
    }

    /// Subscribe to the changes of every session; filter by `session_key`.
    pub fn subscribe(&self) -> broadcast::Receiver<ActivityChange> {
        self.changes.subscribe()
    }

    /// Remove one entry and announce the change. A no-op for an unknown entry.
    fn end(&self, session_key: &str, entry_id: u64) {
        let snapshot = {
            let mut entries = self.lock_entries();
            let Some(session_entries) = entries.get_mut(session_key) else {
                return;
            };
            let Some(position) = session_entries.iter().position(|e| e.id == entry_id) else {
                return;
            };
            session_entries.remove(position);
            let snapshot = snapshot_of(session_entries);
            if session_entries.is_empty() {
                entries.remove(session_key);
            }
            snapshot
        };
        self.announce(session_key, snapshot);
    }

    /// Broadcast a change; having no subscriber is fine. Called without the
    /// entries lock held.
    fn announce(&self, session_key: &str, snapshot: ActivitySnapshot) {
        let _ = self.changes.send(ActivityChange {
            session_key: session_key.to_string(),
            snapshot,
        });
    }

    fn lock_entries(&self) -> std::sync::MutexGuard<'_, HashMap<String, Vec<ActivityEntry>>> {
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The snapshot of one session's entries, oldest first.
fn snapshot_of(entries: &[ActivityEntry]) -> ActivitySnapshot {
    let mut snapshot = ActivitySnapshot::default();
    for entry in entries {
        match &entry.kind {
            ActivityKind::Spawn => snapshot.spawn_count += 1,
            ActivityKind::Acp { agent } => snapshot.acp_agents.push(agent.clone()),
        }
    }
    snapshot
}

/// Keeps one piece of work counted. Dropping it (normally, on an early return
/// or while unwinding from a panic) ends the entry and announces the change.
pub struct ActivityGuard {
    tracker: Arc<BackgroundActivity>,
    session_key: String,
    entry_id: u64,
}

impl Drop for ActivityGuard {
    fn drop(&mut self) {
        self.tracker.end(&self.session_key, self.entry_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acp(agent: &str) -> ActivityKind {
        ActivityKind::Acp {
            agent: agent.to_string(),
        }
    }

    #[test]
    fn new_tracker_is_idle_for_every_session() {
        let tracker = BackgroundActivity::new();
        assert!(tracker.snapshot("cli:direct").is_idle());
    }

    #[test]
    fn begin_counts_and_dropping_the_guard_uncounts() {
        let tracker = BackgroundActivity::new();
        let first = tracker.begin("cli:direct", ActivityKind::Spawn);
        let second = tracker.begin("cli:direct", ActivityKind::Spawn);
        assert_eq!(tracker.snapshot("cli:direct").spawn_count, 2);
        drop(first);
        assert_eq!(tracker.snapshot("cli:direct").spawn_count, 1);
        drop(second);
        assert!(tracker.snapshot("cli:direct").is_idle());
    }

    #[test]
    fn sessions_are_isolated() {
        let tracker = BackgroundActivity::new();
        let _guard = tracker.begin("cli:direct", ActivityKind::Spawn);
        assert_eq!(tracker.snapshot("cli:direct").spawn_count, 1);
        assert!(tracker.snapshot("websocket:abc").is_idle());
    }

    #[test]
    fn acp_entries_are_listed_by_name_oldest_first() {
        let tracker = BackgroundActivity::new();
        let _reviewer = tracker.begin("s", acp("reviewer"));
        let _spawn = tracker.begin("s", ActivityKind::Spawn);
        let _coder = tracker.begin("s", acp("coder"));
        let snapshot = tracker.snapshot("s");
        assert_eq!(snapshot.spawn_count, 1);
        assert_eq!(snapshot.acp_agents, vec!["reviewer", "coder"]);
    }

    #[test]
    fn every_change_is_broadcast_with_the_new_state() {
        let tracker = BackgroundActivity::new();
        let mut changes = tracker.subscribe();
        let guard = tracker.begin("s", ActivityKind::Spawn);
        let started = changes.try_recv().expect("start announced");
        assert_eq!(started.session_key, "s");
        assert_eq!(started.snapshot.spawn_count, 1);
        drop(guard);
        let ended = changes.try_recv().expect("end announced");
        assert!(ended.snapshot.is_idle());
        assert!(changes.try_recv().is_err(), "exactly two changes");
    }

    #[test]
    fn dropping_a_guard_while_unwinding_from_a_panic_uncounts() {
        let tracker = BackgroundActivity::new();
        let tracker_in_thread = Arc::clone(&tracker);
        let outcome = std::thread::spawn(move || {
            let _guard = tracker_in_thread.begin("s", ActivityKind::Spawn);
            panic!("work blew up");
        })
        .join();
        assert!(outcome.is_err());
        assert!(tracker.snapshot("s").is_idle());
    }

    #[test]
    fn ending_an_entry_twice_is_a_no_op() {
        let tracker = BackgroundActivity::new();
        let _other = tracker.begin("s", ActivityKind::Spawn);
        let guard = tracker.begin("s", ActivityKind::Spawn);
        let (session_key, entry_id) = (guard.session_key.clone(), guard.entry_id);
        drop(guard);
        tracker.end(&session_key, entry_id);
        assert_eq!(tracker.snapshot("s").spawn_count, 1);
    }

    #[test]
    fn banner_is_none_when_idle() {
        assert_eq!(ActivitySnapshot::default().banner(), None);
    }

    #[test]
    fn banner_uses_singular_and_plural_for_spawn_tasks() {
        let one = ActivitySnapshot {
            spawn_count: 1,
            acp_agents: vec![],
        };
        assert_eq!(
            one.banner().as_deref(),
            Some("Waiting for 1 background agent to finish")
        );
        let many = ActivitySnapshot {
            spawn_count: 3,
            acp_agents: vec![],
        };
        assert_eq!(
            many.banner().as_deref(),
            Some("Waiting for 3 background agents to finish")
        );
    }

    #[test]
    fn banner_lists_acp_agents_with_prefix() {
        let only_acp = ActivitySnapshot {
            spawn_count: 0,
            acp_agents: vec!["reviewer".to_string()],
        };
        assert_eq!(
            only_acp.banner().as_deref(),
            Some("Waiting for acp::reviewer to finish")
        );
        let mixed = ActivitySnapshot {
            spawn_count: 2,
            acp_agents: vec!["reviewer".to_string(), "coder".to_string()],
        };
        assert_eq!(
            mixed.banner().as_deref(),
            Some("Waiting for 2 background agents, acp::reviewer, acp::coder to finish")
        );
    }
}
