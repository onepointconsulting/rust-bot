//! Pure mapping helpers between rust-bot tool calls and ACP tool-call updates.
//!
//! Nothing here touches the connection or the agent loop, so every rule
//! (tool kind, permission policy, locations, completion status) is unit-tested
//! in isolation.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use agent_client_protocol::schema::v1::{
    ContentBlock, TextContent, ToolCall, ToolCallContent, ToolCallLocation, ToolCallStatus,
    ToolCallUpdate, ToolCallUpdateFields, ToolKind,
};
use serde_json::Value;

use crate::providers::base::ToolCallRequest;
use crate::utils::tool_hints::format_tool_hints;

/// Longest tool result forwarded to the client; the model still sees the full text.
pub const MAX_TOOL_OUTPUT_CHARS: usize = 4000;

/// Argument names under which rust-bot tools take a file path.
const PATH_ARGUMENT_KEYS: [&str; 2] = ["path", "file_path"];

/// Map a rust-bot tool name to the ACP tool kind clients use for icons and policy.
pub fn tool_kind_for(tool_name: &str) -> ToolKind {
    match tool_name {
        "read_file" => ToolKind::Read,
        "write_file" | "edit_file" => ToolKind::Edit,
        "list_dir" | "glob" | "grep" => ToolKind::Search,
        "shell" => ToolKind::Execute,
        "web_search" | "web_fetch" => ToolKind::Fetch,
        _ => ToolKind::Other,
    }
}

/// Whether a call of this kind must be approved by the ACP client first.
///
/// Reading and searching never ask. Everything else (edit, execute, fetch and
/// `other`, which includes MCP tools whose side effects are unknown) always
/// asks. With `confirm_before_execute` every call asks, so that setting keeps
/// its current meaning.
pub fn requires_permission(kind: ToolKind, confirm_before_execute: bool) -> bool {
    match kind {
        ToolKind::Read | ToolKind::Search => confirm_before_execute,
        _ => true,
    }
}

/// File locations a tool call touches, so an editor can follow along.
///
/// ACP wants absolute paths: a relative argument is resolved against `base_dir`
/// (the session's project folder) and dropped when no base is known.
pub fn tool_locations(
    arguments: &HashMap<String, Value>,
    base_dir: Option<&Path>,
) -> Vec<ToolCallLocation> {
    let Some(raw_path) = PATH_ARGUMENT_KEYS
        .iter()
        .find_map(|key| arguments.get(*key).and_then(Value::as_str))
        .filter(|path| !path.is_empty())
    else {
        return Vec::new();
    };
    let path = PathBuf::from(raw_path);
    let absolute = if path.is_absolute() {
        path
    } else if let Some(base_dir) = base_dir {
        base_dir.join(path)
    } else {
        return Vec::new();
    };
    vec![ToolCallLocation::new(absolute)]
}

/// Build the `tool_call` notification announcing a call before it runs.
pub fn build_tool_call(call: &ToolCallRequest, base_dir: Option<&Path>) -> ToolCall {
    let title = format_tool_hints(vec![call.clone()]);
    let title = if title.is_empty() {
        call.name.clone()
    } else {
        title
    };
    ToolCall::new(call.id.clone(), title)
        .kind(tool_kind_for(&call.name))
        .status(ToolCallStatus::Pending)
        .locations(tool_locations(&call.arguments, base_dir))
        .raw_input(Value::Object(call.arguments.clone().into_iter().collect()))
}

/// Build the `tool_call_update` that moves a call to `status`.
pub fn build_status_update(call_id: &str, status: ToolCallStatus) -> ToolCallUpdate {
    ToolCallUpdate::new(
        call_id.to_string(),
        ToolCallUpdateFields::new().status(status),
    )
}

/// Build the `tool_call_update` reporting a finished call and its result text.
pub fn build_completion_update(call_id: &str, output: &str, failed: bool) -> ToolCallUpdate {
    let status = if failed {
        ToolCallStatus::Failed
    } else {
        ToolCallStatus::Completed
    };
    ToolCallUpdate::new(
        call_id.to_string(),
        ToolCallUpdateFields::new()
            .status(status)
            .content(vec![ToolCallContent::from(ContentBlock::Text(
                TextContent::new(truncate_output(output)),
            ))]),
    )
}

/// Cut `output` to [`MAX_TOOL_OUTPUT_CHARS`] characters, marking the cut.
pub fn truncate_output(output: &str) -> String {
    if output.chars().count() <= MAX_TOOL_OUTPUT_CHARS {
        return output.to_string();
    }
    let mut truncated: String = output.chars().take(MAX_TOOL_OUTPUT_CHARS).collect();
    truncated.push_str("\n… (truncated)");
    truncated
}

/// Whether a runner tool event (`"status"` of `ok` / `error` / `blocked`) is a failure.
pub fn event_is_failure(event: &HashMap<String, String>) -> bool {
    event.get("status").is_some_and(|status| status != "ok")
}

