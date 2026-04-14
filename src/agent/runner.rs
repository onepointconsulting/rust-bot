use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agent::hook::{AgentHook, AgentHookContext};
use crate::utils::prompt_templates::render_template;
use crate::agent::registry::ToolRegistry;
use crate::providers::base::LLMProviderDyn;
use crate::utils::helpers::{
    build_assistant_message,
    estimate_message_tokens,
    estimate_prompt_tokens_chain,
    find_legal_message_start,
    maybe_persist_tool_result,
    truncate_text
};
use crate::utils::runtime::{
    EMPTY_FINAL_RESPONSE_MESSAGE,
    build_finalization_retry_message,
    build_length_recovery_message,
    ensure_nonempty_tool_result,
    is_blank_text,
    repeated_external_lookup_error,
};

const DEFAULT_ERROR_MESSAGE: &str = "Sorry, I encountered an error calling the AI model.";
const MAX_EMPTY_RETRIES: usize = 2;
const MAX_LENGTH_RECOVERIES: usize = 3;
const SNIP_SAFETY_BUFFER: usize = 1024;
const MICROCOMPACT_KEEP_RECENT: usize = 10;
const MICROCOMPACT_MIN_CHARS: usize = 500;
const COMPACTABLE_TOOLS: &[&str] = &["read_file", "exec", "grep", "glob", "web_search", "web_fetch", "list_dir"];
const BACKFILL_CONTENT: &str = "[Tool result unavailable — call was interrupted or lost]";

/// Configuration for a single agent execution.
pub struct AgentRunSpec {
    pub initial_messages: Vec<Value>,
    pub tools: ToolRegistry,
    pub model: String,
    pub max_iterations: usize,
    pub max_tool_result_chars: usize,
    pub temperature: Option<f64>,
    pub max_tokens: Option<usize>,
    pub reasoning_effort: Option<String>,
    pub hook: Option<Arc<dyn AgentHook>>,
    pub error_message: Option<String>,
    pub max_iterations_message: Option<String>,
    pub concurrent_tools: bool,
    pub fail_on_tool_error: bool,
    pub workspace: Option<PathBuf>,
    pub session_key: Option<String>,
    pub context_window_tokens: Option<usize>,
    pub context_block_limit: Option<usize>,
    pub provider_retry_mode: String,
    pub progress_callback: Option<Arc<dyn Fn(Value) + Send + Sync>>,
    pub checkpoint_callback: Option<Arc<dyn Fn(Value) + Send + Sync>>,
}

impl Default for AgentRunSpec {
    fn default() -> Self {
        Self {
            initial_messages: Vec::new(),
            tools: ToolRegistry::new(),
            model: String::new(),
            max_iterations: 0,
            max_tool_result_chars: 0,
            temperature: None,
            max_tokens: None,
            reasoning_effort: None,
            hook: None,
            error_message: Some(DEFAULT_ERROR_MESSAGE.to_string()),
            max_iterations_message: None,
            concurrent_tools: false,
            fail_on_tool_error: false,
            workspace: None,
            session_key: None,
            context_window_tokens: None,
            context_block_limit: None,
            provider_retry_mode: "standard".to_string(),
            progress_callback: None,
            checkpoint_callback: None,
        }
    }
}

/// Outcome of a shared agent execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRunResult {
    pub final_content: Option<String>,
    pub messages: Vec<Value>,
    pub tools_used: Vec<String>,
    pub usage: HashMap<String, u64>,
    pub stop_reason: String,
    pub error: Option<String>,
    pub tool_events: Vec<HashMap<String, String>>,
}

impl Default for AgentRunResult {
    fn default() -> Self {
        Self {
            final_content: None,
            messages: Vec::new(),
            tools_used: Vec::new(),
            usage: HashMap::new(),
            stop_reason: "completed".to_string(),
            error: None,
            tool_events: Vec::new(),
        }
    }
}

/// Run a tool-capable LLM loop without product-layer concerns.
pub struct AgentRunner {
    pub provider: Arc<dyn LLMProviderDyn>,
}

impl AgentRunner {
    pub fn new(provider: Arc<dyn LLMProviderDyn>) -> Self {
        Self { provider }
    }

