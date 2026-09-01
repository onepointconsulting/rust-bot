use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::agent::circuit_breaker::{
    CIRCUIT_BREAKER_STOP_REASON, CIRCUIT_BREAKER_USER_MESSAGE, CircuitDecision,
    DEFAULT_N_IDENTICAL_MESSAGES, MessageCircuitBreaker, is_tripped_error, tripped_error_message,
};
use crate::agent::hook::{AgentHook, AgentHookContext};
use crate::agent::tools::registry::ToolRegistry;
use crate::providers::base::{
    BoxedStreamCallback, LLMProviderDyn, LLMResponse, LLMUsage, ToolCallRequest,
    is_unsupported_image_input_error,
};
use crate::utils::helpers::{
    build_assistant_message, estimate_message_tokens, estimate_prompt_tokens,
    find_legal_message_start, maybe_persist_tool_result, truncate_text,
};

use crate::utils::runtime::{
    EMPTY_FINAL_RESPONSE_MESSAGE, build_finalization_retry_message, build_length_recovery_message,
    build_truncated_tool_call_recovery_message, coerce_tool_execute_result,
    ensure_nonempty_tool_result, is_blank_text, repeated_external_lookup_error,
};

const DEFAULT_ERROR_MESSAGE: &str = "Sorry, I encountered an error calling the AI model.";
const UNSUPPORTED_IMAGE_INPUT_MESSAGE: &str = "The selected model does not support image input. Switch to a vision-capable model, or send the message without an image.";
const MAX_EMPTY_RETRIES: usize = 2;
const MAX_LENGTH_RECOVERIES: usize = 3;
const SNIP_SAFETY_BUFFER: usize = 1024;
const MICROCOMPACT_KEEP_RECENT: usize = 10;
const MICROCOMPACT_MIN_CHARS: usize = 500;
const COMPACTABLE_TOOLS: &[&str] = &[
    "read_file",
    "exec",
    "grep",
    "glob",
    "web_search",
    "web_fetch",
    "list_dir",
];
const BACKFILL_CONTENT: &str = "[Tool result unavailable — call was interrupted or lost]";
const ARG_PARSE_ERROR_KEY: &str = "__args_json_parse_error";
const ARG_PARSE_RAW_KEY: &str = "__args_json_raw";

