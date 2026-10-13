//! Crash-recovery policy for interrupted tool calls and runs.
//!
//! Pure decision logic over what the run journal shows; it executes nothing
//! and never touches the tool-approval broker (an unsafe interrupted call is
//! never re-executed, so recovery cannot trigger an approval prompt).
//!
//! Not to be confused with `agent::acp::replay`, which replays a transcript to
//! an ACP client. "Replay" here means re-executing a tool call.
//!
//! Policy, decided per `(turn_id, call_id)`:
//! * a terminal record exists: never execute again (at-most-once for finished calls);
//! * intent only, tool declared `replay_safe`: re-execute once with the args
//!   stored in the intent record;
//! * intent only, otherwise: synthesize an "interrupted" result, never repeat.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use crate::agent::checkpoint_payload;
use crate::session::journal::{
    JournalEntry, JournalRecord, RunJournal, ToolCallStatus, TurnOutcome,
};

/// Result text given to the model for a call that was cut off and not repeated.
pub const INTERRUPTED_TOOL_RESULT: &str = "Error: Task interrupted before this tool finished.";

/// Default for how many times one turn may be resumed before it is abandoned.
pub const DEFAULT_MAX_RESUME_ATTEMPTS: u32 = 2;

/// What recovery does with one journaled tool call.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolRecovery {
    /// The call finished (or never ran); its recorded outcome stands.
    Skip,
    /// Re-execute once with these exact arguments.
    Replay { args: serde_json::Value },
    /// Do not execute; hand the model [`INTERRUPTED_TOOL_RESULT`].
    Synthesize,
}

/// Latest known state of one tool call, folded from its journal records.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCallState {
    pub turn_id: String,
    pub call_id: String,
    pub tool_name: String,
    pub args: serde_json::Value,
    pub replay_safe: bool,
    pub status: ToolCallStatus,
    /// Outcome text of a terminal record (result, error or block reason).
    pub result: Option<String>,
    /// How many `Intent` records the call has: more than one means an earlier
    /// recovery already re-executed it.
    pub intent_count: u32,
}

impl ToolCallState {
    /// Whether a previous recovery attempt already re-executed this call.
    pub fn was_replayed(&self) -> bool {
        self.intent_count > 1
    }
}

/// Fold journal entries into the latest state per `(turn_id, call_id)`,
/// in order of first appearance. Keying by turn keeps provider call ids that
/// are reused across turns apart.
pub fn tool_call_states(entries: &[JournalEntry]) -> Vec<ToolCallState> {
    let mut order: Vec<(String, String)> = Vec::new();
    let mut states: HashMap<(String, String), ToolCallState> = HashMap::new();
    for entry in entries {
        let JournalRecord::ToolIntent {
            turn_id,
            call_id,
            tool_name,
            args,
            replay_safe,
            status,
            result,
        } = &entry.record
        else {
            continue;
        };
        let key = (turn_id.clone(), call_id.clone());
        match states.get_mut(&key) {
            Some(state) => {
                // A terminal state is final; a duplicate terminal never reopens it.
                if *status == ToolCallStatus::Intent {
                    state.intent_count += 1;
                }
                if !state.status.is_terminal() {
                    state.status = *status;
                    state.result = result.clone();
                }
            }
            None => {
                order.push(key.clone());
                states.insert(
                    key,
                    ToolCallState {
                        turn_id: turn_id.clone(),
                        call_id: call_id.clone(),
                        tool_name: tool_name.clone(),
                        args: args.clone(),
                        replay_safe: *replay_safe,
                        status: *status,
                        result: result.clone(),
                        intent_count: u32::from(*status == ToolCallStatus::Intent),
                    },
                );
            }
        }
    }
    order
        .into_iter()
        .filter_map(|key| states.remove(&key))
        .collect()
}

/// Decide what to do with one call. `already_replayed` is true when an earlier
/// recovery attempt already re-executed it (and was itself interrupted): a
/// safe call is replayed at most once, so it is synthesized instead.
pub fn decide_tool_recovery(state: &ToolCallState, already_replayed: bool) -> ToolRecovery {
    if state.status.is_terminal() {
        return ToolRecovery::Skip;
    }
    if state.replay_safe && !already_replayed {
        return ToolRecovery::Replay {
            args: state.args.clone(),
        };
    }
    ToolRecovery::Synthesize
}

/// What to do with a turn that has no terminal record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnRecovery {
    /// Resume it as attempt number `attempt`.
    Resume { attempt: u32 },
    /// Too many attempts: close it as [`TurnOutcome::Abandoned`] instead of crash-looping.
    Abandon,
}

