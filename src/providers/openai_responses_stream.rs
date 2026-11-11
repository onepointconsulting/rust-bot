//! Server-sent event state machine for [`super::openai_responses_provider`].
//!
//! Text and reasoning deltas are forwarded as they arrive; the final
//! [`LLMResponse`] is parsed from the terminal `response.completed` /
//! `response.incomplete` event by the same parser as a non-streamed body. Output
//! items seen on the way (`response.output_item.*`) fill in what the terminal event
//! leaves out.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use crate::providers::base::LLMResponse;
use crate::providers::openai_compat_stream::{SseEventAccumulator, StreamChunkDeltas};
use crate::providers::openai_responses_provider::{failure_message, parse_response_object};

/// Ends the error text of a stream that stopped early. Contains the transient
/// marker `connection`, so `chat_with_retry` retries the request.
const CLOSED_EARLY: &str = "connection closed before response completed";

/// Separates reasoning summary parts in the streamed reasoning text.
const SUMMARY_PART_SEPARATOR: &str = "\n\n";

/// Folds Responses API stream events into an [`LLMResponse`].
#[derive(Debug)]
pub(crate) struct ResponsesStreamAccumulator {
    /// Recorded in the response's replay log.
    model_name: String,
    /// Output items by `output_index`: as announced by `output_item.added`, with
    /// function-call argument deltas applied, then replaced by `output_item.done`.
    output_items: BTreeMap<u64, Value>,
    /// `output_index` of every function call whose `output_item.done` has not arrived.
    unfinished_function_calls: BTreeSet<u64>,
    /// The `response` of the terminal `response.completed` / `response.incomplete`.
    terminal_response: Option<Value>,
}

impl ResponsesStreamAccumulator {
    pub(crate) fn new(model_name: impl Into<String>) -> Self {
        Self {
            model_name: model_name.into(),
            output_items: BTreeMap::new(),
            unfinished_function_calls: BTreeSet::new(),
            terminal_response: None,
        }
    }

    /// Start tracking the item of an `output_item.added` event.
    fn add_item(&mut self, event: &Value) {
        let (Some(index), Some(item)) = (output_index(event), event.get("item")) else {
            return;
        };
        if is_function_call(item) {
            self.unfinished_function_calls.insert(index);
        }
        self.output_items.insert(index, item.clone());
    }

    /// Replace a tracked item with the final one of an `output_item.done` event.
    fn finish_item(&mut self, event: &Value) {
        let (Some(index), Some(item)) = (output_index(event), event.get("item")) else {
            return;
        };
        self.unfinished_function_calls.remove(&index);
        self.output_items.insert(index, item.clone());
    }

    /// Apply a `function_call_arguments.delta` (append) or `.done` (replace) event.
    fn update_arguments(&mut self, event: &Value, replace: bool) {
        let Some(item) = output_index(event).and_then(|index| self.output_items.get_mut(&index))
        else {
            return;
        };
        if !is_function_call(item) {
            return;
        }
        let text_key = if replace { "arguments" } else { "delta" };
        let Some(text) = event.get(text_key).and_then(Value::as_str) else {
            return;
        };
        let arguments = if replace {
            text.to_string()
        } else {
            let existing = item.get("arguments").and_then(Value::as_str).unwrap_or("");
            format!("{existing}{text}")
        };
        item["arguments"] = Value::String(arguments);
    }

    /// The items seen during the stream, in output order.
    fn streamed_items(&self) -> Vec<Value> {
        self.output_items.values().cloned().collect()
    }

    /// The terminal response, completed with the streamed items: they become its
    /// output when it has none, and lend it any `encrypted_content` it lacks.
    fn completed_response(&self, mut response: Value) -> Value {
        let streamed_items = self.streamed_items();
        match response.get_mut("output").and_then(Value::as_array_mut) {
            Some(output) if !output.is_empty() => {
                backfill_encrypted_content(output, &streamed_items)
            }
            _ => response["output"] = Value::Array(streamed_items),
        }
        response
    }
}

impl SseEventAccumulator for ResponsesStreamAccumulator {
    fn apply_event(&mut self, event: &Value) -> Result<StreamChunkDeltas, String> {
        let mut deltas = StreamChunkDeltas::default();
        match event.get("type").and_then(Value::as_str).unwrap_or("") {
            "response.output_item.added" => self.add_item(event),
            "response.output_item.done" => self.finish_item(event),
            "response.output_text.delta" | "response.refusal.delta" => {
                deltas.content = delta_text(event);
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                deltas.reasoning = delta_text(event);
            }
            "response.reasoning_summary_part.done" => {
                deltas.reasoning = Some(SUMMARY_PART_SEPARATOR.to_string());
            }
            "response.function_call_arguments.delta" => self.update_arguments(event, false),
            "response.function_call_arguments.done" => self.update_arguments(event, true),
            "response.completed" | "response.incomplete" => {
                self.terminal_response = event.get("response").cloned();
            }
            "response.failed" => return Err(failure_message(event.get("response"))),
            "error" => return Err(error_event_message(event)),
            _ => {}
        }
        Ok(deltas)
    }

