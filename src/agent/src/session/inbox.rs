//! Inbox ledger: effectively-once model turns for inbound messages.
//!
//! Every inbound message that carries a `request_id` is recorded in the
//! session's run journal as `Queued` when it is published, `Processing` when
//! its turn starts and `Done` (with the reply text) when the turn finished. A
//! redelivered message is recognised by its `request_id`:
//!
//! * `Done`: the stored reply is sent again and no new turn runs;
//! * `Queued` / `Processing`: the duplicate is dropped (the first delivery is
//!   still being handled, or will be re-driven after a crash).
//!
//! Messages without a `request_id` are never recorded and never deduplicated.
//! The ledger is at-least-once on the outbound side: a crash between sending a
//! reply and writing `Done` can send that reply once more after a restart.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::bus::events::{InboundMessage, OutboundMessage};
use crate::bus::queue::{InboundAdmission, InboundGuard};
use crate::session::journal::{InboxState, JournalEntry, JournalRecord, JournalStore, RunJournal};
use crate::utils::helpers::truncate_text;

/// Longest reply text kept in a `Done` record.
pub const MAX_OUTCOME_CHARS: usize = 4096;

/// Latest known state of one request, folded from its ledger records.
#[derive(Debug, Clone, PartialEq)]
pub struct InboxEntry {
    pub request_id: String,
    pub channel: String,
    pub chat_id: String,
    pub session_key: String,
    pub state: InboxState,
    /// Reply text once `Done`.
    pub outcome: Option<String>,
    /// Turn that handled the request, once known.
    pub turn_id: Option<String>,
    /// The serialized inbound message, until the request is `Done`.
    pub message: Option<Value>,
    /// Timestamp of the latest record.
    pub updated_at: String,
}

/// What publishing an inbound message should do.
#[derive(Debug, Clone, PartialEq)]
pub enum Admission {
    /// First sighting (recorded as `Queued`), or no request id: process it.
    New,
    /// Already handled or in progress.
    Duplicate {
        state: InboxState,
        outcome: Option<String>,
    },
}

/// The inbox ledger of one session.
pub struct InboxLedger {
    journal: Arc<RunJournal>,
}

/// Fold ledger records into one entry per request id, ordered by first appearance.
pub fn fold_entries(entries: &[JournalEntry]) -> Vec<InboxEntry> {
    let mut folded: Vec<InboxEntry> = Vec::new();
    for entry in entries {
        let JournalRecord::Inbox {
            request_id,
            channel,
            chat_id,
            session_key,
            state,
            outcome,
            turn_id,
            message,
        } = &entry.record
        else {
            continue;
        };
        match folded
            .iter_mut()
            .find(|known| &known.request_id == request_id)
        {
            Some(known) => {
                known.state = *state;
                known.updated_at = entry.ts.clone();
                if outcome.is_some() {
                    known.outcome = outcome.clone();
                }
                if turn_id.is_some() {
                    known.turn_id = turn_id.clone();
                }
                if message.is_some() {
                    known.message = message.clone();
                }
                if *state == InboxState::Done {
                    known.message = None;
                }
            }
            None => folded.push(InboxEntry {
                request_id: request_id.clone(),
                channel: channel.clone(),
                chat_id: chat_id.clone(),
                session_key: session_key.clone(),
                state: *state,
                outcome: outcome.clone(),
                turn_id: turn_id.clone(),
                message: if *state == InboxState::Done {
                    None
                } else {
                    message.clone()
                },
                updated_at: entry.ts.clone(),
            }),
        }
    }
    folded
}

impl InboxLedger {
    /// The ledger of `session_key` in `store`.
    pub fn for_session(store: &Arc<dyn JournalStore>, session_key: &str) -> Self {
        Self {
            journal: RunJournal::new(Arc::clone(store), session_key),
        }
    }

    /// Ledger over an existing journal handle.
    pub fn new(journal: Arc<RunJournal>) -> Self {
        Self { journal }
    }

