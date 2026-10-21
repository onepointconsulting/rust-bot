//! Startup planning for resuming turns that a crash interrupted.
//!
//! [`plan_resumes`] scans the run journals under a sessions directory and
//! decides, per session, what to do with each unfinished turn. It only plans:
//! executing the plan (replaying safe tools, re-driving the model) is
//! `AgentLoop::resume_interrupted_turns`.
//!
//! Guards, in the order they apply:
//! * a turn with any `TurnEnd` (including `Cancelled` from `/stop`) or a
//!   `TurnRestored` is not unfinished and never appears here;
//! * `auto_resume = false` turns everything off;
//! * turns older than the age limit are reported as stale, not resumed;
//! * a turn that already used up its resume attempts is abandoned instead of
//!   crash-looping the service;
//! * per session only the newest unfinished turn is resumed, older ones are
//!   abandoned (their work still reaches the transcript through restore).

use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::agent::recovery::{
    TurnRecovery, UnfinishedTurn, decide_turn_recovery, unfinished_turns,
};
use crate::session::journal::JournalStore;

/// Limits applied when planning resumes.
#[derive(Debug, Clone)]
pub struct ResumePolicy {
    pub auto_resume: bool,
    /// Turns that started longer ago than this are left alone.
    pub max_age: Duration,
    pub max_attempts: u32,
}

impl ResumePolicy {
    /// Policy from the `durability` config section.
    pub fn from_config(config: &crate::config::schema::DurabilityConfig) -> Self {
        Self {
            auto_resume: config.auto_resume,
            max_age: Duration::from_secs(config.resume_max_age_secs),
            max_attempts: config.max_resume_attempts,
        }
    }
}

/// What to do with one unfinished turn.
#[derive(Debug, Clone, PartialEq)]
pub enum ResumeAction {
    /// Resume as attempt number `attempt`.
    Resume { turn: UnfinishedTurn, attempt: u32 },
    /// Close the turn as abandoned.
    Abandon { turn: UnfinishedTurn },
    /// Too old to act on; left unfinished (restore still merges it on next use).
    Stale { turn: UnfinishedTurn },
}

/// Plan the recovery of every unfinished turn found in `store`.
///
/// Only turns that started before `started_before` (the start of this process)
/// count: a turn begun by the running process is simply in flight, not interrupted.
pub fn plan_resumes(
    store: &dyn JournalStore,
    now: DateTime<Utc>,
    started_before: DateTime<Utc>,
    policy: &ResumePolicy,
) -> Vec<ResumeAction> {
    store
        .scan_all()
        .iter()
        .flat_map(|scan| {
            let interrupted = unfinished_turns(&scan.entries)
                .into_iter()
                .filter(|turn| started_before_cutoff(&turn.started_at, started_before))
                .collect();
            plan_session(interrupted, now, policy)
        })
        .collect()
}

/// Plan one session's unfinished turns (oldest first, as scanned).
pub fn plan_session(
    mut unfinished: Vec<UnfinishedTurn>,
    now: DateTime<Utc>,
    policy: &ResumePolicy,
) -> Vec<ResumeAction> {
    if !policy.auto_resume || unfinished.is_empty() {
        return Vec::new();
    }
    let newest = unfinished.pop().expect("checked non-empty");
    let mut actions: Vec<ResumeAction> = unfinished
        .into_iter()
        .map(|turn| ResumeAction::Abandon { turn })
        .collect();
    actions.push(if is_stale(&newest, now, policy.max_age) {
        ResumeAction::Stale { turn: newest }
    } else {
        match decide_turn_recovery(newest.attempts, policy.max_attempts) {
            TurnRecovery::Resume { attempt } => ResumeAction::Resume {
                turn: newest,
                attempt,
            },
            TurnRecovery::Abandon => ResumeAction::Abandon { turn: newest },
        }
    });
    actions
}

/// Whether `turn` started before `now - max_age`. An unparsable timestamp counts as stale.
fn is_stale(turn: &UnfinishedTurn, now: DateTime<Utc>, max_age: Duration) -> bool {
    let Ok(started) = DateTime::parse_from_rfc3339(&turn.started_at) else {
        return true;
    };
    let age = now.signed_duration_since(started.with_timezone(&Utc));
    age.to_std().is_ok_and(|age| age > max_age)
}

/// Whether the RFC 3339 timestamp `at` is before `cutoff`. Unparsable counts as before.
pub fn started_before_cutoff(at: &str, cutoff: DateTime<Utc>) -> bool {
    DateTime::parse_from_rfc3339(at).is_ok_and(|at| at.with_timezone(&Utc) < cutoff)
        || DateTime::parse_from_rfc3339(at).is_err()
}