/// Decide whether an unfinished turn that has already been tried `attempts_so_far`
/// times (0 for a turn never resumed) may be resumed once more.
pub fn decide_turn_recovery(attempts_so_far: u32, max_resume_attempts: u32) -> TurnRecovery {
    if attempts_so_far >= max_resume_attempts {
        TurnRecovery::Abandon
    } else {
        TurnRecovery::Resume {
            attempt: attempts_so_far + 1,
        }
    }
}

/// An unfinished turn found in a journal.
#[derive(Debug, Clone, PartialEq)]
pub struct UnfinishedTurn {
    pub turn_id: String,
    pub session_key: String,
    /// Highest `attempt` seen in this turn's `TurnStart` records (0 = original run).
    pub attempts: u32,
    pub channel: Option<String>,
    pub chat_id: Option<String>,
    /// Inbound request that started the turn, when it had a stable id.
    pub request_id: Option<String>,
    /// Timestamp of the turn's first `TurnStart`.
    pub started_at: String,
}

/// Turns with a `TurnStart` but no `TurnEnd` (of any outcome, including
/// `Cancelled`), oldest first.
pub fn unfinished_turns(entries: &[JournalEntry]) -> Vec<UnfinishedTurn> {
    let mut turns: Vec<UnfinishedTurn> = Vec::new();
    let mut ended: Vec<&str> = Vec::new();
    for entry in entries {
        match &entry.record {
            JournalRecord::TurnStart {
                turn_id,
                session_key,
                request_id,
                attempt,
                channel,
                chat_id,
                ..
            } => match turns.iter_mut().find(|turn| &turn.turn_id == turn_id) {
                Some(turn) => turn.attempts = turn.attempts.max(*attempt),
                None => turns.push(UnfinishedTurn {
                    turn_id: turn_id.clone(),
                    session_key: session_key.clone(),
                    attempts: *attempt,
                    channel: channel.clone(),
                    chat_id: chat_id.clone(),
                    request_id: request_id.clone(),
                    started_at: entry.ts.clone(),
                }),
            },
            JournalRecord::TurnEnd { turn_id, .. } | JournalRecord::TurnRestored { turn_id } => {
                ended.push(turn_id)
            }
            _ => {}
        }
    }
    turns.retain(|turn| !ended.contains(&turn.turn_id.as_str()));
    turns
}

/// Turn ids whose messages are missing from the session transcript and must be
/// merged by recovery, oldest first.
///
/// That is every turn that never ended, plus turns closed as `Cancelled` or
/// `Abandoned` (their partial work was never saved), minus turns already
/// restored. `Completed` and `Failed` turns were saved by the normal path.
pub fn turns_to_restore(entries: &[JournalEntry]) -> Vec<String> {
    let mut candidates: Vec<&str> = Vec::new();
    let mut settled: Vec<&str> = Vec::new();
    for entry in entries {
        match &entry.record {
            JournalRecord::TurnStart { turn_id, .. } if !candidates.contains(&turn_id.as_str()) => {
                candidates.push(turn_id)
            }
            JournalRecord::TurnEnd {
                turn_id,
                outcome: TurnOutcome::Completed | TurnOutcome::Failed,
            } => settled.push(turn_id),
            JournalRecord::TurnRestored { turn_id } => settled.push(turn_id),
            _ => {}
        }
    }
    candidates
        .into_iter()
        .filter(|turn_id| !settled.contains(turn_id))
        .map(str::to_string)
        .collect()
}

/// Content the model sees for a tool call that has no result message in the
/// restored transcript: the journaled outcome if the call finished, otherwise
/// the "interrupted" text. Never executes anything.
pub fn pending_call_content(states: &[ToolCallState], turn_id: &str, call_id: &str) -> String {
    let state = states
        .iter()
        .find(|state| state.turn_id == turn_id && state.call_id == call_id);
    match state {
        Some(state) => match (state.status, &state.result) {
            (ToolCallStatus::Done | ToolCallStatus::Error, Some(result)) => result.clone(),
            (ToolCallStatus::Blocked, Some(reason)) => {
                format!("Error: tool call blocked by hook: {reason}")
            }
            _ => INTERRUPTED_TOOL_RESULT.to_string(),
        },
        None => INTERRUPTED_TOOL_RESULT.to_string(),
    }
}

