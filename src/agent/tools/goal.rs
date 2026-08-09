use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::Value;

use crate::agent::tools::base::Tool;
use crate::session::goal_state::{self, GoalUpdateAction};
use crate::session::manager::SessionManager;

/// Tool to complete/cancel/block/replace the sustained goal active for this
/// chat. Starting a goal is handled by the `/goal` command directly (see the
/// port plan for why `CreateGoalTool` isn't ported as a model-callable
/// tool) — this is the model's own lever for ending or redirecting one during
/// later turns.
pub struct UpdateGoalTool {
    session_manager: Arc<Mutex<SessionManager>>,
    channel: Mutex<String>,
    chat_id: Mutex<String>,
}

impl UpdateGoalTool {
    pub fn new(session_manager: Arc<Mutex<SessionManager>>) -> Self {
        Self {
            session_manager,
            channel: Mutex::new(String::new()),
            chat_id: Mutex::new(String::new()),
        }
    }

    fn session_key(&self) -> Option<String> {
        let channel = self.channel.lock().unwrap().clone();
        let chat_id = self.chat_id.lock().unwrap().clone();
        if channel.is_empty() || chat_id.is_empty() {
            None
        } else {
            Some(format!("{channel}:{chat_id}"))
        }
    }
}

#[async_trait]
impl Tool for UpdateGoalTool {
    fn name(&self) -> String {
        "update_goal".to_string()
    }

    fn description(&self) -> String {
        "Complete, cancel, block, or replace the sustained goal active for this chat. \
         Use when the current objective is finished, abandoned, blocked, or needs to \
         change to a different objective."
            .to_string()
    }

    fn set_tool_context(&self, channel: &str, chat_id: &str, _message_id: Option<&str>) {
        *self.channel.lock().unwrap() = channel.to_string();
        *self.chat_id.lock().unwrap() = chat_id.to_string();
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["complete", "cancel", "block", "replace"],
                    "description": "complete: objective achieved. cancel: abandoned. \
                        block: stuck on something outside your control. \
                        replace: swap in a new objective, keeping the goal active.",
                },
                "recap": {
                    "type": "string",
                    "maxLength": 8000,
                    "description": "Short summary of what happened, shown to the user.",
                },
                "objective": {
                    "type": "string",
                    "description": "Required for replace: the new objective text.",
                },
                "ui_summary": {
                    "type": "string",
                    "maxLength": 120,
                    "description": "Optional for replace: short label for the new objective.",
                },
            },
            "required": ["action"],
        })
    }

    async fn execute(&self, params: &Value) -> String {
        let Some(session_key) = self.session_key() else {
            return "Error: no session context (channel/chat_id)".to_string();
        };
        let action = match params.get("action").and_then(Value::as_str).unwrap_or("") {
            "complete" => GoalUpdateAction::Complete,
            "cancel" => GoalUpdateAction::Cancel,
            "block" => GoalUpdateAction::Block,
            "replace" => GoalUpdateAction::Replace,
            other => return format!("Error: unknown action '{other}'"),
        };
        let recap = params.get("recap").and_then(Value::as_str);
        let objective = params.get("objective").and_then(Value::as_str);
        let ui_summary = params.get("ui_summary").and_then(Value::as_str);

        let mut session_manager = self.session_manager.lock().unwrap_or_else(|e| e.into_inner());
        match goal_state::update_session_goal(
            &mut session_manager,
            &session_key,
            action,
            recap,
            objective,
            ui_summary,
        ) {
            Ok(message) => message,
            Err(e) => format!("Error: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool() -> UpdateGoalTool {
        let dir = tempfile::tempdir().unwrap();
        let manager = SessionManager::new(dir.path().to_path_buf());
        UpdateGoalTool::new(Arc::new(Mutex::new(manager)))
    }

    #[tokio::test]
    async fn execute_without_context_errors() {
        let tool = tool();
        let result = tool.execute(&serde_json::json!({"action": "complete"})).await;
        assert!(result.contains("no session context"), "{result}");
    }

    #[tokio::test]
    async fn execute_without_active_goal_errors() {
        let tool = tool();
        tool.set_tool_context("cli", "direct", None);
        let result = tool.execute(&serde_json::json!({"action": "complete"})).await;
        assert!(result.contains("No active goal"), "{result}");
    }

    #[tokio::test]
    async fn execute_completes_an_active_goal() {
        let tool = tool();
        tool.set_tool_context("cli", "direct", None);
        goal_state::create_session_goal(
            &mut tool.session_manager.lock().unwrap(),
            "cli:direct",
            "ship the feature",
            None,
        )
        .unwrap();

        let result = tool
            .execute(&serde_json::json!({"action": "complete", "recap": "shipped it"}))
            .await;
        assert!(result.contains("completed"), "{result}");
        assert!(result.contains("shipped it"), "{result}");
    }

    #[tokio::test]
    async fn execute_replace_requires_objective() {
        let tool = tool();
        tool.set_tool_context("cli", "direct", None);
        goal_state::create_session_goal(
            &mut tool.session_manager.lock().unwrap(),
            "cli:direct",
            "old objective",
            None,
        )
        .unwrap();

        let result = tool.execute(&serde_json::json!({"action": "replace"})).await;
        assert!(result.contains("Error"), "{result}");
    }

    #[tokio::test]
    async fn execute_rejects_unknown_action() {
        let tool = tool();
        tool.set_tool_context("cli", "direct", None);
        let result = tool.execute(&serde_json::json!({"action": "bogus"})).await;
        assert!(result.contains("unknown action"), "{result}");
    }
}