/// Configuration for a single agent execution.
pub struct AgentRunSpec {
    pub initial_messages: Vec<Value>,
    pub tools: ToolRegistry,
    pub model: String,
    pub max_iterations: usize,
    pub max_tool_result_chars: usize,
    pub temperature: Option<f32>,
    pub max_tokens: Option<usize>,
    pub reasoning_effort: Option<String>,
    pub hook: Option<Arc<dyn AgentHook>>,
    pub error_message: Option<String>,
    pub max_iterations_message: Option<String>,
    pub concurrent_tools: bool,
    pub fail_on_tool_error: bool,
    pub workspace: Option<PathBuf>,
    pub session_key: Option<String>,
    pub context_window_tokens: Option<u64>,
    pub context_block_limit: Option<u32>,
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
            max_tokens: Some(4096 * 2),
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
    pub usage: LLMUsage,
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
            usage: LLMUsage::new(),
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
    /// Tool results after the last user message (the current turn) are never
    /// compacted — including earlier tool rounds in that turn — so the model
    /// keeps the context it is actively using. Among older compactable results,
    /// keeps the most recent `MICROCOMPACT_KEEP_RECENT` intact and collapses any
    /// older ones that exceed `MICROCOMPACT_MIN_CHARS` down to a single
    /// placeholder line.
    fn microcompact(messages: &[Value]) -> Vec<Value> {
        let current_turn_start = messages
            .iter()
            .rposition(|msg| msg.get("role").and_then(Value::as_str) == Some("user"));

        let compactable_indices: Vec<usize> = messages
            .iter()
            .enumerate()
            .filter(|(idx, msg)| {
                if current_turn_start.is_some_and(|start| *idx > start) {
                    return false;
                }
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

            let normalized =
                self.normalize_tool_result(spec, &tool_call_id, &tool_name, content.clone());

            if normalized != content {
                let updated = updated.get_or_insert_with(|| messages.to_vec());
                updated[idx]["content"] = normalized;
            }
        }

        updated.unwrap_or_else(|| messages.to_vec())
    }

    /// Trim the oldest non-system messages so the prompt fits within the context-window budget.
    ///
    /// The algorithm works as follows:
    ///   1. Do nothing if no messages are provided or no context window limit is configured.
    ///   2. Compute a token budget: prefer `context_block_limit` when set; otherwise derive it
    ///      as `context_window_tokens - max_output - SNIP_SAFETY_BUFFER`, where `max_output` is
    ///      `spec.max_tokens` (or 4096 as a fallback).
    ///   3. Estimate the current prompt size; return unchanged if it already fits.
    ///   4. Separate system messages (which are always kept) from the rest.
    ///   5. Locate the last `user` message — it anchors the *current* turn, which must never be
    ///      dropped even if its accompanying tool results alone exceed the remaining budget.
    ///      Everything from that user message to the end of the list is kept unconditionally.
    ///      Older turns are then considered newest-first against the leftover budget
    ///      (`total budget - system tokens - tool-definition tokens`, minimum 128). A full
    ///      prior turn is kept when it fits; otherwise the turn is compacted to the user
    ///      message plus the last text-only assistant reply (dropping tool dumps). This
    ///      keeps in-session facts ("I live in …") even when a previous tool-heavy turn
    ///      would otherwise consume the whole window. If no `user` message exists at all
    ///      (e.g. a system-triggered turn), fall back to greedy newest-to-oldest
    ///      accumulation, skipping messages that do not fit rather than stopping.
    ///   6. Trim the kept slice so it has no orphaned tool results (via `find_legal_message_start`).
    ///   7. If nothing survives the trim, fall back to the last four non-system messages and
    ///      apply the same legality check.
    ///   8. Return system messages followed by the trimmed history.
    fn snip_history(&self, spec: &AgentRunSpec, messages: Vec<Value>) -> Vec<Value> {
        let Some(context_window_tokens) = spec.context_window_tokens else {
            return messages;
        };
        if messages.is_empty() {
            return messages;
        }

        let max_output = spec.max_tokens.unwrap_or(4096 * 2);
        let budget = spec
            .context_block_limit
            .map(|n| n as usize)
            .unwrap_or_else(|| {
                (context_window_tokens as usize)
                    .saturating_sub(max_output)
                    .saturating_sub(SNIP_SAFETY_BUFFER)
            });
        if budget == 0 {
            return messages;
        }

        let tool_defs = spec.tools.get_definitions();
        let tools_slice: Option<&[Value]> = if tool_defs.is_empty() {
            None
        } else {
            Some(&tool_defs)
        };
        let estimate = estimate_prompt_tokens(&messages, tools_slice);
        log::debug!("estimate: {}, budget: {}", estimate, budget);
        if estimate <= budget {
            return messages;
        }

        let system_messages: Vec<Value> = messages
            .iter()
            .filter(|m| m.get("role").and_then(Value::as_str) == Some("system"))
            .cloned()
            .collect();
        let non_system: Vec<Value> = messages
            .iter()
            .filter(|m| m.get("role").and_then(Value::as_str) != Some("system"))
            .cloned()
            .collect();
        if non_system.is_empty() {
            return messages;
        }

        let system_tokens: usize = system_messages.iter().map(estimate_message_tokens).sum();
        let tool_defs_tokens = tools_slice
            .map(|slice| estimate_prompt_tokens(&[], Some(slice)))
            .unwrap_or(0);
        let remaining_budget = budget
            .saturating_sub(system_tokens)
            .saturating_sub(tool_defs_tokens)
            .max(128);

        let last_user_idx = non_system
            .iter()
            .rposition(|m| m.get("role").and_then(Value::as_str) == Some("user"));

        let mut kept: Vec<Value> = Vec::new();
        match last_user_idx {
            Some(idx) => {
                // The current turn (last user message onward) is mandatory: it is
                // kept in full even if it alone exceeds the remaining budget, so the
                // model never loses sight of the question it is meant to answer.
                let mandatory = &non_system[idx..];
                let mut kept_tokens: usize = mandatory.iter().map(estimate_message_tokens).sum();
                let prefix = Self::select_prior_turns_for_budget(
                    &non_system[..idx],
                    remaining_budget,
                    &mut kept_tokens,
                );
                kept = prefix;
                kept.extend(mandatory.iter().cloned());
            }
            None => {
                // No user message in this batch — greedy newest-to-oldest, skipping
                // messages that do not fit so a large tool dump cannot hide a
                // smaller earlier message.
                let mut kept_tokens = 0usize;
                for message in non_system.iter().rev() {
                    let msg_tokens = estimate_message_tokens(message);
                    if !kept.is_empty() && kept_tokens + msg_tokens > remaining_budget {
                        continue;
                    }
                    kept.push(message.clone());
                    kept_tokens += msg_tokens;
                }
                kept.reverse();
            }
        }

        if !kept.is_empty() {
            let start = find_legal_message_start(&kept);
            if start > 0 {
                kept = kept[start..].to_vec();
            }
        }

        if kept.is_empty() {
            let tail = non_system.len().min(4);
            kept = non_system[non_system.len() - tail..].to_vec();
            let start = find_legal_message_start(&kept);
            if start > 0 {
                kept = kept[start..].to_vec();
            }
        }

        let mut result = system_messages;
        result.extend(kept);
        if result.len() != messages.len() {
            log::info!(
                "snip_history dropped {} of {} messages (estimate={estimate}, budget={budget}, kept={})",
                messages.len().saturating_sub(result.len()),
                messages.len(),
                result.len(),
            );
        }
        result
    }

    /// Keep as much of `prefix` as fits `remaining_budget`, preferring whole
    /// prior turns and falling back to user text + the last text-only reply.
    ///
    /// `kept_tokens` is the token count already committed (typically the
    /// current turn). It is updated as prefix messages are selected.
    fn select_prior_turns_for_budget(
        prefix: &[Value],
        remaining_budget: usize,
        kept_tokens: &mut usize,
    ) -> Vec<Value> {
        if prefix.is_empty() {
            return Vec::new();
        }

        let mut user_starts: Vec<usize> = prefix
            .iter()
            .enumerate()
            .filter_map(|(i, m)| {
                (m.get("role").and_then(Value::as_str) == Some("user")).then_some(i)
            })
            .collect();

        // Leading non-user rows (e.g. a restored assistant/tool prefix) are
        // treated as their own turn so we still try to keep or compact them.
        if user_starts.first().copied() != Some(0) {
            user_starts.insert(0, 0);
        }

        let mut selected: Vec<Value> = Vec::new();
        for (turn_idx, &start) in user_starts.iter().enumerate().rev() {
            let end = user_starts
                .get(turn_idx + 1)
                .copied()
                .unwrap_or(prefix.len());
            if start >= end {
                continue;
            }
            let turn = &prefix[start..end];
            let turn_tokens: usize = turn.iter().map(estimate_message_tokens).sum();
            if *kept_tokens + turn_tokens <= remaining_budget {
                selected.splice(0..0, turn.iter().cloned());
                *kept_tokens += turn_tokens;
                continue;
            }

            let compact = Self::compact_prior_turn(turn);
            if compact.is_empty() {
                continue;
            }
            let compact_tokens: usize = compact.iter().map(estimate_message_tokens).sum();
            if *kept_tokens + compact_tokens <= remaining_budget {
                selected.splice(0..0, compact);
                *kept_tokens += compact_tokens;
            }
        }
        selected
    }

    /// Reduce a prior turn to the user message and the last text-only assistant
    /// reply, dropping tool calls and their (usually huge) results.
    fn compact_prior_turn(turn: &[Value]) -> Vec<Value> {
        let mut out = Vec::new();
        if let Some(first) = turn.first() {
            if first.get("role").and_then(Value::as_str) == Some("user") {
                out.push(first.clone());
            }
        }
        if let Some(assistant) = turn.iter().rev().find(|m| {
            m.get("role").and_then(Value::as_str) == Some("assistant")
                && Self::assistant_has_text(m)
                && !Self::assistant_has_tool_calls(m)
        }) {
            out.push(assistant.clone());
        }
        out
    }

    fn assistant_has_tool_calls(message: &Value) -> bool {
        message
            .get("tool_calls")
            .and_then(Value::as_array)
            .is_some_and(|calls| !calls.is_empty())
    }

    fn assistant_has_text(message: &Value) -> bool {
        match message.get("content") {
            Some(Value::String(s)) => !s.is_empty(),
            Some(Value::Array(blocks)) => !blocks.is_empty(),
            _ => false,
        }
    }

    /// Send one LLM request and return the response.
    ///
    /// Mirrors Python's `_request_model`:
    ///   - Builds the request parameters from `spec`.
    ///   - If the hook requests streaming, each content delta is forwarded to
    ///     `hook.on_stream` as it arrives from the provider.
    pub async fn request_model(
        &self,
        spec: &AgentRunSpec,
        messages: Vec<Value>,
        hook: Arc<dyn AgentHook>,
        context: &mut AgentHookContext,
    ) -> LLMResponse {
        log::info!("Using model: {}", spec.model);
        let tools = spec.tools.get_definitions();
        let tools_opt = if tools.is_empty() { None } else { Some(tools) };

        if hook.wants_streaming() {
            // `BoxedStreamCallback` is `'static + Send + Sync`, so it cannot
            // capture `&dyn AgentHook` or `&mut AgentHookContext`. Hold the hook
            // in an `Arc` and clone an owned context snapshot instead.
            let hook_cb = Arc::clone(&hook);
            let ctx_snapshot = context.clone();
            let callback: BoxedStreamCallback = Box::new(move |delta: String| {
                let hook = Arc::clone(&hook_cb);
                let mut ctx = ctx_snapshot.clone();
                Box::pin(async move {
                    hook.on_stream(&mut ctx, &delta).await;
                })
            });

            return self
                .provider
                .chat_stream_with_retry_boxed(
                    messages,
                    tools_opt,
                    Some(spec.model.clone()),
                    spec.max_tokens,
                    spec.temperature.map(|t| t as f32),
                    spec.reasoning_effort.clone(),
                    None,
                    Some(callback),
                )
                .await;
        }

        self.provider
            .chat_with_retry(
                messages,
                tools_opt,
                Some(spec.model.clone()),
                spec.max_tokens,
                spec.temperature.map(|t| t as f32),
                spec.reasoning_effort.clone(),
                None,
            )
            .await
    }

    fn accumulate_usage(target: &mut LLMUsage, addition: &LLMUsage) {
        target.add(addition);
    }

    fn emit_checkpoint(spec: &AgentRunSpec, payload: Value) {
        if let Some(ref callback) = spec.checkpoint_callback {
            callback(payload);
        }
    }

    /// Execute all tool calls and return their results, telemetry events, and
    /// the first fatal error (if any).
    ///
    /// Mirrors Python's `_execute_tools`:
    ///   - Partitions the calls into sequential / concurrent batches via
    ///     `partition_tool_batches`.
    ///   - For multi-tool concurrent batches the Python uses `asyncio.gather`.
    ///     This implementation awaits each call sequentially within a batch;
    ///     true parallelism would additionally require `external_lookup_counts`
    ///     to be `Arc<Mutex<HashMap<String, usize>>>` so it can be shared across
    ///     concurrent tasks.
    ///   - Collects every `(result, event, error)` triple into three parallel
    ///     vecs and captures the **first** fatal error encountered.
    ///
    /// Returns `(results, events, fatal_error)`.
    async fn execute_tools(
        spec: &AgentRunSpec,
        tool_calls: &[ToolCallRequest],
        external_lookup_counts: Arc<Mutex<HashMap<String, usize>>>,
        message_circuit_breaker: Arc<Mutex<MessageCircuitBreaker>>,
    ) -> (Vec<String>, Vec<HashMap<String, String>>, Option<String>) {
        let batches = Self::partition_tool_batches(spec, tool_calls);
        let mut tool_results: Vec<(String, HashMap<String, String>, Option<String>)> =
            Vec::with_capacity(tool_calls.len());

        for batch in batches {
            if spec.concurrent_tools && batch.len() > 1 {
                let results = futures::future::join_all(batch.iter().map(|tool_call| {
                    Self::run_tool(
                        spec,
                        tool_call,
                        Arc::clone(&external_lookup_counts),
                        Arc::clone(&message_circuit_breaker),
                    )
                }))
                .await;
                tool_results.extend(results);
            } else {
                for tool_call in batch {
                    tool_results.push(
                        Self::run_tool(
                            spec,
                            tool_call,
                            Arc::clone(&external_lookup_counts),
                            Arc::clone(&message_circuit_breaker),
                        )
                        .await,
                    );
                }
            }
        }

        let mut results: Vec<String> = Vec::with_capacity(tool_results.len());
        let mut events: Vec<HashMap<String, String>> = Vec::with_capacity(tool_results.len());
        let mut fatal_error: Option<String> = None;

        for (result, event, error) in tool_results {
            log::info!(
                "Tool result: {}",
                result.chars().take(100).collect::<String>()
            );
            results.push(result);
            events.push(event);
            if error.is_some() && fatal_error.is_none() {
                fatal_error = error;
            }
        }

        (results, events, fatal_error)
    }

    /// Group tool calls into batches for (optionally concurrent) execution.
    ///
    /// When `spec.concurrent_tools` is `false` every call gets its own
    /// singleton batch, preserving strict sequential ordering.
    ///
    /// When concurrent execution is enabled, consecutive tool calls whose
    /// tool reports `concurrency_safe() == true` are merged into one batch
    /// (to be run in parallel).  A non-safe tool — or one not found in the
    /// registry — flushes the accumulated batch and becomes its own singleton,
    /// acting as a serialisation barrier.  This mirrors the Python behaviour
    /// where `tool.concurrency_safe` is checked via attribute access.
    fn partition_tool_batches<'a>(
        spec: &AgentRunSpec,
        tool_calls: &'a [ToolCallRequest],
    ) -> Vec<Vec<&'a ToolCallRequest>> {
        if !spec.concurrent_tools {
            return tool_calls.iter().map(|tc| vec![tc]).collect();
        }

        let mut batches: Vec<Vec<&ToolCallRequest>> = Vec::new();
        let mut current: Vec<&ToolCallRequest> = Vec::new();

        for tool_call in tool_calls {
            let can_batch = spec
                .tools
                .get(&tool_call.name)
                .map(|t| t.concurrency_safe())
                .unwrap_or(false);

            if can_batch {
                current.push(tool_call);
            } else {
                if !current.is_empty() {
                    batches.push(std::mem::take(&mut current));
                }
                batches.push(vec![tool_call]);
            }
        }

