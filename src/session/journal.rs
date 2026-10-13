//! Per-session append-only run journal (the durability write-ahead log).
//!
//! The journal holds one record per event of a turn: when it started, which
//! tool calls were announced and how they ended, step checkpoints, and the
//! inbound-message ledger. Recovery code reads them back with
//! [`RunJournal::scan`].
//!
//! **Storage is behind [`JournalStore`]**, so the backend can be swapped (a
//! file per session today: [`crate::session::journal_file::FileJournalStore`];
//! [`InMemoryJournalStore`] for tests). Every implementation must pass the
//! shared conformance checks at the bottom of this file. [`RunJournal`] is the
//! cheap per-session handle the rest of the code uses; it knows nothing about
//! files.
//!
//! Durability contract: an append is atomic (a record is whole or absent, and
//! concurrent appends never interleave) and has reached the OS when the call
//! returns, so it survives a process kill. [`Durability::Synced`] additionally
//! asks for power-loss durability.
//!
//! Journal I/O never panics: callers get an [`io::Result`], and
//! [`RunJournal::mark_degraded`] lets them record that crash safety was lost.

use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Schema version written into every record (`"v"`). Records with a higher
/// version are skipped with a warning instead of being misread.
pub const JOURNAL_SCHEMA_VERSION: u32 = 1;

/// How a turn ended; the terminal record of a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnOutcome {
    Completed,
    /// The user stopped the turn (`/stop`); it must never be resumed.
    Cancelled,
    Failed,
    /// Resumed too many times without finishing; closed instead of crash-looping.
    Abandoned,
}

/// Lifecycle state of one journaled tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallStatus {
    /// Written before `Tool::execute`; without a terminal status the call was interrupted.
    Intent,
    Done,
    Error,
    /// A hook denied the call; it never executed.
    Blocked,
    /// The turn was cancelled while the call was in flight.
    Cancelled,
}

impl ToolCallStatus {
    /// Whether the call reached a final state (anything but [`Self::Intent`]).
    pub fn is_terminal(self) -> bool {
        self != Self::Intent
    }
}

/// Position in the step loop at which a checkpoint was taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepKind {
    PreModel,
    PreTool,
    PreCompaction,
    Final,
}

/// State of an inbound message in the inbox ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InboxState {
    Queued,
    Processing,
    Done,
}

/// Payload of one journal line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JournalRecord {
    /// A turn began. Stores the user message because the session file only
    /// receives it when the turn ends.
    TurnStart {
        turn_id: String,
        session_key: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        user_message: Option<Value>,
        /// Resume attempt counter: 0 for the original run.
        #[serde(default)]
        attempt: u32,
        /// Where the turn's reply goes, needed to deliver a resumed turn's answer.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        channel: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        chat_id: Option<String>,
    },
    /// Terminal record of a turn.
    TurnEnd {
        turn_id: String,
        outcome: TurnOutcome,
    },
    /// The turn's messages were merged into the session transcript by
    /// recovery. Closes the turn for good; it is never restored twice.
    TurnRestored { turn_id: String },
    /// Write-ahead record of a tool call (status `intent`) and its outcome.
    ToolIntent {
        turn_id: String,
        call_id: String,
        tool_name: String,
        args: Value,
        replay_safe: bool,
        status: ToolCallStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<String>,
    },
    /// Snapshot of the turn at a step boundary.
    StepCheckpoint {
        turn_id: String,
        step: StepKind,
        payload: Value,
    },
    /// Inbound-message ledger entry, see the idempotency ledger.
    Inbox {
        request_id: String,
        channel: String,
        chat_id: String,
        session_key: String,
        state: InboxState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        outcome: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
        /// The serialized inbound message, kept while it may still need re-driving.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<Value>,
    },
    /// The journal could not be written; crash safety was lost for the turn.
    Degraded {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
        reason: String,
    },
}

impl JournalRecord {
    /// The turn this record belongs to, when it belongs to one.
    pub fn turn_id(&self) -> Option<&str> {
        match self {
            Self::TurnStart { turn_id, .. }
            | Self::TurnEnd { turn_id, .. }
            | Self::TurnRestored { turn_id }
            | Self::ToolIntent { turn_id, .. }
            | Self::StepCheckpoint { turn_id, .. } => Some(turn_id),
            Self::Inbox { turn_id, .. } | Self::Degraded { turn_id, .. } => turn_id.as_deref(),
        }
    }
}

