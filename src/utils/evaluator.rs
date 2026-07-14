use std::sync::{Arc, LazyLock};

use tera::Context;

use crate::providers::base::LLMProviderDyn;
use crate::utils::prompt_templates::render_template;

/// Post-run evaluation for background tasks (heartbeat & cron).
///
/// After the agent executes a background task, this module makes a lightweight
/// LLM call to decide whether the result warrants notifying the user.

const EVALUATE_TOOL: LazyLock<Vec<serde_json::Value>> = LazyLock::new(|| {
    vec![serde_json::json!({
        "type": "function",
        "function": {
            "name": "evaluate_notification",
            "description": "Decide whether the user should be notified about this background task result.",
            "parameters": {
                "type": "object",
                "properties": {
                    "should_notify": {
                        "type": "boolean",
                        "description": "true = result contains actionable/important info the user should see; false = routine or empty, safe to suppress",
                    },
                    "reason": {
                        "type": "string",
                        "description": "One-sentence reason for the decision",
                    },
                },
                "required": ["should_notify"],
            },
        },
    })]
});

/// Decide whether a background-task result should be delivered to the user.
///
/// Uses a lightweight tool-call LLM request (same pattern as heartbeat
/// `_decide()`). Falls back to `true` (notify) on any failure so that
/// important messages are never silently dropped.
pub async fn evaluate_response(
    response: &str,
    task_context: &str,
    provider: Arc<dyn LLMProviderDyn>,
    model: &str,
) -> bool {
    let system = {
        let mut ctx = Context::new();
        ctx.insert("part", "system");
        match render_template("agent/evaluator.md", &ctx, true) {
            Ok(s) => s,
            Err(e) => {
                log::error!("evaluate_response: failed to render system template: {e}");
                return true;
            }
        }
    };
    let user = {
        let mut ctx = Context::new();
        ctx.insert("part", "user");
        ctx.insert("task_context", task_context);
        ctx.insert("response", response);
        match render_template("agent/evaluator.md", &ctx, true) {
            Ok(s) => s,
            Err(e) => {
                log::error!("evaluate_response: failed to render user template: {e}");
                return true;
            }
        }
    };

    let llm_response = provider
        .chat_with_retry(
            vec![
                serde_json::json!({ "role": "system", "content": system }),
                serde_json::json!({ "role": "user", "content": user }),
            ],
            Some(EVALUATE_TOOL.clone()),
            Some(model.to_string()),
            Some(256),
            Some(0.0),
            None,
            None,
        )
        .await;

    if llm_response.finish_reason == "error" {
        log::error!(
            "evaluate_response failed, defaulting to notify: {}",
            llm_response.content.as_deref().unwrap_or("unknown error")
        );
        return true;
    }

    if !llm_response.has_tool_calls() {
        log::warn!("evaluate_response: no tool call returned, defaulting to notify");
        return true;
    }

    let args = &llm_response.tool_calls[0].arguments;
    let should_notify = args
        .get("should_notify")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let reason = args
        .get("reason")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    log::info!("evaluate_response: should_notify={should_notify}, reason={reason}");
    should_notify
}
