//! Stage-by-stage execution of a single tool call.
//!
//! A call goes through two phases. [`prepare_tool_call`] runs the stages that
//! decide *whether* the call may run; [`execute_prepared_call`] runs it. The
//! runner prepares the calls of a batch one after another (so approval prompts
//! are asked one at a time, in order) and then executes them, concurrently where
//! the tools allow it. [`run_tool_call`] chains both for a single call.
//!
//! | # | stage              | phase   | what it does                                       |
//! |---|--------------------|---------|----------------------------------------------------|
//! | 1 | parse guard        | prepare | reject arguments the provider could not parse      |
//! | 2 | resolve + validate | prepare | find the declared tool, cast and validate arguments|
//! | 3 | authorize          | prepare | `AgentHook::authorize_call`; a denial is journaled |
//! | 4 | state guards       | prepare | circuit breaker, repeated-lookup block             |
//! | 5 | journal            | execute | write-ahead intent (may refuse in strict mode)     |
//! | 6 | execute            | execute | run under the call identity, isolate panics,       |
//! |   |                    |         | enforce the optional timeout                       |
//! | 7 | post-execute       | execute | `AgentHook::after_tool_call`: redact, withhold or  |
//! |   |                    |         | annotate the raw result                            |
//! | 8 | normalize          | both    | make the text non-empty, persist an oversized one, |
//! |   |                    |         | cut it to the budget; add the retry hint           |
//! | 9 | journal finish     | execute | terminal record with the normalized text           |
//! | 10| observe            | both    | map an error to a fatal one if `fail_on_tool_error`|
//!
//! The post-execute hook runs on the raw result, before normalizing, so a
//! redaction also reaches the copy that an oversized result is persisted as and
//! the journal. Normalizing before the terminal record means the journal and the
//! transcript hold the same text, from one function and one budget.
//!
//! The state guards run after authorize on purpose: a call the user denied must
//! not count towards the circuit breaker's streak or the repeated-lookup count.
//!
//! Every stage returns `Result<_, ToolOutcome>`: a failing stage short-circuits
//! the pipeline with an error *result* (never a panic or an early `return` of a
//! hand-built tuple), so the model always gets a tool message for the call.

// Every stage fails with the finished `ToolOutcome` (a complete tool result, not
// an error code), so the `Err` variant is the size of an outcome. The stages run
// once per tool call, next to an LLM round trip, so boxing it would only add noise.
#![allow(clippy::result_large_err)]

use std::any::Any;
use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::FutureExt;
use serde_json::Value;

use crate::agent::circuit_breaker::{
    CircuitDecision, MessageCircuitBreaker, tripped_error_message,
};
use crate::agent::crash_points::crash_point;
use crate::agent::hook::{
    CallDecision, HOOK_PANIC_DENY_REASON, HOOK_PANIC_RESULT_REASON, PostCallDecision,
    ToolCallAuthorization, ToolCallResultView,
};
use crate::agent::runner::AgentRunSpec;
use crate::agent::tool_call_context::{ToolCallIdentity, with_tool_call};
use crate::agent::tool_event::{ToolEvent, ToolStatus};
use crate::agent::tools::base::{Tool, ToolResult};
use crate::agent::tools::registry::ToolRegistry;
use crate::agent::turn_journal::InFlightToolCall;
use crate::providers::base::ToolCallRequest;
use crate::session::journal::ToolCallStatus;
use crate::utils::helpers::{maybe_persist_tool_result, truncate_text};
use crate::utils::runtime::{
    coerce_tool_execute_result, ensure_nonempty_tool_result, repeated_external_lookup_error,
};

/// Appended to every error result to nudge the model into self-correcting.
pub const ERROR_HINT: &str = "\n\n[Analyze the error above and try a different approach.]";

/// Argument key set by providers when the model's arguments were not valid JSON.
const ARG_PARSE_ERROR_KEY: &str = "__args_json_parse_error";
/// Argument key holding the raw, unparsable arguments text.
const ARG_PARSE_RAW_KEY: &str = "__args_json_raw";
/// Longest `detail` string carried by a telemetry event.
const DETAIL_MAX_CHARS: usize = 120;
/// Longest raw-arguments preview echoed back to the model on a parse error.
const RAW_ARGUMENTS_PREVIEW_CHARS: usize = 200;
/// Longest panic message echoed back to the model.
const PANIC_REASON_PREVIEW_CHARS: usize = 200;

/// Everything the pipeline produces for one tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutcome {
    /// Text fed back to the model as the tool result.
    content: String,
    status: ToolStatus,
    /// Short single-line summary for telemetry.
    detail: String,
    /// The error text without the retry hint; `Some` for ordinary errors.
    error: Option<String>,
    /// Set when the agent loop must abort because of this call.
    fatal: Option<String>,
    /// How long the call took once cleared to run; set by [`complete_tool_call`].
    duration: Duration,
    /// Texts a post-execute hook wants the model to get after the whole batch.
    added_context: Vec<String>,
}

impl ToolOutcome {
    /// A successful call; `result` reaches the model unchanged.
    pub fn ok(result: String) -> Self {
        let detail = success_detail(&result);
        Self {
            content: result,
            status: ToolStatus::Ok,
            detail,
            error: None,
            fatal: None,
            duration: Duration::ZERO,
            added_context: Vec::new(),
        }
    }

    /// A failed call with an explicit telemetry `detail`. The model sees
    /// `message` followed by [`ERROR_HINT`].
    pub fn error(message: String, detail: String) -> Self {
        Self {
            content: format!("{message}{ERROR_HINT}"),
            status: ToolStatus::Error,
            detail,
            error: Some(message),
            fatal: None,
            duration: Duration::ZERO,
            added_context: Vec::new(),
        }
    }

    /// A call rejected before it ran (unknown tool, invalid parameters). The
    /// detail is the message without its `"Error: "`-style prefix.
    pub fn rejected(message: String) -> Self {
        let detail = take_chars(strip_error_prefix(&message), DETAIL_MAX_CHARS);
        Self::error(message, detail)
    }

    /// The message circuit breaker tripped: always fatal, and no retry hint
    /// because retrying is exactly what the breaker forbids.
    pub fn circuit_tripped(message: String) -> Self {
        Self {
            content: message.clone(),
            status: ToolStatus::Error,
            detail: "message circuit breaker tripped".to_string(),
            error: None,
            fatal: Some(message),
            duration: Duration::ZERO,
            added_context: Vec::new(),
        }
    }

    /// A call a hook denied; it never ran.
    pub fn blocked_by_hook(reason: &str) -> Self {
        Self {
            content: format!("Error: tool call blocked by hook: {reason}"),
            status: ToolStatus::Blocked,
            detail: single_line_detail(reason),
            error: None,
            fatal: None,
            duration: Duration::ZERO,
            added_context: Vec::new(),
        }
    }