    /// Fails when the stream ended without a terminal event, or with a function call
    /// still unfinished: running a tool with cut-off arguments is worse than a retry.
    fn finish(self) -> LLMResponse {
        let Some(terminal_response) = self.terminal_response.clone() else {
            return LLMResponse::error(format!("OpenAI Responses stream error: {CLOSED_EARLY}"));
        };
        let response = parse_response_object(
            &self.completed_response(terminal_response),
            &self.model_name,
        );
        let unfinished_call = self
            .unfinished_function_calls
            .iter()
            .find_map(|index| self.output_items.get(index));
        match unfinished_call {
            Some(call) if response.finish_reason == "tool_calls" => LLMResponse::error(format!(
                "OpenAI Responses stream ended with the tool call {} unfinished: {CLOSED_EARLY}",
                call.get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("(unnamed)")
            )),
            _ => response,
        }
    }
}

/// The `output_index` of an event.
fn output_index(event: &Value) -> Option<u64> {
    event.get("output_index").and_then(Value::as_u64)
}

fn is_function_call(item: &Value) -> bool {
    item.get("type").and_then(Value::as_str) == Some("function_call")
}

/// The non-empty `delta` text of a delta event.
fn delta_text(event: &Value) -> Option<String> {
    event
        .get("delta")
        .and_then(Value::as_str)
        .filter(|delta| !delta.is_empty())
        .map(str::to_string)
}

/// The text of an `error` event: `code: message`, or the message alone.
fn error_event_message(event: &Value) -> String {
    let message = event
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("unknown error");
    match event.get("code").and_then(Value::as_str) {
        Some(code) => format!("{code}: {message}"),
        None => message.to_string(),
    }
}

