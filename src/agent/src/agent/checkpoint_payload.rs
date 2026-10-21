//! The one place that knows the shape of a runtime checkpoint payload.
//!
//! The runner writes payloads with [`build`]; crash recovery reads them with
//! [`materialize`]. Keeping both here means the writer and the reader cannot
//! drift apart silently (a reader that no longer understands a payload would
//! restore nothing and say nothing).
//!
//! Payload keys:
//! * `type` — what the runner was about to do: `pre_model`, `awaiting_tools`
//!   or `final_response`;
//! * `turn_messages` — every message the turn produced so far, user message
//!   first (turn-local, not the whole session);
//! * `assistant_message`, `completed_tool_results`, `pending_tool_calls` — the
//!   last assistant message that requested tools, the results that are already
//!   in, and the calls that still lack one. These three are the older shape
//!   that pre-journal sessions carry in their metadata.

use chrono::Utc;
use serde_json::{Map, Value, json};

/// Result text given to the model for a tool call that was cut off.
pub const INTERRUPTED_RESULT: &str = crate::agent::recovery::INTERRUPTED_TOOL_RESULT;

/// Checkpoint taken right after the assistant requested tools.
pub const TYPE_AWAITING_TOOLS: &str = "awaiting_tools";
/// Checkpoint taken after every tool call of a batch finished, before the next model request.
pub const TYPE_TOOLS_COMPLETED: &str = "tools_completed";
/// Checkpoint taken before the first model request of a turn.
pub const TYPE_PRE_MODEL: &str = "pre_model";
/// Checkpoint taken once the final answer is in `turn_messages`.
pub const TYPE_FINAL: &str = "final_response";

/// Build the payload for `kind` from the turn-local messages produced so far.
pub fn build(kind: &str, turn_messages: &[Value]) -> Value {
    let mut payload = json!({
        "type": kind,
        "turn_messages": turn_messages,
    });
    if let Some(unresolved) = last_tool_request(turn_messages) {
        payload["assistant_message"] = unresolved.assistant_message;
        payload["completed_tool_results"] = Value::Array(unresolved.completed_results);
        payload["pending_tool_calls"] = Value::Array(unresolved.pending_calls);
    }
    payload
}

/// The turn-local messages stored in `payload`, if it has them.
pub fn turn_messages(payload: &Value) -> Option<Vec<Value>> {
    payload.get("turn_messages")?.as_array().cloned()
}