    /// The outcome of what a tool returned: an error result or a success.
    pub fn from_tool_result(result: ToolResult) -> Self {
        if !result.is_error {
            return Self::ok(result.content);
        }
        log::error!("Tool error: {}", result.content);
        let detail = single_line_detail(&result.content);
        Self::error(result.content, detail)
    }

    /// Observe stage: with `fail_on_tool_error`, an ordinary error becomes fatal.
    pub fn observed(mut self, fail_on_tool_error: bool) -> Self {
        if fail_on_tool_error && self.fatal.is_none() {
            self.fatal = self.error.clone();
        }
        self
    }

    /// The text this outcome was built from, before the retry hint: the error
    /// message of an error, otherwise the content.
    fn raw_text(&self) -> &str {
        self.error.as_deref().unwrap_or(&self.content)
    }

    /// Replace the model-facing text by its normalized form. An error keeps its
    /// retry hint, which is appended after normalizing so a budget never cuts it.
    fn with_normalized_content(mut self, normalized: String) -> Self {
        self.content = if self.error.is_some() {
            format!("{normalized}{ERROR_HINT}")
        } else {
            normalized
        };
        self
    }

    /// Attach the texts a post-execute hook wants the model to get.
    fn with_added_context(mut self, added_context: Vec<String>) -> Self {
        self.added_context = added_context;
        self
    }

    /// Hand over the texts a post-execute hook wants the model to get after the
    /// batch; the outcome keeps none.
    pub fn take_added_context(&mut self) -> Vec<String> {
        std::mem::take(&mut self.added_context)
    }

    /// Record how long the call took.
    pub fn with_duration(mut self, duration: Duration) -> Self {
        self.duration = duration;
        self
    }

    /// The telemetry event of this outcome for the call `call_id` of `tool_name`.
    pub fn event(&self, call_id: &str, tool_name: &str) -> ToolEvent {
        ToolEvent {
            call_id: call_id.to_string(),
            tool_name: tool_name.to_string(),
            status: self.status,
            detail: self.detail.clone(),
            duration_ms: u64::try_from(self.duration.as_millis()).unwrap_or(u64::MAX),
        }
    }

    pub fn status(&self) -> ToolStatus {
        self.status
    }

    pub fn content(&self) -> &str {
        &self.content
    }

    pub fn fatal(&self) -> Option<&str> {
        self.fatal.as_deref()
    }

    /// Flatten into the `(result, event, fatal)` triple the runner consumes.
    ///
    /// The event map carries `"name"`, `"status"` and `"detail"`.
    pub fn into_triple(self, tool_name: &str) -> (String, HashMap<String, String>, Option<String>) {
        let event = self.event("", tool_name).to_map();
        (self.content, event, self.fatal)
    }
}

// ── detail helpers ────────────────────────────────────────────────────────────

/// First `max_chars` characters of `text`.
fn take_chars(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

/// `text` as one trimmed line, capped for telemetry.
fn single_line_detail(text: &str) -> String {
    take_chars(text.replace('\n', " ").trim(), DETAIL_MAX_CHARS)
}

/// `message` without its leading `"Error: "`-style prefix.
fn strip_error_prefix(message: &str) -> &str {
    message
        .split_once(": ")
        .map(|(_, rest)| rest)
        .unwrap_or(message)
}

/// Telemetry detail of a successful result: one line, `(empty)` when blank,
/// and `...` when cut.
fn success_detail(result: &str) -> String {
    let flattened = result.replace('\n', " ");
    let trimmed = flattened.trim();
    if trimmed.is_empty() {
        return "(empty)".to_string();
    }
    if trimmed.chars().count() > DETAIL_MAX_CHARS {
        return format!("{}...", take_chars(trimmed, DETAIL_MAX_CHARS));
    }
    trimmed.to_string()
}

/// Normalise a raw tool result: make it non-empty, persist an oversized payload
/// to disk, and truncate what still exceeds the limit.
///
/// The one place the model-facing text of a tool result is shaped; the run loop
/// applies it again to older results when it re-budgets the transcript.
pub fn normalize_tool_result(
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

    if let Value::String(ref s) = content
        && spec.max_tool_result_chars > 0
        && s.len() > spec.max_tool_result_chars
    {
        return Value::String(truncate_text(s, spec.max_tool_result_chars));
    }
    content
}

/// [`normalize_tool_result`] for the text of a call.
///
/// A text that is a JSON array of content blocks (an image result) is normalized
/// as the blocks it denotes, then written back as JSON, so it is never cut in
/// the middle of its syntax.
fn normalized_text(spec: &AgentRunSpec, tool_call: &ToolCallRequest, text: &str) -> String {
    let normalized = normalize_tool_result(
        spec,
        &tool_call.id,
        &tool_call.name,
        coerce_tool_execute_result(text.to_string()),
    );
    match normalized {
        Value::String(text) => text,
        blocks => blocks.to_string(),
    }
}

// ── pipeline ──────────────────────────────────────────────────────────────────

/// A call that passed every stage before execution and is ready to run.
pub struct PreparedCall<'a> {
    tool_call: &'a ToolCallRequest,
    tool: Arc<dyn Tool>,
    /// The cast, validated arguments the tool runs with.
    params: Value,
}

/// Run one tool call through every stage and return its [`ToolOutcome`].
pub async fn run_tool_call(
    spec: &AgentRunSpec,
    tool_call: &ToolCallRequest,
    external_lookup_counts: Arc<Mutex<HashMap<String, usize>>>,
    message_circuit_breaker: Arc<Mutex<MessageCircuitBreaker>>,
) -> ToolOutcome {
    let prepared = prepare_tool_call(
        spec,
        tool_call,
        &external_lookup_counts,
        &message_circuit_breaker,
    )
    .await;
    complete_tool_call(spec, tool_call, prepared).await
}

/// Prepare phase (stages 1-4): everything that decides whether the call runs.
///
/// Fails with the error result the model should see: malformed arguments, an
/// unknown or invalid call, a denial, or a tripped guard.
pub async fn prepare_tool_call<'a>(
    spec: &AgentRunSpec,
    tool_call: &'a ToolCallRequest,
    external_lookup_counts: &Mutex<HashMap<String, usize>>,
    message_circuit_breaker: &Mutex<MessageCircuitBreaker>,
) -> Result<PreparedCall<'a>, ToolOutcome> {
    guard_well_formed_arguments(tool_call)?;
    let (tool, params) = resolve_and_validate(&spec.tools, tool_call)?;
    authorize(spec, tool_call, &params).await?;
    guard_message_circuit_breaker(tool_call, message_circuit_breaker)?;
    guard_repeated_external_lookup(tool_call, external_lookup_counts)?;
    Ok(PreparedCall {
        tool_call,
        tool,
        params,
    })
}

