//! The child-agents inventory shown by `/subagents` and `rust-bot subagents`.
//!
//! [`list_acp_rows`] joins the stored definitions ([`ChildAgentStore`]) with the
//! remembered sessions ([`ChildSessionIndex`]) and a live "is this child running
//! in this process" probe; [`format_acp_agents`] renders the rows. Both surfaces
//! share them: the chat command passes [`AcpManager::is_busy`] as the probe, the
//! standalone CLI (another process, so no view of the locks) passes `|_| false`.
//!
//! [`AcpManager::is_busy`]: crate::agent::acp::manager::AcpManager::is_busy

use chrono::{DateTime, Utc};

use crate::agent::acp::session_index::{ChildSessionIndex, SessionEntry};
use crate::agent::acp::store::{AgentSummary, ChildAgentStore};
use crate::utils::relative_time::format_relative_rfc3339;

/// Longest purpose shown on a row; the full text is in `acp_list_agents`.
const PURPOSE_MAX_CHARS: usize = 100;

/// What can be said about a child *definition* right now. This is not a run
/// status: `Idle` is not a failure and `RunningHere` is a snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcpAgentState {
    /// This process holds the child's run lock.
    RunningHere,
    /// A child session is remembered and no run is in progress here.
    Idle,
    /// No child session has ever been remembered for this agent.
    NeverRun,
    /// The session index could not be read, so `Idle`/`NeverRun` is unknown.
    Unknown,
}

impl AcpAgentState {
    /// Lower-case label shown in listings.
    pub fn as_str(&self) -> &'static str {
        match self {
            AcpAgentState::RunningHere => "running here",
            AcpAgentState::Idle => "idle",
            AcpAgentState::NeverRun => "never run",
            AcpAgentState::Unknown => "unknown",
        }
    }
}

/// One child agent as listed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcpAgentRow {
    pub name: String,
    pub purpose: String,
    pub preset: String,
    pub created_at: String,
    pub created_by: String,
    pub depth: u32,
    pub cwd: Option<String>,
    /// RFC 3339 time of the most recent remembered session use.
    pub last_used: Option<String>,
    /// The ACP session id of that most recent use.
    pub child_session: Option<String>,
    pub state: AcpAgentState,
}

/// The most recently used entry among `entries` (an agent has one per parent session).
fn most_recent_entry<'a>(entries: &[&'a SessionEntry]) -> Option<&'a SessionEntry> {
    entries
        .iter()
        .copied()
        .max_by_key(|entry| entry.last_used.parse::<DateTime<Utc>>().ok())
}

/// Build the rows for `summaries`. `entries` is the session index, or `None`
/// when it could not be read; `is_busy(name)` tells whether this process is
/// running that child now.
pub fn build_acp_rows(
    summaries: &[AgentSummary],
    entries: Option<&[SessionEntry]>,
    is_busy: impl Fn(&str) -> bool,
) -> Vec<AcpAgentRow> {
    summaries
        .iter()
        .map(|summary| {
            let meta = &summary.meta;
            let own_entries: Vec<&SessionEntry> = entries
                .unwrap_or_default()
                .iter()
                .filter(|entry| entry.agent == meta.name)
                .collect();
            let latest = most_recent_entry(&own_entries);
            let state = if is_busy(&meta.name) {
                AcpAgentState::RunningHere
            } else if entries.is_none() {
                AcpAgentState::Unknown
            } else if latest.is_some() {
                AcpAgentState::Idle
            } else {
                AcpAgentState::NeverRun
            };
            AcpAgentRow {
                name: meta.name.clone(),
                purpose: meta.purpose.clone(),
                preset: meta.preset.clone(),
                created_at: meta.created_at.clone(),
                created_by: meta.created_by.clone(),
                depth: meta.depth,
                cwd: meta.cwd.clone(),
                last_used: latest.map(|entry| entry.last_used.clone()),
                child_session: latest.map(|entry| entry.acp_session_id.clone()),
                state,
            }
        })
        .collect()
}

/// Read the stored children and their remembered sessions and build the rows.
/// An unreadable index degrades every row to [`AcpAgentState::Unknown`] instead
/// of failing the listing. With no children the index is not touched at all
/// (reading it creates its folder and lock file).
pub fn list_acp_rows(
    store: &ChildAgentStore,
    index: &ChildSessionIndex,
    is_busy: impl Fn(&str) -> bool,
) -> Vec<AcpAgentRow> {
    let summaries = store.list();
    if summaries.is_empty() {
        return Vec::new();
    }
    let entries = match index.entries() {
        Ok(entries) => Some(entries),
        Err(error) => {
            log::warn!("cannot read the child session index: {error}");
            None
        }
    };
    build_acp_rows(&summaries, entries.as_deref(), is_busy)
}

/// Render `rows` as the "Child agents" section of the listing.
pub fn format_acp_agents(rows: &[AcpAgentRow]) -> String {
    format_acp_agents_at(rows, Utc::now())
}

/// [`format_acp_agents`] with an explicit "now", for deterministic tests.
fn format_acp_agents_at(rows: &[AcpAgentRow], now: DateTime<Utc>) -> String {
    if rows.is_empty() {
        return "No child agents.".to_string();
    }
    let mut lines = vec!["Child agents (durable definitions):".to_string()];
    for row in rows {
        lines.push(format!(
            "  {name} [{state}] preset: {preset}, depth: {depth} — {purpose}",
            name = row.name,
            state = row.state.as_str(),
            preset = row.preset,
            depth = row.depth,
            purpose = truncate_purpose(&row.purpose),
        ));
        let last_used = match &row.last_used {
            Some(stamp) => format_relative_rfc3339(now, stamp),
            None => "never".to_string(),
        };
        lines.push(format!(
            "      cwd: {cwd}; created {created} by {created_by}; last used: {last_used}",
            cwd = row.cwd.as_deref().unwrap_or("—"),
            created = format_relative_rfc3339(now, &row.created_at),
            created_by = row.created_by,
        ));
    }
    lines.join("\n")
}