        if !current.is_empty() {
            batches.push(current);
        }

        batches
    }

    /// Resolve, validate, and execute one tool call.
    ///
    /// Mirrors Python's `_request_model` / `_run_tool`:
    ///   - Blocks repeated identical external lookups.
    ///   - Delegates to `ToolRegistry::prepare_call` for parameter resolution
    ///     and validation, then calls `Tool::execute` directly on the resolved
    ///     tool.
    ///   - Classifies every failure path (lookup block, prep error, error result)
    ///     into a uniform `(result, event, fatal_error)` triple.
    ///
    /// Returns `(result, event, fatal_error)`:
    ///   - `result`      — string fed back to the LLM as the tool result.
    ///   - `event`       — telemetry map (`"name"`, `"status"`, `"detail"`).
    ///   - `fatal_error` — `Some(msg)` when the message circuit breaker trips
    ///                     (always abort), or when `spec.fail_on_tool_error` is
    ///                     set and an error occurred; the agent loop should abort.
    async fn run_tool(
        spec: &AgentRunSpec,
        tool_call: &ToolCallRequest,
        external_lookup_counts: Arc<Mutex<HashMap<String, usize>>>,
        message_circuit_breaker: Arc<Mutex<MessageCircuitBreaker>>,
    ) -> (String, HashMap<String, String>, Option<String>) {
        const HINT: &str = "\n\n[Analyze the error above and try a different approach.]";

        // ── 0. Message circuit breaker (consecutive identical sends) ──────────
        let decision = message_circuit_breaker
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .observe(&tool_call.name, &tool_call.arguments);
        if let CircuitDecision::Tripped { streak } = decision {
            let err = tripped_error_message(streak);
            let event = HashMap::from([
                ("name".to_string(), tool_call.name.clone()),
                ("status".to_string(), "error".to_string()),
                (
                    "detail".to_string(),
                    "message circuit breaker tripped".to_string(),
                ),
            ]);
            return (err.clone(), event, Some(err));
        }

        // ── 1. Block repeated identical external lookups ──────────────────────
        if let Some(lookup_error) = repeated_external_lookup_error(
            &tool_call.name,
            &tool_call.arguments,
            &mut external_lookup_counts.lock().unwrap(),
        ) {
            let event = HashMap::from([
                ("name".to_string(), tool_call.name.clone()),
                ("status".to_string(), "error".to_string()),
                (
                    "detail".to_string(),
                    "repeated external lookup blocked".to_string(),
                ),
            ]);
            let error = if spec.fail_on_tool_error {
                Some(lookup_error.clone())
            } else {
                None
            };
            return (format!("{lookup_error}{HINT}"), event, error);
        }

        // ── 2. Malformed arguments JSON (provider parse failure) ─────────────
        if let Some(parse_error) = tool_call
            .arguments
            .get(ARG_PARSE_ERROR_KEY)
            .and_then(Value::as_str)
        {
            let raw = tool_call
                .arguments
                .get(ARG_PARSE_RAW_KEY)
                .and_then(Value::as_str)
                .unwrap_or("");
            let raw_preview: String = raw.chars().take(200).collect();
            let err = format!(
                "Error: malformed tool arguments JSON for '{}': {}. Raw arguments: {}",
                tool_call.name, parse_error, raw_preview
            );
            let detail: String = err.replace('\n', " ").trim().chars().take(120).collect();
            let event = HashMap::from([
                ("name".to_string(), tool_call.name.clone()),
                ("status".to_string(), "error".to_string()),
                ("detail".to_string(), detail),
            ]);
            let error = if spec.fail_on_tool_error {
                Some(err.clone())
            } else {
                None
            };
            return (format!("{err}{HINT}"), event, error);
        }

        // ── 3. Resolve and validate parameters ───────────────────────────────
        // Convert HashMap<String, Value> → Value::Object so prepare_call can
        // cast and validate the parameters against the tool's JSON schema.
        let params_value = Value::Object(
            tool_call
                .arguments
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        );
        let (tool, cast_params, prep_error) =
            spec.tools.prepare_call(&tool_call.name, &params_value);

        if let Some(err) = prep_error {
            // Strip the leading "Error: " / "Error: Tool '…':" prefix for the
            // short telemetry detail (mirrors Python's `split(": ", 1)[-1]`).
            let detail: String = err
                .splitn(2, ": ")
                .nth(1)
                .unwrap_or(&err)
                .chars()
                .take(120)
                .collect();
            let event = HashMap::from([
                ("name".to_string(), tool_call.name.clone()),
                ("status".to_string(), "error".to_string()),
                ("detail".to_string(), detail),
            ]);
            let error = if spec.fail_on_tool_error {
                Some(err.clone())
            } else {
                None
            };
            return (format!("{err}{HINT}"), event, error);
        }

        // ── 4. Execute ────────────────────────────────────────────────────────
        // `prepare_call` guarantees `tool = Some(_)` when `prep_error = None`.
        // Python's `await tool.execute(**params)` maps to the sync call below.
        // Unlike Python, Rust tools do not raise exceptions; a tool that wants
        // to signal failure returns a string starting with "Error".
        let result = tool
            .expect("prepare_call returned no tool and no error")
            .execute(&cast_params)
            .await;

        // ── 5. Treat "Error…" result strings as soft errors ──────────────────
        if result.starts_with("Error") {
            let detail: String = result.replace('\n', " ").trim().chars().take(120).collect();
            log::error!("Tool error: {}", result);
            let event = HashMap::from([
                ("name".to_string(), tool_call.name.clone()),
                ("status".to_string(), "error".to_string()),
                ("detail".to_string(), detail),
            ]);
            let error = if spec.fail_on_tool_error {
                Some(result.clone())
            } else {
                None
            };
            return (format!("{result}{HINT}"), event, error);
        }

        // ── 6. Success ────────────────────────────────────────────────────────
        let replaced = result.replace('\n', " ");
        let trimmed = replaced.trim();
        let detail = if trimmed.is_empty() {
            "(empty)".to_string()
        } else if trimmed.chars().count() > 120 {
            let cutoff = trimmed
                .char_indices()
                .nth(120)
                .map(|(i, _)| i)
                .unwrap_or(trimmed.len());
            format!("{}...", &trimmed[..cutoff])
        } else {
            trimmed.to_string()
        };

        let event = HashMap::from([
            ("name".to_string(), tool_call.name.clone()),
            ("status".to_string(), "ok".to_string()),
            ("detail".to_string(), detail),
        ]);
        (result, event, None)
    }

    fn append_final_message(messages: &mut Vec<Value>, content: Option<&str>) {
        if content.is_none() {
            return;
        }
        if !messages.is_empty()
            && messages
                .last()
                .and_then(|m| m.get("role").and_then(Value::as_str))
                == Some("assistant")
            && messages
                .last()
                .and_then(|m| m.get("tool_calls").and_then(Value::as_array))
                .is_none()
        {
            if messages
                .last()
                .and_then(|m| m.get("content").and_then(Value::as_str))
                == content
            {
                return;
            }
            let last_idx = messages.len() - 1;
            messages[last_idx] =
                build_assistant_message(content, Option::None, Option::None, Option::None);
            return;
        }
        messages.push(build_assistant_message(
            content,
            Option::None,
            Option::None,
            Option::None,
        ));
    }

    fn format_provider_error_for_user(detail: Option<&str>, fallback: &str) -> String {
        let detail = detail.unwrap_or("").trim();
        if is_unsupported_image_input_error(Some(detail)) {
            return UNSUPPORTED_IMAGE_INPUT_MESSAGE.to_string();
        }
        if detail.is_empty() {
            return fallback.to_string();
        }
        format!("Error: {detail}\n{fallback}")
    }

    async fn abort_with_provider_error(
        spec: &AgentRunSpec,
        hook: &Arc<dyn AgentHook>,
        ctx: &mut AgentHookContext,
        messages: &mut Vec<Value>,
        detail: Option<&str>,
    ) -> String {
        let fallback = spec
            .error_message
            .as_deref()
            .unwrap_or(DEFAULT_ERROR_MESSAGE);
        let error_msg = Self::format_provider_error_for_user(detail, fallback);
        if hook.wants_streaming() {
            hook.on_stream(ctx, &error_msg).await;
        }
        hook.on_stream_end(ctx, false).await;
        Self::append_final_message(messages, Some(&error_msg));
        log::error!("Provider-signalled error: {}", error_msg);
        ctx.stop_reason = Some("error".to_string());
        ctx.error = Some(error_msg.clone());
        ctx.final_content = Some(error_msg.clone());
        hook.after_iteration(ctx).await;
        error_msg
    }

    async fn request_finalization_retry(
        &self,
        spec: &AgentRunSpec,
        messages: &[Value],
    ) -> LLMResponse {
        let mut retry_messages = messages.to_vec();
        retry_messages.push(build_finalization_retry_message());
        self.provider
            .chat_with_retry(
                retry_messages,
                Option::None,
                Some(spec.model.clone()),
                spec.max_tokens.clone(),
                spec.temperature.clone(),
                spec.reasoning_effort.clone(),
                Option::None,
            )
            .await
    }

    /// Main agent iteration loop.
    ///
    /// Calls the LLM repeatedly, executing any tool calls it requests, until the
    /// model produces a final text response, an error occurs, or `max_iterations`
    /// is exhausted.  Returns a fully-populated [`AgentRunResult`].
    pub async fn run(&self, spec: AgentRunSpec) -> AgentRunResult {
        // ── Initialise per-run state ──────────────────────────────────────────
        let hook: Arc<dyn AgentHook> = spec.hook.clone().unwrap_or_else(|| Arc::new(NoopHook));

        let mut messages = spec.initial_messages.clone();
        let mut usage = LLMUsage::new();
        let mut all_tool_events: Vec<HashMap<String, String>> = Vec::new();
        let mut tools_used: Vec<String> = Vec::new();
        let mut stop_reason = "completed".to_string();
        let mut final_result_content: Option<String> = None;
        let mut final_error: Option<String> = None;

        let external_lookup_counts: Arc<Mutex<HashMap<String, usize>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let message_circuit_breaker: Arc<Mutex<MessageCircuitBreaker>> = Arc::new(Mutex::new(
            MessageCircuitBreaker::new(DEFAULT_N_IDENTICAL_MESSAGES),
        ));

        let mut empty_retries = 0usize;
        let mut length_recoveries = 0usize;
        // Tracks whether the loop ran to completion without breaking — Rust's
        // equivalent of Python's `for … else` idiom.
        let mut exhausted = true;

        'outer: for iteration in 0..spec.max_iterations {
            // ── Context governance ────────────────────────────────────────────
            let backfilled = Self::backfill_missing_tool_results(&messages);
            let compacted = Self::microcompact(&backfilled);
            let budgeted = self.apply_tool_result_budget(&spec, &compacted);
            let messages_for_model = self.snip_history(&spec, budgeted);

            let mut ctx = AgentHookContext::new(iteration, messages.clone());
            hook.before_iteration(&mut ctx).await;

            log::debug!(
                "Messages for model: {}",
                messages_for_model
                    .clone()
                    .iter()
                    .map(|m| m.to_string().chars().take(400).collect::<String>())
                    .collect::<Vec<String>>()
                    .join("\n")
            );
            // ── LLM call ──────────────────────────────────────────────────────
            let response = self
                .request_model(
                    &spec,
                    messages_for_model.clone(),
                    Arc::clone(&hook),
                    &mut ctx,
                )
                .await;
            log::info!(
                "Response: {}",
                response
                    .content
                    .clone()
                    .unwrap_or("".to_string())
                    .chars()
                    .take(400)
                    .collect::<String>()
            );
            log::info!("Usage: {}", response.usage);

            Self::accumulate_usage(&mut usage, &response.usage);
            ctx.usage = response.usage;
            ctx.response = Some(response.clone());

            // ── Tool calls branch ─────────────────────────────────────────────
            if !response.tool_calls.is_empty() {
                if response.finish_reason == "length" && length_recoveries < MAX_LENGTH_RECOVERIES {
                    hook.on_stream_end(&mut ctx, true).await;
                    log::warn!(
                        "Tool call response truncated (finish_reason=length); skipping execution"
                    );
                    messages.push(build_truncated_tool_call_recovery_message());
                    length_recoveries += 1;
                    hook.after_iteration(&mut ctx).await;
                    continue;
                }
                log::info!(
                    "Tool calls: {}",
                    response
                        .tool_calls
                        .iter()
                        .map(|tc| tc.to_string())
                        .collect::<Vec<String>>()
                        .join("\n")
                );
                hook.on_stream_end(&mut ctx, true).await;

                let tool_calls_json: Vec<Value> = response
                    .tool_calls
                    .iter()
                    .map(|tc| tc.to_openai_tool_call())
                    .collect();
                let thinking = Self::thinking_blocks_as_values(response.thinking_blocks.as_ref());
                let assistant_msg = build_assistant_message(
                    response.content.as_deref(),
                    Some(tool_calls_json),
                    response.reasoning_content.as_deref(),
                    thinking,
                );
                log::info!(
                    "Assistant message: {}",
                    assistant_msg
                        .clone()
                        .to_string()
                        .chars()
                        .take(400)
                        .collect::<String>()
                );
                messages.push(assistant_msg);
                Self::emit_checkpoint(&spec, serde_json::json!({"type": "awaiting_tools"}));

                ctx.tool_calls = response.tool_calls.clone();
                hook.before_execute_tools(&mut ctx).await;

                let (results, events, fatal_error) = Self::execute_tools(
                    &spec,
                    &response.tool_calls,
                    Arc::clone(&external_lookup_counts),
                    Arc::clone(&message_circuit_breaker),
                )
                .await;

                for tc in &response.tool_calls {
                    if !tools_used.contains(&tc.name) {
                        tools_used.push(tc.name.clone());
                    }
                }
                all_tool_events.extend(events.clone());
                ctx.tool_events = events;

                if let Some(err) = fatal_error.as_ref().filter(|e| !is_tripped_error(e)) {
                    log::error!("Fatal tool error: {}", err);
                    let error_msg = format!(
                        "Error: {}\n{}",
                        err,
                        spec.error_message
                            .as_deref()
                            .unwrap_or(DEFAULT_ERROR_MESSAGE)
                    );
                    stop_reason = "tool_error".to_string();
                    Self::append_final_message(&mut messages, Some(&error_msg));
                    ctx.stop_reason = Some("tool_error".to_string());
                    ctx.error = Some(err.clone());
                    hook.after_iteration(&mut ctx).await;
                    final_result_content = Some(error_msg.to_string());
                    final_error = Some(err.clone());
                    exhausted = false;
                    break 'outer;
                }

                // Append normalised tool result messages.
                for (tc, result) in response.tool_calls.iter().zip(results.into_iter()) {
                    let normalized = self.normalize_tool_result(
                        &spec,
                        &tc.id,
                        &tc.name,
                        coerce_tool_execute_result(result),
                    );
                    let tool_msg = serde_json::json!({
                        "role": "tool",
                        "tool_call_id": tc.id,
                        "name": tc.name,
                        "content": normalized,
                    });
                    ctx.tool_results.push(tool_msg.clone());
                    messages.push(tool_msg);
                }

                if fatal_error.as_deref().is_some_and(is_tripped_error) {
                    log::warn!("{CIRCUIT_BREAKER_USER_MESSAGE}");
                    stop_reason = CIRCUIT_BREAKER_STOP_REASON.to_string();
                    Self::append_final_message(&mut messages, Some(CIRCUIT_BREAKER_USER_MESSAGE));
                    ctx.stop_reason = Some(CIRCUIT_BREAKER_STOP_REASON.to_string());
                    ctx.error = fatal_error.clone();
                    hook.after_iteration(&mut ctx).await;
                    final_result_content = Some(CIRCUIT_BREAKER_USER_MESSAGE.to_string());
                    final_error = fatal_error;
                    exhausted = false;
                    break 'outer;
                }

                Self::emit_checkpoint(&spec, serde_json::json!({"type": "tools_completed"}));
                empty_retries = 0;
                length_recoveries = 0;
                hook.after_iteration(&mut ctx).await;
                continue;
            }

            // ── Text response branch ──────────────────────────────────────────
            let content = hook.finalize_content(&ctx, response.content.clone());

            // Provider-signalled error — surface it before empty-retry logic
            // so streaming clients receive the message in the open bubble
            // instead of an empty stream_end.
            if response.finish_reason == "error" {
                let error_msg = Self::abort_with_provider_error(
                    &spec,
                    &hook,
                    &mut ctx,
                    &mut messages,
                    content.as_deref().or(response.content.as_deref()),
                )
                .await;
                stop_reason = "error".to_string();
                final_result_content = Some(error_msg.clone());
                final_error = Some(error_msg);
                exhausted = false;
                break 'outer;
            }

            // Blank with retries remaining — keep going.
            if is_blank_text(content.as_deref()) && empty_retries < MAX_EMPTY_RETRIES {
                hook.on_stream_end(&mut ctx, false).await;
                ctx.final_content = content;
                hook.after_iteration(&mut ctx).await;
                empty_retries += 1;
                continue;
            }

            // Blank with retries exhausted — one last finalization attempt.
            let (final_content, finish_reason) = if is_blank_text(content.as_deref()) {
                let retry_resp = self
                    .request_finalization_retry(&spec, &messages_for_model)
                    .await;
                Self::accumulate_usage(&mut usage, &retry_resp.usage);
                let retry_content = hook.finalize_content(&ctx, retry_resp.content.clone());
                (retry_content, retry_resp.finish_reason)
            } else {
                log::info!("final_content: {:?}", content);
                (content, response.finish_reason.clone())
            };

            // Length-truncated response — append partial + recovery prompt.
            if finish_reason == "length" && length_recoveries < MAX_LENGTH_RECOVERIES {
                hook.on_stream_end(&mut ctx, true).await;
                let partial_thinking =
                    Self::thinking_blocks_as_values(response.thinking_blocks.as_ref());
                let partial_msg = build_assistant_message(
                    final_content.as_deref(),
                    None,
                    response.reasoning_content.as_deref(),
                    partial_thinking,
                );
                messages.push(partial_msg);
                messages.push(build_length_recovery_message());
                ctx.final_content = final_content;
                hook.after_iteration(&mut ctx).await;
                length_recoveries += 1;
                continue;
            }

            // Finalization retry (or a late-arriving provider error) can still
            // signal failure after the original response looked blank.
            if finish_reason == "error" {
                let error_msg = Self::abort_with_provider_error(
                    &spec,
                    &hook,
                    &mut ctx,
                    &mut messages,
                    final_content.as_deref(),
                )
                .await;
                stop_reason = "error".to_string();
                final_result_content = Some(error_msg.clone());
                final_error = Some(error_msg);
                exhausted = false;
                break 'outer;
            }

            hook.on_stream_end(&mut ctx, false).await;

            // Still blank after all retry paths.
            if is_blank_text(final_content.as_deref()) {
                stop_reason = "empty_final_response".to_string();
                Self::append_final_message(&mut messages, Some(EMPTY_FINAL_RESPONSE_MESSAGE));
                ctx.stop_reason = Some("empty_final_response".to_string());
                hook.after_iteration(&mut ctx).await;
                final_result_content = Some(EMPTY_FINAL_RESPONSE_MESSAGE.to_string());
                exhausted = false;
                break 'outer;
            }

            // Normal completion.
            let normal_thinking =
                Self::thinking_blocks_as_values(response.thinking_blocks.as_ref());
            let assistant_msg = build_assistant_message(
                final_content.as_deref(),
                None,
                response.reasoning_content.as_deref(),
                normal_thinking,
            );
            messages.push(assistant_msg);
            Self::emit_checkpoint(&spec, serde_json::json!({"type": "final_response"}));
            ctx.final_content = final_content.clone();
            hook.after_iteration(&mut ctx).await;
            final_result_content = final_content;
            exhausted = false;
            break 'outer;
        }

        // ── Post-loop: max_iterations exhausted ───────────────────────────────
        if exhausted {
            stop_reason = "max_iterations".to_string();
            let max_iter_msg = spec.max_iterations_message.as_deref();
            Self::append_final_message(&mut messages, max_iter_msg);
            final_result_content = max_iter_msg.map(str::to_string);
        }

        AgentRunResult {
            final_content: final_result_content,
            messages,
            tools_used,
            usage,
            stop_reason,
            error: final_error,
            tool_events: all_tool_events,
        }
    }

    /// Convert `LLMResponse::thinking_blocks` into the `Vec<Value>` form that
    /// `build_assistant_message` expects.
    fn thinking_blocks_as_values(
        blocks: Option<&Vec<HashMap<String, serde_json::Value>>>,
    ) -> Option<Vec<Value>> {
        blocks.map(|tb| {
            tb.iter()
                .map(|m| Value::Object(m.clone().into_iter().collect()))
                .collect()
        })
    }
}