    /// All requests of this session, oldest first.
    pub fn entries(&self) -> Vec<InboxEntry> {
        fold_entries(&self.journal.scan().entries)
    }

    /// Requests that were accepted but not finished (`Queued` / `Processing`), oldest first.
    pub fn pending(&self) -> Vec<InboxEntry> {
        self.entries()
            .into_iter()
            .filter(|entry| entry.state != InboxState::Done)
            .collect()
    }

    /// Decide whether `msg` is new and record it as `Queued` if so.
    ///
    /// A failing journal write is logged and the message is accepted anyway:
    /// losing dedupe is better than losing the message.
    pub fn admit(&self, msg: &InboundMessage) -> Admission {
        let Some(request_id) = &msg.request_id else {
            return Admission::New;
        };
        if let Some(known) = self
            .entries()
            .into_iter()
            .find(|entry| &entry.request_id == request_id)
        {
            return Admission::Duplicate {
                state: known.state,
                outcome: known.outcome,
            };
        }
        self.append(
            msg,
            request_id,
            InboxState::Queued,
            None,
            None,
            serde_json::to_value(msg).ok(),
        );
        Admission::New
    }

    /// Record that the turn for `msg` started.
    pub fn mark_processing(&self, msg: &InboundMessage) {
        if let Some(request_id) = &msg.request_id {
            self.append(msg, request_id, InboxState::Processing, None, None, None);
        }
    }

    /// Record that the request was answered with `outcome` (truncated).
    pub fn mark_done(&self, msg: &InboundMessage, outcome: &str, turn_id: Option<String>) {
        if let Some(request_id) = &msg.request_id {
            let outcome = truncate_text(outcome, MAX_OUTCOME_CHARS);
            self.append(
                msg,
                request_id,
                InboxState::Done,
                Some(outcome),
                turn_id,
                None,
            );
        }
    }

    /// Record `Done` for a request known only by id (used by startup reconciliation).
    pub fn mark_entry_done(&self, entry: &InboxEntry, outcome: Option<String>) {
        let written = self.journal.append(JournalRecord::Inbox {
            request_id: entry.request_id.clone(),
            channel: entry.channel.clone(),
            chat_id: entry.chat_id.clone(),
            session_key: entry.session_key.clone(),
            state: InboxState::Done,
            outcome: outcome.map(|text| truncate_text(&text, MAX_OUTCOME_CHARS)),
            turn_id: entry.turn_id.clone(),
            message: None,
        });
        if let Err(e) = written {
            log::error!("inbox ledger: could not close {}: {e}", entry.request_id);
        }
    }

    fn append(
        &self,
        msg: &InboundMessage,
        request_id: &str,
        state: InboxState,
        outcome: Option<String>,
        turn_id: Option<String>,
        message: Option<Value>,
    ) {
        let sync = state == InboxState::Done || state == InboxState::Queued;
        let record = JournalRecord::Inbox {
            request_id: request_id.to_string(),
            channel: msg.channel.clone(),
            chat_id: msg.chat_id.clone(),
            session_key: msg.session_key(),
            state,
            outcome,
            turn_id,
            message,
        };
        let written = if sync {
            self.journal.append_synced(record)
        } else {
            self.journal.append(record)
        };
        if let Err(e) = written {
            log::error!("inbox ledger: journal write failed for {request_id}: {e}");
        }
    }