/// A record together with its envelope (version, sequence number, timestamp).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JournalEntry {
    pub v: u32,
    pub seq: u64,
    pub ts: String,
    #[serde(flatten)]
    pub record: JournalRecord,
}

/// Everything [`RunJournal::scan`] could read, plus what it had to skip.
#[derive(Debug, Default, Clone)]
pub struct JournalScan {
    pub entries: Vec<JournalEntry>,
    /// Lines that were torn, corrupt, of an unknown kind or of a future version.
    pub skipped_lines: usize,
    /// `(expected, found)` pairs where `seq` jumped, i.e. records went missing.
    pub seq_gaps: Vec<(u64, u64)>,
}

impl JournalEntry {
    /// An entry for `record` with sequence number `seq`, stamped now.
    pub fn new(seq: u64, record: JournalRecord) -> Self {
        Self {
            v: JOURNAL_SCHEMA_VERSION,
            seq,
            ts: Utc::now().to_rfc3339(),
            record,
        }
    }
}

impl JournalScan {
    /// A scan result for `entries`, computing the sequence gaps.
    pub fn from_entries(entries: Vec<JournalEntry>, skipped_lines: usize) -> Self {
        let mut seq_gaps = Vec::new();
        let mut expected: Option<u64> = None;
        for entry in &entries {
            if let Some(expected) = expected
                && entry.seq != expected
            {
                seq_gaps.push((expected, entry.seq));
            }
            expected = Some(entry.seq + 1);
        }
        Self {
            entries,
            skipped_lines,
            seq_gaps,
        }
    }
}

/// How hard an append must try to reach stable storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Durability {
    /// Handed to the operating system: survives a process kill.
    Flushed,
    /// Also forced to disk: survives power loss.
    Synced,
}

/// Where journals are kept. One journal per session key.
///
/// Implementations serialize operations on one session themselves; callers
/// may use a store from any thread. Sequence numbers are per session, start at
/// 1 and increase for the life of the store instance, even across rewrites.
pub trait JournalStore: Send + Sync {
    /// Append `record` to the journal of `session_key` and return its sequence number.
    fn append(
        &self,
        session_key: &str,
        record: JournalRecord,
        durability: Durability,
    ) -> io::Result<u64>;

    /// Every intact record of `session_key`, in order. A missing journal is empty.
    fn scan(&self, session_key: &str) -> JournalScan;

    /// The scan of every journal in the store, in no particular order. Used at
    /// startup, when the session keys are not known in advance.
    fn scan_all(&self) -> Vec<JournalScan>;

    /// Replace the records of `session_key` with what `transform` returns for
    /// the current ones, atomically (a reader sees the old or the new journal,
    /// never a mix). Sequence numbers in the result are kept as given.
    fn rewrite(
        &self,
        session_key: &str,
        transform: &mut dyn FnMut(Vec<JournalEntry>) -> Vec<JournalEntry>,
    ) -> io::Result<()>;

    /// Remove the journal of `session_key`. Idempotent.
    fn delete(&self, session_key: &str) -> io::Result<()>;
}

/// Handle to the journal of one session. Cheap to create; all state lives in the store.
pub struct RunJournal {
    store: Arc<dyn JournalStore>,
    session_key: String,
    degraded: AtomicBool,
}