/// The messages of `turn_id` to merge into the transcript.
///
/// Built from the turn's latest step checkpoint; pending tool calls take the
/// outcome the journal recorded for them, else the "interrupted" result. A turn
/// that crashed before its first checkpoint contributes just its user message.
pub fn turn_messages_for_restore(entries: &[JournalEntry], turn_id: &str) -> Vec<Value> {
    let states = tool_call_states(entries);
    let latest_checkpoint = entries.iter().rev().find_map(|entry| match &entry.record {
        JournalRecord::StepCheckpoint {
            turn_id: id,
            payload,
            ..
        } if id == turn_id => Some(payload),
        _ => None,
    });
    if let Some(payload) = latest_checkpoint.and_then(Value::as_object) {
        return checkpoint_payload::materialize(payload, |tool_call| {
            let call_id = checkpoint_payload::tool_call_id(tool_call).unwrap_or_default();
            pending_call_content(&states, turn_id, call_id)
        });
    }
    entries
        .iter()
        .find_map(|entry| match &entry.record {
            JournalRecord::TurnStart {
                turn_id: id,
                user_message: Some(message),
                ..
            } if id == turn_id => Some(vec![message.clone()]),
            _ => None,
        })
        .unwrap_or_default()
}

/// Turn ids that still need recovery work: unfinished turns and turns to restore.
fn open_turn_ids(entries: &[JournalEntry]) -> HashSet<String> {
    turns_to_restore(entries)
        .into_iter()
        .chain(
            unfinished_turns(entries)
                .into_iter()
                .map(|turn| turn.turn_id),
        )
        .collect()
}

/// Whether compaction may drop `entry`: everything about turns that need no
/// further recovery, while ledger records and the records of open turns are kept.
pub fn is_prunable(entries: &[JournalEntry], entry: &JournalEntry) -> bool {
    is_prunable_given(&open_turn_ids(entries), entry)
}

fn is_prunable_given(open: &HashSet<String>, entry: &JournalEntry) -> bool {
    match &entry.record {
        JournalRecord::Inbox { .. } => false,
        record => record
            .turn_id()
            .is_some_and(|turn_id| !open.contains(turn_id)),
    }
}