    /// Shrink the ledger: one record per request, no stored message once
    /// `Done`, `Done` entries older than `ttl` dropped, and at most
    /// `max_entries` entries kept (oldest `Done` ones go first; unfinished
    /// requests are never evicted). An evicted request id is reprocessed if it
    /// is delivered again.
    pub fn compact(
        &self,
        now: DateTime<Utc>,
        ttl: Duration,
        max_entries: usize,
    ) -> std::io::Result<()> {
        self.journal.rewrite(|entries| {
            let ledger = fold_entries(&entries);
            let keep = retained_entries(ledger, now, ttl, max_entries);
            let mut result: Vec<JournalEntry> = entries
                .into_iter()
                .filter(|entry| !matches!(entry.record, JournalRecord::Inbox { .. }))
                .collect();
            // Re-create one record per retained request, after the other records.
            let last_seq = result.iter().map(|entry| entry.seq).max().unwrap_or(0);
            for (offset, entry) in keep.into_iter().enumerate() {
                result.push(JournalEntry {
                    v: crate::session::journal::JOURNAL_SCHEMA_VERSION,
                    seq: last_seq + 1 + offset as u64,
                    ts: entry.updated_at.clone(),
                    record: JournalRecord::Inbox {
                        request_id: entry.request_id,
                        channel: entry.channel,
                        chat_id: entry.chat_id,
                        session_key: entry.session_key,
                        state: entry.state,
                        outcome: entry.outcome,
                        turn_id: entry.turn_id,
                        message: entry.message,
                    },
                });
            }
            result
        })
    }
}

/// Apply the eviction policy to folded ledger entries (order preserved).
fn retained_entries(
    entries: Vec<InboxEntry>,
    now: DateTime<Utc>,
    ttl: Duration,
    max_entries: usize,
) -> Vec<InboxEntry> {
    let expired = |entry: &InboxEntry| {
        entry.state == InboxState::Done
            && DateTime::parse_from_rfc3339(&entry.updated_at).is_ok_and(|updated| {
                now.signed_duration_since(updated.with_timezone(&Utc))
                    .to_std()
                    .is_ok_and(|age| age > ttl)
            })
    };
    let mut kept: Vec<InboxEntry> = entries
        .into_iter()
        .filter(|entry| !expired(entry))
        .collect();
    let mut excess = kept.len().saturating_sub(max_entries);
    kept.retain(|entry| {
        if excess > 0 && entry.state == InboxState::Done {
            excess -= 1;
            false
        } else {
            true
        }
    });
    kept
}

/// [`InboundGuard`] that runs every published message through its session's ledger.
pub struct LedgerGuard {
    store: Arc<dyn JournalStore>,
}

impl LedgerGuard {
    /// Guard backed by the ledgers in `store`.
    pub fn new(store: Arc<dyn JournalStore>) -> Self {
        Self { store }
    }
}

impl InboundGuard for LedgerGuard {
    fn admit(&self, msg: &InboundMessage) -> InboundAdmission {
        let ledger = InboxLedger::for_session(&self.store, &msg.session_key());
        match ledger.admit(msg) {
            Admission::New => InboundAdmission::Accept,
            Admission::Duplicate {
                state: InboxState::Done,
                outcome: Some(content),
            } if !content.is_empty() => {
                log::info!(
                    "redelivered request {:?}: replaying the stored reply",
                    msg.request_id
                );
                let mut metadata = msg.metadata.clone();
                metadata.insert("_redelivered".into(), Value::Bool(true));
                InboundAdmission::Reply(OutboundMessage {
                    channel: msg.channel.clone(),
                    chat_id: msg.chat_id.clone(),
                    content,
                    reply_to: None,
                    media: vec![],
                    metadata,
                    event: None,
                })
            }
            Admission::Duplicate { .. } => {
                log::info!("redelivered request {:?}: dropped", msg.request_id);
                InboundAdmission::Drop
            }
        }
    }
}

/// What startup reconciliation does with one unfinished ledger entry.
#[derive(Debug, Clone, PartialEq)]
pub enum RedriveAction {
    /// No turn ever started for it: feed the stored message back to the agent.
    Redrive(InboxEntry),
    /// Its turn already ended (or the message is gone): mark it `Done`.
    Close(InboxEntry),
    /// A turn for it is still unfinished: resume or restore owns it, hands off.
    Keep(InboxEntry),
}

impl RedriveAction {
    /// The ledger entry the action is about.
    pub fn entry(&self) -> &InboxEntry {
        match self {
            Self::Redrive(entry) | Self::Close(entry) | Self::Keep(entry) => entry,
        }
    }
}

