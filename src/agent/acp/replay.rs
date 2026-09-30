//! Turning a stored conversation back into `session/update` notifications.
//!
//! `session/load` makes the agent replay the whole conversation so the client
//! can show it again. The stored messages are OpenAI-style JSON (`user`,
//! `assistant` with optional `tool_calls`, `tool`), the same ones the agent loop
//! feeds the model. This module only reads them: replaying never changes the
//! stored history.

use std::collections::HashMap;
use std::path::Path;

use agent_client_protocol::schema::v1::{
    ContentBlock, ContentChunk, SessionUpdate, TextContent, ToolCallStatus,
};
use serde_json::Value;

use super::mapping::{build_completion_update, build_tool_call, tool_result_text};
use crate::providers::base::ToolCallRequest;
use crate::session::history_visibility::is_hidden_history_message;

fn text_chunk(text: String) -> ContentChunk {
    ContentChunk::new(ContentBlock::Text(TextContent::new(text)))
}

/// Text of a message's `content`: a plain string, or the text blocks of a list.
/// Non-text blocks (images) become a short placeholder so the turn stays readable.
pub fn content_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| match block.get("type").and_then(Value::as_str) {
                Some("text") => block
                    .get("text")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                Some(other) => Some(format!("[{other}]")),
                None => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// A stored `tool_calls` entry as the request the agent originally made.
fn tool_call_request(stored: &Value) -> Option<ToolCallRequest> {
    let id = stored.get("id")?.as_str()?.to_string();
    let function = stored.get("function")?;
    let name = function.get("name")?.as_str()?.to_string();
    // Arguments are stored as a JSON string; keep the raw text if it does not parse.
    let arguments: HashMap<String, Value> = match function.get("arguments") {
        Some(Value::String(raw)) => match serde_json::from_str::<Value>(raw) {
            Ok(Value::Object(map)) => map.into_iter().collect(),
            _ => HashMap::from([("arguments".to_string(), Value::String(raw.clone()))]),
        },
        Some(Value::Object(map)) => map.clone().into_iter().collect(),
        _ => HashMap::new(),
    };
    Some(ToolCallRequest {
        id,
        name,
        arguments,
        extra_content: None,
        provider_specific_fields: None,
        function_provider_specific_fields: None,
    })
}

/// The notifications that re-create `messages` in a client, in order.
///
/// * `user` → `user_message_chunk`
/// * `assistant` → `agent_thought_chunk` (its reasoning, if stored),
///   `agent_message_chunk` (its text), then one `tool_call` per call it made
/// * `tool` → `tool_call_update` completing that call with its result
///
/// Hidden history (internal automation turns) and other roles are skipped.
/// `base_dir` resolves relative file paths of tool calls into locations.
pub fn replay_updates(messages: &[Value], base_dir: Option<&Path>) -> Vec<SessionUpdate> {
    let mut updates = Vec::new();
    for message in messages {
        if is_hidden_history_message(message) {
            continue;
        }
        let content = message.get("content").map(content_text).unwrap_or_default();
        match message.get("role").and_then(Value::as_str) {
            Some("user") if !content.is_empty() => {
                updates.push(SessionUpdate::UserMessageChunk(text_chunk(content)));
            }
            Some("assistant") => {
                if let Some(reasoning) = message
                    .get("reasoning_content")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                {
                    updates.push(SessionUpdate::AgentThoughtChunk(text_chunk(
                        reasoning.to_string(),
                    )));
                }
                if !content.is_empty() {
                    updates.push(SessionUpdate::AgentMessageChunk(text_chunk(content)));
                }
                for call in message
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(tool_call_request)
                {
                    let mut tool_call = build_tool_call(&call, base_dir);
                    // The call finished long ago; its result follows as an update.
                    tool_call.status = ToolCallStatus::InProgress;
                    updates.push(SessionUpdate::ToolCall(tool_call));
                }
            }
            Some("tool") => {
                let Some(call_id) = message.get("tool_call_id").and_then(Value::as_str) else {
                    continue;
                };
                let output = tool_result_text(message);
                let failed = output.starts_with("Error");
                updates.push(SessionUpdate::ToolCallUpdate(build_completion_update(
                    call_id, &output, failed,
                )));
            }
            _ => {}
        }
    }
    updates
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::ToolKind;
    use serde_json::json;

    fn kinds(updates: &[SessionUpdate]) -> Vec<&'static str> {
        updates
            .iter()
            .map(|update| match update {
                SessionUpdate::UserMessageChunk(_) => "user",
                SessionUpdate::AgentMessageChunk(_) => "agent",
                SessionUpdate::AgentThoughtChunk(_) => "thought",
                SessionUpdate::ToolCall(_) => "tool_call",
                SessionUpdate::ToolCallUpdate(_) => "tool_update",
                _ => "other",
            })
            .collect()
    }

    fn text_of(update: &SessionUpdate) -> String {
        match update {
            SessionUpdate::UserMessageChunk(chunk)
            | SessionUpdate::AgentMessageChunk(chunk)
            | SessionUpdate::AgentThoughtChunk(chunk) => match &chunk.content {
                ContentBlock::Text(text) => text.text.clone(),
                _ => String::new(),
            },
            _ => String::new(),
        }
    }

    /// A conversation shaped exactly like the stored sessions on disk.
    fn stored_conversation() -> Vec<Value> {
        vec![
            json!({"role": "user", "content": "what is in notes.txt?", "timestamp": "t1"}),
            json!({
                "role": "assistant", "content": null,
                "reasoning_content": "I should read the file.",
                "tool_calls": [{
                    "id": "call-1", "type": "function",
                    "function": {"name": "read_file", "arguments": "{\n  \"path\": \"notes.txt\"\n}"}
                }],
                "timestamp": "t2"
            }),
            json!({"role": "tool", "tool_call_id": "call-1", "name": "read_file",
                   "content": "1| hello from the project", "timestamp": "t3"}),
            json!({"role": "assistant", "content": "It says hello.", "timestamp": "t4"}),
        ]
    }

    #[test]
    fn a_conversation_replays_in_order() {
        let updates = replay_updates(&stored_conversation(), None);
        assert_eq!(
            kinds(&updates),
            vec!["user", "thought", "tool_call", "tool_update", "agent"]
        );
        assert_eq!(text_of(&updates[0]), "what is in notes.txt?");
        assert_eq!(text_of(&updates[1]), "I should read the file.");
        assert_eq!(text_of(&updates[4]), "It says hello.");
    }

    #[test]
    fn a_tool_call_keeps_its_id_kind_arguments_and_gets_its_result() {
        let base = std::env::temp_dir();
        let updates = replay_updates(&stored_conversation(), Some(&base));
        let SessionUpdate::ToolCall(call) = &updates[2] else {
            panic!("expected a tool_call");
        };
        assert_eq!(call.tool_call_id.0.as_ref(), "call-1");
        assert_eq!(call.kind, ToolKind::Read);
        assert_eq!(call.raw_input, Some(json!({"path": "notes.txt"})));
        assert_eq!(call.locations[0].path, base.join("notes.txt"));
        assert_eq!(call.status, ToolCallStatus::InProgress);

        let SessionUpdate::ToolCallUpdate(update) = &updates[3] else {
            panic!("expected a tool_call_update");
        };
        assert_eq!(update.tool_call_id.0.as_ref(), "call-1");
        assert_eq!(update.fields.status, Some(ToolCallStatus::Completed));
    }

    #[test]
    fn a_failed_tool_result_is_replayed_as_failed() {
        let messages = vec![json!({
            "role": "tool", "tool_call_id": "c9", "name": "shell",
            "content": "Error: command timed out"
        })];
        let updates = replay_updates(&messages, None);
        let SessionUpdate::ToolCallUpdate(update) = &updates[0] else {
            panic!("expected a tool_call_update");
        };
        assert_eq!(update.fields.status, Some(ToolCallStatus::Failed));
    }

    #[test]
    fn several_calls_in_one_assistant_message_each_get_a_tool_call() {
        let messages = vec![json!({
            "role": "assistant", "content": "Looking.",
            "tool_calls": [
                {"id": "a", "type": "function", "function": {"name": "grep", "arguments": "{\"pattern\": \"x\"}"}},
                {"id": "b", "type": "function", "function": {"name": "glob", "arguments": "{\"pattern\": \"*.rs\"}"}}
            ]
        })];
        assert_eq!(
            kinds(&replay_updates(&messages, None)),
            vec!["agent", "tool_call", "tool_call"]
        );
    }

    #[test]
    fn hidden_and_system_messages_are_skipped() {
        let messages = vec![
            json!({"role": "system", "content": "you are rust-bot"}),
            json!({"role": "user", "content": "hello"}),
            json!({"role": "user", "content": "internal", "_hidden_history": true}),
        ];
        let updates = replay_updates(&messages, None);
        assert_eq!(kinds(&updates), vec!["user"]);
        assert_eq!(text_of(&updates[0]), "hello");
    }

    #[test]
    fn content_blocks_keep_text_and_mark_images() {
        let content = json!([
            {"type": "text", "text": "look at this"},
            {"type": "image_url", "image_url": {"url": "data:..."}}
        ]);
        assert_eq!(content_text(&content), "look at this\n[image_url]");
        assert_eq!(content_text(&json!("plain")), "plain");
        assert_eq!(content_text(&Value::Null), "");
    }

    #[test]
    fn unparseable_arguments_are_kept_as_raw_text() {
        let messages = vec![json!({
            "role": "assistant", "content": null,
            "tool_calls": [{"id": "z", "type": "function",
                            "function": {"name": "shell", "arguments": "not json"}}]
        })];
        let updates = replay_updates(&messages, None);
        let SessionUpdate::ToolCall(call) = &updates[0] else {
            panic!("expected a tool_call");
        };
        assert_eq!(call.raw_input, Some(json!({"arguments": "not json"})));
    }

    #[test]
    fn malformed_entries_are_skipped_not_fatal() {
        let messages = vec![
            json!("just a string"),
            json!({"role": "tool", "content": "no call id"}),
            json!({"role": "assistant", "tool_calls": [{"id": "x"}]}),
            json!({"role": "user", "content": "still here"}),
        ];
        assert_eq!(kinds(&replay_updates(&messages, None)), vec!["user"]);
    }

    #[test]
    fn an_empty_conversation_replays_nothing() {
        assert!(replay_updates(&[], None).is_empty());
    }
}
