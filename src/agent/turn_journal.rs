//! Best-effort journaling of one agent turn (the write side of the durability WAL).
//!
//! [`TurnJournal`] binds a session's [`RunJournal`] to a `turn_id` and exposes
//! the few events the runner and the agent loop emit: tool-call intent and
//! outcome, hook-blocked calls and the turn's terminal record.
//!
//! Policy: availability over durability. A journal write that fails is logged,
//! flags the journal as degraded and records a `Degraded` marker when it can,
//! but never fails or blocks the tool call or the turn.
//!
//! The two guards ([`InFlightToolCall`], [`TurnGuard`]) write a `Cancelled`
//! terminal when dropped unfinished, which is what happens when `/stop` aborts
//! the task. A process kill never runs destructors, so it leaves exactly the
//! unfinished records that recovery looks for. Unwinding from a panic is
//! treated like a kill (no terminal is written), so crash tests can simulate
//! one.

use std::sync::Arc;

use serde_json::Value;

use crate::session::journal::{JournalRecord, RunJournal, ToolCallStatus, TurnOutcome};

/// A session journal bound to one turn.
#[derive(Clone)]
pub struct TurnJournal {
    journal: Arc<RunJournal>,
    turn_id: String,
}

impl TurnJournal {
    /// Bind `journal` to the turn identified by `turn_id`.
    pub fn new(journal: Arc<RunJournal>, turn_id: impl Into<String>) -> Self {
        Self {
            journal,
            turn_id: turn_id.into(),
        }
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
    /// Intents of tools that are not replay safe are synced to disk: after a
    /// crash they decide between "never repeat" and "unknown".
    pub fn begin_tool_call(
        &self,
        call_id: &str,
        tool_name: &str,
        args: &Value,
        replay_safe: bool,
    ) -> InFlightToolCall {
        let call = InFlightToolCall {
            turn: self.clone(),
            call_id: call_id.to_string(),
            tool_name: tool_name.to_string(),
            args: args.clone(),
            replay_safe,
            finished: false,
        };
        call.write(ToolCallStatus::Intent, None, !replay_safe);
        call
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
    fn write(&self, status: ToolCallStatus, result: Option<String>, sync: bool) {
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
        );
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
    use crate::session::journal_file::FileJournalStore;
    use serde_json::json;

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
        let call = turn.begin_tool_call("c1", "read_file", &json!({"path": "a"}), true);
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
        drop(turn.begin_tool_call("c1", "exec", &json!({}), false));
        assert_eq!(
            statuses(&turn),
            [ToolCallStatus::Intent, ToolCallStatus::Cancelled]
        );
    }

    #[test]
    fn leaked_call_models_a_process_kill_and_stays_intent_only() {
        let dir = tempfile::tempdir().unwrap();
        let turn = turn_in(&dir);
        std::mem::forget(turn.begin_tool_call("c1", "exec", &json!({}), false));
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
            .finish(ToolCallStatus::Done, String::new());
        assert!(journal.is_degraded());
    }
}