/// No-op fallback hook used when `AgentRunSpec::hook` is `None`.
struct NoopHook;

#[async_trait::async_trait]
impl AgentHook for NoopHook {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tools::message::MessageTool;
    use crate::bus::events::OutboundMessage;
    use crate::providers::base::{GenerationSettings, LLMProviderDyn, LLMResponse};
    use crate::providers::registry::ProviderSpec;

    fn unused_message_circuit_breaker() -> Arc<Mutex<MessageCircuitBreaker>> {
        Arc::new(Mutex::new(MessageCircuitBreaker::new(
            DEFAULT_N_IDENTICAL_MESSAGES,
        )))
    }

    /// Minimal provider that satisfies `LLMProviderDyn` for tests that don't
    /// exercise the provider (e.g. `normalize_tool_result`).
    struct StubProvider {
        settings: GenerationSettings,
    }

    impl StubProvider {
        fn new() -> Arc<dyn LLMProviderDyn> {
            Arc::new(Self {
                settings: GenerationSettings::new(),
            })
        }
    }

    #[async_trait::async_trait]
    impl LLMProviderDyn for StubProvider {
        fn api_key(&self) -> Option<String> {
            None
        }
        fn api_base(&self) -> Option<String> {
            None
        }
        fn extra_headers(&self) -> Option<std::collections::HashMap<String, String>> {
            None
        }
        fn generation_settings(&self) -> &GenerationSettings {
            &self.settings
        }
        fn generation_settings_mut(&mut self) -> &mut GenerationSettings {
            &mut self.settings
        }
        fn spec(&self) -> Option<&ProviderSpec> {
            None
        }
        fn get_default_model(&self) -> String {
            String::new()
        }
        async fn chat(
            &self,
            _: Vec<Value>,
            _: Option<Vec<Value>>,
            _: Option<String>,
            _: usize,
            _: Option<f32>,
            _: Option<String>,
            _: Option<Value>,
        ) -> LLMResponse {
            unimplemented!()
        }
        async fn safe_chat(
            &self,
            _: Vec<Value>,
            _: Option<Vec<Value>>,
            _: Option<String>,
            _: usize,
            _: Option<f32>,
            _: Option<String>,
            _: Option<Value>,
        ) -> LLMResponse {
            unimplemented!()
        }
        async fn chat_with_retry(
            &self,
            _: Vec<Value>,
            _: Option<Vec<Value>>,
            _: Option<String>,
            _: Option<usize>,
            _: Option<f32>,
            _: Option<String>,
            _: Option<Value>,
        ) -> LLMResponse {
            LLMResponse {
                content: Some("Hello, world!".to_string()),
                finish_reason: "stop".to_string(),
                tool_calls: Vec::new(),
                usage: LLMUsage::new(),
                reasoning_content: None,
                thinking_blocks: None,
            }
        }
        async fn chat_stream_with_retry_boxed(
            &self,
            _: Vec<Value>,
            _: Option<Vec<Value>>,
            _: Option<String>,
            _: Option<usize>,
            _: Option<f32>,
            _: Option<String>,
            _: Option<Value>,
            _: Option<BoxedStreamCallback>,
        ) -> LLMResponse {
            unimplemented!()
        }
    }

