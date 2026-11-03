//! Stage-by-stage execution of a single tool call.
//!
//! [`run_tool_call`] runs one call through a fixed sequence of stages:
//!
//! | # | stage              | what it does                                              |
//! |---|--------------------|-----------------------------------------------------------|
//! | 0 | authorize          | batch-level `AgentHook::before_execute_tools`, run by the |
//! |   |                    | caller before any call reaches this module                |
//! | 1 | guard              | circuit breaker, repeated-lookup block, malformed args    |
//! | 2 | resolve + validate | find the declared tool, cast and validate its arguments   |
//! | 3 | journal            | write-ahead intent (may refuse the call in strict mode)   |
//! | 4 | execute            | run the tool under the call identity, isolating panics   |
//! |   |                    | and enforcing the optional execute timeout                |
//! | 5 | journal finish     | terminal record, `Done` or `Error`                        |
//! | 6 | normalize          | classify the result, add the retry hint, build the detail |
//! | 7 | observe            | map an error to a fatal one when `fail_on_tool_error` set |
//!
//! Every stage returns `Result<_, ToolOutcome>`: a failing stage short-circuits
//! the pipeline with an error *result* (never a panic or an early `return` of a
//! hand-built tuple), so the model always gets a tool message for the call.

use std::any::Any;
use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::FutureExt;
use serde_json::Value;

use crate::agent::circuit_breaker::{
    CircuitDecision, MessageCircuitBreaker, tripped_error_message,
};
use crate::agent::crash_points::crash_point;
use crate::agent::runner::AgentRunSpec;
use crate::agent::tool_call_context::{ToolCallIdentity, with_tool_call};
use crate::agent::tools::base::Tool;
use crate::agent::tools::registry::ToolRegistry;
use crate::agent::turn_journal::InFlightToolCall;
use crate::providers::base::ToolCallRequest;
use crate::session::journal::ToolCallStatus;
use crate::utils::helpers::truncate_text;
use crate::utils::runtime::repeated_external_lookup_error;

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

/// Whether a tool result text denotes a failure.
///
/// Tools signal failure by returning text that starts with `"Error"`. This is
/// the single place that convention is encoded.
pub fn is_error_result(result: &str) -> bool {
    result.starts_with("Error")
}

/// Telemetry status of a finished tool call (the `"status"` event field).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    Ok,
    Error,
    Blocked,
}