/// Execute phase (stages 5-9) for a prepared call, or the observe stage alone
/// for a call that failed preparation.
///
/// The observe stage also tells the hook (`AgentHook::on_tool_result`) the
/// moment the call finished, so a concurrent batch reports each call as it
/// completes rather than all at the end.
pub async fn complete_tool_call(
    spec: &AgentRunSpec,
    tool_call: &ToolCallRequest,
    prepared: Result<PreparedCall<'_>, ToolOutcome>,
) -> ToolOutcome {
    let started = Instant::now();
    let outcome = match prepared {
        Ok(prepared) => execute_prepared(spec, prepared).await,
        Err(outcome) => {
            let normalized = normalized_text(spec, tool_call, outcome.raw_text());
            outcome.with_normalized_content(normalized)
        }
    };
    let outcome = outcome
        .observed(spec.fail_on_tool_error)
        .with_duration(started.elapsed());
    notify_tool_result(spec, &outcome.event(&tool_call.id, &tool_call.name)).await;
    outcome
}

/// Tell the hook about a finished call. An observer must not be able to break
/// the turn, so a panic is logged and dropped.
async fn notify_tool_result(spec: &AgentRunSpec, event: &ToolEvent) {
    let Some(hook) = &spec.hook else {
        return;
    };
    if AssertUnwindSafe(hook.on_tool_result(event))
        .catch_unwind()
        .await
        .is_err()
    {
        log::error!(
            "AgentHook.on_tool_result panicked for tool '{}'",
            event.tool_name
        );
    }
}

/// Stages 5-8; the first failing stage short-circuits with its outcome.
async fn execute_prepared(spec: &AgentRunSpec, prepared: PreparedCall<'_>) -> ToolOutcome {
    let PreparedCall {
        tool_call,
        tool,
        params,
    } = prepared;
    let in_flight = match journal_begin(spec, tool_call, tool.as_ref(), &params) {
        Ok(in_flight) => in_flight,
        Err(outcome) => return outcome,
    };
    let result = execute(spec, tool_call, tool.as_ref(), &params).await;
    let (result, added_context) = post_call(spec, tool_call, result).await;
    let normalized = normalized_text(spec, tool_call, &result.content);
    journal_finish(
        in_flight,
        &ToolResult {
            content: normalized.clone(),
            is_error: result.is_error,
        },
    );
    // The telemetry detail still comes from what the tool returned, not from
    // the placeholder or the cut-down form the model sees.
    ToolOutcome::from_tool_result(result)
        .with_normalized_content(normalized)
        .with_added_context(added_context)
}

/// Stage 7: let the hook redact, withhold or annotate the raw result.
///
/// Returns the result the rest of the pipeline works with, and the texts to give
/// the model after the batch. A blocked result becomes an error naming the
/// reason, since the call ran but its output may not be shown. Fails closed: a
/// hook that panics blocks the result.
async fn post_call(
    spec: &AgentRunSpec,
    tool_call: &ToolCallRequest,
    result: ToolResult,
) -> (ToolResult, Vec<String>) {
    let Some(hook) = &spec.hook else {
        return (result, Vec::new());
    };
    let view = ToolCallResultView {
        call_id: &tool_call.id,
        tool_name: &tool_call.name,
        content: &result.content,
        is_error: result.is_error,
        channel: spec.channel.as_deref(),
        chat_id: spec.chat_id.as_deref(),
    };
    let decision = match AssertUnwindSafe(hook.after_tool_call(&view))
        .catch_unwind()
        .await
    {
        Ok(decision) => decision,
        Err(_) => {
            log::error!(
                "AgentHook.after_tool_call panicked for tool '{}'",
                tool_call.name
            );
            PostCallDecision::block(HOOK_PANIC_RESULT_REASON)
        }
    };
    let PostCallDecision {
        replacement,
        block_reason,
        added_context,
    } = decision;
    if let Some(reason) = block_reason {
        let blocked = ToolResult::error(format!("Error: tool result blocked by hook: {reason}"));
        return (blocked, added_context);
    }
    let content = replacement.unwrap_or(result.content);
    (
        ToolResult {
            content,
            is_error: result.is_error,
        },
        added_context,
    )
}

/// Stage 3: ask the hook whether the validated call may run.
///
/// Fails closed: a hook that panics denies. A denied call is journaled as
/// blocked, since it will never execute.
async fn authorize(
    spec: &AgentRunSpec,
    tool_call: &ToolCallRequest,
    params: &Value,
) -> Result<(), ToolOutcome> {
    let Some(hook) = &spec.hook else {
        return Ok(());
    };
    let request = ToolCallAuthorization {
        call_id: &tool_call.id,
        tool_name: &tool_call.name,
        arguments: params,
        channel: spec.channel.as_deref(),
        chat_id: spec.chat_id.as_deref(),
        turn_id: spec.journal.as_ref().map(|journal| journal.turn_id()),
    };
    let decision = match AssertUnwindSafe(hook.authorize_call(&request))
        .catch_unwind()
        .await
    {
        Ok(decision) => decision,
        Err(_) => {
            log::error!(
                "AgentHook.authorize_call panicked for tool '{}'",
                tool_call.name
            );
            CallDecision::deny(HOOK_PANIC_DENY_REASON)
        }
    };
    let CallDecision::Deny { reason } = decision else {
        return Ok(());
    };
    if let Some(journal) = &spec.journal {
        journal.record_blocked_call(&tool_call.id, &tool_call.name, params, &reason);
    }
    Err(ToolOutcome::blocked_by_hook(&reason))
}

/// Stage 4a: trip on consecutive identical message sends.
fn guard_message_circuit_breaker(
    tool_call: &ToolCallRequest,
    message_circuit_breaker: &Mutex<MessageCircuitBreaker>,
) -> Result<(), ToolOutcome> {
    let decision = message_circuit_breaker
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .observe(&tool_call.name, &tool_call.arguments);
    match decision {
        CircuitDecision::Tripped { streak } => {
            Err(ToolOutcome::circuit_tripped(tripped_error_message(streak)))
        }
        _ => Ok(()),
    }
}

/// Stage 4b: block the model from repeating an identical external lookup.
fn guard_repeated_external_lookup(
    tool_call: &ToolCallRequest,
    external_lookup_counts: &Mutex<HashMap<String, usize>>,
) -> Result<(), ToolOutcome> {
    let blocked = repeated_external_lookup_error(
        &tool_call.name,
        &tool_call.arguments,
        &mut external_lookup_counts.lock().unwrap(),
    );
    match blocked {
        Some(message) => Err(ToolOutcome::error(
            message,
            "repeated external lookup blocked".to_string(),
        )),
        None => Ok(()),
    }
}

/// Stage 1: reject calls whose arguments the provider could not parse as JSON.
fn guard_well_formed_arguments(tool_call: &ToolCallRequest) -> Result<(), ToolOutcome> {
    let Some(parse_error) = tool_call
        .arguments
        .get(ARG_PARSE_ERROR_KEY)
        .and_then(Value::as_str)
    else {
        return Ok(());
    };
    let raw = tool_call
        .arguments
        .get(ARG_PARSE_RAW_KEY)
        .and_then(Value::as_str)
        .unwrap_or("");
    let message = format!(
        "Error: malformed tool arguments JSON for '{}': {}. Raw arguments: {}",
        tool_call.name,
        parse_error,
        take_chars(raw, RAW_ARGUMENTS_PREVIEW_CHARS)
    );
    let detail = single_line_detail(&message);
    Err(ToolOutcome::error(message, detail))
}