/// Tool calls of the last tool-requesting assistant message that have no result yet.
pub fn pending_tool_calls(payload: &Value) -> Vec<Value> {
    payload
        .get("pending_tool_calls")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// Id of a tool call in OpenAI format.
pub fn tool_call_id(tool_call: &Value) -> Option<&str> {
    tool_call.get("id").and_then(Value::as_str)
}

/// Name of a tool call in OpenAI format.
pub fn tool_call_name(tool_call: &Value) -> &str {
    tool_call
        .get("function")
        .and_then(|function| function.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("tool")
}

/// A tool-result message for `tool_call` carrying `content`.
pub fn tool_result_message(tool_call: &Value, content: &str) -> Value {
    json!({
        "role": "tool",
        "tool_call_id": tool_call.get("id").cloned().unwrap_or(Value::Null),
        "name": tool_call_name(tool_call),
        "content": content,
        "timestamp": Utc::now().to_rfc3339(),
    })
}

/// Messages a checkpoint stands for.
///
/// With `turn_messages` (journal payloads) those are the base; otherwise the
/// older three-key shape is expanded. Calls in `pending_tool_calls` get the
/// content `resolve_pending` returns for them, which is how callers plug in
/// results the journal knows or the "interrupted" text.
pub fn materialize(
    checkpoint: &Map<String, Value>,
    resolve_pending: impl Fn(&Value) -> String,
) -> Vec<Value> {
    let mut restored = Vec::new();
    if let Some(turn) = checkpoint.get("turn_messages").and_then(Value::as_array) {
        restored.extend(turn.iter().cloned());
    } else {
        if let Some(assistant) = checkpoint
            .get("assistant_message")
            .and_then(Value::as_object)
        {
            restored.push(with_timestamp(assistant));
        }
        if let Some(results) = checkpoint
            .get("completed_tool_results")
            .and_then(Value::as_array)
        {
            restored.extend(
                results
                    .iter()
                    .filter_map(Value::as_object)
                    .map(with_timestamp),
            );
        }
    }
    if let Some(pending) = checkpoint
        .get("pending_tool_calls")
        .and_then(Value::as_array)
    {
        for tool_call in pending.iter().filter(|call| call.is_object()) {
            restored.push(tool_result_message(tool_call, &resolve_pending(tool_call)));
        }
    }
    restored
}

fn with_timestamp(message: &Map<String, Value>) -> Value {
    let mut restored = message.clone();
    restored
        .entry("timestamp")
        .or_insert_with(|| Value::String(Utc::now().to_rfc3339()));
    Value::Object(restored)
}

struct ToolRequest {
    assistant_message: Value,
    completed_results: Vec<Value>,
    pending_calls: Vec<Value>,
}

/// The last assistant message that requested tools, split into calls with and
/// without a result among the messages after it. `None` if no such message
/// exists or the model already answered after it.
fn last_tool_request(turn_messages: &[Value]) -> Option<ToolRequest> {
    let index = turn_messages.iter().rposition(|message| {
        message.get("role").and_then(Value::as_str) == Some("assistant")
            && message
                .get("tool_calls")
                .and_then(Value::as_array)
                .is_some_and(|calls| !calls.is_empty())
    })?;
    let assistant_message = turn_messages[index].clone();
    let completed_results: Vec<Value> = turn_messages[index + 1..]
        .iter()
        .filter(|message| message.get("role").and_then(Value::as_str) == Some("tool"))
        .cloned()
        .collect();
    let pending_calls = assistant_message["tool_calls"]
        .as_array()?
        .iter()
        .filter(|call| {
            !completed_results
                .iter()
                .any(|result| result.get("tool_call_id") == call.get("id"))
        })
        .cloned()
        .collect();
    Some(ToolRequest {
        assistant_message,
        completed_results,
        pending_calls,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user() -> Value {
        json!({"role": "user", "content": "do it"})
    }

    fn assistant_calling(ids: &[&str]) -> Value {
        let calls: Vec<Value> = ids
            .iter()
            .map(|id| json!({"id": id, "type": "function", "function": {"name": "t", "arguments": "{}"}}))
            .collect();
        json!({"role": "assistant", "content": null, "tool_calls": calls})
    }

    fn result(id: &str) -> Value {
        json!({"role": "tool", "tool_call_id": id, "name": "t", "content": "r"})
    }

    fn materialized(payload: &Value) -> Vec<Value> {
        materialize(payload.as_object().unwrap(), |_| {
            INTERRUPTED_RESULT.to_string()
        })
    }

    #[test]
    fn payload_without_tool_requests_has_only_the_turn_messages() {
        let payload = build(TYPE_PRE_MODEL, &[user()]);
        assert_eq!(payload["type"], TYPE_PRE_MODEL);
        assert!(payload.get("pending_tool_calls").is_none());
        assert_eq!(turn_messages(&payload).unwrap(), vec![user()]);
    }

    #[test]
    fn payload_splits_completed_and_pending_calls() {
        let turn = [user(), assistant_calling(&["a", "b"]), result("a")];
        let payload = build(TYPE_AWAITING_TOOLS, &turn);
        assert_eq!(payload["completed_tool_results"], json!([result("a")]));
        let pending = pending_tool_calls(&payload);
        assert_eq!(pending.len(), 1);
        assert_eq!(tool_call_id(&pending[0]), Some("b"));
    }

    #[test]
    fn round_trip_restores_every_iteration_plus_interrupted_pending_call() {
        let turn = [
            user(),
            assistant_calling(&["a"]),
            result("a"),
            assistant_calling(&["b", "c"]),
            result("b"),
        ];
        let restored = materialized(&build(TYPE_AWAITING_TOOLS, &turn));

        assert_eq!(restored.len(), 6);
        assert_eq!(&restored[..5], &turn[..]);
        assert_eq!(restored[5]["tool_call_id"], "c");
        assert_eq!(restored[5]["content"], INTERRUPTED_RESULT);
    }

    #[test]
    fn pending_calls_take_the_content_the_caller_resolves() {
        let turn = [user(), assistant_calling(&["a"])];
        let payload = build(TYPE_AWAITING_TOOLS, &turn);
        let restored = materialize(payload.as_object().unwrap(), |call| {
            format!("result for {}", tool_call_id(call).unwrap())
        });
        assert_eq!(restored[2]["content"], "result for a");
    }

    #[test]
    fn a_turn_whose_tools_all_finished_has_nothing_pending() {
        let turn = [user(), assistant_calling(&["a"]), result("a")];
        let payload = build(TYPE_PRE_MODEL, &turn);
        assert!(pending_tool_calls(&payload).is_empty());
        assert_eq!(materialized(&payload).len(), 3);
    }

    #[test]
    fn legacy_three_key_payload_still_materializes() {
        let legacy = json!({
            "assistant_message": assistant_calling(&["a", "b"]),
            "completed_tool_results": [result("a")],
            "pending_tool_calls": [assistant_calling(&["b"])["tool_calls"][0].clone()],
        });
        let restored = materialized(&legacy);
        assert_eq!(restored.len(), 3);
        assert_eq!(restored[0]["role"], "assistant");
        assert_eq!(restored[1]["tool_call_id"], "a");
        assert_eq!(restored[2]["tool_call_id"], "b");
        assert_eq!(restored[2]["content"], INTERRUPTED_RESULT);
    }

    #[test]
    fn a_coarse_type_only_payload_materializes_to_nothing() {
        assert!(materialized(&json!({"type": "final_response"})).is_empty());
    }
}