impl RunJournal {
    /// The journal of `session_key` in `store`. Nothing is created until the first append.
    pub fn new(store: Arc<dyn JournalStore>, session_key: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            store,
            session_key: session_key.into(),
            degraded: AtomicBool::new(false),
        })
    }

    /// Session this journal belongs to.
    pub fn session_key(&self) -> &str {
        &self.session_key
    }

    /// Append `record` and hand it to the OS (survives a process kill).
    ///
    /// Returns the sequence number assigned to it.
    pub fn append(&self, record: JournalRecord) -> io::Result<u64> {
        self.store
            .append(&self.session_key, record, Durability::Flushed)
    }

    /// Like [`Self::append`] but also forced to disk, so the record survives power loss.
    pub fn append_synced(&self, record: JournalRecord) -> io::Result<u64> {
        self.store
            .append(&self.session_key, record, Durability::Synced)
    }

    /// Read every intact record, skipping (and counting) damaged ones.
    pub fn scan(&self) -> JournalScan {
        self.store.scan(&self.session_key)
    }

    /// Rewrite the journal keeping only entries for which `keep` is true.
    ///
    /// Sequence numbers are preserved, so gaps after compaction are expected.
    /// Returns how many entries were dropped.
    pub fn compact(&self, keep: impl Fn(&JournalEntry) -> bool) -> io::Result<usize> {
        let mut dropped = 0;
        self.rewrite(|entries| {
            let total = entries.len();
            let kept: Vec<JournalEntry> = entries.into_iter().filter(|e| keep(e)).collect();
            dropped = total - kept.len();
            kept
        })?;
        Ok(dropped)
    }

    /// Replace the journal's entries with what `transform` returns for the current ones.
    pub fn rewrite(
        &self,
        transform: impl FnOnce(Vec<JournalEntry>) -> Vec<JournalEntry>,
    ) -> io::Result<()> {
        let mut transform = Some(transform);
        self.store
            .rewrite(&self.session_key, &mut |entries| match transform.take() {
                Some(transform) => transform(entries),
                None => entries,
            })
    }

    /// Record that journaling failed. Returns `true` only the first time, so
    /// callers can surface exactly one user-visible notice.
    pub fn mark_degraded(&self) -> bool {
        !self.degraded.swap(true, Ordering::SeqCst)
    }

    /// Whether [`Self::mark_degraded`] was called.
    pub fn is_degraded(&self) -> bool {
        self.degraded.load(Ordering::SeqCst)
    }
}

/// A [`JournalStore`] that keeps everything in memory. Nothing survives the
/// process, so it gives no crash safety; it exists for tests and as the second
/// implementation that keeps the trait honest.
#[derive(Default)]
pub struct InMemoryJournalStore {
    sessions: Mutex<HashMap<String, MemoryJournal>>,
}

#[derive(Default)]
struct MemoryJournal {
    entries: Vec<JournalEntry>,
    next_seq: u64,
}

impl InMemoryJournalStore {
    /// An empty store.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn sessions(&self) -> std::sync::MutexGuard<'_, HashMap<String, MemoryJournal>> {
        self.sessions.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl JournalStore for InMemoryJournalStore {
    fn append(
        &self,
        session_key: &str,
        record: JournalRecord,
        _durability: Durability,
    ) -> io::Result<u64> {
        let mut sessions = self.sessions();
        let journal = sessions.entry(session_key.to_string()).or_default();
        let seq = journal.next_seq.max(1);
        journal.entries.push(JournalEntry::new(seq, record));
        journal.next_seq = seq + 1;
        Ok(seq)
    }

    fn scan(&self, session_key: &str) -> JournalScan {
        let entries = self
            .sessions()
            .get(session_key)
            .map(|journal| journal.entries.clone())
            .unwrap_or_default();
        JournalScan::from_entries(entries, 0)
    }

    fn scan_all(&self) -> Vec<JournalScan> {
        self.sessions()
            .values()
            .filter(|journal| !journal.entries.is_empty())
            .map(|journal| JournalScan::from_entries(journal.entries.clone(), 0))
            .collect()
    }

    fn rewrite(
        &self,
        session_key: &str,
        transform: &mut dyn FnMut(Vec<JournalEntry>) -> Vec<JournalEntry>,
    ) -> io::Result<()> {
        let mut sessions = self.sessions();
        let journal = sessions.entry(session_key.to_string()).or_default();
        let highest = journal.entries.last().map_or(0, |entry| entry.seq);
        journal.next_seq = journal.next_seq.max(highest + 1);
        journal.entries = transform(std::mem::take(&mut journal.entries));
        Ok(())
    }

    fn delete(&self, session_key: &str) -> io::Result<()> {
        if let Some(journal) = self.sessions().get_mut(session_key) {
            // The counter stays, so sequence numbers are never reused.
            journal.entries.clear();
        }
        Ok(())
    }
}

/// Checks every [`JournalStore`] implementation must pass.
///
/// Each implementation's module calls [`conformance::run`] from a test with a
/// factory for fresh, empty stores.
#[cfg(test)]
pub mod conformance {
    use super::*;
    use serde_json::json;

