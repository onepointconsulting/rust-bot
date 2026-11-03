//! Best-effort journaling of one agent turn (the write side of the durability WAL).
//!
//! [`TurnJournal`] binds a session's [`RunJournal`] to a `turn_id` and exposes
//! the few events the runner and the agent loop emit: tool-call intent and
//! outcome, hook-blocked calls and the turn's terminal record.
//!
//! Policy depends on the [`JournalMode`]. In the default best-effort mode a
//! journal write that fails is logged, flags the journal as degraded and records
//! a `Degraded` marker when it can, but never fails or blocks the tool call or
//! the turn. In strict mode a tool call whose *intent* cannot be written is not
//! executed ([`TurnJournal::begin_tool_call`] returns an error); every other
//! write stays best-effort.
//!
//! The two guards ([`InFlightToolCall`], [`TurnGuard`]) write a `Cancelled`
//! terminal when dropped unfinished, which is what happens when `/stop` aborts
//! the task. A process kill never runs destructors, so it leaves exactly the
//! unfinished records that recovery looks for. Unwinding from a panic is
//! treated like a kill (no terminal is written), so crash tests can simulate
//! one.

use std::fmt;
use std::sync::Arc;

use serde_json::Value;

use crate::config::schema::JournalMode;
use crate::session::journal::{JournalRecord, RunJournal, ToolCallStatus, TurnOutcome};

/// A session journal bound to one turn.
#[derive(Clone)]
pub struct TurnJournal {
    journal: Arc<RunJournal>,
    turn_id: String,
    mode: JournalMode,
}

/// A tool call was refused because its intent could not be journaled in
/// [`JournalMode::Strict`]; the tool was not executed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntentNotPersisted {
    pub tool_name: String,
}

impl fmt::Display for IntentNotPersisted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "tool '{}' was not executed: its intent could not be written to the durability journal              (durability.journalMode is \"strict\"). Check the journal's disk, or set              durability.journalMode to \"bestEffort\".",
            self.tool_name
        )
    }
}

impl std::error::Error for IntentNotPersisted {}

impl TurnJournal {
    /// Bind `journal` to the turn identified by `turn_id`, in best-effort mode.
    pub fn new(journal: Arc<RunJournal>, turn_id: impl Into<String>) -> Self {
        Self {
            journal,
            turn_id: turn_id.into(),
            mode: JournalMode::default(),
        }
    }

    /// Use `mode` to decide what a failed intent write does to a tool call.
    pub fn with_mode(mut self, mode: JournalMode) -> Self {
        self.mode = mode;
        self
    }

    /// Identifier shared by every record of this turn.
    pub fn turn_id(&self) -> &str {
        &self.turn_id
    }

    /// The underlying session journal.
    pub fn journal(&self) -> &Arc<RunJournal> {
        &self.journal
    }

    /// Append `record`, degrading gracefully on failure.
    ///
    /// `sync` also forces the record to disk. Returns whether it was written.
    pub fn append(&self, record: JournalRecord, sync: bool) -> bool {
        let written = if sync {
            self.journal.append_synced(record)
        } else {
            self.journal.append(record)
        };
        match written {
            Ok(_) => true,
            Err(error) => {
                self.degrade(&error.to_string());
                false
            }
        }
    }

    /// Note that crash safety is lost: log, flag and try to leave a marker.
    fn degrade(&self, reason: &str) {
        log::error!(
            "journal of session {} write failed (turn {}), crash safety degraded: {reason}",
            self.journal.session_key(),
            self.turn_id
        );
        if self.journal.mark_degraded() {
            // The marker may fail too (same disk); that is fine.
            let _ = self.journal.append(JournalRecord::Degraded {
                turn_id: Some(self.turn_id.clone()),
                reason: reason.to_string(),
            });
        }
    }

