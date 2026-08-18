//! Session-metadata helpers for explicit sustained goals — an objective the
//! agent tracks across many turns. Port of `nanobot/session/goal_state.py`,
//! scoped to what's needed given `/goal` executes directly rather than
//! tagging-and-falling-through (see the plan): no legacy-key migration, no
//! `explicit_goal_requested`/`sustained_goal_turn`/`runner_wall_llm_timeout_s`,
//! no turn-scoped mutation permission gate.

use std::collections::HashMap;

use chrono::Utc;
use serde_json::{Map, Value, json};

use crate::session::manager::{Session, SessionManager};

pub use crate::session::keys::GOAL_STATE_KEY;
pub const MAX_GOAL_OBJECTIVE_CHARS: usize = 4000;
const MAX_GOAL_OBJECTIVE_WS_CHARS: usize = 600; // nanobot's `_MAX_OBJECTIVE_WS`

/// Errors from [`create_goal`]/[`update_goal`]. Mirrors the plain-string
/// errors nanobot's tools return today, but typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoalError {
    AlreadyActive,
    EmptyObjective,
    NoActiveGoal,
}

impl std::fmt::Display for GoalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyActive => write!(f, "A goal is already active for this chat."),
            Self::EmptyObjective => write!(f, "Objective must not be empty."),
            Self::NoActiveGoal => write!(f, "No active goal to update."),
        }
    }
}

impl std::error::Error for GoalError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoalUpdateAction {
    Complete,
    Cancel,
    Block,
    Replace,
}

impl GoalUpdateAction {
    fn status_label(&self) -> &'static str {
        match self {
            Self::Complete => "completed",
            Self::Cancel => "cancelled",
            Self::Block => "blocked",
            Self::Replace => "active",
        }
    }
}

fn goal_object(metadata: &HashMap<String, Value>) -> Option<&Map<String, Value>> {
    metadata.get(GOAL_STATE_KEY)?.as_object()
}

/// Whether this session has an active sustained objective. Mirrors
/// `sustained_goal_active`.
pub fn sustained_goal_active(metadata: &HashMap<String, Value>) -> bool {
    goal_object(metadata)
        .and_then(|g| g.get("status"))
        .and_then(Value::as_str)
        == Some("active")
}

/// Lines appended inside the runtime context block when a goal is active.
/// Mirrors `goal_state_runtime_lines`.
pub fn goal_state_runtime_lines(metadata: &HashMap<String, Value>) -> Vec<String> {
    let Some(goal) = goal_object(metadata) else {
        return Vec::new();
    };
    if goal.get("status").and_then(Value::as_str) != Some("active") {
        return Vec::new();
    }
    let objective = goal
        .get("objective")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if objective.is_empty() {
        return vec!["Goal: active (no objective text stored).".to_string()];
    }
    let objective = if objective.chars().count() > MAX_GOAL_OBJECTIVE_CHARS {
        let truncated: String = objective.chars().take(MAX_GOAL_OBJECTIVE_CHARS).collect();
        format!("{}\n… (truncated)", truncated.trim_end())
    } else {
        objective.to_string()
    };
    let mut out = vec!["Goal (active):".to_string(), objective];
    if let Some(summary) = goal.get("ui_summary").and_then(Value::as_str) {
        let summary = summary.trim();
        if !summary.is_empty() {
            out.push(format!("Summary: {summary}"));
        }
    }
    out
}