/// Reconcile the ledger of one journal with the turns recorded beside it.
///
/// `Queued` entries, and `Processing` entries whose turn never wrote a
/// `TurnStart`, are re-driven in ledger order. A `Processing` entry whose
/// turns all ended is closed; one with a still-unfinished turn is left to
/// resume. A turn abandoned for crash-looping therefore never blocks the
/// queue: its request is closed and the entries behind it are re-driven.
pub fn plan_redrive(entries: &[JournalEntry]) -> Vec<RedriveAction> {
    let turns_of = |request_id: &str| -> Vec<&str> {
        entries
            .iter()
            .filter_map(|entry| match &entry.record {
                JournalRecord::TurnStart {
                    turn_id,
                    request_id: Some(id),
                    ..
                } if id == request_id => Some(turn_id.as_str()),
                _ => None,
            })
            .collect()
    };
    let has_ended = |turn_id: &str| {
        entries.iter().any(|entry| match &entry.record {
            JournalRecord::TurnEnd { turn_id: id, .. }
            | JournalRecord::TurnRestored { turn_id: id } => id == turn_id,
            _ => false,
        })
    };
    fold_entries(entries)
        .into_iter()
        .filter(|entry| entry.state != InboxState::Done)
        .map(|entry| {
            let turns = turns_of(&entry.request_id);
            if turns.is_empty() {
                if entry.message.is_some() {
                    RedriveAction::Redrive(entry)
                } else {
                    RedriveAction::Close(entry)
                }
            } else if turns.iter().all(|turn| has_ended(turn)) {
                RedriveAction::Close(entry)
            } else {
                RedriveAction::Keep(entry)
            }
        })
        .collect()
}