/// Copy `encrypted_content` from `source` reasoning items onto the `target` reasoning
/// items with the same id that lack it. Some deployments send it in only one of
/// `output_item.done` and the terminal response, and replay needs it.
fn backfill_encrypted_content(target: &mut [Value], source: &[Value]) {
    let has_encrypted_content = |item: &Value| {
        item.get("encrypted_content")
            .and_then(Value::as_str)
            .is_some_and(|content| !content.is_empty())
    };
    for item in target.iter_mut() {
        if item.get("type").and_then(Value::as_str) != Some("reasoning")
            || has_encrypted_content(item)
        {
            continue;
        }
        let donor = source.iter().find(|candidate| {
            candidate.get("id").is_some()
                && candidate.get("id") == item.get("id")
                && has_encrypted_content(candidate)
        });
        if let Some(donor) = donor {
            item["encrypted_content"] = donor["encrypted_content"].clone();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use serde_json::json;

    use super::*;
    use crate::bus::outbound_events::ProgressKind;
    use crate::providers::base::{BoxedProgressCallback, LLMProvider};
    use crate::providers::openai_compat_stream::consume_sse_events;
    use crate::providers::openai_responses_provider::OpenAIResponsesProvider;

    const MODEL: &str = "gpt-6-sol";

    /// What the callbacks received, and the final response.
    struct StreamOutcome {
        response: LLMResponse,
        content_deltas: Vec<String>,
        progress: Vec<(String, ProgressKind)>,
    }

    /// Feed `events` as SSE frames (split mid-frame) through `consume_sse_events`.
    async fn run_stream(events: &[Value]) -> StreamOutcome {
        let sse: String = events
            .iter()
            .map(|event| format!("data: {event}\n\n"))
            .collect();
        let (head, tail) = sse.split_at(sse.len() / 2);
        let byte_stream = futures::stream::iter([
            Ok::<_, reqwest::Error>(head.as_bytes().to_vec()),
            Ok(tail.as_bytes().to_vec()),
        ]);

        let content_deltas = Arc::new(Mutex::new(Vec::new()));
        let content_sink = Arc::clone(&content_deltas);
        let on_content = Some(move |delta: String| {
            content_sink.lock().unwrap().push(delta);
            async {}
        });
        let progress = Arc::new(Mutex::new(Vec::new()));
        let progress_sink = Arc::clone(&progress);
        let on_progress: BoxedProgressCallback = Box::new(move |text, kind| {
            progress_sink.lock().unwrap().push((text, kind));
            Box::pin(async {})
        });

        let response = consume_sse_events(
            byte_stream,
            ResponsesStreamAccumulator::new(MODEL),
            Duration::from_secs(5),
            5,
            &on_content,
            &Some(on_progress),
        )
        .await;
        let content_deltas = content_deltas.lock().unwrap().clone();
        let progress = progress.lock().unwrap().clone();
        StreamOutcome {
            response,
            content_deltas,
            progress,
        }
    }

    fn reasoning_done(encrypted_content: Option<&str>) -> Value {
        let mut item = json!({
            "type": "reasoning",
            "id": "rs_1",
            "summary": [
                { "type": "summary_text", "text": "Plan." },
                { "type": "summary_text", "text": "Check." }
            ]
        });
        if let Some(content) = encrypted_content {
            item["encrypted_content"] = json!(content);
        }
        item
    }

    fn function_call(arguments: &str, status: &str) -> Value {
        json!({
            "type": "function_call", "id": "fc_1", "call_id": "call_1",
            "name": "get_weather", "arguments": arguments, "status": status
        })
    }

    fn message_done(text: &str) -> Value {
        json!({
            "type": "message", "id": "msg_1", "role": "assistant", "status": "completed",
            "content": [{ "type": "output_text", "text": text, "annotations": [] }]
        })
    }

    fn completed(output: Value) -> Value {
        json!({
            "type": "response.completed",
            "response": {
                "status": "completed",
                "output": output,
                "usage": { "input_tokens": 10, "output_tokens": 5 }
            }
        })
    }

    /// reasoning (two summary parts) → text → function call with streamed arguments.
    fn tool_turn_events(terminal: Value) -> Vec<Value> {
        vec![
            json!({ "type": "response.created", "response": { "id": "resp_1", "status": "in_progress" } }),
            json!({ "type": "response.output_item.added", "output_index": 0,
                    "item": { "type": "reasoning", "id": "rs_1", "summary": [] } }),
            json!({ "type": "response.reasoning_summary_text.delta", "output_index": 0, "delta": "Plan." }),
            json!({ "type": "response.reasoning_summary_part.done", "output_index": 0 }),
            json!({ "type": "response.reasoning_summary_text.delta", "output_index": 0, "delta": "Check." }),
            json!({ "type": "response.output_item.done", "output_index": 0, "item": reasoning_done(Some("enc-1")) }),
            json!({ "type": "response.output_item.added", "output_index": 1,
                    "item": { "type": "message", "id": "msg_1", "role": "assistant", "content": [] } }),
            json!({ "type": "response.output_text.delta", "output_index": 1, "delta": "Let me " }),
            json!({ "type": "response.output_text.delta", "output_index": 1, "delta": "check." }),
            json!({ "type": "response.output_item.done", "output_index": 1, "item": message_done("Let me check.") }),
            json!({ "type": "response.output_item.added", "output_index": 2,
                    "item": function_call("", "in_progress") }),
            json!({ "type": "response.function_call_arguments.delta", "output_index": 2, "delta": "{\"location\":" }),
            json!({ "type": "response.function_call_arguments.delta", "output_index": 2, "delta": "\"Faro\"}" }),
            json!({ "type": "response.function_call_arguments.done", "output_index": 2,
                    "arguments": "{\"location\":\"Faro\"}" }),
            json!({ "type": "response.output_item.done", "output_index": 2,
                    "item": function_call("{\"location\":\"Faro\"}", "completed") }),
            terminal,
        ]
    }

    fn full_output() -> Value {
        json!([
            reasoning_done(Some("enc-1")),
            message_done("Let me check."),
            function_call("{\"location\":\"Faro\"}", "completed")
        ])
    }

    #[tokio::test]
    async fn text_and_reasoning_deltas_reach_the_callbacks() {
        let outcome = run_stream(&tool_turn_events(completed(full_output()))).await;
        assert_eq!(outcome.content_deltas, vec!["Let me ", "check."]);
        assert_eq!(
            outcome.progress,
            vec![
                ("Plan.".to_string(), ProgressKind::ReasoningDelta),
                ("\n\n".to_string(), ProgressKind::ReasoningDelta),
                ("Check.".to_string(), ProgressKind::ReasoningDelta),
                (String::new(), ProgressKind::ReasoningEnd),
            ]
        );
    }

    #[tokio::test]
    async fn the_result_matches_the_parsed_terminal_response() {
        let terminal = completed(full_output());
        let outcome = run_stream(&tool_turn_events(terminal.clone())).await;
        assert_eq!(
            outcome.response,
            parse_response_object(&terminal["response"], MODEL)
        );
        assert_eq!(outcome.response.finish_reason, "tool_calls");
        assert_eq!(outcome.response.tool_calls[0].id, "call_1");
        assert_eq!(
            outcome.response.tool_calls[0].arguments["location"],
            json!("Faro")
        );
        assert_eq!(outcome.response.content.as_deref(), Some("Let me check."));
        assert_eq!(
            outcome.response.reasoning_content.as_deref(),
            Some("Plan.\n\nCheck.")
        );
        assert_eq!(outcome.response.usage.output_tokens, Some(5));
    }

    #[tokio::test]
    async fn an_empty_terminal_output_falls_back_to_the_streamed_items() {
        let outcome = run_stream(&tool_turn_events(completed(json!([])))).await;
        assert_eq!(
            outcome.response,
            parse_response_object(&completed(full_output())["response"], MODEL)
        );
    }

    #[test]
    fn argument_deltas_accumulate_until_done_replaces_them() {
        let mut accumulator = ResponsesStreamAccumulator::new(MODEL);
        let events = [
            json!({ "type": "response.output_item.added", "output_index": 0, "item": function_call("", "in_progress") }),
            json!({ "type": "response.function_call_arguments.delta", "output_index": 0, "delta": "{\"a\":" }),
            json!({ "type": "response.function_call_arguments.delta", "output_index": 0, "delta": "1}" }),
        ];
        for event in &events {
            accumulator.apply_event(event).unwrap();
        }
        assert_eq!(
            accumulator.output_items[&0]["arguments"],
            json!("{\"a\":1}")
        );
        accumulator
            .apply_event(&json!({ "type": "response.function_call_arguments.done",
                                  "output_index": 0, "arguments": "{\"a\":2}" }))
            .unwrap();
        assert_eq!(
            accumulator.output_items[&0]["arguments"],
            json!("{\"a\":2}")
        );
        assert!(accumulator.unfinished_function_calls.contains(&0));
    }

    #[tokio::test]
    async fn encrypted_content_is_backfilled_from_the_streamed_items() {
        let mut terminal_output = full_output();
        terminal_output[0]
            .as_object_mut()
            .unwrap()
            .remove("encrypted_content");
        let outcome = run_stream(&tool_turn_events(completed(terminal_output))).await;
        let blocks = outcome.response.thinking_blocks.unwrap();
        assert_eq!(blocks[0]["item"]["encrypted_content"], json!("enc-1"));
    }

    #[test]
    fn encrypted_content_is_backfilled_onto_the_terminal_items_by_id() {
        let mut target = vec![reasoning_done(None), message_done("x")];
        backfill_encrypted_content(&mut target, &[reasoning_done(Some("enc-9"))]);
        assert_eq!(target[0]["encrypted_content"], json!("enc-9"));
        assert!(target[1].get("encrypted_content").is_none());
    }

    #[tokio::test]
    async fn a_failed_response_is_an_error() {
        let outcome = run_stream(&[json!({
            "type": "response.failed",
            "response": {
                "status": "failed",
                "error": { "code": "server_error", "message": "The model crashed." }
            }
        })])
        .await;
        assert_eq!(outcome.response.finish_reason, "error");
        assert_eq!(
            outcome.response.content.as_deref(),
            Some("server_error: The model crashed.")
        );
        assert!(OpenAIResponsesProvider::is_transient_error(
            outcome.response.content.as_deref()
        ));
    }

    #[tokio::test]
    async fn an_error_event_is_an_error() {
        let outcome = run_stream(&[json!({
            "type": "error",
            "code": "rate_limit_exceeded",
            "message": "Slow down."
        })])
        .await;
        assert_eq!(outcome.response.finish_reason, "error");
        assert_eq!(
            outcome.response.content.as_deref(),
            Some("rate_limit_exceeded: Slow down.")
        );
        assert!(OpenAIResponsesProvider::is_transient_error(
            outcome.response.content.as_deref()
        ));
    }

    #[tokio::test]
    async fn a_stream_without_a_terminal_event_is_a_transient_error() {
        let mut events = tool_turn_events(json!({ "type": "response.in_progress" }));
        events.pop();
        let outcome = run_stream(&events).await;
        assert_eq!(outcome.response.finish_reason, "error");
        assert!(
            outcome
                .response
                .content
                .as_deref()
                .unwrap()
                .contains(CLOSED_EARLY)
        );
        assert!(OpenAIResponsesProvider::is_transient_error(
            outcome.response.content.as_deref()
        ));
    }

    #[tokio::test]
    async fn a_tool_call_without_its_done_event_is_a_transient_error() {
        let mut events = tool_turn_events(completed(full_output()));
        // Drop the function call's `output_item.done`.
        events.remove(events.len() - 2);
        let outcome = run_stream(&events).await;
        assert_eq!(outcome.response.finish_reason, "error");
        let message = outcome.response.content.unwrap();
        assert!(message.contains("get_weather"), "{message}");
        assert!(OpenAIResponsesProvider::is_transient_error(Some(&message)));
    }
}