    /// Write the terminal `TurnEnd` record.
    pub fn record_turn_end(&self, outcome: TurnOutcome) {
        self.append(
            JournalRecord::TurnEnd {
                turn_id: self.turn_id.clone(),
                outcome,
            },
            true,
        );
    }

    /// Journal the intent to run a tool call, before `Tool::execute`.
    ///
    /// In [`JournalMode::Strict`] a failed intent write returns
    /// [`IntentNotPersisted`] and the caller must not execute the tool. A
    /// `Blocked` record is then attempted (best-effort) so the journal does not
    /// show the call as interrupted. In best-effort mode this always succeeds.
    ///
    /// Intents of tools that are not replay safe are synced to disk: after a
    /// crash they decide between "never repeat" and "unknown".
    pub fn begin_tool_call(
        &self,
        call_id: &str,
        tool_name: &str,
        args: &Value,
        replay_safe: bool,
    ) -> Result<InFlightToolCall, IntentNotPersisted> {
        let mut call = InFlightToolCall {
            turn: self.clone(),
            call_id: call_id.to_string(),
            tool_name: tool_name.to_string(),
            args: args.clone(),
            replay_safe,
            finished: false,
        };
        let intent_written = call.write(ToolCallStatus::Intent, None, !replay_safe);
        if intent_written || self.mode == JournalMode::BestEffort {
            return Ok(call);
        }
        // Never executed, so there is no in-flight phase to cancel on drop.
        call.finished = true;
        call.write(
            ToolCallStatus::Blocked,
            Some("intent could not be journaled (strict durability mode)".to_string()),
            false,
        );
        Err(IntentNotPersisted {
            tool_name: call.tool_name.clone(),
        })
    }

    /// Journal a call that a hook denied; it never executed.
    pub fn record_blocked_call(&self, call_id: &str, tool_name: &str, args: &Value, reason: &str) {
        // Born finished: a blocked call has no in-flight phase to cancel.
        let call = InFlightToolCall {
            turn: self.clone(),
            call_id: call_id.to_string(),
            tool_name: tool_name.to_string(),
            args: args.clone(),
            replay_safe: false,
            finished: true,
        };
        call.write(ToolCallStatus::Blocked, Some(reason.to_string()), false);
    }

    /// Guard that writes `TurnEnd{Cancelled}` if dropped before [`TurnGuard::finish`].
    pub fn guard(&self) -> TurnGuard {
        TurnGuard {
            turn: self.clone(),
            finished: false,
        }
    }
}

/// A journaled tool call between its intent and its terminal record.
pub struct InFlightToolCall {
    turn: TurnJournal,
    call_id: String,
    tool_name: String,
    args: Value,
    replay_safe: bool,
    finished: bool,
}

impl InFlightToolCall {
    /// Append one record of this call; returns whether it was written.
    fn write(&self, status: ToolCallStatus, result: Option<String>, sync: bool) -> bool {
        self.turn.append(
            JournalRecord::ToolIntent {
                turn_id: self.turn.turn_id.clone(),
                call_id: self.call_id.clone(),
                tool_name: self.tool_name.clone(),
                args: self.args.clone(),
                replay_safe: self.replay_safe,
                status,
                result,
            },
            sync,
        )
    }

    /// Write the terminal record: [`ToolCallStatus::Done`] or [`ToolCallStatus::Error`].
    pub fn finish(mut self, status: ToolCallStatus, result: String) {
        self.finished = true;
        self.write(status, Some(result), false);
    }
}

impl Drop for InFlightToolCall {
    fn drop(&mut self) {
        if !self.finished && !std::thread::panicking() {
            self.write(ToolCallStatus::Cancelled, None, false);
        }
    }
}

/// Writes the turn's terminal record; `Cancelled` when dropped unfinished.
pub struct TurnGuard {
    turn: TurnJournal,
    finished: bool,
}

impl TurnGuard {
    /// Close the turn with `outcome`.
    pub fn finish(mut self, outcome: TurnOutcome) {
        self.finished = true;
        self.turn.record_turn_end(outcome);
    }
}