/// `purpose` cut to [`PURPOSE_MAX_CHARS`] characters, `...` appended when cut.
fn truncate_purpose(purpose: &str) -> String {
    if purpose.chars().count() > PURPOSE_MAX_CHARS {
        format!(
            "{}...",
            purpose.chars().take(PURPOSE_MAX_CHARS).collect::<String>()
        )
    } else {
        purpose.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::acp::store::{AgentMeta, Overrides};
    use chrono::Duration;

    fn summary(name: &str, purpose: &str) -> AgentSummary {
        AgentSummary {
            meta: AgentMeta {
                name: name.to_string(),
                purpose: purpose.to_string(),
                preset: "rustbot".to_string(),
                created_at: (Utc::now() - Duration::days(2)).to_rfc3339(),
                created_by: "cli:direct".to_string(),
                depth: 1,
                cwd: Some("C:/work/project".to_string()),
                overrides: Overrides::default(),
                resynced_at: None,
            },
            drifted_files: Vec::new(),
        }
    }

    fn entry(agent: &str, parent_session: &str, session: &str, minutes_ago: i64) -> SessionEntry {
        SessionEntry {
            agent: agent.to_string(),
            parent_session_key: parent_session.to_string(),
            acp_session_id: session.to_string(),
            last_used: (Utc::now() - Duration::minutes(minutes_ago)).to_rfc3339(),
        }
    }

    #[test]
    fn state_is_never_run_without_a_remembered_session() {
        let rows = build_acp_rows(&[summary("docs", "writes docs")], Some(&[]), |_| false);
        assert_eq!(rows[0].state, AcpAgentState::NeverRun);
        assert_eq!(rows[0].last_used, None);
        assert_eq!(rows[0].child_session, None);
    }

    #[test]
    fn state_is_idle_with_a_remembered_session_and_picks_the_latest() {
        let entries = [
            entry("docs", "ws:alice", "old-session", 120),
            entry("docs", "ws:bob", "new-session", 5),
            entry("other", "ws:alice", "unrelated", 1),
        ];
        let rows = build_acp_rows(&[summary("docs", "writes docs")], Some(&entries), |_| false);
        assert_eq!(rows[0].state, AcpAgentState::Idle);
        assert_eq!(rows[0].child_session.as_deref(), Some("new-session"));
    }

    #[test]
    fn a_held_lock_means_running_here_even_without_a_session() {
        let rows = build_acp_rows(
            &[summary("docs", "p"), summary("review", "p")],
            Some(&[]),
            |name| name == "docs",
        );
        assert_eq!(rows[0].state, AcpAgentState::RunningHere);
        assert_eq!(rows[1].state, AcpAgentState::NeverRun);
    }

    #[test]
    fn an_unreadable_index_degrades_to_unknown_but_busy_still_shows() {
        let rows = build_acp_rows(
            &[summary("docs", "p"), summary("review", "p")],
            None,
            |name| name == "review",
        );
        assert_eq!(rows[0].state, AcpAgentState::Unknown);
        assert_eq!(rows[1].state, AcpAgentState::RunningHere);
    }

    #[test]
    fn empty_listing_says_so() {
        assert_eq!(format_acp_agents(&[]), "No child agents.");
    }

    #[test]
    fn format_snapshot_shows_state_definition_and_last_use() {
        let now = Utc::now();
        let mut rows = build_acp_rows(
            &[summary("docs", "writes docs"), summary("review", "reviews")],
            Some(&[]),
            |name| name == "review",
        );
        rows[0].last_used = Some((now - Duration::minutes(7)).to_rfc3339());
        rows[0].state = AcpAgentState::Idle;
        rows[0].created_at = (now - Duration::days(2)).to_rfc3339();
        rows[1].created_at = (now - Duration::days(2)).to_rfc3339();
        rows[1].cwd = None;

        assert_eq!(
            format_acp_agents_at(&rows, now),
            "Child agents (durable definitions):\n\
             \x20 docs [idle] preset: rustbot, depth: 1 — writes docs\n\
             \x20     cwd: C:/work/project; created 2d ago by cli:direct; last used: 7m ago\n\
             \x20 review [running here] preset: rustbot, depth: 1 — reviews\n\
             \x20     cwd: —; created 2d ago by cli:direct; last used: never"
        );
    }

    #[test]
    fn long_and_non_ascii_purposes_are_cut_on_character_boundaries() {
        let purpose = "検査".repeat(100);
        let rows = build_acp_rows(&[summary("jp", &purpose)], Some(&[]), |_| false);
        let text = format_acp_agents(&rows);
        let expected = format!("{}...", "検査".repeat(50));
        assert!(text.contains(&expected));
        assert!(!text.contains(&"検査".repeat(51)));
    }

    #[test]
    fn list_acp_rows_without_children_does_not_touch_the_index() {
        let workspace = tempfile::tempdir().unwrap();
        let store = ChildAgentStore::new(workspace.path());
        let index = ChildSessionIndex::new(workspace.path());
        assert!(list_acp_rows(&store, &index, |_| false).is_empty());
        assert!(!workspace.path().join("acp").exists());
    }
}