/// Stage 2: find the declared tool and cast and validate its arguments.
///
/// Returns the tool and the cast arguments to execute it with.
fn resolve_and_validate(
    tools: &ToolRegistry,
    tool_call: &ToolCallRequest,
) -> Result<(Arc<dyn Tool>, Value), ToolOutcome> {
    let tool = tools
        .resolve_tool(&tool_call.name)
        .map_err(ToolOutcome::rejected)?;
    let raw_params = Value::Object(
        tool_call
            .arguments
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
    );
    let cast_params = ToolRegistry::validate_call(&tool_call.name, &tool, &raw_params)
        .map_err(|(_, message)| ToolOutcome::rejected(message))?;
    Ok((tool, cast_params))
}

/// Stage 5: write-ahead the intent so a crash during execute leaves a record.
///
/// Without a journal this is a no-op. In strict durability mode a failed
/// intent write refuses the call.
fn journal_begin(
    spec: &AgentRunSpec,
    tool_call: &ToolCallRequest,
    tool: &dyn Tool,
    params: &Value,
) -> Result<Option<InFlightToolCall>, ToolOutcome> {
    let Some(journal) = &spec.journal else {
        return Ok(None);
    };
    match journal.begin_tool_call(&tool_call.id, &tool_call.name, params, tool.replay_safe()) {
        Ok(in_flight) => Ok(Some(in_flight)),
        Err(refusal) => {
            let message = format!("Error: {refusal}");
            log::error!("{message}");
            Err(ToolOutcome::error(
                message,
                "intent not journaled (strict durability mode)".to_string(),
            ))
        }
    }
}

/// Stage 6: run the tool and return its result.
///
/// A tool that panics or exceeds the timeout yields an error result instead
/// of unwinding through the turn, so the journal still gets a terminal record.
/// The crash points stay outside the isolation: they simulate a process kill.
async fn execute(
    spec: &AgentRunSpec,
    tool_call: &ToolCallRequest,
    tool: &dyn Tool,
    params: &Value,
) -> ToolResult {
    crash_point("after_intent");
    let guarded =
        AssertUnwindSafe(execute_under_identity(spec, tool_call, tool, params)).catch_unwind();
    let result = match effective_timeout(spec.tool_timeout, tool) {
        Some(limit) => match tokio::time::timeout(limit, guarded).await {
            Ok(finished) => finished,
            // Dropping `guarded` cancels the tool; `shell` kills its process tree on drop.
            Err(_) => Ok(ToolResult::error(timeout_error(&tool_call.name, limit))),
        },
        None => guarded.await,
    };
    let result = result.unwrap_or_else(|payload| {
        ToolResult::error(panic_error(&tool_call.name, payload.as_ref()))
    });
    crash_point("before_terminal");
    result
}

/// Run `tool`, under the call identity when a journal is attached.
async fn execute_under_identity(
    spec: &AgentRunSpec,
    tool_call: &ToolCallRequest,
    tool: &dyn Tool,
    params: &Value,
) -> ToolResult {
    match &spec.journal {
        Some(journal) => {
            let identity = ToolCallIdentity {
                turn_id: journal.turn_id().to_string(),
                call_id: tool_call.id.clone(),
            };
            with_tool_call(identity, tool.execute_result(params)).await
        }
        None => tool.execute_result(params).await,
    }
}

/// The configured timeout as a duration; `0` means no limit.
pub fn tool_timeout_from_seconds(seconds: u32) -> Option<Duration> {
    (seconds > 0).then(|| Duration::from_secs(u64::from(seconds)))
}

/// The limit that applies to `tool`: none when the tool is exempt.
fn effective_timeout(configured: Option<Duration>, tool: &dyn Tool) -> Option<Duration> {
    configured.filter(|_| !tool.exempt_from_execute_timeout())
}

/// Result text for a call cut off by the execute timeout.
fn timeout_error(tool_name: &str, limit: Duration) -> String {
    let message = format!(
        "Error: tool '{tool_name}' timed out after {}s and was cancelled",
        limit.as_secs_f64()
    );
    log::error!("{message}");
    message
}