/// JSON-safe snapshot for WebSocket `goal_state` events (one chat_id per
/// frame). Not yet wired to anything (no `RuntimeEventBus`/live push exists —
/// see the plan) — kept as a self-contained primitive for when it does.
/// Mirrors `goal_state_ws_blob`.
pub fn goal_state_ws_blob(metadata: &HashMap<String, Value>) -> Value {
    let Some(goal) = goal_object(metadata) else {
        return json!({"active": false});
    };
    if goal.get("status").and_then(Value::as_str) != Some("active") {
        return json!({"active": false});
    }
    let mut blob = Map::new();
    blob.insert("active".to_string(), Value::Bool(true));
    if let Some(summary) = goal.get("ui_summary").and_then(Value::as_str) {
        let summary: String = summary.trim().chars().take(120).collect();
        if !summary.is_empty() {
            blob.insert("ui_summary".to_string(), Value::String(summary));
        }
    }
    if let Some(objective) = goal.get("objective").and_then(Value::as_str) {
        let objective = objective.trim();
        if !objective.is_empty() {
            let objective = if objective.chars().count() > MAX_GOAL_OBJECTIVE_WS_CHARS {
                let truncated: String = objective
                    .chars()
                    .take(MAX_GOAL_OBJECTIVE_WS_CHARS)
                    .collect();
                format!("{}…", truncated.trim_end())
            } else {
                objective.to_string()
            };
            blob.insert("objective".to_string(), Value::String(objective));
        }
    }
    Value::Object(blob)
}

/// Start a new sustained goal for `session`. Mirrors `CreateGoalTool.execute`'s
/// validation (minus the dropped permission gate / event publish — see the plan).
pub fn create_goal(
    session: &mut Session,
    objective: &str,
    ui_summary: Option<&str>,
) -> Result<(), GoalError> {
    if sustained_goal_active(&session.metadata) {
        return Err(GoalError::AlreadyActive);
    }
    let objective = objective.trim();
    if objective.is_empty() {
        return Err(GoalError::EmptyObjective);
    }
    let objective: String = objective.chars().take(MAX_GOAL_OBJECTIVE_CHARS).collect();

    let mut blob = Map::new();
    blob.insert("status".to_string(), Value::String("active".to_string()));
    blob.insert("objective".to_string(), Value::String(objective));
    if let Some(summary) = ui_summary.map(str::trim).filter(|s| !s.is_empty()) {
        blob.insert("ui_summary".to_string(), Value::String(summary.to_string()));
    }
    blob.insert(
        "started_at".to_string(),
        Value::String(Utc::now().to_rfc3339()),
    );
    session
        .metadata
        .insert(GOAL_STATE_KEY.to_string(), Value::Object(blob));
    Ok(())
}

/// Complete/cancel/block/replace the active goal for `session`. Mirrors
/// `UpdateGoalTool.execute`'s four branches (minus the dropped permission
/// gate on `replace` / event publish — see the plan). Returns a
/// human-readable confirmation string.
pub fn update_goal(
    session: &mut Session,
    action: GoalUpdateAction,
    recap: Option<&str>,
    objective: Option<&str>,
    ui_summary: Option<&str>,
) -> Result<String, GoalError> {
    if !sustained_goal_active(&session.metadata) {
        return Err(GoalError::NoActiveGoal);
    }
    let recap = recap.map(str::trim).filter(|s| !s.is_empty());

    if action == GoalUpdateAction::Replace {
        let new_objective = objective
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or(GoalError::EmptyObjective)?;
        let new_objective: String = new_objective
            .chars()
            .take(MAX_GOAL_OBJECTIVE_CHARS)
            .collect();

        // The existing objective becomes `previous_objective` on the fresh blob.
        let previous_objective = session
            .metadata
            .get(GOAL_STATE_KEY)
            .and_then(Value::as_object)
            .and_then(|g| g.get("objective"))
            .and_then(Value::as_str)
            .map(str::to_string);

        let mut blob = Map::new();
        blob.insert("status".to_string(), Value::String("active".to_string()));
        blob.insert("objective".to_string(), Value::String(new_objective));
        if let Some(summary) = ui_summary.map(str::trim).filter(|s| !s.is_empty()) {
            blob.insert("ui_summary".to_string(), Value::String(summary.to_string()));
        }
        if let Some(previous) = previous_objective {
            blob.insert("previous_objective".to_string(), Value::String(previous));
        }
        if let Some(recap) = recap {
            blob.insert("recap".to_string(), Value::String(recap.to_string()));
        }
        blob.insert(
            "replaced_at".to_string(),
            Value::String(Utc::now().to_rfc3339()),
        );
        session
            .metadata
            .insert(GOAL_STATE_KEY.to_string(), Value::Object(blob));
        return Ok("Goal replaced.".to_string());
    }

    let ended_at = Utc::now().to_rfc3339();
    let status_label = action.status_label();
    if let Some(goal) = session
        .metadata
        .get_mut(GOAL_STATE_KEY)
        .and_then(Value::as_object_mut)
    {
        goal.insert(
            "status".to_string(),
            Value::String(status_label.to_string()),
        );
        goal.insert("ended_at".to_string(), Value::String(ended_at.clone()));
        if let Some(recap) = recap {
            goal.insert("recap".to_string(), Value::String(recap.to_string()));
        }
    }
    let tail = recap.unwrap_or("(none)");
    Ok(format!(
        "Goal marked {status_label} ({ended_at}). Recap:\n{tail}"
    ))
}