/// Compact `journal` down to what recovery can still need. Returns how many
/// records were dropped. Called when a turn ends so the file stays bounded.
pub fn prune_journal(journal: &RunJournal) -> std::io::Result<usize> {
    let open = open_turn_ids(&journal.scan().entries);
    journal.compact(|entry| !is_prunable_given(&open, entry))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(seq: u64, record: JournalRecord) -> JournalEntry {
        JournalEntry {
            v: 1,
            seq,
            ts: String::new(),
            record,
        }
    }

    fn call(turn: &str, id: &str, safe: bool, status: ToolCallStatus) -> JournalRecord {
        JournalRecord::ToolIntent {
            turn_id: turn.into(),
            call_id: id.into(),
            tool_name: "t".into(),
            args: json!({"a": 1}),
            replay_safe: safe,
            status,
            result: None,
        }
    }

    fn state(safe: bool, status: ToolCallStatus) -> ToolCallState {
        ToolCallState {
            turn_id: "t1".into(),
            call_id: "c1".into(),
            tool_name: "t".into(),
            args: json!({"a": 1}),
            replay_safe: safe,
            status,
            result: None,
            intent_count: 1,
        }
    }

    #[test]
    fn terminal_calls_are_never_executed_again() {
        for status in [
            ToolCallStatus::Done,
            ToolCallStatus::Error,
            ToolCallStatus::Blocked,
            ToolCallStatus::Cancelled,
        ] {
            for safe in [true, false] {
                assert_eq!(
                    decide_tool_recovery(&state(safe, status), false),
                    ToolRecovery::Skip
                );
            }
        }
    }

    #[test]
    fn interrupted_safe_call_is_replayed_with_the_journaled_args() {
        assert_eq!(
            decide_tool_recovery(&state(true, ToolCallStatus::Intent), false),
            ToolRecovery::Replay {
                args: json!({"a": 1})
            }
        );
    }

    #[test]
    fn interrupted_unsafe_call_is_synthesized_never_replayed() {
        assert_eq!(
            decide_tool_recovery(&state(false, ToolCallStatus::Intent), false),
            ToolRecovery::Synthesize
        );
    }

    #[test]
    fn a_second_intent_marks_the_call_as_already_replayed() {
        let entries = vec![
            entry(1, call("t1", "c1", true, ToolCallStatus::Intent)),
            entry(2, call("t1", "c1", true, ToolCallStatus::Intent)),
        ];
        let states = tool_call_states(&entries);
        assert!(states[0].was_replayed());
        assert_eq!(
            decide_tool_recovery(&states[0], states[0].was_replayed()),
            ToolRecovery::Synthesize
        );
    }

    #[test]
    fn a_safe_call_is_replayed_at_most_once() {
        assert_eq!(
            decide_tool_recovery(&state(true, ToolCallStatus::Intent), true),
            ToolRecovery::Synthesize
        );
    }

    #[test]
    fn folding_keeps_the_latest_state_and_separates_turns_sharing_a_call_id() {
        let entries = vec![
            entry(1, call("t1", "c1", true, ToolCallStatus::Intent)),
            entry(2, call("t1", "c1", true, ToolCallStatus::Done)),
            entry(3, call("t2", "c1", true, ToolCallStatus::Intent)),
        ];
        let states = tool_call_states(&entries);
        assert_eq!(states.len(), 2);
        assert_eq!(states[0].status, ToolCallStatus::Done);
        assert_eq!(states[1].status, ToolCallStatus::Intent);
        assert_eq!(states[1].turn_id, "t2");
    }

    #[test]
    fn a_terminal_state_is_not_reopened_by_a_later_record() {
        let entries = vec![
            entry(1, call("t1", "c1", true, ToolCallStatus::Intent)),
            entry(2, call("t1", "c1", true, ToolCallStatus::Done)),
            entry(3, call("t1", "c1", true, ToolCallStatus::Intent)),
        ];
        assert_eq!(tool_call_states(&entries)[0].status, ToolCallStatus::Done);
    }

    #[test]
    fn resume_attempts_are_capped() {
        assert_eq!(
            decide_turn_recovery(0, 2),
            TurnRecovery::Resume { attempt: 1 }
        );
        assert_eq!(
            decide_turn_recovery(1, 2),
            TurnRecovery::Resume { attempt: 2 }
        );
        assert_eq!(decide_turn_recovery(2, 2), TurnRecovery::Abandon);
        assert_eq!(decide_turn_recovery(0, 0), TurnRecovery::Abandon);
    }

    fn start(turn: &str, attempt: u32) -> JournalRecord {
        JournalRecord::TurnStart {
            turn_id: turn.into(),
            session_key: "k".into(),
            request_id: None,
            user_message: None,
            attempt,
            channel: None,
            chat_id: None,
        }
    }

    fn end(turn: &str, outcome: TurnOutcome) -> JournalRecord {
        JournalRecord::TurnEnd {
            turn_id: turn.into(),
            outcome,
        }
    }

    #[test]
    fn unfinished_turns_excludes_every_kind_of_terminal_and_tracks_attempts() {
        let entries = vec![
            entry(1, start("done", 0)),
            entry(2, end("done", TurnOutcome::Completed)),
            entry(3, start("stopped", 0)),
            entry(4, end("stopped", TurnOutcome::Cancelled)),
            entry(5, start("gave-up", 2)),
            entry(6, end("gave-up", TurnOutcome::Abandoned)),
            entry(7, start("open", 0)),
            entry(8, start("open", 1)),
        ];
        let open = unfinished_turns(&entries);
        assert_eq!(open.len(), 1);
        assert_eq!((open[0].turn_id.as_str(), open[0].attempts), ("open", 1));
        assert_eq!(open[0].session_key, "k");
    }

    #[test]
    fn turns_to_restore_covers_unfinished_cancelled_and_abandoned_but_not_settled() {
        let entries = vec![
            entry(1, start("done", 0)),
            entry(2, end("done", TurnOutcome::Completed)),
            entry(3, start("failed", 0)),
            entry(4, end("failed", TurnOutcome::Failed)),
            entry(5, start("stopped", 0)),
            entry(6, end("stopped", TurnOutcome::Cancelled)),
            entry(7, start("gave-up", 0)),
            entry(8, end("gave-up", TurnOutcome::Abandoned)),
            entry(9, start("merged", 0)),
            entry(10, end("merged", TurnOutcome::Cancelled)),
            entry(
                11,
                JournalRecord::TurnRestored {
                    turn_id: "merged".into(),
                },
            ),
            entry(12, start("open", 0)),
        ];
        assert_eq!(turns_to_restore(&entries), ["stopped", "gave-up", "open"]);
    }

    #[test]
    fn restored_unfinished_turn_is_no_longer_unfinished() {
        let entries = vec![
            entry(1, start("t", 0)),
            entry(
                2,
                JournalRecord::TurnRestored {
                    turn_id: "t".into(),
                },
            ),
        ];
        assert!(unfinished_turns(&entries).is_empty());
        assert!(turns_to_restore(&entries).is_empty());
    }

    #[test]
    fn pruning_drops_settled_turns_and_keeps_open_turns_and_the_ledger() {
        let inbox = JournalRecord::Inbox {
            request_id: "r".into(),
            channel: "c".into(),
            chat_id: "x".into(),
            session_key: "k".into(),
            state: crate::session::journal::InboxState::Done,
            outcome: None,
            turn_id: Some("done".into()),
            message: None,
        };
        let entries = vec![
            entry(1, start("done", 0)),
            entry(2, call("done", "c1", true, ToolCallStatus::Done)),
            entry(3, end("done", TurnOutcome::Completed)),
            entry(4, inbox),
            entry(5, start("open", 0)),
            entry(6, call("open", "c1", true, ToolCallStatus::Intent)),
            entry(7, start("stopped", 0)),
            entry(8, end("stopped", TurnOutcome::Cancelled)),
        ];
        let kept: Vec<u64> = entries
            .iter()
            .filter(|e| !is_prunable(&entries, e))
            .map(|e| e.seq)
            .collect();
        assert_eq!(kept, [4, 5, 6, 7, 8]);
    }

    fn checkpoint(turn: &str, payload: Value) -> JournalRecord {
        JournalRecord::StepCheckpoint {
            turn_id: turn.into(),
            step: crate::session::journal::StepKind::PreTool,
            payload,
        }
    }

    fn finished_call(turn: &str, id: &str, status: ToolCallStatus, result: &str) -> JournalRecord {
        JournalRecord::ToolIntent {
            turn_id: turn.into(),
            call_id: id.into(),
            tool_name: "t".into(),
            args: json!({}),
            replay_safe: false,
            status,
            result: Some(result.into()),
        }
    }

    #[test]
    fn restore_uses_journaled_outcomes_for_finished_calls_and_interrupts_the_rest() {
        let turn = [
            json!({"role": "user", "content": "go"}),
            json!({"role": "assistant", "content": null, "tool_calls": [
                {"id": "a", "type": "function", "function": {"name": "t", "arguments": "{}"}},
                {"id": "b", "type": "function", "function": {"name": "t", "arguments": "{}"}},
                {"id": "c", "type": "function", "function": {"name": "t", "arguments": "{}"}},
                {"id": "d", "type": "function", "function": {"name": "t", "arguments": "{}"}},
            ]}),
        ];
        let entries = vec![
            entry(1, start("t1", 0)),
            entry(
                2,
                checkpoint("t1", checkpoint_payload::build("awaiting_tools", &turn)),
            ),
            entry(
                3,
                finished_call("t1", "a", ToolCallStatus::Done, "A-result"),
            ),
            entry(
                4,
                finished_call("t1", "b", ToolCallStatus::Error, "Error: B"),
            ),
            entry(5, finished_call("t1", "c", ToolCallStatus::Blocked, "no")),
            entry(6, call("t1", "d", false, ToolCallStatus::Intent)),
        ];

        let restored = turn_messages_for_restore(&entries, "t1");

        let contents: Vec<&str> = restored[2..]
            .iter()
            .map(|m| m["content"].as_str().unwrap())
            .collect();
        assert_eq!(restored.len(), 6);
        assert_eq!(
            contents,
            [
                "A-result",
                "Error: B",
                "Error: tool call blocked by hook: no",
                INTERRUPTED_TOOL_RESULT
            ]
        );
    }

    #[test]
    fn restore_without_a_checkpoint_falls_back_to_the_user_message() {
        let user = json!({"role": "user", "content": "hi"});
        let entries = vec![entry(
            1,
            JournalRecord::TurnStart {
                turn_id: "t1".into(),
                session_key: "k".into(),
                request_id: None,
                user_message: Some(user.clone()),
                attempt: 0,
                channel: None,
                chat_id: None,
            },
        )];
        assert_eq!(turn_messages_for_restore(&entries, "t1"), vec![user]);
        assert!(turn_messages_for_restore(&entries, "other").is_empty());
    }

    #[test]
    fn prune_journal_shrinks_the_file_to_open_turns() {
        let journal = RunJournal::new(crate::session::journal::InMemoryJournalStore::new(), "k");
        journal.append(start("done", 0)).unwrap();
        journal.append(end("done", TurnOutcome::Completed)).unwrap();
        journal.append(start("open", 0)).unwrap();

        assert_eq!(prune_journal(&journal).unwrap(), 2);
        let left = journal.scan().entries;
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].record.turn_id(), Some("open"));
    }

    #[test]
    fn empty_journal_has_nothing_to_recover() {
        assert!(unfinished_turns(&[]).is_empty());
        assert!(tool_call_states(&[]).is_empty());
    }

    #[test]
    fn recovery_never_references_the_approval_broker() {
        let source = include_str!("recovery.rs");
        let (production, _tests) = source.split_once("#[cfg(test)]").unwrap();
        assert!(!production.contains(&["Tool", "Approval", "Broker"].concat()));
    }
}