/// Number of requests that were accepted but not finished in `store`.
pub fn count_pending(store: &dyn JournalStore) -> usize {
    store
        .scan_all()
        .iter()
        .map(|scan| {
            fold_entries(&scan.entries)
                .iter()
                .filter(|entry| entry.state != InboxState::Done)
                .count()
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::journal::InMemoryJournalStore;
    use chrono::Duration as ChronoDuration;
    use std::collections::HashMap;

    fn message(request_id: Option<&str>) -> InboundMessage {
        InboundMessage {
            channel: "chan".into(),
            sender_id: "u".into(),
            chat_id: "chat".into(),
            content: "hello".into(),
            timestamp: Utc::now(),
            media: vec![],
            metadata: HashMap::new(),
            session_key_override: None,
            request_id: request_id.map(str::to_string),
        }
    }

    fn ledger() -> InboxLedger {
        InboxLedger::new(RunJournal::new(InMemoryJournalStore::new(), "k"))
    }

    #[test]
    fn a_new_request_is_queued_and_a_redelivery_is_a_duplicate() {
        let ledger = ledger();
        let msg = message(Some("r1"));

        assert_eq!(ledger.admit(&msg), Admission::New);
        assert_eq!(
            ledger.admit(&msg),
            Admission::Duplicate {
                state: InboxState::Queued,
                outcome: None
            }
        );
        assert_eq!(ledger.pending().len(), 1);
    }

    #[test]
    fn a_done_request_returns_its_original_outcome() {
        let ledger = ledger();
        let msg = message(Some("r1"));
        ledger.admit(&msg);
        ledger.mark_processing(&msg);
        ledger.mark_done(&msg, "the reply", Some("t1".into()));

        assert_eq!(
            ledger.admit(&msg),
            Admission::Duplicate {
                state: InboxState::Done,
                outcome: Some("the reply".into())
            }
        );
        assert!(ledger.pending().is_empty());
        assert_eq!(ledger.entries()[0].turn_id.as_deref(), Some("t1"));
    }

    #[test]
    fn processing_duplicates_are_reported_as_in_progress() {
        let ledger = ledger();
        let msg = message(Some("r1"));
        ledger.admit(&msg);
        ledger.mark_processing(&msg);
        assert!(matches!(
            ledger.admit(&msg),
            Admission::Duplicate {
                state: InboxState::Processing,
                ..
            }
        ));
    }

    #[test]
    fn messages_without_a_request_id_pass_through_and_leave_no_record() {
        let ledger = ledger();
        let msg = message(None);
        assert_eq!(ledger.admit(&msg), Admission::New);
        assert_eq!(ledger.admit(&msg), Admission::New);
        ledger.mark_processing(&msg);
        ledger.mark_done(&msg, "x", None);
        assert!(ledger.entries().is_empty());
    }

    #[test]
    fn queued_entries_keep_the_message_for_redrive_and_done_entries_drop_it() {
        let ledger = ledger();
        let msg = message(Some("r1"));
        ledger.admit(&msg);

        let stored = ledger.pending()[0].message.clone().unwrap();
        let restored: InboundMessage = serde_json::from_value(stored).unwrap();
        assert_eq!(restored.content, "hello");
        assert_eq!(restored.request_id.as_deref(), Some("r1"));

        ledger.mark_done(&msg, "ok", None);
        assert_eq!(ledger.entries()[0].message, None);
    }

    #[test]
    fn outcomes_are_truncated() {
        let ledger = ledger();
        let msg = message(Some("r1"));
        ledger.admit(&msg);
        ledger.mark_done(&msg, &"x".repeat(MAX_OUTCOME_CHARS * 3), None);
        let outcome = ledger.entries()[0].outcome.clone().unwrap();
        assert!(outcome.len() < MAX_OUTCOME_CHARS + 64);
    }

    #[test]
    fn the_same_request_id_in_two_sessions_is_independent() {
        let store: Arc<dyn JournalStore> = InMemoryJournalStore::new();
        let first = InboxLedger::for_session(&store, "a");
        let second = InboxLedger::for_session(&store, "b");
        assert_eq!(first.admit(&message(Some("r1"))), Admission::New);
        assert_eq!(second.admit(&message(Some("r1"))), Admission::New);
    }

    #[test]
    fn compaction_applies_ttl_and_cap_but_never_drops_unfinished_requests() {
        let ledger = ledger();
        for id in ["old", "mid", "new"] {
            let msg = message(Some(id));
            ledger.admit(&msg);
            ledger.mark_done(&msg, id, None);
        }
        ledger.admit(&message(Some("open")));

        // Everything is "now"-aged: only the cap applies.
        ledger
            .compact(Utc::now(), Duration::from_secs(3600), 3)
            .unwrap();
        let ids: Vec<String> = ledger.entries().into_iter().map(|e| e.request_id).collect();
        assert_eq!(ids, ["mid", "new", "open"]);

        // Far in the future: every Done entry is past its TTL, the open one stays.
        ledger
            .compact(
                Utc::now() + ChronoDuration::days(30),
                Duration::from_secs(3600),
                100,
            )
            .unwrap();
        let ids: Vec<String> = ledger.entries().into_iter().map(|e| e.request_id).collect();
        assert_eq!(ids, ["open"]);
        // An evicted id is processed again.
        assert_eq!(ledger.admit(&message(Some("old"))), Admission::New);
    }

    #[test]
    fn compaction_collapses_records_and_keeps_the_other_journal_records() {
        let ledger = ledger();
        let journal = ledger.journal.clone();
        journal
            .append(JournalRecord::TurnEnd {
                turn_id: "t".into(),
                outcome: crate::session::journal::TurnOutcome::Cancelled,
            })
            .unwrap();
        let msg = message(Some("r1"));
        ledger.admit(&msg);
        ledger.mark_processing(&msg);
        ledger.mark_done(&msg, "ok", None);

        ledger
            .compact(Utc::now(), Duration::from_secs(3600), 10)
            .unwrap();

        let entries = journal.scan().entries;
        assert_eq!(
            entries.len(),
            2,
            "one TurnEnd plus one collapsed ledger record"
        );
        assert!(journal.scan().seq_gaps.is_empty());
        assert_eq!(ledger.entries()[0].state, InboxState::Done);
    }

    #[test]
    fn guard_accepts_new_drops_in_flight_and_replays_done() {
        let store: Arc<dyn JournalStore> = InMemoryJournalStore::new();
        let guard = LedgerGuard::new(Arc::clone(&store));
        let msg = message(Some("r1"));

        assert!(matches!(guard.admit(&msg), InboundAdmission::Accept));
        assert!(matches!(guard.admit(&msg), InboundAdmission::Drop));

        let ledger = InboxLedger::for_session(&store, &msg.session_key());
        ledger.mark_done(&msg, "the reply", None);
        match guard.admit(&msg) {
            InboundAdmission::Reply(reply) => {
                assert_eq!(reply.content, "the reply");
                assert_eq!(reply.metadata["_redelivered"], Value::Bool(true));
                assert_eq!(
                    (reply.channel.as_str(), reply.chat_id.as_str()),
                    ("chan", "chat")
                );
            }
            other => panic!("expected a stored reply, got {other:?}"),
        }
        assert!(matches!(
            guard.admit(&message(None)),
            InboundAdmission::Accept
        ));
    }

    fn turn_start(turn: &str, request_id: &str) -> JournalRecord {
        JournalRecord::TurnStart {
            turn_id: turn.into(),
            session_key: "k".into(),
            request_id: Some(request_id.into()),
            user_message: None,
            attempt: 0,
            channel: None,
            chat_id: None,
        }
    }

    fn actions_for(ledger: &InboxLedger) -> Vec<RedriveAction> {
        plan_redrive(&ledger.journal.scan().entries)
    }

    #[test]
    fn queued_requests_are_redriven_in_order_and_done_ones_are_ignored() {
        let ledger = ledger();
        for id in ["first", "second", "third"] {
            ledger.admit(&message(Some(id)));
        }
        ledger.mark_done(&message(Some("second")), "ok", None);

        let ids: Vec<String> = actions_for(&ledger)
            .into_iter()
            .map(|action| match action {
                RedriveAction::Redrive(entry) => entry.request_id,
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(ids, ["first", "third"]);
    }

    #[test]
    fn a_processing_request_without_a_turn_is_redriven() {
        let ledger = ledger();
        let msg = message(Some("r1"));
        ledger.admit(&msg);
        ledger.mark_processing(&msg);
        assert!(matches!(
            actions_for(&ledger)[..],
            [RedriveAction::Redrive(_)]
        ));
    }

    #[test]
    fn a_request_whose_turn_is_unfinished_is_left_to_resume() {
        let ledger = ledger();
        let msg = message(Some("r1"));
        ledger.admit(&msg);
        ledger.mark_processing(&msg);
        ledger.journal.append(turn_start("t1", "r1")).unwrap();
        assert!(matches!(actions_for(&ledger)[..], [RedriveAction::Keep(_)]));
    }

    #[test]
    fn a_request_whose_turns_all_ended_is_closed_not_redriven() {
        let ledger = ledger();
        let msg = message(Some("r1"));
        ledger.admit(&msg);
        ledger.mark_processing(&msg);
        ledger.journal.append(turn_start("t1", "r1")).unwrap();
        ledger
            .journal
            .append(JournalRecord::TurnEnd {
                turn_id: "t1".into(),
                outcome: crate::session::journal::TurnOutcome::Abandoned,
            })
            .unwrap();
        assert!(matches!(
            actions_for(&ledger)[..],
            [RedriveAction::Close(_)]
        ));
    }

    #[test]
    fn count_pending_spans_all_journals() {
        let store: Arc<dyn JournalStore> = InMemoryJournalStore::new();
        InboxLedger::for_session(&store, "a").admit(&message(Some("r1")));
        InboxLedger::for_session(&store, "b").admit(&message(Some("r2")));
        assert_eq!(count_pending(store.as_ref()), 2);
    }
}