impl Drop for TurnGuard {
    fn drop(&mut self) {
        if !self.finished && !std::thread::panicking() {
            self.turn.record_turn_end(TurnOutcome::Cancelled);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::journal::{Durability, JournalEntry, JournalScan, JournalStore};
    use crate::session::journal_file::FileJournalStore;
    use serde_json::json;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn turn_in(dir: &tempfile::TempDir) -> TurnJournal {
        let journal = RunJournal::new(FileJournalStore::shared(dir.path().to_path_buf()), "k");
        TurnJournal::new(journal, "t1")
    }

    fn statuses(turn: &TurnJournal) -> Vec<ToolCallStatus> {
        turn.journal()
            .scan()
            .entries
            .into_iter()
            .filter_map(|entry| match entry.record {
                JournalRecord::ToolIntent { status, .. } => Some(status),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn finished_call_leaves_intent_then_terminal() {
        let dir = tempfile::tempdir().unwrap();
        let turn = turn_in(&dir);
        let call = turn
            .begin_tool_call("c1", "read_file", &json!({"path": "a"}), true)
            .unwrap();
        assert_eq!(statuses(&turn), [ToolCallStatus::Intent]);
        call.finish(ToolCallStatus::Done, "ok".to_string());
        assert_eq!(
            statuses(&turn),
            [ToolCallStatus::Intent, ToolCallStatus::Done]
        );
    }

    #[test]
    fn dropped_call_is_marked_cancelled() {
        let dir = tempfile::tempdir().unwrap();
        let turn = turn_in(&dir);
        drop(
            turn.begin_tool_call("c1", "exec", &json!({}), false)
                .unwrap(),
        );
        assert_eq!(
            statuses(&turn),
            [ToolCallStatus::Intent, ToolCallStatus::Cancelled]
        );
    }

    #[test]
    fn leaked_call_models_a_process_kill_and_stays_intent_only() {
        let dir = tempfile::tempdir().unwrap();
        let turn = turn_in(&dir);
        std::mem::forget(
            turn.begin_tool_call("c1", "exec", &json!({}), false)
                .unwrap(),
        );
        assert_eq!(statuses(&turn), [ToolCallStatus::Intent]);
    }

    #[test]
    fn blocked_call_writes_a_single_blocked_record_with_reason() {
        let dir = tempfile::tempdir().unwrap();
        let turn = turn_in(&dir);
        turn.record_blocked_call("c1", "exec", &json!({"cmd": "rm"}), "denied");
        let entries = turn.journal().scan().entries;
        assert_eq!(entries.len(), 1);
        assert!(matches!(
            &entries[0].record,
            JournalRecord::ToolIntent { status: ToolCallStatus::Blocked, result: Some(reason), .. }
                if reason == "denied"
        ));
    }

    #[test]
    fn turn_guard_cancels_unless_finished() {
        let dir = tempfile::tempdir().unwrap();
        let turn = turn_in(&dir);
        drop(turn.guard());
        turn.guard().finish(TurnOutcome::Completed);
        let outcomes: Vec<TurnOutcome> = turn
            .journal()
            .scan()
            .entries
            .into_iter()
            .filter_map(|entry| match entry.record {
                JournalRecord::TurnEnd { outcome, .. } => Some(outcome),
                _ => None,
            })
            .collect();
        assert_eq!(outcomes, [TurnOutcome::Cancelled, TurnOutcome::Completed]);
    }

    #[test]
    fn unwritable_journal_degrades_once_and_never_panics() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, "x").unwrap();
        let journal = RunJournal::new(FileJournalStore::shared(blocker.join("sessions")), "k");
        let turn = TurnJournal::new(journal.clone(), "t1");

        assert!(!turn.append(
            JournalRecord::TurnEnd {
                turn_id: "t1".into(),
                outcome: TurnOutcome::Completed
            },
            false
        ));
        turn.begin_tool_call("c1", "exec", &json!({}), false)
            .expect("best-effort mode never refuses a call")
            .finish(ToolCallStatus::Done, String::new());
        assert!(journal.is_degraded());
    }

    /// A turn whose journal directory cannot be created, so every append fails.
    fn unwritable_turn(dir: &tempfile::TempDir, mode: JournalMode) -> TurnJournal {
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, "x").unwrap();
        let journal = RunJournal::new(FileJournalStore::shared(blocker.join("sessions")), "k");
        TurnJournal::new(journal, "t1").with_mode(mode)
    }

    #[test]
    fn default_mode_is_best_effort() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(turn_in(&dir).mode, JournalMode::BestEffort);
    }

    #[test]
    fn strict_mode_refuses_a_call_whose_intent_cannot_be_written() {
        let dir = tempfile::tempdir().unwrap();
        let turn = unwritable_turn(&dir, JournalMode::Strict);
        for replay_safe in [false, true] {
            let error = turn
                .begin_tool_call("c1", "exec", &json!({}), replay_safe)
                .err()
                .expect("strict mode must refuse");
            assert_eq!(error.tool_name, "exec");
            assert!(error.to_string().contains("strict"), "got: {error}");
        }
        assert!(turn.journal().is_degraded());
    }

    #[test]
    fn strict_mode_runs_calls_normally_while_the_journal_works() {
        let dir = tempfile::tempdir().unwrap();
        let turn = turn_in(&dir).with_mode(JournalMode::Strict);
        let call = turn
            .begin_tool_call("c1", "exec", &json!({}), false)
            .expect("a writable journal must not block");
        call.finish(ToolCallStatus::Done, "ok".to_string());
        assert_eq!(
            statuses(&turn),
            [ToolCallStatus::Intent, ToolCallStatus::Done]
        );
    }

    #[test]
    fn best_effort_mode_lets_a_call_through_when_the_intent_cannot_be_written() {
        let dir = tempfile::tempdir().unwrap();
        let turn = unwritable_turn(&dir, JournalMode::BestEffort);
        assert!(
            turn.begin_tool_call("c1", "exec", &json!({}), false)
                .is_ok()
        );
        assert!(turn.journal().is_degraded());
    }

    /// Store that fails its first append (a transient fault) and delegates the rest.
    struct FailFirstAppendStore {
        inner: Arc<dyn JournalStore>,
        failed: AtomicBool,
    }

    impl JournalStore for FailFirstAppendStore {
        fn append(
            &self,
            session_key: &str,
            record: JournalRecord,
            durability: Durability,
        ) -> std::io::Result<u64> {
            if !self.failed.swap(true, Ordering::SeqCst) {
                return Err(std::io::Error::other("disk full"));
            }
            self.inner.append(session_key, record, durability)
        }

        fn scan(&self, session_key: &str) -> JournalScan {
            self.inner.scan(session_key)
        }

        fn scan_all(&self) -> Vec<JournalScan> {
            self.inner.scan_all()
        }

        fn rewrite(
            &self,
            session_key: &str,
            transform: &mut dyn FnMut(Vec<JournalEntry>) -> Vec<JournalEntry>,
        ) -> std::io::Result<()> {
            self.inner.rewrite(session_key, transform)
        }

        fn delete(&self, session_key: &str) -> std::io::Result<()> {
            self.inner.delete(session_key)
        }
    }

    #[test]
    fn refused_call_is_recorded_as_blocked_not_as_interrupted() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(FailFirstAppendStore {
            inner: FileJournalStore::shared(dir.path().to_path_buf()),
            failed: AtomicBool::new(false),
        });
        let turn =
            TurnJournal::new(RunJournal::new(store, "k"), "t1").with_mode(JournalMode::Strict);
        assert!(
            turn.begin_tool_call("c1", "exec", &json!({}), false)
                .is_err()
        );
        assert_eq!(statuses(&turn), [ToolCallStatus::Blocked]);
    }
}