/// Get-or-create `session_key`, run [`create_goal`], and persist. Shared by
/// `AgentLoop::create_session_goal` (the command path) — the single place
/// this dance is written.
pub fn create_session_goal(
    session_manager: &mut SessionManager,
    session_key: &str,
    objective: &str,
    ui_summary: Option<&str>,
) -> Result<(), GoalError> {
    let session = session_manager.get_or_create_session(session_key);
    create_goal(session, objective, ui_summary)?;
    let snapshot = session.clone();
    if let Err(e) = session_manager.save(snapshot) {
        log::error!("Failed to save session after creating goal: {e}");
    }
    Ok(())
}

/// Get-or-create `session_key`, run [`update_goal`], and persist. Shared by
/// `AgentLoop::update_session_goal` (the command path) and `UpdateGoalTool`
/// (the tool path) — the single place this dance is written.
pub fn update_session_goal(
    session_manager: &mut SessionManager,
    session_key: &str,
    action: GoalUpdateAction,
    recap: Option<&str>,
    objective: Option<&str>,
    ui_summary: Option<&str>,
) -> Result<String, GoalError> {
    let session = session_manager.get_or_create_session(session_key);
    let result = update_goal(session, action, recap, objective, ui_summary)?;
    let snapshot = session.clone();
    if let Err(e) = session_manager.save(snapshot) {
        log::error!("Failed to save session after updating goal: {e}");
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> Session {
        Session::new("test:session".to_string())
    }

    #[test]
    fn create_goal_rejects_empty_objective() {
        let mut s = session();
        assert_eq!(
            create_goal(&mut s, "   ", None),
            Err(GoalError::EmptyObjective)
        );
    }

    #[test]
    fn create_goal_truncates_long_objective() {
        let mut s = session();
        let long_objective = "x".repeat(MAX_GOAL_OBJECTIVE_CHARS + 500);
        create_goal(&mut s, &long_objective, None).unwrap();
        let stored = s.metadata[GOAL_STATE_KEY]["objective"].as_str().unwrap();
        assert_eq!(stored.chars().count(), MAX_GOAL_OBJECTIVE_CHARS);
    }

    #[test]
    fn create_goal_refuses_when_already_active() {
        let mut s = session();
        create_goal(&mut s, "first objective", None).unwrap();
        assert_eq!(
            create_goal(&mut s, "second objective", None),
            Err(GoalError::AlreadyActive)
        );
    }

    #[test]
    fn sustained_goal_active_reflects_status() {
        let mut s = session();
        assert!(!sustained_goal_active(&s.metadata));
        create_goal(&mut s, "objective", None).unwrap();
        assert!(sustained_goal_active(&s.metadata));
        update_goal(&mut s, GoalUpdateAction::Complete, None, None, None).unwrap();
        assert!(!sustained_goal_active(&s.metadata));
    }

    #[test]
    fn update_goal_errors_when_nothing_active() {
        let mut s = session();
        assert_eq!(
            update_goal(&mut s, GoalUpdateAction::Complete, None, None, None),
            Err(GoalError::NoActiveGoal)
        );
    }

    #[test]
    fn update_goal_complete_cancel_block_set_status_and_recap() {
        for (action, label) in [
            (GoalUpdateAction::Complete, "completed"),
            (GoalUpdateAction::Cancel, "cancelled"),
            (GoalUpdateAction::Block, "blocked"),
        ] {
            let mut s = session();
            create_goal(&mut s, "objective", None).unwrap();
            let msg = update_goal(&mut s, action, Some("done deal"), None, None).unwrap();
            assert!(msg.contains(label), "expected '{label}' in {msg}");
            assert!(msg.contains("done deal"));
            assert_eq!(s.metadata[GOAL_STATE_KEY]["status"], label);
            assert_eq!(s.metadata[GOAL_STATE_KEY]["recap"], "done deal");
        }
    }

    #[test]
    fn update_goal_replace_requires_new_objective() {
        let mut s = session();
        create_goal(&mut s, "objective", None).unwrap();
        assert_eq!(
            update_goal(&mut s, GoalUpdateAction::Replace, None, None, None),
            Err(GoalError::EmptyObjective)
        );
    }

    #[test]
    fn update_goal_replace_carries_previous_objective_and_stays_active() {
        let mut s = session();
        create_goal(&mut s, "old objective", None).unwrap();
        update_goal(
            &mut s,
            GoalUpdateAction::Replace,
            Some("pivoting"),
            Some("new objective"),
            None,
        )
        .unwrap();
        assert!(sustained_goal_active(&s.metadata));
        assert_eq!(s.metadata[GOAL_STATE_KEY]["objective"], "new objective");
        assert_eq!(
            s.metadata[GOAL_STATE_KEY]["previous_objective"],
            "old objective"
        );
        assert_eq!(s.metadata[GOAL_STATE_KEY]["recap"], "pivoting");
    }

    #[test]
    fn goal_state_runtime_lines_empty_when_no_active_goal() {
        let s = session();
        assert!(goal_state_runtime_lines(&s.metadata).is_empty());
    }

    #[test]
    fn goal_state_runtime_lines_include_objective_and_summary() {
        let mut s = session();
        create_goal(&mut s, "refactor the auth module", Some("auth refactor")).unwrap();
        let lines = goal_state_runtime_lines(&s.metadata);
        assert!(lines.iter().any(|l| l.contains("refactor the auth module")));
        assert!(lines.iter().any(|l| l.contains("auth refactor")));
    }

    #[test]
    fn goal_state_ws_blob_reflects_active_state() {
        let mut s = session();
        assert_eq!(goal_state_ws_blob(&s.metadata), json!({"active": false}));
        create_goal(&mut s, "objective", None).unwrap();
        let blob = goal_state_ws_blob(&s.metadata);
        assert_eq!(blob["active"], true);
        assert_eq!(blob["objective"], "objective");
    }

    #[test]
    fn create_session_goal_and_update_session_goal_persist_across_manager_calls() {
        let dir = tempfile::tempdir().unwrap();
        let mut manager = SessionManager::new(dir.path().to_path_buf());
        create_session_goal(&mut manager, "cli:direct", "ship the feature", None).unwrap();
        assert!(sustained_goal_active(
            &manager.get_or_create_session("cli:direct").metadata
        ));

        let msg = update_session_goal(
            &mut manager,
            "cli:direct",
            GoalUpdateAction::Complete,
            Some("shipped"),
            None,
            None,
        )
        .unwrap();
        assert!(msg.contains("completed"));
        assert!(!sustained_goal_active(
            &manager.get_or_create_session("cli:direct").metadata
        ));
    }
}