    fn start(turn: &str) -> JournalRecord {
        JournalRecord::TurnStart {
            turn_id: turn.to_string(),
            session_key: "k".to_string(),
            request_id: None,
            user_message: Some(json!({"role": "user", "content": "hi"})),
            attempt: 0,
            channel: None,
            chat_id: None,
        }
    }

    fn turn_ids(scan: &JournalScan) -> Vec<String> {
        scan.entries
            .iter()
            .filter_map(|entry| entry.record.turn_id().map(str::to_string))
            .collect()
    }

    /// Run every check against stores produced by `make`.
    pub fn run(make: &dyn Fn() -> Arc<dyn JournalStore>) {
        scan_returns_records_in_order(make());
        a_missing_journal_scans_empty(make());
        synced_appends_behave_like_flushed_ones(make());
        concurrent_appends_stay_whole_and_gapless(make());
        rewrite_keeps_sequence_numbers_and_never_reuses_them(make());
        rewrite_without_changes_leaves_the_journal_alone(make());
        rewrite_can_add_and_replace_records(make());
        delete_is_idempotent_and_empties_the_journal(make());
        scan_all_covers_every_session(make());
        awkward_session_keys_work(make());
        sessions_are_isolated(make());
    }

    fn scan_returns_records_in_order(store: Arc<dyn JournalStore>) {
        for turn in ["a", "b", "c"] {
            store.append("k", start(turn), Durability::Flushed).unwrap();
        }
        let scan = store.scan("k");
        assert_eq!(turn_ids(&scan), ["a", "b", "c"], "order");
        let seqs: Vec<u64> = scan.entries.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, [1, 2, 3], "sequence numbers start at 1");
        assert_eq!(scan.skipped_lines, 0);
        assert!(scan.seq_gaps.is_empty());
    }

    fn a_missing_journal_scans_empty(store: Arc<dyn JournalStore>) {
        let scan = store.scan("never-written");
        assert!(scan.entries.is_empty());
        assert_eq!(scan.skipped_lines, 0);
    }

    fn synced_appends_behave_like_flushed_ones(store: Arc<dyn JournalStore>) {
        store.append("k", start("a"), Durability::Synced).unwrap();
        store.append("k", start("b"), Durability::Flushed).unwrap();
        assert_eq!(turn_ids(&store.scan("k")), ["a", "b"]);
    }