    fn make_runner() -> AgentRunner {
        AgentRunner::new(StubProvider::new())
    }

    struct FixedResponseProvider {
        settings: GenerationSettings,
        response: LLMResponse,
    }

    impl FixedResponseProvider {
        fn new(response: LLMResponse) -> Arc<dyn LLMProviderDyn> {
            Arc::new(Self {
                settings: GenerationSettings::new(),
                response,
            })
        }
    }

    #[async_trait::async_trait]
    impl LLMProviderDyn for FixedResponseProvider {
        fn api_key(&self) -> Option<String> {
            None
        }
        fn api_base(&self) -> Option<String> {
            None
        }
        fn extra_headers(&self) -> Option<std::collections::HashMap<String, String>> {
            None
        }
        fn generation_settings(&self) -> &GenerationSettings {
            &self.settings
        }
        fn generation_settings_mut(&mut self) -> &mut GenerationSettings {
            &mut self.settings
        }
        fn spec(&self) -> Option<&ProviderSpec> {
            None
        }
        fn get_default_model(&self) -> String {
            String::new()
        }
        async fn chat(
            &self,
            _: Vec<Value>,
            _: Option<Vec<Value>>,
            _: Option<String>,
            _: usize,
            _: Option<f32>,
            _: Option<String>,
            _: Option<Value>,
        ) -> LLMResponse {
            self.response.clone()
        }
        async fn safe_chat(
            &self,
            _: Vec<Value>,
            _: Option<Vec<Value>>,
            _: Option<String>,
            _: usize,
            _: Option<f32>,
            _: Option<String>,
            _: Option<Value>,
        ) -> LLMResponse {
            self.response.clone()
        }
        async fn chat_with_retry(
            &self,
            _: Vec<Value>,
            _: Option<Vec<Value>>,
            _: Option<String>,
            _: Option<usize>,
            _: Option<f32>,
            _: Option<String>,
            _: Option<Value>,
        ) -> LLMResponse {
            self.response.clone()
        }
        async fn chat_stream_with_retry_boxed(
            &self,
            _: Vec<Value>,
            _: Option<Vec<Value>>,
            _: Option<String>,
            _: Option<usize>,
            _: Option<f32>,
            _: Option<String>,
            _: Option<Value>,
            _: Option<BoxedStreamCallback>,
        ) -> LLMResponse {
            self.response.clone()
        }
    }

    struct RecordingHook {
        wants_stream: bool,
        stream_content: Mutex<String>,
    }

    impl RecordingHook {
        fn streaming() -> Arc<Self> {
            Arc::new(Self {
                wants_stream: true,
                stream_content: Mutex::new(String::new()),
            })
        }

        fn streamed(&self) -> String {
            self.stream_content
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        }
    }

    #[async_trait::async_trait]
    impl AgentHook for RecordingHook {
        fn wants_streaming(&self) -> bool {
            self.wants_stream
        }

        async fn on_stream(&self, _ctx: &mut AgentHookContext, delta: &str) {
            self.stream_content
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push_str(delta);
        }
    }