    /// Insert synthetic error results for orphaned tool_use blocks.
    fn backfill_missing_tool_results(messages: &[Value]) -> Vec<Value> {
        // (assistant_idx, call_id, name)
        let mut declared: Vec<(usize, String, String)> = Vec::new();
        let mut fulfilled: std::collections::HashSet<String> = std::collections::HashSet::new();

        for (idx, msg) in messages.iter().enumerate() {
            let role = msg.get("role").and_then(Value::as_str).unwrap_or("");
            if role == "assistant" {
                if let Some(tool_calls) = msg.get("tool_calls").and_then(Value::as_array) {
                    for tc in tool_calls {
                        if let Some(id) = tc.get("id").and_then(Value::as_str) {
                            let name = tc
                                .get("function")
                                .and_then(|f| f.get("name"))
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string();
                            declared.push((idx, id.to_string(), name));
                        }
                    }
                }
            } else if role == "tool" {
                if let Some(tid) = msg.get("tool_call_id").and_then(Value::as_str) {
                    fulfilled.insert(tid.to_string());
                }
            }
        }

        let missing: Vec<(usize, String, String)> = declared
            .into_iter()
            .filter(|(_, cid, _)| !fulfilled.contains(cid))
            .collect();

        if missing.is_empty() {
            return messages.to_vec();
        }

        let mut updated = messages.to_vec();
        let mut offset = 0usize;
        for (assistant_idx, call_id, name) in missing {
            let mut insert_at = assistant_idx + 1 + offset;
            while insert_at < updated.len()
                && updated[insert_at].get("role").and_then(Value::as_str) == Some("tool")
            {
                insert_at += 1;
            }
            updated.insert(
                insert_at,
                serde_json::json!({
                    "role": "tool",
                    "tool_call_id": call_id,
                    "name": name,
                    "content": BACKFILL_CONTENT,
                }),
            );
            offset += 1;
        }
        updated
    }

    /// Replace old compactable tool results with one-line summaries.
    ///
    /// Keeps the most recent `MICROCOMPACT_KEEP_RECENT` compactable results intact
    /// so the model retains fresh context, and collapses any older ones that exceed
    /// `MICROCOMPACT_MIN_CHARS` down to a single placeholder line.
    fn microcompact(messages: &[Value]) -> Vec<Value> {
        let compactable_indices: Vec<usize> = messages
            .iter()
            .enumerate()
            .filter(|(_, msg)| {
                msg.get("role").and_then(Value::as_str) == Some("tool")
                    && msg
                        .get("name")
                        .and_then(Value::as_str)
                        .map(|name| COMPACTABLE_TOOLS.contains(&name))
                        .unwrap_or(false)
            })
            .map(|(idx, _)| idx)
            .collect();

        if compactable_indices.len() <= MICROCOMPACT_KEEP_RECENT {
            return messages.to_vec();
        }

        let stale_count = compactable_indices.len() - MICROCOMPACT_KEEP_RECENT;
        let stale = &compactable_indices[..stale_count];

        let mut updated: Option<Vec<Value>> = None;
        for &idx in stale {
            let msg = &messages[idx];
            let content = msg.get("content").and_then(Value::as_str).unwrap_or("");
            if content.len() < MICROCOMPACT_MIN_CHARS {
                continue;
            }
            let name = msg.get("name").and_then(Value::as_str).unwrap_or("tool");
            let summary = format!("[{name} result omitted from context]");
            let updated = updated.get_or_insert_with(|| messages.to_vec());
            updated[idx]["content"] = Value::String(summary);
        }

        updated.unwrap_or_else(|| messages.to_vec())
    }

    /// Normalise a raw tool result: ensure it is non-empty, optionally persist
    /// large payloads to disk, and truncate anything that still exceeds the limit.
    fn normalize_tool_result(
        &self,
        spec: &AgentRunSpec,
        tool_call_id: &str,
        tool_name: &str,
        result: Value,
    ) -> Value {
        let result = ensure_nonempty_tool_result(tool_name, result);

        let content = maybe_persist_tool_result(
            spec.workspace.as_deref(),
            spec.session_key.as_deref(),
            tool_call_id,
            result,
            spec.max_tool_result_chars,
        );

        if let Value::String(ref s) = content {
            if spec.max_tool_result_chars > 0 && s.len() > spec.max_tool_result_chars {
                return Value::String(truncate_text(s, spec.max_tool_result_chars));
            }
        }
        content
    }