    fn concurrent_appends_stay_whole_and_gapless(store: Arc<dyn JournalStore>) {
        let handles: Vec<_> = (0..8)
            .map(|worker| {
                let store = Arc::clone(&store);
                std::thread::spawn(move || {
                    for n in 0..25 {
                        let turn = format!("t{worker}-{n}");
                        store
                            .append("k", start(&turn), Durability::Flushed)
                            .unwrap();
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        let scan = store.scan("k");
        assert_eq!(scan.entries.len(), 200, "every append is present");
        assert_eq!(scan.skipped_lines, 0, "no torn or interleaved record");
        assert!(
            scan.seq_gaps.is_empty(),
            "no gap or duplicate sequence number"
        );
    }

    fn rewrite_keeps_sequence_numbers_and_never_reuses_them(store: Arc<dyn JournalStore>) {
        for turn in ["a", "b", "c"] {
            store.append("k", start(turn), Durability::Flushed).unwrap();
        }
        store
            .rewrite("k", &mut |entries| {
                entries.into_iter().filter(|e| e.seq == 1).collect()
            })
            .unwrap();
        let after = store.scan("k");
        assert_eq!(turn_ids(&after), ["a"]);
        let next = store.append("k", start("d"), Durability::Flushed).unwrap();
        assert!(
            next > 3,
            "a sequence number was reused after a rewrite: {next}"
        );
    }

    fn rewrite_without_changes_leaves_the_journal_alone(store: Arc<dyn JournalStore>) {
        store.append("k", start("a"), Durability::Flushed).unwrap();
        let before = store.scan("k").entries;
        store.rewrite("k", &mut |entries| entries).unwrap();
        assert_eq!(store.scan("k").entries, before);
    }

    fn rewrite_can_add_and_replace_records(store: Arc<dyn JournalStore>) {
        store
            .append("k", start("old"), Durability::Flushed)
            .unwrap();
        store
            .rewrite("k", &mut |entries| {
                let last = entries.last().map_or(0, |e| e.seq);
                vec![JournalEntry::new(last + 1, start("new"))]
            })
            .unwrap();
        assert_eq!(turn_ids(&store.scan("k")), ["new"]);
    }

    fn delete_is_idempotent_and_empties_the_journal(store: Arc<dyn JournalStore>) {
        store.append("k", start("a"), Durability::Flushed).unwrap();
        store.delete("k").unwrap();
        store.delete("k").unwrap();
        store.delete("never-written").unwrap();
        assert!(store.scan("k").entries.is_empty());
        store
            .append("k", start("again"), Durability::Flushed)
            .unwrap();
        assert_eq!(turn_ids(&store.scan("k")), ["again"], "no stale records");
    }

    fn scan_all_covers_every_session(store: Arc<dyn JournalStore>) {
        store
            .append("one", start("a"), Durability::Flushed)
            .unwrap();
        store
            .append("two", start("b"), Durability::Flushed)
            .unwrap();
        store
            .append("gone", start("c"), Durability::Flushed)
            .unwrap();
        store.delete("gone").unwrap();

        let mut turns: Vec<String> = store.scan_all().iter().flat_map(turn_ids).collect();
        turns.sort();
        assert_eq!(turns, ["a", "b"]);
    }

    fn awkward_session_keys_work(store: Arc<dyn JournalStore>) {
        let key = "chan:chat/1 with spaces?";
        store.append(key, start("a"), Durability::Flushed).unwrap();
        assert_eq!(turn_ids(&store.scan(key)), ["a"]);
        store.delete(key).unwrap();
        assert!(store.scan(key).entries.is_empty());
    }

    fn sessions_are_isolated(store: Arc<dyn JournalStore>) {
        store
            .append("one", start("a"), Durability::Flushed)
            .unwrap();
        assert!(store.scan("two").entries.is_empty());
        store.delete("two").unwrap();
        assert_eq!(turn_ids(&store.scan("one")), ["a"]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn start(turn_id: &str) -> JournalRecord {
        JournalRecord::TurnStart {
            turn_id: turn_id.to_string(),
            session_key: "k".to_string(),
            request_id: None,
            user_message: Some(json!({"role": "user", "content": "hi"})),
            attempt: 0,
            channel: None,
            chat_id: None,
        }
    }

    fn journal() -> Arc<RunJournal> {
        RunJournal::new(InMemoryJournalStore::new(), "k")
    }

    #[test]
    fn the_in_memory_store_conforms() {
        conformance::run(&|| InMemoryJournalStore::new());
    }

    #[test]
    fn handle_round_trips_and_reports_its_session() {
        let journal = journal();
        assert_eq!(journal.session_key(), "k");
        journal.append(start("a")).unwrap();
        journal.append_synced(start("b")).unwrap();
        let scan = journal.scan();
        assert_eq!(scan.entries.len(), 2);
        assert_eq!(scan.entries[0].record, start("a"));
    }

    #[test]
    fn handles_for_one_store_and_key_share_one_journal() {
        let store: Arc<dyn JournalStore> = InMemoryJournalStore::new();
        let first = RunJournal::new(Arc::clone(&store), "k");
        let second = RunJournal::new(store, "k");
        assert_eq!(first.append(start("a")).unwrap(), 1);
        assert_eq!(second.append(start("b")).unwrap(), 2);
    }

    #[test]
    fn compact_reports_how_many_entries_it_dropped() {
        let journal = journal();
        for turn in ["a", "b", "c"] {
            journal.append(start(turn)).unwrap();
        }
        assert_eq!(journal.compact(|e| e.seq != 2).unwrap(), 1);
        assert_eq!(journal.scan().seq_gaps, [(2, 3)]);
        assert_eq!(journal.append(start("d")).unwrap(), 4);
    }

    #[test]
    fn degraded_flag_reports_first_transition_only() {
        let journal = journal();
        assert!(!journal.is_degraded());
        assert!(journal.mark_degraded());
        assert!(!journal.mark_degraded());
        assert!(journal.is_degraded());
    }

    #[test]
    fn terminal_status_classification() {
        assert!(!ToolCallStatus::Intent.is_terminal());
        for status in [
            ToolCallStatus::Done,
            ToolCallStatus::Error,
            ToolCallStatus::Blocked,
            ToolCallStatus::Cancelled,
        ] {
            assert!(status.is_terminal());
        }
    }
}