/// Number of unfinished turns in `store`, for the one-line startup notice
/// of modes that do not auto-resume.
pub fn count_unfinished_turns(store: &dyn JournalStore) -> usize {
    store
        .scan_all()
        .iter()
        .map(|scan| unfinished_turns(&scan.entries).len())
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::journal::{InMemoryJournalStore, JournalRecord, RunJournal, TurnOutcome};
    use chrono::Duration as ChronoDuration;
    use std::sync::Arc;

    fn policy() -> ResumePolicy {
        ResumePolicy {
            auto_resume: true,
            max_age: Duration::from_secs(3600),
            max_attempts: 2,
        }
    }

    fn turn(id: &str, attempts: u32, age_secs: i64, now: DateTime<Utc>) -> UnfinishedTurn {
        UnfinishedTurn {
            turn_id: id.into(),
            session_key: "k".into(),
            attempts,
            channel: Some("c".into()),
            chat_id: Some("x".into()),
            request_id: None,
            started_at: (now - ChronoDuration::seconds(age_secs)).to_rfc3339(),
        }
    }

    #[test]
    fn a_fresh_turn_is_resumed_as_attempt_one() {
        let now = Utc::now();
        let actions = plan_session(vec![turn("t", 0, 10, now)], now, &policy());
        assert!(matches!(
            actions[..],
            [ResumeAction::Resume { attempt: 1, .. }]
        ));
    }

    #[test]
    fn auto_resume_off_plans_nothing() {
        let now = Utc::now();
        let off = ResumePolicy {
            auto_resume: false,
            ..policy()
        };
        assert!(plan_session(vec![turn("t", 0, 10, now)], now, &off).is_empty());
    }

    #[test]
    fn a_stale_turn_is_reported_not_resumed() {
        let now = Utc::now();
        let actions = plan_session(vec![turn("t", 0, 7200, now)], now, &policy());
        assert!(matches!(actions[..], [ResumeAction::Stale { .. }]));
    }

    #[test]
    fn a_turn_out_of_attempts_is_abandoned() {
        let now = Utc::now();
        let actions = plan_session(vec![turn("t", 2, 10, now)], now, &policy());
        assert!(matches!(actions[..], [ResumeAction::Abandon { .. }]));
    }

    #[test]
    fn only_the_newest_turn_of_a_session_is_resumed() {
        let now = Utc::now();
        let actions = plan_session(
            vec![turn("old", 0, 100, now), turn("new", 0, 10, now)],
            now,
            &policy(),
        );
        assert!(matches!(&actions[0], ResumeAction::Abandon { turn } if turn.turn_id == "old"));
        assert!(matches!(&actions[1], ResumeAction::Resume { turn, .. } if turn.turn_id == "new"));
    }

    #[test]
    fn an_unparsable_start_time_counts_as_stale() {
        let now = Utc::now();
        let mut broken = turn("t", 0, 10, now);
        broken.started_at = "garbage".into();
        let actions = plan_session(vec![broken], now, &policy());
        assert!(matches!(actions[..], [ResumeAction::Stale { .. }]));
    }

    fn start(journal: &RunJournal, id: &str) {
        journal
            .append(JournalRecord::TurnStart {
                turn_id: id.into(),
                session_key: "k".into(),
                request_id: None,
                user_message: None,
                attempt: 0,
                channel: Some("c".into()),
                chat_id: Some("x".into()),
            })
            .unwrap();
    }

    #[test]
    fn scanning_a_store_skips_finished_and_cancelled_turns() {
        let store: Arc<dyn JournalStore> = InMemoryJournalStore::new();
        let journal = RunJournal::new(Arc::clone(&store), "k");
        start(&journal, "completed");
        journal
            .append(JournalRecord::TurnEnd {
                turn_id: "completed".into(),
                outcome: TurnOutcome::Completed,
            })
            .unwrap();
        start(&journal, "stopped");
        journal
            .append(JournalRecord::TurnEnd {
                turn_id: "stopped".into(),
                outcome: TurnOutcome::Cancelled,
            })
            .unwrap();
        start(&journal, "open");

        let later = Utc::now() + ChronoDuration::hours(1);
        let actions = plan_resumes(store.as_ref(), Utc::now(), later, &policy());

        assert_eq!(actions.len(), 1);
        // A turn begun by the running process is not an interrupted one.
        let earlier = Utc::now() - ChronoDuration::hours(1);
        assert!(plan_resumes(store.as_ref(), Utc::now(), earlier, &policy()).is_empty());
        assert!(matches!(&actions[0], ResumeAction::Resume { turn, .. } if turn.turn_id == "open"));
        assert_eq!(count_unfinished_turns(store.as_ref()), 1);
        let empty = InMemoryJournalStore::new();
        assert!(plan_resumes(empty.as_ref(), Utc::now(), later, &policy()).is_empty());
    }

    #[test]
    fn planning_never_references_the_approval_broker() {
        let source = include_str!("resume.rs");
        let (production, _tests) = source.split_once("#[cfg(test)]").unwrap();
        assert!(!production.contains(&["Tool", "Approval", "Broker"].concat()));
    }
}