impl ToolStatus {
    /// The wire string used in telemetry events.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Error => "error",
            Self::Blocked => "blocked",
        }
    }
}

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
        }
    }

    /// Classify the raw text a tool returned: an error result or a success.
    pub fn from_tool_result(result: String) -> Self {
        if !is_error_result(&result) {
            return Self::ok(result);
        }
        log::error!("Tool error: {result}");
        let detail = single_line_detail(&result);
        Self::error(result, detail)
    }

    /// Observe stage: with `fail_on_tool_error`, an ordinary error becomes fatal.
    pub fn observed(mut self, fail_on_tool_error: bool) -> Self {
        if fail_on_tool_error && self.fatal.is_none() {
            self.fatal = self.error.clone();
        }
        self
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
        let event = HashMap::from([
            ("name".to_string(), tool_name.to_string()),
            ("status".to_string(), self.status.as_str().to_string()),
            ("detail".to_string(), self.detail),
        ]);
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

/// Tool result as stored in the journal, capped like the transcript copy.
pub fn journaled_result(spec: &AgentRunSpec, result: &str) -> String {
    if spec.max_tool_result_chars == 0 {
        return result.to_string();
    }
    truncate_text(result, spec.max_tool_result_chars)
}

// ── pipeline ──────────────────────────────────────────────────────────────────

/// Run one tool call through every stage and return its [`ToolOutcome`].
pub async fn run_tool_call(
    spec: &AgentRunSpec,
    tool_call: &ToolCallRequest,
    external_lookup_counts: Arc<Mutex<HashMap<String, usize>>>,
    message_circuit_breaker: Arc<Mutex<MessageCircuitBreaker>>,
) -> ToolOutcome {
    let outcome = run_stages(
        spec,
        tool_call,
        &external_lookup_counts,
        &message_circuit_breaker,
    )
    .await;
    let (Ok(outcome) | Err(outcome)) = outcome;
    outcome.observed(spec.fail_on_tool_error)
}

/// Stages 1–6; the first failing stage short-circuits with its outcome.
async fn run_stages(
    spec: &AgentRunSpec,
    tool_call: &ToolCallRequest,
    external_lookup_counts: &Mutex<HashMap<String, usize>>,
    message_circuit_breaker: &Mutex<MessageCircuitBreaker>,
) -> Result<ToolOutcome, ToolOutcome> {
    guard(tool_call, external_lookup_counts, message_circuit_breaker)?;
    let (tool, params) = resolve_and_validate(&spec.tools, tool_call)?;
    let in_flight = journal_begin(spec, tool_call, tool.as_ref(), &params)?;
    let result = execute(spec, tool_call, tool.as_ref(), &params).await;
    journal_finish(spec, in_flight, &result);
    Ok(ToolOutcome::from_tool_result(result))
}

/// Stage 1: cheap checks that need no tool lookup.
fn guard(
    tool_call: &ToolCallRequest,
    external_lookup_counts: &Mutex<HashMap<String, usize>>,
    message_circuit_breaker: &Mutex<MessageCircuitBreaker>,
) -> Result<(), ToolOutcome> {
    guard_message_circuit_breaker(tool_call, message_circuit_breaker)?;
    guard_repeated_external_lookup(tool_call, external_lookup_counts)?;
    guard_well_formed_arguments(tool_call)
}

/// Trip on consecutive identical message sends.
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

/// Block the model from repeating an identical external lookup.
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

/// Reject calls whose arguments the provider could not parse as JSON.
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

/// Stage 3: write-ahead the intent so a crash during execute leaves a record.
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

/// Stage 4: run the tool and return its result text.
///
/// A tool that panics or exceeds the timeout yields an error result instead
/// of unwinding through the turn, so the journal still gets a terminal record.
/// The crash points stay outside the isolation: they simulate a process kill.
async fn execute(
    spec: &AgentRunSpec,
    tool_call: &ToolCallRequest,
    tool: &dyn Tool,
    params: &Value,
) -> String {
    crash_point("after_intent");
    let guarded =
        AssertUnwindSafe(execute_under_identity(spec, tool_call, tool, params)).catch_unwind();
    let result = match effective_timeout(spec.tool_timeout, tool) {
        Some(limit) => match tokio::time::timeout(limit, guarded).await {
            Ok(finished) => finished,
            // Dropping `guarded` cancels the tool; `shell` kills its process tree on drop.
            Err(_) => Ok(timeout_error(&tool_call.name, limit)),
        },
        None => guarded.await,
    };
    let result = result.unwrap_or_else(|payload| panic_error(&tool_call.name, payload.as_ref()));
    crash_point("before_terminal");
    result
}

/// Run `tool`, under the call identity when a journal is attached.
async fn execute_under_identity(
    spec: &AgentRunSpec,
    tool_call: &ToolCallRequest,
    tool: &dyn Tool,
    params: &Value,
) -> String {
    match &spec.journal {
        Some(journal) => {
            let identity = ToolCallIdentity {
                turn_id: journal.turn_id().to_string(),
                call_id: tool_call.id.clone(),
            };
            with_tool_call(identity, tool.execute(params)).await
        }
        None => tool.execute(params).await,
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

/// Stage 5: write the terminal journal record (no-op without a journal).
fn journal_finish(spec: &AgentRunSpec, in_flight: Option<InFlightToolCall>, result: &str) {
    let Some(in_flight) = in_flight else {
        return;
    };
    let status = if is_error_result(result) {
        ToolCallStatus::Error
    } else {
        ToolCallStatus::Done
    };
    in_flight.finish(status, journaled_result(spec, result));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::circuit_breaker::{DEFAULT_N_IDENTICAL_MESSAGES, is_tripped_error};
    use crate::agent::tools::test_support::{PanickingTool, SlowTool, spec_with_tool};

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

    // ── is_error_result / details ─────────────────────────────────────────────

    #[test]
    fn error_results_are_recognised_by_their_prefix() {
        assert!(is_error_result("Error: boom"));
        assert!(is_error_result("Error"));
        assert!(!is_error_result("ok"));
        assert!(!is_error_result(" Error: leading space"));
    }

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
    fn from_tool_result_classifies_by_prefix() {
        assert_eq!(
            ToolOutcome::from_tool_result("Error: no".into()).status(),
            ToolStatus::Error
        );
        assert_eq!(
            ToolOutcome::from_tool_result("fine".into()).status(),
            ToolStatus::Ok
        );
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
}