    fn vision_error_response() -> LLMResponse {
        LLMResponse {
            content: Some("No endpoints found that support image input".to_string()),
            finish_reason: "error".to_string(),
            tool_calls: Vec::new(),
            usage: LLMUsage::new(),
            reasoning_content: None,
            thinking_blocks: None,
        }
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
        assert_eq!(result.usage, LLMUsage::new());
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
        let backfilled = result
            .iter()
            .find(|m| m["tool_call_id"] == "absent")
            .unwrap();
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
            assert_eq!(
                result[i]["content"],
                "[read_file result omitted from context]"
            );
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

    #[test]
    fn test_microcompact_never_collapses_current_turn_results() {
        // Tools after the last user message are the current turn, including
        // earlier assistant rounds. They must stay intact even when they
        // outnumber MICROCOMPACT_KEEP_RECENT.
        let large_content = "x".repeat(MICROCOMPACT_MIN_CHARS + 1);
        let tool =
            serde_json::json!({"role": "tool", "name": "read_file", "content": large_content});
        let mut messages = vec![
            serde_json::json!({"role": "user", "content": "do the task"}),
            serde_json::json!({"role": "assistant", "content": "round 1"}),
        ];
        messages.extend((0..MICROCOMPACT_KEEP_RECENT + 1).map(|_| tool.clone()));
        messages.push(serde_json::json!({"role": "assistant", "content": "round 2"}));
        messages.push(tool);

        let result = AgentRunner::microcompact(&messages);
        assert_eq!(result, messages);
    }

    #[test]
    fn test_microcompact_still_collapses_previous_turn_results() {
        // Compactable results before the last user message are still stale.
        // Current-turn tools (including earlier rounds) do not count toward
        // KEEP_RECENT and are left intact.
        let large_content = "x".repeat(MICROCOMPACT_MIN_CHARS + 1);
        let tool =
            serde_json::json!({"role": "tool", "name": "read_file", "content": large_content});
        let omitted = "[read_file result omitted from context]";
        let previous_count = MICROCOMPACT_KEEP_RECENT + 2;

        let mut messages = vec![serde_json::json!({"role": "user", "content": "old question"})];
        messages.extend((0..previous_count).map(|_| tool.clone()));
        messages.push(serde_json::json!({"role": "assistant", "content": "done"}));
        messages.push(serde_json::json!({"role": "user", "content": "next question"}));
        messages.push(serde_json::json!({"role": "assistant", "content": "round 1"}));
        messages.push(tool.clone());
        messages.push(serde_json::json!({"role": "assistant", "content": "round 2"}));
        messages.push(tool);

        let result = AgentRunner::microcompact(&messages);
        assert_eq!(result.len(), messages.len());

        // First two previous-turn tools are stale and large — collapsed.
        // previous-turn tools occupy indices 1..=previous_count.
        assert_eq!(result[1]["content"], omitted);
        assert_eq!(result[2]["content"], omitted);
        for i in 3..=previous_count {
            assert_eq!(result[i]["content"], large_content.as_str());
        }
        // Last user message onward is untouched.
        let current_turn_start = previous_count + 2; // assistant after old tools, then user
        assert_eq!(
            &result[current_turn_start..],
            &messages[current_turn_start..]
        );
    }

    // ── normalize_tool_result ─────────────────────────────────────────────────

    #[test]
    fn test_normalize_null_result_replaced() {
        // Null input is replaced by ensure_nonempty_tool_result with a non-empty message.
        let runner = make_runner();
        let spec = AgentRunSpec {
            max_tool_result_chars: 1000,
            ..Default::default()
        };
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
        let spec = AgentRunSpec {
            max_tool_result_chars: 1000,
            ..Default::default()
        };
        let out =
            runner.normalize_tool_result(&spec, "call_1", "my_tool", Value::String("   ".into()));
        match out {
            Value::String(s) => assert!(!s.trim().is_empty()),
            other => panic!("expected non-blank String, got {other:?}"),
        }
    }

    #[test]
    fn test_normalize_content_within_limit_unchanged() {
        // A short result within max_tool_result_chars is returned as-is.
        let runner = make_runner();
        let spec = AgentRunSpec {
            max_tool_result_chars: 1000,
            ..Default::default()
        };
        let content = "hello world".to_string();
        let out = runner.normalize_tool_result(
            &spec,
            "call_1",
            "my_tool",
            Value::String(content.clone()),
        );
        assert_eq!(out, Value::String(content));
    }

    #[test]
    fn test_normalize_content_truncated_when_over_limit() {
        // A string longer than max_tool_result_chars is truncated (no workspace, so no persist).
        let runner = make_runner();
        let limit = 20;
        let spec = AgentRunSpec {
            max_tool_result_chars: limit,
            ..Default::default()
        };
        let long_content = "a".repeat(limit + 50);
        let out =
            runner.normalize_tool_result(&spec, "call_1", "my_tool", Value::String(long_content));
        match out {
            Value::String(s) => assert!(s.len() <= limit + "\n... (truncated)".len()),
            other => panic!("expected truncated String, got {other:?}"),
        }
    }

    #[test]
    fn test_normalize_zero_limit_disables_truncation() {
        // max_tool_result_chars = 0 means unlimited: even very long content is not truncated.
        let runner = make_runner();
        let spec = AgentRunSpec {
            max_tool_result_chars: 0,
            ..Default::default()
        };
        let long_content = "z".repeat(10_000);
        let out = runner.normalize_tool_result(
            &spec,
            "call_1",
            "my_tool",
            Value::String(long_content.clone()),
        );
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
        let out = runner.normalize_tool_result(
            &spec,
            "call_persist",
            "read_file",
            Value::String(large_content),
        );
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
        let spec = AgentRunSpec {
            max_tool_result_chars: 10,
            ..Default::default()
        };
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
        let spec = AgentRunSpec {
            max_tool_result_chars: 1000,
            ..Default::default()
        };
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
        let spec = AgentRunSpec {
            max_tool_result_chars: limit,
            ..Default::default()
        };
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
        let spec = AgentRunSpec {
            max_tool_result_chars: 1000,
            ..Default::default()
        };
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
        let spec = AgentRunSpec {
            max_tool_result_chars: 1000,
            ..Default::default()
        };
        let messages = vec![serde_json::json!({"role": "tool", "name": "exec", "content": "done"})];
        // No panic — fallback id is generated internally.
        let result = runner.apply_tool_result_budget(&spec, &messages);
        assert_eq!(result[0]["content"].as_str().unwrap(), "done");
    }

    #[test]
    fn test_apply_budget_only_modifies_tool_messages() {
        // Non-tool messages adjacent to a truncated tool message are untouched.
        let runner = make_runner();
        let limit = 10;
        let spec = AgentRunSpec {
            max_tool_result_chars: limit,
            ..Default::default()
        };
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

    #[test]
    fn test_snip_history_no_change_when_within_budget() {
        // Two tiny messages — well within any reasonable budget, returned unchanged.
        let messages = vec![
            serde_json::json!({"role": "user", "content": "go"}),
            serde_json::json!({"role": "assistant", "content": "done"}),
        ];
        let spec = AgentRunSpec {
            context_window_tokens: Some(8_000),
            context_block_limit: Some(4_000),
            ..Default::default()
        };
        let runner = make_runner();
        let result = runner.snip_history(&spec, messages.clone());
        assert_eq!(result, messages);
    }

    #[test]
    fn test_snip_history_trims_old_messages() {
        // Build a history where old turns are very large and a recent turn is tiny.
        // The budget (200 tokens) fits only the recent user+assistant pair; the two
        // old pairs must be dropped.
        //
        // Token estimates (char / 4 + 4 per message):
        //   old message  : 2 000 chars → ~504 tokens each
        //   recent message:   20 chars →   ~9 tokens each
        //   recent pair total           ≈   18 tokens  ≪  200 budget  ✓
        //   adding any old message      ≈  522 tokens  ≫  200 budget  ✓  (triggers break)
        let old_content = "x".repeat(2_000);
        let recent_user = serde_json::json!({"role": "user",     "content": "what is 2+2?"});
        let recent_asst = serde_json::json!({"role": "assistant", "content": "the answer is 4"});

        let messages = vec![
            serde_json::json!({"role": "user",     "content": old_content.clone()}),
            serde_json::json!({"role": "assistant", "content": old_content.clone()}),
            serde_json::json!({"role": "user",     "content": old_content.clone()}),
            serde_json::json!({"role": "assistant", "content": old_content.clone()}),
            recent_user.clone(),
            recent_asst.clone(),
        ];

        let spec = AgentRunSpec {
            context_window_tokens: Some(8_000),
            context_block_limit: Some(200),
            ..Default::default()
        };
        let runner = make_runner();
        let result = runner.snip_history(&spec, messages);

        // Only the recent pair should survive.
        assert_eq!(result.len(), 2, "expected only the recent pair to be kept");
        assert_eq!(result[0], recent_user);
        assert_eq!(result[1], recent_asst);
    }

    #[test]
    fn test_snip_history_never_drops_current_user_turn() {
        // Regression test for the bug where a single turn's large tool results
        // (e.g. several web_search calls) consumed the entire snip budget on
        // their own, causing the *current* user question to be dropped while
        // its tool results were kept — leaving the model with results but no
        // idea what was asked. The current turn (from the last user message
        // onward) must always survive snipping, even if it alone exceeds the
        // configured budget.
        let user_msg = serde_json::json!({
            "role": "user",
            "content": "What are the best newsletter software offerings for a small business?"
        });
        let assistant_msg = serde_json::json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [
                {"id": "call_1", "type": "function", "function": {"name": "web_search", "arguments": "{}"}},
                {"id": "call_2", "type": "function", "function": {"name": "web_search", "arguments": "{}"}},
                {"id": "call_3", "type": "function", "function": {"name": "web_search", "arguments": "{}"}},
            ]
        });
        let large_result = "x".repeat(2_000);
        let tool_1 = serde_json::json!({"role": "tool", "tool_call_id": "call_1", "name": "web_search", "content": large_result.clone()});
        let tool_2 = serde_json::json!({"role": "tool", "tool_call_id": "call_2", "name": "web_search", "content": large_result.clone()});
        let tool_3 = serde_json::json!({"role": "tool", "tool_call_id": "call_3", "name": "web_search", "content": large_result.clone()});

        let messages = vec![
            user_msg.clone(),
            assistant_msg.clone(),
            tool_1.clone(),
            tool_2.clone(),
            tool_3.clone(),
        ];

        // A tiny budget that the three large tool results alone vastly exceed.
        let spec = AgentRunSpec {
            context_window_tokens: Some(8_000),
            context_block_limit: Some(100),
            ..Default::default()
        };
        let runner = make_runner();
        let result = runner.snip_history(&spec, messages.clone());

        // The user question must survive, and the whole sequence must remain
        // legal (every tool result's call id is declared by a preceding
        // assistant message within the kept window).
        assert!(
            result
                .iter()
                .any(|m| m.get("role").and_then(Value::as_str) == Some("user")),
            "current user turn was dropped by snip_history: {result:?}"
        );
        assert_eq!(
            result, messages,
            "the whole single turn should be kept intact"
        );
    }