/// Result text for a call whose tool panicked; `payload` is the panic payload.
fn panic_error(tool_name: &str, payload: &(dyn Any + Send)) -> String {
    let reason = payload
        .downcast_ref::<&str>()
        .map(|text| (*text).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".to_string());
    let message = format!(
        "Error: tool '{tool_name}' crashed: {}",
        take_chars(&reason, PANIC_REASON_PREVIEW_CHARS)
    );
    log::error!("{message}");
    message
}

/// Stage 8: write the terminal journal record (no-op without a journal).
fn journal_finish(in_flight: Option<InFlightToolCall>, result: &ToolResult) {
    let Some(in_flight) = in_flight else {
        return;
    };
    let status = if result.is_error {
        ToolCallStatus::Error
    } else {
        ToolCallStatus::Done
    };
    in_flight.finish(status, result.content.clone());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::circuit_breaker::{DEFAULT_N_IDENTICAL_MESSAGES, is_tripped_error};
    use crate::agent::hook::AgentHook;
    use crate::agent::tools::test_support::{
        FlaggedTool, NamedTool, PanickingTool, ScriptedTool, SlowTool, spec_with_tool,
    };

    fn call(name: &str, arguments: &[(&str, &str)]) -> ToolCallRequest {
        ToolCallRequest {
            id: "call-1".to_string(),
            name: name.to_string(),
            arguments: arguments
                .iter()
                .map(|(key, value)| (key.to_string(), Value::String(value.to_string())))
                .collect(),
            extra_content: None,
            provider_specific_fields: None,
            function_provider_specific_fields: None,
        }
    }

    // ── details ───────────────────────────────────────────────────────────────

    #[test]
    fn success_detail_flattens_pads_empty_and_ellipsizes() {
        assert_eq!(success_detail("a\nb"), "a b");
        assert_eq!(success_detail("  \n "), "(empty)");
        assert_eq!(success_detail(&"x".repeat(120)), "x".repeat(120));
        assert_eq!(
            success_detail(&"x".repeat(121)),
            format!("{}...", "x".repeat(120))
        );
    }

    #[test]
    fn success_detail_cuts_on_character_boundaries() {
        let detail = success_detail(&"é".repeat(130));
        assert_eq!(detail, format!("{}...", "é".repeat(120)));
    }

    #[test]
    fn single_line_detail_caps_without_ellipsis() {
        assert_eq!(single_line_detail("Error: a\nb "), "Error: a b");
        assert_eq!(single_line_detail(&"y".repeat(500)).chars().count(), 120);
    }

    #[test]
    fn strip_error_prefix_drops_text_before_the_first_colon_space() {
        assert_eq!(strip_error_prefix("Error: Tool 'x': bad"), "Tool 'x': bad");
        assert_eq!(strip_error_prefix("no colon here"), "no colon here");
    }

    // ── ToolOutcome ───────────────────────────────────────────────────────────

    #[test]
    fn ok_outcome_keeps_the_result_verbatim() {
        let (content, event, fatal) = ToolOutcome::ok("fine".into()).into_triple("t");
        assert_eq!(content, "fine");
        assert_eq!(event["name"], "t");
        assert_eq!(event["status"], "ok");
        assert_eq!(event["detail"], "fine");
        assert_eq!(fatal, None);
    }

    #[test]
    fn error_outcome_appends_the_hint_and_keeps_the_raw_message() {
        let outcome = ToolOutcome::error("Error: x".into(), "x".into());
        assert_eq!(outcome.content(), format!("Error: x{ERROR_HINT}"));
        assert_eq!(outcome.status(), ToolStatus::Error);
        assert_eq!(outcome.fatal(), None);
    }

    #[test]
    fn observed_makes_ordinary_errors_fatal_only_when_configured() {
        let error = || ToolOutcome::error("Error: x".into(), "x".into());
        assert_eq!(error().observed(false).fatal(), None);
        assert_eq!(error().observed(true).fatal(), Some("Error: x"));
        assert_eq!(ToolOutcome::ok("fine".into()).observed(true).fatal(), None);
    }

    #[test]
    fn blocked_outcome_is_never_fatal_even_when_configured() {
        let outcome = ToolOutcome::blocked_by_hook("no\nway").observed(true);
        assert_eq!(outcome.status(), ToolStatus::Blocked);
        assert_eq!(
            outcome.content(),
            "Error: tool call blocked by hook: no\nway"
        );
        assert_eq!(outcome.fatal(), None);
        let (_, event, _) = outcome.into_triple("t");
        assert_eq!(event["detail"], "no way");
    }

    #[test]
    fn tripped_outcome_is_fatal_without_a_hint() {
        let outcome = ToolOutcome::circuit_tripped("Error: stop".into()).observed(false);
        assert_eq!(outcome.content(), "Error: stop");
        assert_eq!(outcome.fatal(), Some("Error: stop"));
    }

    #[test]
    fn rejected_outcome_detail_omits_the_error_prefix() {
        let (_, event, _) =
            ToolOutcome::rejected("Error: Tool 'x' not found. Available: ".into()).into_triple("x");
        assert_eq!(event["detail"], "Tool 'x' not found. Available: ");
    }

    #[test]
    fn from_tool_result_follows_the_flag_not_the_text() {
        assert_eq!(
            ToolOutcome::from_tool_result(ToolResult::error("(server said no)")).status(),
            ToolStatus::Error
        );
        let output = ToolOutcome::from_tool_result(ToolResult::ok("Error: just output"));
        assert_eq!(output.status(), ToolStatus::Ok);
        assert_eq!(output.content(), "Error: just output");
    }

    // ── guards ────────────────────────────────────────────────────────────────

    #[test]
    fn well_formed_arguments_pass_the_guard() {
        assert!(guard_well_formed_arguments(&call("t", &[("path", "a")])).is_ok());
    }

    #[test]
    fn malformed_arguments_are_rejected_with_a_capped_raw_preview() {
        let raw = "r".repeat(500);
        let request = call(
            "write_file",
            &[(ARG_PARSE_ERROR_KEY, "bad json"), (ARG_PARSE_RAW_KEY, &raw)],
        );
        let outcome = guard_well_formed_arguments(&request).unwrap_err();
        assert!(
            outcome
                .content()
                .starts_with("Error: malformed tool arguments JSON for 'write_file': bad json.")
        );
        assert!(outcome.content().contains(&"r".repeat(200)));
        assert!(!outcome.content().contains(&"r".repeat(201)));
    }

    #[test]
    fn repeated_external_lookup_guard_blocks_after_three_identical_calls() {
        let counts = Mutex::new(HashMap::new());
        let request = call("web_search", &[("query", "same")]);
        for _ in 0..3 {
            assert!(guard_repeated_external_lookup(&request, &counts).is_ok());
        }
        let outcome = guard_repeated_external_lookup(&request, &counts).unwrap_err();
        assert_eq!(outcome.status(), ToolStatus::Error);
        let (_, event, _) = outcome.into_triple("web_search");
        assert_eq!(event["detail"], "repeated external lookup blocked");
    }

    #[test]
    fn circuit_breaker_guard_trips_with_a_fatal_outcome() {
        let breaker = Mutex::new(MessageCircuitBreaker::new(DEFAULT_N_IDENTICAL_MESSAGES));
        let request = call("message", &[("content", "same")]);
        for _ in 0..DEFAULT_N_IDENTICAL_MESSAGES - 1 {
            assert!(guard_message_circuit_breaker(&request, &breaker).is_ok());
        }
        let outcome = guard_message_circuit_breaker(&request, &breaker).unwrap_err();
        assert!(is_tripped_error(outcome.fatal().unwrap()));
    }

    // ── resolve + validate ────────────────────────────────────────────────────

    #[test]
    fn unknown_tool_is_rejected_at_the_resolve_stage() {
        let tools = ToolRegistry::new();
        let outcome = resolve_and_validate(&tools, &call("ghost", &[]))
            .err()
            .expect("unknown tool must be rejected");
        assert!(
            outcome
                .content()
                .starts_with("Error: Tool 'ghost' not found.")
        );
        assert!(outcome.content().ends_with(ERROR_HINT));
    }

    // ── execute stage: panic isolation and timeout ───────────────────────────

    /// Run `request` through the whole pipeline with fresh guard state.
    async fn run(spec: &AgentRunSpec, request: &ToolCallRequest) -> ToolOutcome {
        run_tool_call(
            spec,
            request,
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(Mutex::new(MessageCircuitBreaker::new(
                DEFAULT_N_IDENTICAL_MESSAGES,
            ))),
        )
        .await
    }

    #[tokio::test]
    async fn a_panicking_tool_becomes_an_error_result() {
        for string_payload in [false, true] {
            let spec = spec_with_tool(PanickingTool { string_payload });
            let outcome = run(&spec, &call("panicky", &[])).await;
            assert_eq!(outcome.status(), ToolStatus::Error);
            assert_eq!(
                outcome.content(),
                format!("Error: tool 'panicky' crashed: boom{ERROR_HINT}")
            );
            assert_eq!(outcome.fatal(), None);
        }
    }

    #[tokio::test]
    async fn a_panicking_tool_is_fatal_when_fail_on_tool_error_is_set() {
        let mut spec = spec_with_tool(PanickingTool {
            string_payload: false,
        });
        spec.fail_on_tool_error = true;
        let outcome = run(&spec, &call("panicky", &[])).await;
        assert_eq!(outcome.fatal(), Some("Error: tool 'panicky' crashed: boom"));
    }

    #[test]
    fn panic_error_handles_unknown_payloads_and_caps_long_messages() {
        let unknown: Box<dyn Any + Send> = Box::new(42u8);
        assert_eq!(
            panic_error("t", unknown.as_ref()),
            "Error: tool 't' crashed: unknown panic"
        );
        let long: Box<dyn Any + Send> = Box::new("z".repeat(500));
        let message = panic_error("t", long.as_ref());
        assert_eq!(
            message,
            format!("Error: tool 't' crashed: {}", "z".repeat(200))
        );
    }

    #[tokio::test]
    async fn a_tool_that_exceeds_the_timeout_is_cancelled_with_an_error_result() {
        let mut spec = spec_with_tool(SlowTool {
            delay: Duration::from_secs(30),
            exempt: false,
        });
        spec.tool_timeout = Some(Duration::from_millis(50));
        let started = std::time::Instant::now();
        let outcome = run(&spec, &call("slow", &[])).await;
        assert!(started.elapsed() < Duration::from_secs(5), "must not wait");
        assert_eq!(outcome.status(), ToolStatus::Error);
        assert_eq!(
            outcome.content(),
            format!("Error: tool 'slow' timed out after 0.05s and was cancelled{ERROR_HINT}")
        );
    }

    #[tokio::test]
    async fn an_exempt_tool_ignores_the_timeout() {
        let mut spec = spec_with_tool(SlowTool {
            delay: Duration::from_millis(150),
            exempt: true,
        });
        spec.tool_timeout = Some(Duration::from_millis(20));
        let outcome = run(&spec, &call("slow", &[])).await;
        assert_eq!(outcome.status(), ToolStatus::Ok);
        assert_eq!(outcome.content(), "slow done");
    }

    #[tokio::test]
    async fn without_a_timeout_a_slow_tool_just_finishes() {
        let spec = spec_with_tool(SlowTool {
            delay: Duration::from_millis(100),
            exempt: false,
        });
        assert_eq!(spec.tool_timeout, None, "the timeout is off by default");
        let outcome = run(&spec, &call("slow", &[])).await;
        assert_eq!(outcome.content(), "slow done");
    }

    #[test]
    fn effective_timeout_is_dropped_for_exempt_tools_only() {
        let limit = Some(Duration::from_secs(5));
        let exempt = SlowTool {
            delay: Duration::ZERO,
            exempt: true,
        };
        let bound = SlowTool {
            delay: Duration::ZERO,
            exempt: false,
        };
        assert_eq!(effective_timeout(limit, &exempt), None);
        assert_eq!(effective_timeout(limit, &bound), limit);
        assert_eq!(effective_timeout(None, &bound), None);
    }

    #[test]
    fn configured_seconds_zero_means_no_timeout() {
        assert_eq!(tool_timeout_from_seconds(0), None);
        assert_eq!(tool_timeout_from_seconds(30), Some(Duration::from_secs(30)));
    }

    // ── authorize stage ───────────────────────────────────────────────────────

    /// What an [`AuthorizingHook`] was asked: tool name, arguments, channel, chat id.
    type Asked = (String, Value, Option<String>, Option<String>);

    /// Hook that answers every authorization with `decision` and records the request.
    struct AuthorizingHook {
        decision: CallDecision,
        asked: Arc<Mutex<Vec<Asked>>>,
    }

    impl AuthorizingHook {
        fn new(decision: CallDecision) -> (Arc<Self>, Arc<Mutex<Vec<Asked>>>) {
            let asked = Arc::new(Mutex::new(Vec::new()));
            let hook = Arc::new(Self {
                decision,
                asked: Arc::clone(&asked),
            });
            (hook, asked)
        }
    }

    #[async_trait::async_trait]
    impl AgentHook for AuthorizingHook {
        async fn authorize_call(&self, request: &ToolCallAuthorization<'_>) -> CallDecision {
            self.asked.lock().unwrap().push((
                request.tool_name.to_string(),
                request.arguments.clone(),
                request.channel.map(str::to_string),
                request.chat_id.map(str::to_string),
            ));
            self.decision.clone()
        }
    }

    /// Hook whose `authorize_call` panics.
    struct PanickingAuthorizer;

    #[async_trait::async_trait]
    impl AgentHook for PanickingAuthorizer {
        async fn authorize_call(&self, _: &ToolCallAuthorization<'_>) -> CallDecision {
            panic!("deliberate panic");
        }
    }

    fn scripted_spec_with_hook(hook: Arc<dyn AgentHook>) -> AgentRunSpec {
        let mut spec = spec_with_tool(ScriptedTool {
            reply: "ran".into(),
        });
        spec.hook = Some(hook);
        spec
    }

    #[tokio::test]
    async fn the_hook_is_asked_with_the_validated_call_and_its_turn() {
        let (hook, asked) = AuthorizingHook::new(CallDecision::Allow);
        let mut spec = scripted_spec_with_hook(hook);
        spec.channel = Some("websocket".into());
        spec.chat_id = Some("chat-7".into());

        let outcome = run(&spec, &call("scripted", &[("path", "a.txt")])).await;

        assert_eq!(outcome.content(), "ran");
        assert_eq!(
            asked.lock().unwrap().as_slice(),
            [(
                "scripted".to_string(),
                serde_json::json!({"path": "a.txt"}),
                Some("websocket".to_string()),
                Some("chat-7".to_string()),
            )]
        );
    }

    #[tokio::test]
    async fn a_denied_call_is_blocked_not_run_and_never_fatal() {
        let (hook, _) = AuthorizingHook::new(CallDecision::deny("no way"));
        let mut spec = scripted_spec_with_hook(hook);
        spec.fail_on_tool_error = true;

        let outcome = run(&spec, &call("scripted", &[("path", "a.txt")])).await;

        assert_eq!(outcome.status(), ToolStatus::Blocked);
        assert_eq!(
            outcome.content(),
            "Error: tool call blocked by hook: no way"
        );
        assert_eq!(outcome.fatal(), None, "a denial is not a tool failure");
    }

    #[tokio::test]
    async fn a_panicking_authorize_hook_denies_the_call() {
        let spec = scripted_spec_with_hook(Arc::new(PanickingAuthorizer));

        let outcome = run(&spec, &call("scripted", &[("path", "a.txt")])).await;

        assert_eq!(outcome.status(), ToolStatus::Blocked);
        assert!(outcome.content().contains(HOOK_PANIC_DENY_REASON));
    }

    #[tokio::test]
    async fn an_invalid_call_is_rejected_before_the_hook_is_asked() {
        let (hook, asked) = AuthorizingHook::new(CallDecision::Allow);
        let spec = scripted_spec_with_hook(hook);

        // `path` is required.
        let outcome = run(&spec, &call("scripted", &[])).await;
        let unknown = run(&spec, &call("ghost", &[])).await;

        assert_eq!(outcome.status(), ToolStatus::Error);
        assert_eq!(unknown.status(), ToolStatus::Error);
        assert!(asked.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn denied_calls_do_not_count_towards_the_circuit_breaker() {
        let (hook, _) = AuthorizingHook::new(CallDecision::deny("no"));
        let mut spec = spec_with_tool(NamedTool {
            name: "message".into(),
        });
        spec.hook = Some(hook);
        let request = call("message", &[("content", "same")]);

        for _ in 0..DEFAULT_N_IDENTICAL_MESSAGES * 2 {
            let outcome = run(&spec, &request).await;
            assert_eq!(
                outcome.status(),
                ToolStatus::Blocked,
                "a denied send is not a send, so the breaker must never trip"
            );
        }

        // The same repeated sends do trip it once they are allowed.
        spec.hook = None;
        let breaker = Arc::new(Mutex::new(MessageCircuitBreaker::new(
            DEFAULT_N_IDENTICAL_MESSAGES,
        )));
        let mut last = None;
        for _ in 0..DEFAULT_N_IDENTICAL_MESSAGES {
            last = Some(
                run_tool_call(
                    &spec,
                    &request,
                    Arc::new(Mutex::new(HashMap::new())),
                    Arc::clone(&breaker),
                )
                .await,
            );
        }
        assert!(is_tripped_error(last.unwrap().fatal().unwrap()));
    }

    // ── on_tool_result ────────────────────────────────────────────────────────

    /// Hook that records every finished call it is told about.
    struct EventHook {
        events: Arc<Mutex<Vec<ToolEvent>>>,
        decision: CallDecision,
    }

    #[async_trait::async_trait]
    impl AgentHook for EventHook {
        async fn authorize_call(&self, _: &ToolCallAuthorization<'_>) -> CallDecision {
            self.decision.clone()
        }
        async fn on_tool_result(&self, event: &ToolEvent) {
            self.events.lock().unwrap().push(event.clone());
        }
    }

    fn spec_observed_by(
        mut spec: AgentRunSpec,
        decision: CallDecision,
    ) -> (AgentRunSpec, Arc<Mutex<Vec<ToolEvent>>>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        spec.hook = Some(Arc::new(EventHook {
            events: Arc::clone(&events),
            decision,
        }));
        (spec, events)
    }

    #[tokio::test]
    async fn the_hook_is_told_about_a_finished_call_with_its_duration() {
        let (spec, events) = spec_observed_by(
            spec_with_tool(SlowTool {
                delay: Duration::from_millis(60),
                exempt: false,
            }),
            CallDecision::Allow,
        );

        run(&spec, &call("slow", &[])).await;

        let events = events.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].call_id, "call-1");
        assert_eq!(events[0].tool_name, "slow");
        assert_eq!(events[0].status, ToolStatus::Ok);
        assert!(events[0].duration_ms >= 50, "got {}", events[0].duration_ms);
    }

    #[tokio::test]
    async fn rejected_and_denied_calls_are_reported_too() {
        let (spec, events) = spec_observed_by(
            spec_with_tool(ScriptedTool {
                reply: "ran".into(),
            }),
            CallDecision::Allow,
        );
        run(&spec, &call("ghost", &[])).await;
        run(&spec, &call("scripted", &[])).await; // `path` is required

        let (denying, denied_events) = spec_observed_by(
            spec_with_tool(ScriptedTool {
                reply: "ran".into(),
            }),
            CallDecision::deny("no"),
        );
        run(&denying, &call("scripted", &[("path", "a")])).await;

        let statuses: Vec<ToolStatus> = events.lock().unwrap().iter().map(|e| e.status).collect();
        assert_eq!(statuses, [ToolStatus::Error, ToolStatus::Error]);
        let denied = denied_events.lock().unwrap();
        assert_eq!(denied.len(), 1);
        assert_eq!(denied[0].status, ToolStatus::Blocked);
        assert_eq!(denied[0].duration_ms, 0);
    }

    #[tokio::test]
    async fn a_panicking_observer_cannot_change_the_result() {
        struct PanickingObserver;
        #[async_trait::async_trait]
        impl AgentHook for PanickingObserver {
            async fn on_tool_result(&self, _: &ToolEvent) {
                panic!("deliberate panic");
            }
        }
        let mut spec = spec_with_tool(ScriptedTool {
            reply: "ran".into(),
        });
        spec.hook = Some(Arc::new(PanickingObserver));

        let outcome = run(&spec, &call("scripted", &[("path", "a")])).await;

        assert_eq!(outcome.content(), "ran");
        assert_eq!(outcome.status(), ToolStatus::Ok);
    }

    #[test]
    fn an_outcome_converts_to_the_event_of_its_call() {
        let event = ToolOutcome::error("Error: boom".into(), "boom".into())
            .with_duration(Duration::from_millis(1500))
            .event("c9", "shell");
        assert_eq!(event.call_id, "c9");
        assert_eq!(event.tool_name, "shell");
        assert_eq!(event.status, ToolStatus::Error);
        assert_eq!(event.detail, "boom");
        assert_eq!(event.duration_ms, 1500);
    }

    // ── the error flag ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn an_error_without_the_error_prefix_is_still_an_error() {
        let mut spec = spec_with_tool(FlaggedTool {
            content: "(MCP tool call failed: connection reset)".into(),
            is_error: true,
        });
        spec.fail_on_tool_error = true;

        let outcome = run(&spec, &call("flagged", &[])).await;

        assert_eq!(outcome.status(), ToolStatus::Error);
        assert_eq!(
            outcome.content(),
            format!("(MCP tool call failed: connection reset){ERROR_HINT}")
        );
        assert_eq!(
            outcome.fatal(),
            Some("(MCP tool call failed: connection reset)")
        );
    }

    #[tokio::test]
    async fn output_starting_with_error_is_ok_when_the_tool_says_so() {
        let spec = spec_with_tool(FlaggedTool {
            content: "Error: 3 lines in the log".into(),
            is_error: false,
        });

        let outcome = run(&spec, &call("flagged", &[])).await;

        assert_eq!(outcome.status(), ToolStatus::Ok);
        assert_eq!(
            outcome.content(),
            "Error: 3 lines in the log",
            "no retry hint"
        );
        assert_eq!(outcome.fatal(), None);
    }

    // ── normalize stage ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn an_oversized_result_is_cut_to_the_budget_in_the_outcome() {
        let mut spec = spec_with_tool(ScriptedTool {
            reply: "x".repeat(200),
        });
        spec.max_tool_result_chars = 40;

        let outcome = run(&spec, &call("scripted", &[("path", "a")])).await;

        assert_eq!(outcome.content(), truncate_text(&"x".repeat(200), 40));
        assert_eq!(outcome.status(), ToolStatus::Ok);
    }

    #[tokio::test]
    async fn an_empty_result_gets_a_placeholder_but_the_detail_still_says_empty() {
        let spec = spec_with_tool(ScriptedTool { reply: "".into() });

        let outcome = run(&spec, &call("scripted", &[("path", "a")])).await;

        assert_eq!(outcome.content(), "(scripted completed with no output)");
        let (_, event, _) = outcome.into_triple("scripted");
        assert_eq!(event["detail"], "(empty)");
    }

    #[tokio::test]
    async fn the_retry_hint_survives_the_budget() {
        let mut spec = spec_with_tool(FlaggedTool {
            content: "z".repeat(200),
            is_error: true,
        });
        spec.max_tool_result_chars = 40;

        let outcome = run(&spec, &call("flagged", &[])).await;

        assert_eq!(
            outcome.content(),
            format!("{}{ERROR_HINT}", truncate_text(&"z".repeat(200), 40))
        );
    }

    #[tokio::test]
    async fn a_call_rejected_before_it_ran_is_normalized_too() {
        let mut spec = spec_with_tool(ScriptedTool {
            reply: "ran".into(),
        });
        spec.max_tool_result_chars = 30;

        let outcome = run(&spec, &call("a_tool_with_a_very_long_unknown_name", &[])).await;

        assert!(outcome.content().contains("(truncated)"));
        assert!(outcome.content().ends_with(ERROR_HINT));
    }

    #[tokio::test]
    async fn an_image_result_is_never_cut_in_the_middle_of_its_json() {
        let blocks = r#"[{"type":"text","text":"hi"},{"type":"image_url","image_url":{"url":"data:image/png;base64,AAAA"}}]"#;
        let mut spec = spec_with_tool(ScriptedTool {
            reply: blocks.into(),
        });
        spec.max_tool_result_chars = 20;

        let outcome = run(&spec, &call("scripted", &[("path", "a")])).await;

        let parsed: Value =
            serde_json::from_str(outcome.content()).expect("still valid JSON, not a cut string");
        assert_eq!(parsed, serde_json::from_str::<Value>(blocks).unwrap());
    }

    // ── post-execute hook ─────────────────────────────────────────────────────

    /// What a [`ResultPolicyHook`] saw: each result's content and error flag.
    type SeenResults = Arc<Mutex<Vec<(String, bool)>>>;

    /// Hook that answers every `after_tool_call` with `decision`, recording what it saw.
    struct ResultPolicyHook {
        decision: PostCallDecision,
        seen: SeenResults,
    }

    #[async_trait::async_trait]
    impl AgentHook for ResultPolicyHook {
        async fn after_tool_call(&self, result: &ToolCallResultView<'_>) -> PostCallDecision {
            self.seen
                .lock()
                .unwrap()
                .push((result.content.to_string(), result.is_error));
            self.decision.clone()
        }
    }

    /// Hook whose `after_tool_call` panics.
    struct PanickingResultHook;

    #[async_trait::async_trait]
    impl AgentHook for PanickingResultHook {
        async fn after_tool_call(&self, _: &ToolCallResultView<'_>) -> PostCallDecision {
            panic!("deliberate panic");
        }
    }

    fn spec_with_result_hook(
        reply: &str,
        decision: PostCallDecision,
    ) -> (AgentRunSpec, SeenResults) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut spec = spec_with_tool(ScriptedTool {
            reply: reply.into(),
        });
        spec.hook = Some(Arc::new(ResultPolicyHook {
            decision,
            seen: Arc::clone(&seen),
        }));
        (spec, seen)
    }

    #[tokio::test]
    async fn a_hook_can_replace_the_result_the_model_sees() {
        let (spec, seen) = spec_with_result_hook(
            "password=hunter2",
            PostCallDecision::replace("password=[redacted]"),
        );

        let mut outcome = run(&spec, &call("scripted", &[("path", "a")])).await;

        assert_eq!(
            seen.lock().unwrap().as_slice(),
            [("password=hunter2".to_string(), false)]
        );
        assert_eq!(outcome.content(), "password=[redacted]");
        assert_eq!(outcome.status(), ToolStatus::Ok);
        assert!(outcome.take_added_context().is_empty());
        let (_, event, _) = outcome.into_triple("scripted");
        assert_eq!(
            event["detail"], "password=[redacted]",
            "the telemetry detail must not leak what was redacted"
        );
    }

    #[tokio::test]
    async fn a_hook_can_withhold_a_result_that_ran() {
        let (spec, _) = spec_with_result_hook("secret", PostCallDecision::block("no secrets"));

        let outcome = run(&spec, &call("scripted", &[("path", "a")])).await;

        assert_eq!(outcome.status(), ToolStatus::Error);
        assert_eq!(
            outcome.content(),
            format!("Error: tool result blocked by hook: no secrets{ERROR_HINT}")
        );
    }

    #[tokio::test]
    async fn a_hook_can_add_context_without_touching_the_result() {
        let (spec, _) = spec_with_result_hook("ran", PostCallDecision::add_context("remember X"));

        let mut outcome = run(&spec, &call("scripted", &[("path", "a")])).await;

        assert_eq!(outcome.content(), "ran");
        assert_eq!(outcome.take_added_context(), ["remember X"]);
        assert!(outcome.take_added_context().is_empty(), "handed over once");
    }

    #[tokio::test]
    async fn a_panicking_result_hook_withholds_the_result() {
        let mut spec = spec_with_tool(ScriptedTool {
            reply: "secret output".into(),
        });
        spec.hook = Some(Arc::new(PanickingResultHook));

        let outcome = run(&spec, &call("scripted", &[("path", "a")])).await;

        assert_eq!(outcome.status(), ToolStatus::Error);
        assert!(outcome.content().contains(HOOK_PANIC_RESULT_REASON));
        assert!(!outcome.content().contains("secret output"));
    }

    #[tokio::test]
    async fn the_hook_sees_the_raw_result_and_its_replacement_is_still_budgeted() {
        let (mut spec, seen) =
            spec_with_result_hook(&"r".repeat(500), PostCallDecision::replace("s".repeat(300)));
        spec.max_tool_result_chars = 60;

        let outcome = run(&spec, &call("scripted", &[("path", "a")])).await;

        assert_eq!(
            seen.lock().unwrap()[0].0.len(),
            500,
            "before the budget cut it"
        );
        assert_eq!(outcome.content(), truncate_text(&"s".repeat(300), 60));
    }

    #[tokio::test]
    async fn the_hook_is_not_asked_about_calls_that_never_ran() {
        let (spec, seen) = spec_with_result_hook("ran", PostCallDecision::default());

        run(&spec, &call("ghost", &[])).await; // unknown tool
        run(&spec, &call("scripted", &[])).await; // `path` is required

        assert!(seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_hook_is_told_when_the_tool_itself_failed() {
        let mut spec = spec_with_tool(FlaggedTool {
            content: "boom".into(),
            is_error: true,
        });
        let seen = Arc::new(Mutex::new(Vec::new()));
        spec.hook = Some(Arc::new(ResultPolicyHook {
            decision: PostCallDecision::default(),
            seen: Arc::clone(&seen),
        }));

        run(&spec, &call("flagged", &[])).await;

        assert_eq!(
            seen.lock().unwrap().as_slice(),
            [("boom".to_string(), true)]
        );
    }
}