    /// Apply the tool-result character budget across all tool messages.
    ///
    /// Each tool message is run through `normalize_tool_result`. The message list
    /// is only cloned on the first message that actually changes (copy-on-write),
    /// so calls where nothing changes are allocation-free.
    fn apply_tool_result_budget(&self, spec: &AgentRunSpec, messages: &[Value]) -> Vec<Value> {
        let mut updated: Option<Vec<Value>> = None;

        for (idx, message) in messages.iter().enumerate() {
            if message.get("role").and_then(Value::as_str) != Some("tool") {
                continue;
            }
            let tool_call_id = message
                .get("tool_call_id")
                .and_then(Value::as_str)
                .map(|s| s.to_string())
                .unwrap_or_else(|| format!("tool_{idx}"));
            let tool_name = message
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("tool")
                .to_string();
            let content = message.get("content").cloned().unwrap_or(Value::Null);

            let normalized = self.normalize_tool_result(spec, &tool_call_id, &tool_name, content.clone());

            if normalized != content {
                let updated = updated.get_or_insert_with(|| messages.to_vec());
                updated[idx]["content"] = normalized;
            }
        }

        updated.unwrap_or_else(|| messages.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tools::filesystem::ListDirTool;
    use crate::providers::base::{GenerationSettings, LLMProviderDyn, LLMResponse};
    use crate::providers::registry::ProviderSpec;

    /// Minimal provider that satisfies `LLMProviderDyn` for tests that don't
    /// exercise the provider (e.g. `normalize_tool_result`).
    struct StubProvider {
        settings: GenerationSettings,
    }

    impl StubProvider {
        fn new() -> Arc<dyn LLMProviderDyn> {
            Arc::new(Self { settings: GenerationSettings::new() })
        }
    }

    #[async_trait::async_trait(?Send)]
    impl LLMProviderDyn for StubProvider {
        fn api_key(&self) -> Option<String> { None }
        fn api_base(&self) -> Option<String> { None }
        fn extra_headers(&self) -> Option<std::collections::HashMap<String, String>> { None }
        fn generation_settings(&self) -> &GenerationSettings { &self.settings }
        fn generation_settings_mut(&mut self) -> &mut GenerationSettings { &mut self.settings }
        fn spec(&self) -> Option<&ProviderSpec> { None }
        fn get_default_model(&self) -> String { String::new() }
        async fn chat(&self, _: Vec<Value>, _: Option<Vec<Value>>, _: Option<String>, _: usize, _: f32, _: Option<String>, _: Option<Value>) -> LLMResponse {
            unimplemented!()
        }
        async fn safe_chat(&self, _: Vec<Value>, _: Option<Vec<Value>>, _: Option<String>, _: usize, _: f32, _: Option<String>, _: Option<Value>) -> LLMResponse {
            unimplemented!()
        }
        async fn chat_with_retry(&self, _: Vec<Value>, _: Option<Vec<Value>>, _: Option<String>, _: Option<usize>, _: Option<f32>, _: Option<String>, _: Option<Value>) -> LLMResponse {
            unimplemented!()
        }
    }

    fn make_runner() -> AgentRunner {
        AgentRunner::new(StubProvider::new())
    }

    fn make_list_dir_tool() -> ListDirTool {
        // Create temp directory as workspace
        let workspace = tempfile::tempdir().unwrap();
        let workspace_path = workspace.path().to_path_buf();
        ListDirTool::new(Some(workspace_path), None, None)
    }

    #[test]
    fn test_default_spec() {
        let spec = AgentRunSpec::default();
        assert_eq!(spec.initial_messages, Vec::<Value>::new());
        assert_eq!(spec.model, String::new());
        assert_eq!(spec.max_iterations, 0);
        assert_eq!(spec.max_tool_result_chars, 0);
        assert_eq!(spec.temperature, None);
    }

    #[test]
    fn test_default_result() {
        let result = AgentRunResult::default();
        println!("result: {:?}", result);
        assert_eq!(result.final_content, None);
        assert_eq!(result.messages, Vec::<Value>::new());
        assert_eq!(result.tools_used, Vec::<String>::new());
        assert_eq!(result.usage, HashMap::<String, u64>::new());
        assert_eq!(result.stop_reason, "completed");
        assert_eq!(result.error, None);
    }

    #[test]
    fn test_backfill_missing_tool_results() {
        // No tool calls at all — messages returned unchanged.
        let no_tools = vec![
            serde_json::json!({"role": "user", "content": "hello"}),
            serde_json::json!({"role": "assistant", "content": "hi"}),
        ];
        let result = AgentRunner::backfill_missing_tool_results(&no_tools);
        assert_eq!(result, no_tools);
    }

    #[test]
    fn test_all_tools_with_matching_results() {
        // All tool calls already have matching results — no insertion.
        let fulfilled = vec![
            serde_json::json!({"role": "user", "content": "go"}),
            serde_json::json!({
                "role": "assistant",
                "tool_calls": [{"id": "call_1", "function": {"name": "read_file"}}]
            }),
            serde_json::json!({"role": "tool", "tool_call_id": "call_1", "name": "read_file", "content": "data"}),
        ];
        let result = AgentRunner::backfill_missing_tool_results(&fulfilled);
        assert_eq!(result, fulfilled);
    }

    #[test]
    fn test_backfill_missing_tool_results_with_missing_results() {
        // One orphaned tool call — a synthetic result should be inserted after the assistant message.
        let orphaned = vec![
            serde_json::json!({"role": "user", "content": "go"}),
            serde_json::json!({
                "role": "assistant",
                "tool_calls": [{"id": "call_missing", "function": {"name": "exec"}}]
            }),
            serde_json::json!({"role": "assistant", "content": "done"}),
        ];
        let result = AgentRunner::backfill_missing_tool_results(&orphaned);
        assert_eq!(result.len(), 4);
        let inserted = &result[2];
        assert_eq!(inserted["role"], "tool");
        assert_eq!(inserted["tool_call_id"], "call_missing");
        assert_eq!(inserted["name"], "exec");
        assert_eq!(inserted["content"], BACKFILL_CONTENT);
    }

    #[test]
    fn test_two_orphaned_calls_in_same_assistant_message() {
        // Two orphaned calls in the same assistant message — both get synthetic results inserted
        // in order, before the next non-tool message.
        let two_orphaned = vec![
            serde_json::json!({
                "role": "assistant",
                "tool_calls": [
                    {"id": "id_a", "function": {"name": "grep"}},
                    {"id": "id_b", "function": {"name": "glob"}},
                ]
            }),
            serde_json::json!({"role": "assistant", "content": "done"}),
        ];
        let result = AgentRunner::backfill_missing_tool_results(&two_orphaned);
        assert_eq!(result.len(), 4);
        assert_eq!(result[1]["tool_call_id"], "id_a");
        assert_eq!(result[2]["tool_call_id"], "id_b");
        assert_eq!(result[3]["role"], "assistant");
    }

    #[test]
    fn test_backfill_mixed_fulfilled_and_orphaned() {
        // Mixed: one fulfilled, one orphaned — only the unfulfilled one gets backfilled.
        let mixed = vec![
            serde_json::json!({
                "role": "assistant",
                "tool_calls": [
                    {"id": "present", "function": {"name": "read_file"}},
                    {"id": "absent",  "function": {"name": "exec"}},
                ]
            }),
            serde_json::json!({"role": "tool", "tool_call_id": "present", "name": "read_file", "content": "ok"}),
        ];
        let result = AgentRunner::backfill_missing_tool_results(&mixed);
        assert_eq!(result.len(), 3);
        let backfilled = result.iter().find(|m| m["tool_call_id"] == "absent").unwrap();
        assert_eq!(backfilled["name"], "exec");
        assert_eq!(backfilled["content"], BACKFILL_CONTENT);
    }

    #[test]
    fn test_microcompact_no_change_when_few_results() {
        // Fewer compactable results than the keep-recent threshold — no compaction.
        let large_content = "x".repeat(MICROCOMPACT_MIN_CHARS);
        let messages: Vec<Value> = (0..MICROCOMPACT_KEEP_RECENT)
            .map(|_| serde_json::json!({"role": "tool", "name": "read_file", "content": large_content}))
            .collect();
        assert_eq!(messages.len(), MICROCOMPACT_KEEP_RECENT);
        assert_eq!(MICROCOMPACT_MIN_CHARS, large_content.len());
        let result = AgentRunner::microcompact(&messages);
        assert_eq!(result, messages);
    }

    #[test]
    fn test_microcompact_collapses_stale_large_results() {
        // Build MICROCOMPACT_KEEP_RECENT + 2 large compactable results.
        // The first 2 (stale) should be collapsed; the rest stay intact.
        let large_content = "x".repeat(MICROCOMPACT_MIN_CHARS + 1);
        let total = MICROCOMPACT_KEEP_RECENT + 2;
        let messages: Vec<Value> = (0..total)
            .map(|_| serde_json::json!({"role": "tool", "name": "read_file", "content": large_content}))
            .collect();

        let result = AgentRunner::microcompact(&messages);
        assert_eq!(result.len(), total);

        // First 2 are stale and large — must be collapsed.
        for i in 0..2 {
            assert_eq!(result[i]["content"], "[read_file result omitted from context]");
        }
        // Remaining MICROCOMPACT_KEEP_RECENT are untouched.
        for i in 2..total {
            assert_eq!(result[i]["content"], large_content.as_str());
        }
    }

    #[test]
    fn test_microcompact_skips_small_stale_results() {
        // Stale results below MICROCOMPACT_MIN_CHARS are left alone.
        let small_content = "x".repeat(MICROCOMPACT_MIN_CHARS - 1);
        let total = MICROCOMPACT_KEEP_RECENT + 1;
        let messages: Vec<Value> = (0..total)
            .map(|_| serde_json::json!({"role": "tool", "name": "grep", "content": small_content}))
            .collect();

        let result = AgentRunner::microcompact(&messages);
        assert_eq!(result, messages);
    }

    #[test]
    fn test_microcompact_ignores_non_compactable_tools() {
        // Tool messages whose name is not in COMPACTABLE_TOOLS are never touched.
        let large_content = "x".repeat(MICROCOMPACT_MIN_CHARS + 1);
        let total = MICROCOMPACT_KEEP_RECENT + 2;
        let messages: Vec<Value> = (0..total)
            .map(|_| serde_json::json!({"role": "tool", "name": "custom_tool", "content": large_content}))
            .collect();

        let result = AgentRunner::microcompact(&messages);
        assert_eq!(result, messages);
    }

    // ── normalize_tool_result ─────────────────────────────────────────────────

    #[test]
    fn test_normalize_null_result_replaced() {
        // Null input is replaced by ensure_nonempty_tool_result with a non-empty message.
        let runner = make_runner();
        let spec = AgentRunSpec { max_tool_result_chars: 1000, ..Default::default() };
        let out = runner.normalize_tool_result(&spec, "call_1", "my_tool", Value::Null);
        match out {
            Value::String(s) => assert!(!s.is_empty()),
            other => panic!("expected String, got {other:?}"),
        }
    }

    #[test]
    fn test_normalize_blank_string_result_replaced() {
        // A whitespace-only string is also considered empty and gets replaced.
        let runner = make_runner();
        let spec = AgentRunSpec { max_tool_result_chars: 1000, ..Default::default() };
        let out = runner.normalize_tool_result(&spec, "call_1", "my_tool", Value::String("   ".into()));
        match out {
            Value::String(s) => assert!(!s.trim().is_empty()),
            other => panic!("expected non-blank String, got {other:?}"),
        }
    }

    #[test]
    fn test_normalize_content_within_limit_unchanged() {
        // A short result within max_tool_result_chars is returned as-is.
        let runner = make_runner();
        let spec = AgentRunSpec { max_tool_result_chars: 1000, ..Default::default() };
        let content = "hello world".to_string();
        let out = runner.normalize_tool_result(&spec, "call_1", "my_tool", Value::String(content.clone()));
        assert_eq!(out, Value::String(content));
    }

    #[test]
    fn test_normalize_content_truncated_when_over_limit() {
        // A string longer than max_tool_result_chars is truncated (no workspace, so no persist).
        let runner = make_runner();
        let limit = 20;
        let spec = AgentRunSpec { max_tool_result_chars: limit, ..Default::default() };
        let long_content = "a".repeat(limit + 50);
        let out = runner.normalize_tool_result(&spec, "call_1", "my_tool", Value::String(long_content));
        match out {
            Value::String(s) => assert!(s.len() <= limit + "\n... (truncated)".len()),
            other => panic!("expected truncated String, got {other:?}"),
        }
    }

    #[test]
    fn test_normalize_zero_limit_disables_truncation() {
        // max_tool_result_chars = 0 means unlimited: even very long content is not truncated.
        let runner = make_runner();
        let spec = AgentRunSpec { max_tool_result_chars: 0, ..Default::default() };
        let long_content = "z".repeat(10_000);
        let out = runner.normalize_tool_result(&spec, "call_1", "my_tool", Value::String(long_content.clone()));
        assert_eq!(out, Value::String(long_content));
    }

    #[test]
    fn test_normalize_persists_large_result_to_workspace() {
        // With a workspace and a result over the limit, maybe_persist_tool_result
        // writes the content to disk and returns a reference string instead.
        let runner = make_runner();
        let workspace = tempfile::tempdir().unwrap();
        let limit = 50;
        let spec = AgentRunSpec {
            max_tool_result_chars: limit,
            workspace: Some(workspace.path().to_path_buf()),
            session_key: Some("test-session".into()),
            ..Default::default()
        };
        let large_content = "x".repeat(limit + 200);
        let out = runner.normalize_tool_result(&spec, "call_persist", "read_file", Value::String(large_content));
        // The persisted reference is a short string pointing to the file on disk,
        // not the original large payload.
        match out {
            Value::String(s) => assert!(s.len() <= limit + "\n... (truncated)".len()),
            other => panic!("expected reference String, got {other:?}"),
        }
    }

    // ── apply_tool_result_budget ──────────────────────────────────────────────

    #[test]
    fn test_apply_budget_no_tool_messages_unchanged() {
        // No tool messages — returned unchanged, no allocation.
        let runner = make_runner();
        let spec = AgentRunSpec { max_tool_result_chars: 10, ..Default::default() };
        let messages = vec![
            serde_json::json!({"role": "user", "content": "hello"}),
            serde_json::json!({"role": "assistant", "content": "hi"}),
        ];
        let result = runner.apply_tool_result_budget(&spec, &messages);
        assert_eq!(result, messages);
    }

    #[test]
    fn test_apply_budget_short_content_unchanged() {
        // Tool message content within the limit is not modified.
        let runner = make_runner();
        let spec = AgentRunSpec { max_tool_result_chars: 1000, ..Default::default() };
        let messages = vec![
            serde_json::json!({"role": "tool", "tool_call_id": "c1", "name": "exec", "content": "ok"}),
        ];
        let result = runner.apply_tool_result_budget(&spec, &messages);
        assert_eq!(result, messages);
    }

    #[test]
    fn test_apply_budget_truncates_long_content() {
        // Tool message content exceeding the limit is truncated.
        let runner = make_runner();
        let limit = 20;
        let spec = AgentRunSpec { max_tool_result_chars: limit, ..Default::default() };
        let long = "x".repeat(limit + 100);
        let messages = vec![
            serde_json::json!({"role": "tool", "tool_call_id": "c1", "name": "exec", "content": long}),
        ];
        let result = runner.apply_tool_result_budget(&spec, &messages);
        let out = result[0]["content"].as_str().unwrap();
        assert!(out.len() <= limit + "\n... (truncated)".len());
    }

    #[test]
    fn test_apply_budget_null_content_replaced() {
        // A null content field is replaced with a non-empty placeholder.
        let runner = make_runner();
        let spec = AgentRunSpec { max_tool_result_chars: 1000, ..Default::default() };
        let messages = vec![
            serde_json::json!({"role": "tool", "tool_call_id": "c1", "name": "exec", "content": null}),
        ];
        let result = runner.apply_tool_result_budget(&spec, &messages);
        let out = result[0]["content"].as_str().unwrap();
        assert!(!out.is_empty());
    }

    #[test]
    fn test_apply_budget_missing_tool_call_id_uses_fallback() {
        // When tool_call_id is absent the method falls back to "tool_{idx}".
        let runner = make_runner();
        let spec = AgentRunSpec { max_tool_result_chars: 1000, ..Default::default() };
        let messages = vec![
            serde_json::json!({"role": "tool", "name": "exec", "content": "done"}),
        ];
        // No panic — fallback id is generated internally.
        let result = runner.apply_tool_result_budget(&spec, &messages);
        assert_eq!(result[0]["content"].as_str().unwrap(), "done");
    }

    #[test]
    fn test_apply_budget_only_modifies_tool_messages() {
        // Non-tool messages adjacent to a truncated tool message are untouched.
        let runner = make_runner();
        let limit = 10;
        let spec = AgentRunSpec { max_tool_result_chars: limit, ..Default::default() };
        let long = "y".repeat(limit + 50);
        let messages = vec![
            serde_json::json!({"role": "user", "content": "go"}),
            serde_json::json!({"role": "tool", "tool_call_id": "c1", "name": "exec", "content": long}),
            serde_json::json!({"role": "assistant", "content": "done"}),
        ];
        let result = runner.apply_tool_result_budget(&spec, &messages);
        assert_eq!(result[0], messages[0]);
        assert_eq!(result[2], messages[2]);
        let tool_out = result[1]["content"].as_str().unwrap();
        assert!(tool_out.len() <= limit + "\n... (truncated)".len());
    }
}