    #[test]
    fn test_snip_history_keeps_prior_user_facts_when_old_tools_overflow() {
        // Mirrors the Willesden Green session: a prior user fact plus a
        // tool-heavy weather lookup, then a short follow-up. Newest-first
        // greedy snip used to hit the huge tool result and drop the fact.
        let prior_user = serde_json::json!({
            "role": "user",
            "content": "I live in Willesden Green. How is the weather there?"
        });
        let lookup = serde_json::json!({
            "role": "assistant",
            "content": "I'll look up the weather.",
            "tool_calls": [
                {"id": "call_1", "type": "function", "function": {"name": "web_search", "arguments": "{}"}}
            ]
        });
        let huge_tool = serde_json::json!({
            "role": "tool",
            "tool_call_id": "call_1",
            "name": "web_search",
            "content": "x".repeat(2_000)
        });
        let prior_reply = serde_json::json!({
            "role": "assistant",
            "content": "Damp and mild, about 17C."
        });
        let follow_up = serde_json::json!({"role": "user", "content": "Where do I live?"});

        let messages = vec![
            prior_user.clone(),
            lookup,
            huge_tool,
            prior_reply.clone(),
            follow_up.clone(),
        ];

        // Fits current question + compacted prior turn, but not the tool dump.
        let spec = AgentRunSpec {
            context_window_tokens: Some(8_000),
            context_block_limit: Some(80),
            ..Default::default()
        };
        let runner = make_runner();
        let result = runner.snip_history(&spec, messages);

        let roles: Vec<&str> = result
            .iter()
            .filter_map(|m| m.get("role").and_then(Value::as_str))
            .collect();
        assert_eq!(
            roles,
            vec!["user", "assistant", "user"],
            "expected compacted prior turn + follow-up, got {result:?}"
        );
        assert_eq!(result[0], prior_user);
        assert_eq!(result[1], prior_reply);
        assert_eq!(result[2], follow_up);
        assert!(
            !result
                .iter()
                .any(|m| m.get("role").and_then(Value::as_str) == Some("tool")),
            "huge prior tool result should have been dropped"
        );
    }