/// Result text of a runner `tool` message (`content` may be a string or JSON).
pub fn tool_result_text(tool_message: &Value) -> String {
    match tool_message.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(id: &str, name: &str, arguments: Value) -> ToolCallRequest {
        let arguments = arguments
            .as_object()
            .map(|map| map.clone().into_iter().collect())
            .unwrap_or_default();
        ToolCallRequest {
            id: id.to_string(),
            name: name.to_string(),
            arguments,
            extra_content: None,
            provider_specific_fields: None,
            function_provider_specific_fields: None,
        }
    }

    #[test]
    fn tool_kind_covers_every_builtin_family() {
        assert_eq!(tool_kind_for("read_file"), ToolKind::Read);
        assert_eq!(tool_kind_for("write_file"), ToolKind::Edit);
        assert_eq!(tool_kind_for("edit_file"), ToolKind::Edit);
        assert_eq!(tool_kind_for("list_dir"), ToolKind::Search);
        assert_eq!(tool_kind_for("glob"), ToolKind::Search);
        assert_eq!(tool_kind_for("grep"), ToolKind::Search);
        assert_eq!(tool_kind_for("shell"), ToolKind::Execute);
        assert_eq!(tool_kind_for("web_fetch"), ToolKind::Fetch);
        assert_eq!(tool_kind_for("web_search"), ToolKind::Fetch);
        assert_eq!(tool_kind_for("some_mcp_tool"), ToolKind::Other);
    }

    #[test]
    fn read_and_search_never_ask_unless_confirm_before_execute() {
        assert!(!requires_permission(ToolKind::Read, false));
        assert!(!requires_permission(ToolKind::Search, false));
        assert!(requires_permission(ToolKind::Read, true));
        assert!(requires_permission(ToolKind::Search, true));
    }

    #[test]
    fn edit_execute_fetch_and_other_always_ask() {
        for kind in [
            ToolKind::Edit,
            ToolKind::Execute,
            ToolKind::Fetch,
            ToolKind::Other,
        ] {
            assert!(requires_permission(kind, false));
            assert!(requires_permission(kind, true));
        }
    }

    #[test]
    fn absolute_path_argument_becomes_a_location() {
        let absolute = std::env::temp_dir().join("a.txt");
        let args = call("1", "read_file", json!({"path": absolute})).arguments;
        let locations = tool_locations(&args, None);
        assert_eq!(locations.len(), 1);
        assert_eq!(locations[0].path, absolute);
    }

    #[test]
    fn relative_path_is_resolved_against_the_project_folder() {
        let base = std::env::temp_dir();
        let args = call("1", "read_file", json!({"path": "src/main.rs"})).arguments;
        let locations = tool_locations(&args, Some(&base));
        assert_eq!(locations[0].path, base.join("src/main.rs"));
    }

    #[test]
    fn relative_path_without_a_base_is_dropped() {
        let args = call("1", "read_file", json!({"path": "src/main.rs"})).arguments;
        assert!(tool_locations(&args, None).is_empty());
    }

    #[test]
    fn file_path_key_and_missing_path_are_handled() {
        let base = std::env::temp_dir();
        let with_file_path = call("1", "edit", json!({"file_path": "x.rs"})).arguments;
        assert_eq!(
            tool_locations(&with_file_path, Some(&base))[0].path,
            base.join("x.rs")
        );
        let without_path = call("1", "shell", json!({"command": "ls"})).arguments;
        assert!(tool_locations(&without_path, Some(&base)).is_empty());
        let empty_path = call("1", "read_file", json!({"path": ""})).arguments;
        assert!(tool_locations(&empty_path, Some(&base)).is_empty());
    }

    #[test]
    fn tool_call_carries_id_kind_pending_status_and_raw_input() {
        let base = std::env::temp_dir();
        let request = call("call-7", "read_file", json!({"path": "a.txt"}));
        let tool_call = build_tool_call(&request, Some(&base));
        assert_eq!(tool_call.tool_call_id.0.as_ref(), "call-7");
        assert_eq!(tool_call.kind, ToolKind::Read);
        assert_eq!(tool_call.status, ToolCallStatus::Pending);
        assert_eq!(tool_call.raw_input, Some(json!({"path": "a.txt"})));
        assert_eq!(tool_call.locations[0].path, base.join("a.txt"));
        assert!(!tool_call.title.is_empty());
    }

    #[test]
    fn completion_update_reports_status_and_truncated_output() {
        let done = build_completion_update("c1", "ok", false);
        assert_eq!(done.fields.status, Some(ToolCallStatus::Completed));
        let failed = build_completion_update("c1", "Error: boom", true);
        assert_eq!(failed.fields.status, Some(ToolCallStatus::Failed));
        assert_eq!(failed.fields.content.as_ref().map(Vec::len), Some(1));
    }

    #[test]
    fn truncate_output_keeps_short_text_and_marks_long_text() {
        assert_eq!(truncate_output("short"), "short");
        let long = "x".repeat(MAX_TOOL_OUTPUT_CHARS + 50);
        let cut = truncate_output(&long);
        assert!(cut.ends_with("(truncated)"));
        assert!(cut.chars().count() < long.chars().count());
    }

    #[test]
    fn event_failure_is_anything_but_ok() {
        let event = |status: &str| HashMap::from([("status".to_string(), status.to_string())]);
        assert!(!event_is_failure(&event("ok")));
        assert!(event_is_failure(&event("error")));
        assert!(event_is_failure(&event("blocked")));
        // No status reported: not treated as a failure.
        assert!(!event_is_failure(&HashMap::new()));
    }

    #[test]
    fn tool_result_text_handles_strings_and_json() {
        assert_eq!(tool_result_text(&json!({"content": "hi"})), "hi");
        assert_eq!(tool_result_text(&json!({"content": [1, 2]})), "[1,2]");
        assert_eq!(tool_result_text(&json!({})), "");
    }
}