    // ── run_tool ──────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_run_tool_repeated_external_lookup_blocked() {
        // A repeated external lookup is blocked and returns an error.
        let spec = AgentRunSpec {
            fail_on_tool_error: true,
            ..Default::default()
        };
        let tool_call = crate::providers::base::ToolCallRequest {
            id: "1".to_string(),
            name: "dummy_tool".to_string(),
            arguments: HashMap::new(),
            extra_content: None,
            provider_specific_fields: None,
            function_provider_specific_fields: None,
        };
        let external_lookup_counts = HashMap::<String, usize>::new();
        let (result, event, fatal_error) = AgentRunner::run_tool(
            &spec,
            &tool_call,
            Arc::new(Mutex::new(external_lookup_counts)),
            unused_message_circuit_breaker(),
        )
        .await;
        assert_eq!(
            result,
            "Error: Tool 'dummy_tool' not found. Available: \n\n[Analyze the error above and try a different approach.]"
        );
        assert_eq!(event.get("name").unwrap(), &"dummy_tool".to_string());
        assert_eq!(event.get("status").unwrap(), &"error".to_string());
        assert_eq!(
            event.get("detail").unwrap(),
            &"Tool 'dummy_tool' not found. Available: ".to_string()
        );
        assert_eq!(
            fatal_error,
            Some("Error: Tool 'dummy_tool' not found. Available: ".to_string())
        );
    }

    #[tokio::test]
    async fn test_run_tool_malformed_arguments_json_returns_first_class_error() {
        let spec = AgentRunSpec {
            fail_on_tool_error: true,
            ..Default::default()
        };
        let mut arguments = HashMap::new();
        arguments.insert(
            "__args_json_parse_error".to_string(),
            Value::String("expected `,` at line 1 column 17".to_string()),
        );
        arguments.insert(
            "__args_json_raw".to_string(),
            Value::String("{\"path\":\"a.txt\" \"content\":\"x\"}".to_string()),
        );
        let tool_call = crate::providers::base::ToolCallRequest {
            id: "1".to_string(),
            name: "write_file".to_string(),
            arguments,
            extra_content: None,
            provider_specific_fields: None,
            function_provider_specific_fields: None,
        };
        let external_lookup_counts = HashMap::<String, usize>::new();
        let (result, event, fatal_error) = AgentRunner::run_tool(
            &spec,
            &tool_call,
            Arc::new(Mutex::new(external_lookup_counts)),
            unused_message_circuit_breaker(),
        )
        .await;
        assert!(result.contains("Error: malformed tool arguments JSON for 'write_file'"));
        assert!(result.contains("expected `,` at line 1 column 17"));
        assert_eq!(event.get("name").unwrap(), &"write_file".to_string());
        assert_eq!(event.get("status").unwrap(), &"error".to_string());
        assert!(
            fatal_error
                .unwrap_or_default()
                .contains("malformed tool arguments JSON for 'write_file'")
        );
    }

    #[tokio::test]
    async fn test_run_tool_message_circuit_breaker_skips_nth_send_and_is_always_fatal() {
        let captured: Arc<Mutex<Vec<OutboundMessage>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&captured);
        let callback: crate::agent::tools::message::SendCallback = Arc::new(move |msg| {
            let sink = Arc::clone(&sink);
            Box::pin(async move {
                sink.lock().unwrap().push(msg);
            })
        });
        let mut tools = ToolRegistry::new();
        tools.register(Box::new(MessageTool::new(
            Some(callback),
            "websocket",
            "chat-1",
            None,
        )));
        let spec = AgentRunSpec {
            tools,
            fail_on_tool_error: false,
            ..Default::default()
        };
        let breaker = Arc::new(Mutex::new(MessageCircuitBreaker::new(
            DEFAULT_N_IDENTICAL_MESSAGES,
        )));
        let mut arguments = HashMap::new();
        arguments.insert(
            "content".to_string(),
            Value::String("placeholder".to_string()),
        );
        let tool_call = crate::providers::base::ToolCallRequest {
            id: "1".to_string(),
            name: "message".to_string(),
            arguments,
            extra_content: None,
            provider_specific_fields: None,
            function_provider_specific_fields: None,
        };

        for i in 0..DEFAULT_N_IDENTICAL_MESSAGES {
            let (result, event, fatal_error) = AgentRunner::run_tool(
                &spec,
                &tool_call,
                Arc::new(Mutex::new(HashMap::new())),
                Arc::clone(&breaker),
            )
            .await;
            if i + 1 < DEFAULT_N_IDENTICAL_MESSAGES {
                assert!(fatal_error.is_none(), "call {} should be allowed", i + 1);
                assert_eq!(event.get("status").unwrap(), "ok");
                assert!(result.contains("Message sent"));
            } else {
                assert!(is_tripped_error(fatal_error.as_deref().unwrap_or("")));
                assert_eq!(event.get("status").unwrap(), "error");
                assert!(is_tripped_error(&result));
            }
        }

        assert_eq!(
            captured.lock().unwrap().len(),
            DEFAULT_N_IDENTICAL_MESSAGES - 1,
            "the tripping call must not be sent"
        );
    }

    // ── partition_tool_batches ────────────────────────────────────────────────

    /// Build a minimal `ToolCallRequest` with the given name and no arguments.
    fn tc(name: &str) -> ToolCallRequest {
        ToolCallRequest {
            id: name.to_string(),
            name: name.to_string(),
            arguments: HashMap::new(),
            extra_content: None,
            provider_specific_fields: None,
            function_provider_specific_fields: None,
        }
    }

    /// Flatten batch references into names for easy assertions.
    fn batch_names<'a>(batches: &[Vec<&'a ToolCallRequest>]) -> Vec<Vec<&'a str>> {
        batches
            .iter()
            .map(|b| b.iter().map(|tc| tc.name.as_str()).collect())
            .collect()
    }

    /// A tool that explicitly opts in to concurrent execution.
    struct ConcurrentSafeTool(String);
    #[async_trait::async_trait]
    impl crate::agent::tools::base::Tool for ConcurrentSafeTool {
        fn name(&self) -> String {
            self.0.clone()
        }
        fn description(&self) -> String {
            "safe".into()
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type":"object","properties":{}})
        }
        async fn execute(&self, _: &serde_json::Value) -> String {
            "ok".into()
        }
        fn concurrency_safe(&self) -> bool {
            true
        }
    }

    /// A tool that relies on the default (non-concurrent-safe).
    struct SerialTool(String);
    #[async_trait::async_trait]
    impl crate::agent::tools::base::Tool for SerialTool {
        fn name(&self) -> String {
            self.0.clone()
        }
        fn description(&self) -> String {
            "serial".into()
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type":"object","properties":{}})
        }
        async fn execute(&self, _: &serde_json::Value) -> String {
            "ok".into()
        }
        // concurrency_safe() defaults to false
    }

    fn registry_with(tools: Vec<Box<dyn crate::agent::tools::base::Tool>>) -> ToolRegistry {
        let mut reg = ToolRegistry::new();
        for t in tools {
            reg.register(t);
        }
        reg
    }

    fn spec_concurrent(tools: ToolRegistry) -> AgentRunSpec {
        AgentRunSpec {
            concurrent_tools: true,
            tools,
            ..Default::default()
        }
    }

    fn spec_sequential(tools: ToolRegistry) -> AgentRunSpec {
        AgentRunSpec {
            concurrent_tools: false,
            tools,
            ..Default::default()
        }
    }

    #[test]
    fn test_partition_empty_input() {
        // No tool calls → no batches.
        let spec = spec_concurrent(ToolRegistry::new());
        let result = AgentRunner::partition_tool_batches(&spec, &[]);
        assert!(result.is_empty());
    }

    #[test]
    fn test_partition_non_concurrent_mode_always_singletons() {
        // Even concurrency-safe tools must be serialised when the flag is off.
        let reg = registry_with(vec![
            Box::new(ConcurrentSafeTool("a".into())),
            Box::new(ConcurrentSafeTool("b".into())),
        ]);
        let calls = vec![tc("a"), tc("b")];
        let spec = spec_sequential(reg);
        assert_eq!(
            batch_names(&AgentRunner::partition_tool_batches(&spec, &calls)),
            vec![vec!["a"], vec!["b"]]
        );
    }

    #[test]
    fn test_partition_all_safe_merged_into_one_batch() {
        // All safe tools with concurrency enabled → single batch.
        let reg = registry_with(vec![
            Box::new(ConcurrentSafeTool("x".into())),
            Box::new(ConcurrentSafeTool("y".into())),
            Box::new(ConcurrentSafeTool("z".into())),
        ]);
        let calls = vec![tc("x"), tc("y"), tc("z")];
        let spec = spec_concurrent(reg);
        assert_eq!(
            batch_names(&AgentRunner::partition_tool_batches(&spec, &calls)),
            vec![vec!["x", "y", "z"]]
        );
    }

    #[test]
    fn test_partition_all_serial_each_is_singleton() {
        // Non-safe tools are serialisation barriers — each gets its own batch.
        let reg = registry_with(vec![
            Box::new(SerialTool("p".into())),
            Box::new(SerialTool("q".into())),
        ]);
        let calls = vec![tc("p"), tc("q")];
        let spec = spec_concurrent(reg);
        assert_eq!(
            batch_names(&AgentRunner::partition_tool_batches(&spec, &calls)),
            vec![vec!["p"], vec!["q"]]
        );
    }

    #[test]
    fn test_partition_safe_batch_flushed_by_serial_tool() {
        // safe, safe, serial → [safe, safe] then [serial].
        let reg = registry_with(vec![
            Box::new(ConcurrentSafeTool("a".into())),
            Box::new(ConcurrentSafeTool("b".into())),
            Box::new(SerialTool("c".into())),
        ]);
        let calls = vec![tc("a"), tc("b"), tc("c")];
        let spec = spec_concurrent(reg);
        assert_eq!(
            batch_names(&AgentRunner::partition_tool_batches(&spec, &calls)),
            vec![vec!["a", "b"], vec!["c"]]
        );
    }

    #[test]
    fn test_partition_serial_tool_followed_by_safe_batch() {
        // serial, safe, safe → [serial] then [safe, safe].
        let reg = registry_with(vec![
            Box::new(SerialTool("a".into())),
            Box::new(ConcurrentSafeTool("b".into())),
            Box::new(ConcurrentSafeTool("c".into())),
        ]);
        let calls = vec![tc("a"), tc("b"), tc("c")];
        let spec = spec_concurrent(reg);
        assert_eq!(
            batch_names(&AgentRunner::partition_tool_batches(&spec, &calls)),
            vec![vec!["a"], vec!["b", "c"]]
        );
    }

    #[test]
    fn test_partition_serial_tool_splits_safe_batches() {
        // safe, safe, serial, safe, safe → two safe batches separated by serial.
        let reg = registry_with(vec![
            Box::new(ConcurrentSafeTool("a".into())),
            Box::new(ConcurrentSafeTool("b".into())),
            Box::new(SerialTool("barrier".into())),
            Box::new(ConcurrentSafeTool("c".into())),
            Box::new(ConcurrentSafeTool("d".into())),
        ]);
        let calls = vec![tc("a"), tc("b"), tc("barrier"), tc("c"), tc("d")];
        let spec = spec_concurrent(reg);
        assert_eq!(
            batch_names(&AgentRunner::partition_tool_batches(&spec, &calls)),
            vec![vec!["a", "b"], vec!["barrier"], vec!["c", "d"]]
        );
    }

    #[test]
    fn test_partition_unknown_tool_treated_as_non_safe() {
        // A tool not in the registry is treated as a serialisation barrier.
        let reg = registry_with(vec![Box::new(ConcurrentSafeTool("known".into()))]);
        let calls = vec![tc("known"), tc("ghost"), tc("known")];
        let spec = spec_concurrent(reg);
        assert_eq!(
            batch_names(&AgentRunner::partition_tool_batches(&spec, &calls)),
            vec![vec!["known"], vec!["ghost"], vec!["known"]]
        );
    }

    #[test]
    fn test_append_final_message_empty_messages() {
        let mut messages = Vec::new();
        AgentRunner::append_final_message(&mut messages, Some("Hello, world!"));
        assert_eq!(messages.len(), 1);
        assert_eq!(
            messages,
            vec![build_assistant_message(
                Some("Hello, world!"),
                Option::None,
                Option::None,
                Option::None
            )]
        );
    }

    #[test]
    fn test_assistant_message_unchanged() {
        let mut messages = vec![build_assistant_message(
            Some("Hello, world!"),
            Option::None,
            Option::None,
            Option::None,
        )];
        AgentRunner::append_final_message(&mut messages, Some("Hello, world!"));
        assert_eq!(messages.len(), 1);
        assert_eq!(
            messages,
            vec![build_assistant_message(
                Some("Hello, world!"),
                Option::None,
                Option::None,
                Option::None
            )]
        );
    }

    #[test]
    fn test_assistant_message_changed() {
        let mut messages = vec![build_assistant_message(
            Some("Hello, world!"),
            Option::None,
            Option::None,
            Option::None,
        )];
        AgentRunner::append_final_message(&mut messages, Some("Hello, universe!"));
        assert_eq!(messages.len(), 1);
        assert_eq!(
            messages,
            vec![build_assistant_message(
                Some("Hello, universe!"),
                Option::None,
                Option::None,
                Option::None
            )]
        );
    }

    #[tokio::test]
    async fn request_finalization_retry_empty_messages() {
        let runner = make_runner();
        let spec = AgentRunSpec {
            model: "gpt-4o".to_string(),
            max_tokens: Some(1000),
            temperature: Some(0.5),
            reasoning_effort: Some("medium".to_string()),
            ..Default::default()
        };
        let mut messages = Vec::new();
        let result = runner
            .request_finalization_retry(&spec, &mut messages)
            .await;
        assert_eq!(result.content, Some("Hello, world!".to_string()));
    }

    #[test]
    fn format_provider_error_for_user_uses_vision_message() {
        let msg = AgentRunner::format_provider_error_for_user(
            Some("No endpoints found that support image input"),
            DEFAULT_ERROR_MESSAGE,
        );
        assert_eq!(msg, UNSUPPORTED_IMAGE_INPUT_MESSAGE);
        assert!(!msg.contains(DEFAULT_ERROR_MESSAGE));
    }

    #[test]
    fn format_provider_error_for_user_includes_generic_detail() {
        let msg = AgentRunner::format_provider_error_for_user(
            Some("model overloaded"),
            DEFAULT_ERROR_MESSAGE,
        );
        assert!(msg.contains("model overloaded"));
        assert!(msg.contains(DEFAULT_ERROR_MESSAGE));
    }

    #[tokio::test]
    async fn run_surfaces_unsupported_image_input_as_error() {
        let runner = AgentRunner::new(FixedResponseProvider::new(vision_error_response()));
        let result = runner
            .run(AgentRunSpec {
                initial_messages: vec![serde_json::json!({
                    "role": "user",
                    "content": "describe this image"
                })],
                model: "test".to_string(),
                max_iterations: 2,
                ..Default::default()
            })
            .await;
        assert_eq!(result.stop_reason, "error");
        assert_eq!(
            result.final_content.as_deref(),
            Some(UNSUPPORTED_IMAGE_INPUT_MESSAGE)
        );
        assert_eq!(
            result.error.as_deref(),
            Some(UNSUPPORTED_IMAGE_INPUT_MESSAGE)
        );
    }

    #[tokio::test]
    async fn run_streams_unsupported_image_input_error_to_hook() {
        let hook = RecordingHook::streaming();
        let runner = AgentRunner::new(FixedResponseProvider::new(vision_error_response()));
        let result = runner
            .run(AgentRunSpec {
                initial_messages: vec![serde_json::json!({
                    "role": "user",
                    "content": "describe this image"
                })],
                model: "test".to_string(),
                max_iterations: 2,
                hook: Some(Arc::clone(&hook) as Arc<dyn AgentHook>),
                ..Default::default()
            })
            .await;
        assert_eq!(result.stop_reason, "error");
        assert_eq!(hook.streamed(), UNSUPPORTED_IMAGE_INPUT_MESSAGE);
    }
}
