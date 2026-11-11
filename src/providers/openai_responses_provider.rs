//! OpenAI Responses API provider (`POST {base}/responses`), used for api.openai.com.
//!
//! Chat Completions cannot combine function tools with reasoning on the GPT-6 family,
//! so every request whose base URL is api.openai.com is routed here by
//! `providers::factory`. Other OpenAI-compatible gateways keep using
//! [`OpenAICompatProvider`].
//!
//! Requests are stateless (`store: false`): the whole conversation is resent each time,
//! and the model's reasoning is carried across turns by replaying its encrypted
//! reasoning items, which `include: ["reasoning.encrypted_content"]` asks for.
//!
//! # Replay log
//!
//! Each assistant reply keeps the output items it came from in `thinking_blocks`, one
//! block per item and in their original order:
//! `{"type": "openai_responses_item", "model": <model>, "item": <item>}`. Reasoning
//! items are stored whole; message items as `id` + `phase`; function calls as `id` +
//! `call_id`. The text and tool calls themselves stay in `content` / `tool_calls`,
//! which remain the source of truth (they may be edited after the reply). The
//! Anthropic provider only replays `"thinking"` blocks and the Chat Completions
//! provider drops `thinking_blocks`, so the log is invisible to both.
//!
//! The log is replayed only to the model that wrote it. History from another model or
//! provider is sent without reasoning and without item ids, which avoids OpenAI's
//! validation of reasoning/item pairing.

use std::collections::{HashMap, HashSet};

use serde_json::{Value, json};

use crate::providers::base::{GenerationSettings, LLMProvider, LLMResponse, ToolCallRequest};
use crate::providers::cache_control::strip_internal_tool_fields;
use crate::providers::openai_compat_provider::{OpenAICompatProvider, post_json, post_sse};
use crate::providers::openai_compat_stream::{consume_sse_events, stream_idle_timeout};
use crate::providers::openai_responses_stream::ResponsesStreamAccumulator;
use crate::providers::registry::ProviderSpec;

/// `type` of a `thinking_blocks` entry that holds one Responses output item for replay.
pub(crate) const REPLAY_BLOCK_TYPE: &str = "openai_responses_item";

/// The smallest `max_output_tokens` the Responses API accepts.
const MIN_MAX_OUTPUT_TOKENS: usize = 16;

/// The reasoning effort that means "let the model decide": sent as no effort at all.
const ADAPTIVE_EFFORT: &str = "adaptive";

/// Prefix OpenAI requires on a replayed function-call item id.
const FUNCTION_CALL_ID_PREFIX: &str = "fc_";

pub struct OpenAIResponsesProvider {
    api_key: Option<String>,
    api_base: Option<String>,
    default_model: Option<String>,
    extra_headers: HashMap<String, String>,
    spec: Option<ProviderSpec>,
    generation: GenerationSettings,
    /// Shared reqwest client for streamed and non-streamed requests.
    http_client: reqwest::Client,
    responses_url: String,
}

impl OpenAIResponsesProvider {
    const DEFAULT_MODEL: &str = "gpt-6-luna";

    /// The request body for `model_name`, without the `stream` flag.
    ///
    /// `temperature` is never sent: every OpenAI model served here is a reasoning
    /// model, and those reject it.
    fn build_request(
        model_name: &str,
        messages: &[Value],
        tools: Option<Vec<Value>>,
        max_tokens: usize,
        reasoning_effort: Option<&str>,
        tool_choice: Option<Value>,
    ) -> Value {
        let mut request = json!({
            "model": model_name,
            "input": map_messages_to_input(messages, model_name),
            "store": false,
            "max_output_tokens": max_tokens.max(MIN_MAX_OUTPUT_TOKENS),
            "include": ["reasoning.encrypted_content"],
            "reasoning": reasoning_config(reasoning_effort),
        });
        if let Some(tools) = tools.filter(|tools| !tools.is_empty()) {
            request["tools"] = Value::Array(map_tools(tools));
            // A tool choice without tools is rejected, so it only goes with them.
            if let Some(choice) = tool_choice {
                request["tool_choice"] = map_tool_choice(choice);
            }
        }
        request
    }

    /// The model a request goes to: the explicit one, else the provider default.
    fn model_name(&self, model: Option<String>) -> String {
        model.unwrap_or_else(|| self.get_default_model())
    }
}

/// The `reasoning` request object: always a summary, plus the configured effort.
/// No effort, or `"adaptive"`, leaves the effort to the model's default.
fn reasoning_config(reasoning_effort: Option<&str>) -> Value {
    let mut reasoning = json!({ "summary": "auto" });
    if let Some(effort) = reasoning_effort
        .map(str::trim)
        .filter(|effort| !effort.is_empty() && *effort != ADAPTIVE_EFFORT)
    {
        reasoning["effort"] = json!(effort);
    }
    reasoning
}

/// Turn Chat Completions tool definitions (`{type, function: {name, description,
/// parameters}}`) into the flat Responses form, without strict schema enforcement.
fn map_tools(mut tools: Vec<Value>) -> Vec<Value> {
    strip_internal_tool_fields(&mut tools);
    tools
        .iter()
        .filter_map(|tool| {
            let function = tool.get("function")?;
            let mut mapped = json!({
                "type": "function",
                "name": function.get("name")?.clone(),
                "strict": false,
            });
            for key in ["description", "parameters"] {
                if let Some(value) = function.get(key).filter(|value| !value.is_null()) {
                    mapped[key] = value.clone();
                }
            }
            Some(mapped)
        })
        .collect()
}

/// Turn a Chat Completions `tool_choice` into the Responses form: a named function
/// (`{type: "function", function: {name}}`) becomes `{type: "function", name}`;
/// `"auto"`, `"none"`, `"required"` and anything else pass through.
fn map_tool_choice(choice: Value) -> Value {
    match choice
        .get("function")
        .and_then(|function| function.get("name"))
    {
        Some(name) => json!({ "type": "function", "name": name }),
        None => choice,
    }
}

/// The `type` of a JSON item, or `""`.
fn item_type(item: &Value) -> &str {
    item.get("type").and_then(Value::as_str).unwrap_or("")
}

/// A string field of a JSON item.
fn str_field<'a>(item: &'a Value, key: &str) -> Option<&'a str> {
    item.get(key).and_then(Value::as_str)
}

/// Map the conversation (Chat Completions message format) onto Responses `input` items.
/// `model_name` decides whether assistant replay logs are replayed (see the module docs).
fn map_messages_to_input(messages: &[Value], model_name: &str) -> Vec<Value> {
    let sanitized = <OpenAIResponsesProvider as LLMProvider>::sanitize_empty_content(messages);
    let mut input = Vec::with_capacity(sanitized.len());
    for message in &sanitized {
        match str_field(message, "role").unwrap_or("user") {
            "system" | "developer" => input.extend(developer_item(message.get("content"))),
            "assistant" => input.extend(assistant_items(message, model_name)),
            "tool" => input.push(function_call_output_item(message)),
            _ => input.extend(user_item(message.get("content"))),
        }
    }
    input
}

/// A system prompt as a `developer` message, its text parts as `input_text` (any
/// `cache_control` markers dropped). `None` when it has no text.
fn developer_item(content: Option<&Value>) -> Option<Value> {
    let parts: Vec<Value> = input_content_parts(content)
        .into_iter()
        .filter(|part| item_type(part) == "input_text")
        .collect();
    (!parts.is_empty()).then(|| json!({ "role": "developer", "content": parts }))
}

/// A user message with `input_text` / `input_image` parts. `None` when nothing is left.
fn user_item(content: Option<&Value>) -> Option<Value> {
    let parts = input_content_parts(content);
    (!parts.is_empty()).then(|| json!({ "role": "user", "content": parts }))
}

/// Map message content (a string, or Chat Completions `text` / `image_url` parts)
/// onto Responses input parts. Empty text and unknown part types are dropped.
fn input_content_parts(content: Option<&Value>) -> Vec<Value> {
    match content {
        Some(Value::String(text)) if !text.is_empty() => {
            vec![json!({ "type": "input_text", "text": text })]
        }
        Some(Value::Array(parts)) => parts.iter().filter_map(input_content_part).collect(),
        _ => Vec::new(),
    }
}

/// One content part as an `input_text` or `input_image` part, if it is either.
fn input_content_part(part: &Value) -> Option<Value> {
    match item_type(part) {
        "text" | "input_text" => {
            let text = str_field(part, "text").filter(|text| !text.is_empty())?;
            Some(json!({ "type": "input_text", "text": text }))
        }
        "image_url" => {
            let url = part
                .get("image_url")
                .and_then(|image| str_field(image, "url"))
                .filter(|url| !url.is_empty())?;
            Some(json!({ "type": "input_image", "image_url": url, "detail": "auto" }))
        }
        "input_image" => Some(part.clone()),
        _ => None,
    }
}

/// A tool result as a `function_call_output`. The output is a string, or
/// `input_text` / `input_image` parts when the result contains images.
fn function_call_output_item(message: &Value) -> Value {
    let call_id = str_field(message, "tool_call_id").unwrap_or("");
    let output = match message.get("content") {
        Some(Value::String(text)) => Value::String(text.clone()),
        Some(Value::Array(parts)) => {
            let mapped = input_content_parts(message.get("content"));
            if mapped.iter().any(|part| item_type(part) == "input_image") {
                Value::Array(mapped)
            } else {
                Value::String(joined_text(parts, "\n"))
            }
        }
        None | Some(Value::Null) => Value::String(String::new()),
        Some(other) => Value::String(other.to_string()),
    };
    json!({ "type": "function_call_output", "call_id": call_id, "output": output })
}

/// The `text` of every part that has one, joined with `separator`.
fn joined_text(parts: &[Value], separator: &str) -> String {
    parts
        .iter()
        .filter_map(|part| str_field(part, "text"))
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join(separator)
}

/// An assistant message's text: its string content, or its text parts concatenated.
fn assistant_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => joined_text(parts, ""),
        _ => String::new(),
    }
}

/// A tool call of an assistant message, in the form a `function_call` item needs.
#[derive(Debug, Clone, PartialEq)]
struct FunctionCall {
    call_id: String,
    name: String,
    arguments: String,
}

/// The assistant message's `tool_calls` (Chat Completions form) as [`FunctionCall`]s.
fn assistant_function_calls(tool_calls: Option<&Value>) -> Vec<FunctionCall> {
    let Some(tool_calls) = tool_calls.and_then(Value::as_array) else {
        return Vec::new();
    };
    tool_calls
        .iter()
        .filter_map(|tool_call| {
            let function = tool_call.get("function")?;
            let arguments = match function.get("arguments") {
                Some(Value::String(text)) => text.clone(),
                Some(Value::Null) | None => "{}".to_string(),
                Some(other) => other.to_string(),
            };
            Some(FunctionCall {
                call_id: str_field(tool_call, "id")?.to_string(),
                name: str_field(function, "name")?.to_string(),
                arguments,
            })
        })
        .collect()
}

/// A `function_call` input item. `item_id` is kept only when it is a function-call id.
fn function_call_item(call: &FunctionCall, item_id: Option<&str>) -> Value {
    let mut item = json!({
        "type": "function_call",
        "call_id": call.call_id,
        "name": call.name,
        "arguments": call.arguments,
    });
    if let Some(id) = item_id.filter(|id| id.starts_with(FUNCTION_CALL_ID_PREFIX)) {
        item["id"] = json!(id);
    }
    item
}

/// An assistant text input item: the original output message (`id`, `phase`) when its
/// id is known, else a plain assistant message.
fn assistant_message_item(text: &str, item_id: Option<&str>, phase: Option<&Value>) -> Value {
    let Some(id) = item_id else {
        return json!({ "role": "assistant", "content": text });
    };
    let mut item = json!({
        "type": "message",
        "role": "assistant",
        "id": id,
        "status": "completed",
        "content": [{ "type": "output_text", "text": text, "annotations": [] }],
    });
    if let Some(phase) = phase.filter(|phase| !phase.is_null()) {
        item["phase"] = phase.clone();
    }
    item
}

/// The replay log of an assistant message (see the module docs), when it can be
/// replayed to `model_name`: present, written by that model, and with the encrypted
/// content of every reasoning item (without it, neither the reasoning nor the items
/// paired with it can be sent under `store: false`).
fn replay_log<'a>(message: &'a Value, model_name: &str) -> Option<Vec<&'a Value>> {
    let blocks: Vec<&Value> = message
        .get("thinking_blocks")?
        .as_array()?
        .iter()
        .filter(|block| item_type(block) == REPLAY_BLOCK_TYPE)
        .collect();
    if blocks.is_empty()
        || blocks
            .iter()
            .any(|block| str_field(block, "model") != Some(model_name))
    {
        return None;
    }
    let items: Vec<&Value> = blocks
        .iter()
        .filter_map(|block| block.get("item"))
        .collect();
    let replayable = items.iter().all(|item| {
        item_type(item) != "reasoning"
            || str_field(item, "encrypted_content").is_some_and(|content| !content.is_empty())
    });
    replayable.then_some(items)
}

/// The input items of one assistant message.
///
/// With a usable replay log the original items are rebuilt in their original order:
/// reasoning verbatim, the message text (from `content`) in the first message item,
/// function calls (from `tool_calls`) with their `fc_` ids. Text the log has no message
/// item for goes before the first function call; tool calls it does not mention go
/// last, without id. Reasoning left with no item after it is dropped, since OpenAI
/// rejects a reasoning item without its following item.
///
/// Without a usable log: the text as a plain assistant message, then the function
/// calls without ids, and no reasoning.
fn assistant_items(message: &Value, model_name: &str) -> Vec<Value> {
    let text = assistant_text(message.get("content"));
    let calls = assistant_function_calls(message.get("tool_calls"));

    let Some(log) = replay_log(message, model_name) else {
        let mut items = Vec::with_capacity(calls.len() + 1);
        if !text.is_empty() {
            items.push(assistant_message_item(&text, None, None));
        }
        items.extend(calls.iter().map(|call| function_call_item(call, None)));
        return items;
    };

    let mut items = Vec::with_capacity(log.len() + 1);
    let mut text_sent = text.is_empty();
    let mut replayed_call_ids: HashSet<&str> = HashSet::new();
    for logged in log {
        match item_type(logged) {
            "reasoning" => items.push(logged.clone()),
            "message" if !text_sent => {
                items.push(assistant_message_item(
                    &text,
                    str_field(logged, "id"),
                    logged.get("phase"),
                ));
                text_sent = true;
            }
            "function_call" => {
                let call = calls
                    .iter()
                    .find(|call| str_field(logged, "call_id") == Some(call.call_id.as_str()));
                if let Some(call) = call
                    && replayed_call_ids.insert(call.call_id.as_str())
                {
                    items.push(function_call_item(call, str_field(logged, "id")));
                }
            }
            _ => {}
        }
    }
    if !text_sent {
        let first_call = items
            .iter()
            .position(|item| item_type(item) == "function_call")
            .unwrap_or(items.len());
        items.insert(first_call, assistant_message_item(&text, None, None));
    }
    items.extend(
        calls
            .iter()
            .filter(|call| !replayed_call_ids.contains(call.call_id.as_str()))
            .map(|call| function_call_item(call, None)),
    );
    while items
        .last()
        .is_some_and(|item| item_type(item) == "reasoning")
    {
        items.pop();
    }
    items
}

/// One replay-log block for `item`, written by `model_name`.
fn replay_block(model_name: &str, item: Value) -> HashMap<String, Value> {
    HashMap::from([
        ("type".to_string(), json!(REPLAY_BLOCK_TYPE)),
        ("model".to_string(), json!(model_name)),
        ("item".to_string(), item),
    ])
}

/// Whether an output item is still being written (or was cut off) rather than done.
fn is_unfinished_item(item: &Value) -> bool {
    matches!(
        str_field(item, "status"),
        Some("in_progress" | "incomplete")
    )
}

/// The text of a `message` output item: its `output_text` and `refusal` parts.
fn message_item_text(item: &Value) -> String {
    let Some(parts) = item.get("content").and_then(Value::as_array) else {
        return String::new();
    };
    parts
        .iter()
        .filter_map(|part| match item_type(part) {
            "output_text" => str_field(part, "text"),
            "refusal" => str_field(part, "refusal"),
            _ => None,
        })
        .collect()
}

/// The readable text of a `reasoning` item: its summary, else its raw reasoning text.
fn reasoning_item_text(item: &Value) -> String {
    let joined = |key: &str| {
        item.get(key)
            .and_then(Value::as_array)
            .map(|parts| joined_text(parts, "\n\n"))
            .unwrap_or_default()
    };
    let summary = joined("summary");
    if summary.is_empty() {
        joined("content")
    } else {
        summary
    }
}

/// A `function_call` output item as a tool call request, keeping OpenAI's `call_id`.
fn tool_call_request(item: &Value) -> Option<ToolCallRequest> {
    let call_id = str_field(item, "call_id").or_else(|| str_field(item, "id"))?;
    Some(ToolCallRequest {
        id: call_id.to_string(),
        name: str_field(item, "name")?.to_string(),
        arguments: OpenAICompatProvider::parse_tool_arguments(
            str_field(item, "arguments").unwrap_or(""),
        ),
        extra_content: None,
        provider_specific_fields: None,
        function_provider_specific_fields: None,
    })
}

/// The error text of a failed response: `code: message`, else the incomplete reason.
pub(crate) fn failure_message(response: Option<&Value>) -> String {
    let error = response
        .and_then(|response| response.get("error"))
        .filter(|error| !error.is_null());
    if let Some(error) = error {
        let code = str_field(error, "code").unwrap_or("unknown");
        let message = str_field(error, "message").unwrap_or("no message");
        return format!("{code}: {message}");
    }
    let reason = response
        .and_then(|response| response.get("incomplete_details"))
        .and_then(|details| str_field(details, "reason"));
    match reason {
        Some(reason) => format!("incomplete: {reason}"),
        None => "OpenAI response failed without error details".to_string(),
    }
}

/// The `finish_reason` for a response `status`, or the error text when the status
/// means the reply failed.
fn finish_reason_for(response: &Value, has_tool_calls: bool) -> Result<String, String> {
    match str_field(response, "status").unwrap_or("completed") {
        "incomplete" => {
            let reason = response
                .get("incomplete_details")
                .and_then(|details| str_field(details, "reason"));
            match reason {
                Some("max_output_tokens") => Ok("length".to_string()),
                Some(reason) => Err(format!("OpenAI response incomplete: {reason}")),
                None => Err("OpenAI response incomplete without a reason".to_string()),
            }
        }
        "failed" | "cancelled" => Err(failure_message(Some(response))),
        _ if has_tool_calls => Ok("tool_calls".to_string()),
        _ => Ok("stop".to_string()),
    }
}

/// Parse a Responses `response` object: the non-streamed body, or the `response` of
/// the terminal stream event. `model_name` is recorded in the replay log.
pub(crate) fn parse_response_object(response: &Value, model_name: &str) -> LLMResponse {
    let usage = response
        .get("usage")
        .filter(|usage| !usage.is_null())
        .map(OpenAICompatProvider::parse_usage)
        .unwrap_or_default();

    let mut content = String::new();
    let mut reasoning_texts: Vec<String> = Vec::new();
    let mut tool_calls = Vec::new();
    let mut replay_blocks = Vec::new();
    let no_output = Vec::new();
    let output = response
        .get("output")
        .and_then(Value::as_array)
        .unwrap_or(&no_output);
    for item in output {
        match item_type(item) {
            "message" => {
                content.push_str(&message_item_text(item));
                let mut logged = json!({ "type": "message" });
                for key in ["id", "phase"] {
                    if let Some(value) = item.get(key).filter(|value| !value.is_null()) {
                        logged[key] = value.clone();
                    }
                }
                replay_blocks.push(replay_block(model_name, logged));
            }
            "reasoning" => {
                let text = reasoning_item_text(item);
                if !text.is_empty() {
                    reasoning_texts.push(text);
                }
                replay_blocks.push(replay_block(model_name, item.clone()));
            }
            "function_call" if !is_unfinished_item(item) => {
                if let Some(call) = tool_call_request(item) {
                    let mut logged = json!({ "type": "function_call", "call_id": call.id });
                    if let Some(id) = item.get("id").filter(|id| !id.is_null()) {
                        logged["id"] = id.clone();
                    }
                    replay_blocks.push(replay_block(model_name, logged));
                    tool_calls.push(call);
                }
            }
            _ => {}
        }
    }

    let finish_reason = match finish_reason_for(response, !tool_calls.is_empty()) {
        Ok(reason) => reason,
        Err(message) => {
            return LLMResponse {
                usage,
                ..LLMResponse::error(message)
            };
        }
    };
    LLMResponse {
        content: (!content.is_empty()).then_some(content),
        tool_calls,
        finish_reason,
        usage,
        reasoning_content: (!reasoning_texts.is_empty()).then(|| reasoning_texts.join("\n\n")),
        thinking_blocks: (!replay_blocks.is_empty()).then_some(replay_blocks),
    }
}

impl LLMProvider for OpenAIResponsesProvider {
    fn new(
        api_key: Option<String>,
        api_base: Option<String>,
        default_model: Option<String>,
        extra_headers: Option<HashMap<String, String>>,
        spec: Option<ProviderSpec>,
    ) -> Self {
        let extra_headers = extra_headers.unwrap_or_default();
        let effective_base =
            api_base.or_else(|| spec.as_ref().and_then(|spec| spec.default_api_base.clone()));
        let header_map = OpenAICompatProvider::build_request_headers(
            api_key.as_deref(),
            spec.as_ref(),
            effective_base.as_deref(),
            &extra_headers,
        );
        let http_client = reqwest::Client::builder()
            .default_headers(header_map)
            .build()
            .unwrap_or_default();
        let responses_url = format!(
            "{}/responses",
            effective_base
                .as_deref()
                .unwrap_or(OpenAICompatProvider::DEFAULT_API_BASE)
                .trim_end_matches('/')
        );
        OpenAICompatProvider::export_provider_env(
            spec.as_ref(),
            api_key.clone(),
            effective_base.clone(),
        );

        Self {
            api_key,
            api_base: effective_base,
            default_model: Some(default_model.unwrap_or_else(|| Self::DEFAULT_MODEL.to_string())),
            extra_headers,
            spec,
            generation: GenerationSettings::new(),
            http_client,
            responses_url,
        }
    }

    fn api_key(&self) -> Option<String> {
        self.api_key.clone()
    }

    fn api_base(&self) -> Option<String> {
        self.api_base.clone()
    }

    fn extra_headers(&self) -> Option<HashMap<String, String>> {
        Some(self.extra_headers.clone())
    }

    fn generation_settings(&self) -> &GenerationSettings {
        &self.generation
    }

    fn generation_settings_mut(&mut self) -> &mut GenerationSettings {
        &mut self.generation
    }

    fn spec(&self) -> Option<&ProviderSpec> {
        self.spec.as_ref()
    }

    fn get_default_model(&self) -> String {
        self.default_model
            .clone()
            .unwrap_or_else(|| Self::DEFAULT_MODEL.to_string())
    }

    /// `temperature` is ignored; see [`OpenAIResponsesProvider::build_request`].
    async fn chat(
        &self,
        messages: Vec<Value>,
        tools: Option<Vec<Value>>,
        model: Option<String>,
        max_tokens: usize,
        _temperature: Option<f32>,
        reasoning_effort: Option<String>,
        tool_choice: Option<Value>,
    ) -> LLMResponse {
        let model_name = self.model_name(model);
        let request = Self::build_request(
            &model_name,
            &messages,
            tools,
            max_tokens,
            reasoning_effort.as_deref(),
            tool_choice,
        );
        match post_json(&self.http_client, &self.responses_url, &request).await {
            Ok(response) => parse_response_object(&response, &model_name),
            Err(message) => LLMResponse::error(message),
        }
    }

    /// `temperature` is ignored; see [`OpenAIResponsesProvider::build_request`].
    async fn chat_stream<F, Fut>(
        &self,
        messages: Vec<Value>,
        tools: Option<Vec<Value>>,
        model: Option<String>,
        max_tokens: usize,
        _temperature: Option<f32>,
        reasoning_effort: Option<String>,
        tool_choice: Option<Value>,
        on_content_delta: &Option<F>,
        on_progress: &Option<crate::providers::base::BoxedProgressCallback>,
    ) -> LLMResponse
    where
        F: Fn(String) -> Fut + Send + Sync,
        Fut: std::future::Future<Output = ()> + Send,
    {
        let model_name = self.model_name(model);
        let mut request = Self::build_request(
            &model_name,
            &messages,
            tools,
            max_tokens,
            reasoning_effort.as_deref(),
            tool_choice,
        );
        request["stream"] = json!(true);

        let response = match post_sse(&self.http_client, &self.responses_url, &request).await {
            Ok(response) => response,
            Err(error_response) => return error_response,
        };
        let (idle_timeout, idle_timeout_s) = stream_idle_timeout();
        consume_sse_events(
            response.bytes_stream(),
            ResponsesStreamAccumulator::new(model_name),
            idle_timeout,
            idle_timeout_s,
            on_content_delta,
            on_progress,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::cache_control::CACHE_BOUNDARY_KEY;

    const MODEL: &str = "gpt-6-astra";

    fn weather_tool() -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "get_weather",
                "description": "Get the current weather",
                "parameters": {
                    "type": "object",
                    "properties": { "location": { "type": "string" } }
                }
            }
        })
    }

    fn request_for(messages: &[Value], reasoning_effort: Option<&str>) -> Value {
        OpenAIResponsesProvider::build_request(
            MODEL,
            messages,
            Some(vec![weather_tool()]),
            4096,
            reasoning_effort,
            None,
        )
    }

    fn user(text: &str) -> Value {
        json!({ "role": "user", "content": text })
    }

    // ── request building ─────────────────────────────────────────────────────

    #[test]
    fn tools_are_flattened_without_strict_mode_or_internal_fields() {
        let mut flagged = weather_tool();
        flagged[CACHE_BOUNDARY_KEY] = json!(true);
        assert_eq!(
            map_tools(vec![flagged]),
            vec![json!({
                "type": "function",
                "name": "get_weather",
                "description": "Get the current weather",
                "parameters": {
                    "type": "object",
                    "properties": { "location": { "type": "string" } }
                },
                "strict": false
            })]
        );
    }

    #[test]
    fn a_named_tool_choice_is_flattened_and_strings_pass_through() {
        assert_eq!(
            map_tool_choice(json!({ "type": "function", "function": { "name": "get_weather" } })),
            json!({ "type": "function", "name": "get_weather" })
        );
        assert_eq!(map_tool_choice(json!("required")), json!("required"));
    }

    #[test]
    fn tool_choice_is_sent_only_with_tools() {
        let choice = || Some(json!("required"));
        let with_tools = OpenAIResponsesProvider::build_request(
            MODEL,
            &[user("hi")],
            Some(vec![weather_tool()]),
            100,
            None,
            choice(),
        );
        assert_eq!(with_tools["tool_choice"], json!("required"));
        let without_tools =
            OpenAIResponsesProvider::build_request(MODEL, &[user("hi")], None, 100, None, choice());
        assert!(without_tools.get("tool_choice").is_none());
        assert!(without_tools.get("tools").is_none());
    }

    #[test]
    fn max_output_tokens_is_at_least_sixteen() {
        let small =
            OpenAIResponsesProvider::build_request(MODEL, &[user("hi")], None, 3, None, None);
        assert_eq!(small["max_output_tokens"], json!(16));
        assert_eq!(
            request_for(&[user("hi")], None)["max_output_tokens"],
            json!(4096)
        );
    }

    #[test]
    fn requests_are_stateless_ask_for_encrypted_reasoning_and_never_send_temperature() {
        let request = request_for(&[user("hi")], Some("medium"));
        assert_eq!(request["store"], json!(false));
        assert_eq!(request["include"], json!(["reasoning.encrypted_content"]));
        assert_eq!(request["model"], json!(MODEL));
        assert!(request.get("temperature").is_none());
        assert!(request.get("stream").is_none());
    }

    #[test]
    fn the_configured_effort_is_sent_and_adaptive_or_none_means_model_default() {
        assert_eq!(
            request_for(&[user("hi")], Some("xhigh"))["reasoning"],
            json!({ "summary": "auto", "effort": "xhigh" })
        );
        for effort in [None, Some("adaptive"), Some("")] {
            assert_eq!(
                request_for(&[user("hi")], effort)["reasoning"],
                json!({ "summary": "auto" }),
                "{effort:?}"
            );
        }
    }

    #[test]
    fn system_prompts_become_developer_messages_without_cache_control() {
        let input = map_messages_to_input(
            &[
                json!({ "role": "system", "content": "Be brief." }),
                json!({ "role": "system", "content": [
                    { "type": "text", "text": "Cached", "cache_control": { "type": "ephemeral" } },
                    { "type": "text", "text": "Fresh" }
                ]}),
            ],
            MODEL,
        );
        assert_eq!(
            input,
            vec![
                json!({ "role": "developer", "content": [{ "type": "input_text", "text": "Be brief." }] }),
                json!({ "role": "developer", "content": [
                    { "type": "input_text", "text": "Cached" },
                    { "type": "input_text", "text": "Fresh" }
                ]}),
            ]
        );
    }

    #[test]
    fn user_text_and_images_become_input_parts() {
        let input = map_messages_to_input(
            &[json!({ "role": "user", "content": [
                { "type": "text", "text": "what is this?" },
                { "type": "image_url", "image_url": { "url": "data:image/png;base64,AAAA" } },
                { "type": "image_url", "image_url": { "url": "" } }
            ]})],
            MODEL,
        );
        assert_eq!(
            input,
            vec![json!({ "role": "user", "content": [
                { "type": "input_text", "text": "what is this?" },
                { "type": "input_image", "image_url": "data:image/png;base64,AAAA", "detail": "auto" }
            ]})]
        );
    }

    #[test]
    fn tool_results_become_function_call_outputs() {
        let input = map_messages_to_input(
            &[
                json!({ "role": "tool", "tool_call_id": "call_1", "name": "read", "content": "42" }),
                json!({ "role": "tool", "tool_call_id": "call_2", "content": [
                    { "type": "text", "text": "line 1" },
                    { "type": "text", "text": "line 2" }
                ]}),
            ],
            MODEL,
        );
        assert_eq!(
            input,
            vec![
                json!({ "type": "function_call_output", "call_id": "call_1", "output": "42" }),
                json!({ "type": "function_call_output", "call_id": "call_2", "output": "line 1\nline 2" }),
            ]
        );
    }

    #[test]
    fn a_tool_result_with_an_image_keeps_its_parts() {
        let input = map_messages_to_input(
            &[
                json!({ "role": "tool", "tool_call_id": "call_1", "content": [
                    { "type": "text", "text": "screenshot:" },
                    { "type": "image_url", "image_url": { "url": "data:image/png;base64,BBBB" } }
                ]}),
            ],
            MODEL,
        );
        assert_eq!(
            input[0]["output"],
            json!([
                { "type": "input_text", "text": "screenshot:" },
                { "type": "input_image", "image_url": "data:image/png;base64,BBBB", "detail": "auto" }
            ])
        );
    }

    // ── replay ───────────────────────────────────────────────────────────────

    fn reasoning_item(id: &str) -> Value {
        json!({
            "type": "reasoning",
            "id": id,
            "summary": [{ "type": "summary_text", "text": "thinking" }],
            "encrypted_content": format!("enc-{id}")
        })
    }

    fn logged(model: &str, item: Value) -> Value {
        json!({ "type": REPLAY_BLOCK_TYPE, "model": model, "item": item })
    }

    fn tool_call(id: &str, name: &str) -> Value {
        json!({
            "id": id,
            "type": "function",
            "function": { "name": name, "arguments": "{\"location\":\"Lisbon\"}" }
        })
    }

    /// An assistant turn as stored by the runner: text, two tool calls and the log of
    /// reasoning → message → reasoning → function_call → function_call.
    fn logged_assistant_turn(model: &str) -> Value {
        json!({
            "role": "assistant",
            "content": "Checking the weather.",
            "tool_calls": [tool_call("call_a", "get_weather"), tool_call("call_b", "get_weather")],
            "reasoning_content": "thinking",
            "thinking_blocks": [
                logged(model, reasoning_item("rs_1")),
                logged(model, json!({ "type": "message", "id": "msg_1", "phase": "commentary" })),
                logged(model, reasoning_item("rs_2")),
                logged(model, json!({ "type": "function_call", "id": "fc_a", "call_id": "call_a" })),
                logged(model, json!({ "type": "function_call", "id": "fc_b", "call_id": "call_b" })),
            ]
        })
    }

    #[test]
    fn the_same_model_gets_its_items_back_in_order_with_ids() {
        let items = assistant_items(&logged_assistant_turn(MODEL), MODEL);
        assert_eq!(
            items,
            vec![
                reasoning_item("rs_1"),
                json!({
                    "type": "message",
                    "role": "assistant",
                    "id": "msg_1",
                    "status": "completed",
                    "content": [{ "type": "output_text", "text": "Checking the weather.", "annotations": [] }],
                    "phase": "commentary"
                }),
                reasoning_item("rs_2"),
                json!({
                    "type": "function_call",
                    "id": "fc_a",
                    "call_id": "call_a",
                    "name": "get_weather",
                    "arguments": "{\"location\":\"Lisbon\"}"
                }),
                json!({
                    "type": "function_call",
                    "id": "fc_b",
                    "call_id": "call_b",
                    "name": "get_weather",
                    "arguments": "{\"location\":\"Lisbon\"}"
                }),
            ]
        );
    }

    #[test]
    fn another_model_gets_no_reasoning_and_no_item_ids() {
        let items = assistant_items(&logged_assistant_turn("gpt-6-sol"), MODEL);
        assert_eq!(
            items,
            vec![
                json!({ "role": "assistant", "content": "Checking the weather." }),
                json!({
                    "type": "function_call",
                    "call_id": "call_a",
                    "name": "get_weather",
                    "arguments": "{\"location\":\"Lisbon\"}"
                }),
                json!({
                    "type": "function_call",
                    "call_id": "call_b",
                    "name": "get_weather",
                    "arguments": "{\"location\":\"Lisbon\"}"
                }),
            ]
        );
    }

    #[test]
    fn foreign_history_with_anthropic_thinking_is_sent_without_reasoning() {
        let message = json!({
            "role": "assistant",
            "content": "Done.",
            "tool_calls": [tool_call("toolu_1", "get_weather")],
            "thinking_blocks": [{ "type": "thinking", "thinking": "hmm", "signature": "sig" }]
        });
        let items = assistant_items(&message, MODEL);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0], json!({ "role": "assistant", "content": "Done." }));
        assert_eq!(items[1]["call_id"], json!("toolu_1"));
        assert!(items[1].get("id").is_none());
    }

    #[test]
    fn a_log_with_reasoning_missing_its_encrypted_content_is_not_replayed() {
        let mut message = logged_assistant_turn(MODEL);
        message["thinking_blocks"][0]["item"]
            .as_object_mut()
            .unwrap()
            .remove("encrypted_content");
        let items = assistant_items(&message, MODEL);
        assert!(items.iter().all(|item| item_type(item) != "reasoning"));
        assert!(items.iter().all(|item| item.get("id").is_none()));
    }

    #[test]
    fn tool_calls_missing_from_the_log_are_appended_without_id() {
        let mut message = logged_assistant_turn(MODEL);
        message["tool_calls"]
            .as_array_mut()
            .unwrap()
            .push(tool_call("call_c", "get_time"));
        let items = assistant_items(&message, MODEL);
        let last = items.last().unwrap();
        assert_eq!(last["call_id"], json!("call_c"));
        assert!(last.get("id").is_none());
        assert_eq!(items[3]["id"], json!("fc_a"));
    }

    #[test]
    fn logged_calls_that_are_no_longer_in_tool_calls_are_skipped() {
        let mut message = logged_assistant_turn(MODEL);
        message["tool_calls"].as_array_mut().unwrap().truncate(1);
        let items = assistant_items(&message, MODEL);
        let call_ids: Vec<&Value> = items
            .iter()
            .filter(|item| item_type(item) == "function_call")
            .map(|item| &item["call_id"])
            .collect();
        assert_eq!(call_ids, vec![&json!("call_a")]);
    }

    #[test]
    fn message_text_comes_from_content() {
        let mut message = logged_assistant_turn(MODEL);
        message["content"] = json!("Edited text.");
        let items = assistant_items(&message, MODEL);
        assert_eq!(items[1]["content"][0]["text"], json!("Edited text."));
    }

    #[test]
    fn text_without_a_logged_message_goes_before_the_first_function_call() {
        let message = json!({
            "role": "assistant",
            "content": "Late text.",
            "tool_calls": [tool_call("call_a", "get_weather")],
            "thinking_blocks": [
                logged(MODEL, reasoning_item("rs_1")),
                logged(MODEL, json!({ "type": "function_call", "id": "fc_a", "call_id": "call_a" })),
            ]
        });
        let items = assistant_items(&message, MODEL);
        assert_eq!(item_type(&items[0]), "reasoning");
        assert_eq!(
            items[1],
            json!({ "role": "assistant", "content": "Late text." })
        );
        assert_eq!(items[2]["id"], json!("fc_a"));
    }

    #[test]
    fn reasoning_without_a_following_item_is_dropped() {
        // A reply cut off after reasoning, or whose text was stripped to nothing.
        let message = json!({
            "role": "assistant",
            "content": "",
            "thinking_blocks": [
                logged(MODEL, reasoning_item("rs_1")),
                logged(MODEL, json!({ "type": "message", "id": "msg_1" })),
            ]
        });
        assert!(assistant_items(&message, MODEL).is_empty());
    }

    #[test]
    fn a_whole_conversation_maps_in_order() {
        let input = map_messages_to_input(
            &[
                json!({ "role": "system", "content": "sys" }),
                user("weather?"),
                logged_assistant_turn(MODEL),
                json!({ "role": "tool", "tool_call_id": "call_a", "content": "sunny" }),
                json!({ "role": "tool", "tool_call_id": "call_b", "content": "sunny" }),
            ],
            MODEL,
        );
        let kinds: Vec<String> = input
            .iter()
            .map(|item| {
                str_field(item, "type")
                    .or_else(|| str_field(item, "role"))
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                "developer",
                "user",
                "reasoning",
                "message",
                "reasoning",
                "function_call",
                "function_call",
                "function_call_output",
                "function_call_output"
            ]
        );
    }

    // ── response parsing ─────────────────────────────────────────────────────

    fn completed(output: Value) -> Value {
        json!({ "id": "resp_1", "status": "completed", "output": output })
    }

    #[test]
    fn text_and_refusal_parts_make_the_content() {
        let response = parse_response_object(
            &completed(json!([{
                "type": "message",
                "id": "msg_1",
                "status": "completed",
                "content": [
                    { "type": "output_text", "text": "Hello. ", "annotations": [] },
                    { "type": "refusal", "refusal": "I can't do that." }
                ]
            }])),
            MODEL,
        );
        assert_eq!(response.content.as_deref(), Some("Hello. I can't do that."));
        assert_eq!(response.finish_reason, "stop");
        assert!(response.tool_calls.is_empty());
    }

    #[test]
    fn function_calls_keep_their_call_ids_and_log_the_items() {
        let response = parse_response_object(
            &completed(json!([
                reasoning_item("rs_1"),
                {
                    "type": "function_call", "id": "fc_1", "call_id": "call_1",
                    "name": "get_weather", "arguments": "{\"location\":\"Porto\"}", "status": "completed"
                },
                {
                    "type": "function_call", "id": "fc_2", "call_id": "call_2",
                    "name": "get_time", "arguments": "{}", "status": "completed"
                }
            ])),
            MODEL,
        );
        assert_eq!(response.finish_reason, "tool_calls");
        let ids: Vec<&str> = response.tool_calls.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, vec!["call_1", "call_2"]);
        assert_eq!(response.tool_calls[0].arguments["location"], json!("Porto"));
        assert_eq!(response.reasoning_content.as_deref(), Some("thinking"));

        let blocks = response.thinking_blocks.unwrap();
        let items: Vec<&Value> = blocks.iter().map(|block| &block["item"]).collect();
        assert_eq!(items[0], &reasoning_item("rs_1"));
        assert_eq!(
            items[1],
            &json!({ "type": "function_call", "id": "fc_1", "call_id": "call_1" })
        );
        assert!(blocks.iter().all(
            |block| block["model"] == json!(MODEL) && block["type"] == json!(REPLAY_BLOCK_TYPE)
        ));
    }

    #[test]
    fn a_parsed_reply_replays_to_the_same_items() {
        let output = json!([
            reasoning_item("rs_1"),
            {
                "type": "message", "role": "assistant", "id": "msg_1", "phase": "final_answer",
                "status": "completed",
                "content": [{ "type": "output_text", "text": "Sunny.", "annotations": [] }]
            }
        ]);
        let response = parse_response_object(&completed(output.clone()), MODEL);
        let thinking_blocks: Vec<Value> = response
            .thinking_blocks
            .unwrap()
            .into_iter()
            .map(|block| Value::Object(block.into_iter().collect()))
            .collect();
        let message = crate::utils::helpers::build_assistant_message(
            response.content.as_deref(),
            None,
            response.reasoning_content.as_deref(),
            Some(thinking_blocks),
        );
        assert_eq!(
            assistant_items(&message, MODEL),
            output.as_array().unwrap().clone()
        );
    }

    #[test]
    fn malformed_arguments_get_the_parse_error_markers() {
        let response = parse_response_object(
            &completed(json!([{
                "type": "function_call", "id": "fc_1", "call_id": "call_1",
                "name": "get_weather", "arguments": "{\"location\":"
            }])),
            MODEL,
        );
        assert!(
            response.tool_calls[0]
                .arguments
                .contains_key("__args_json_parse_error")
        );
    }

    #[test]
    fn reasoning_summaries_are_joined_and_raw_reasoning_is_the_fallback() {
        let response = parse_response_object(
            &completed(json!([
                {
                    "type": "reasoning", "id": "rs_1",
                    "summary": [
                        { "type": "summary_text", "text": "First." },
                        { "type": "summary_text", "text": "Second." }
                    ]
                },
                {
                    "type": "reasoning", "id": "rs_2", "summary": [],
                    "content": [{ "type": "reasoning_text", "text": "Raw." }]
                }
            ])),
            MODEL,
        );
        assert_eq!(
            response.reasoning_content.as_deref(),
            Some("First.\n\nSecond.\n\nRaw.")
        );
    }

    #[test]
    fn usage_includes_cached_and_reasoning_tokens() {
        let mut body = completed(json!([]));
        body["usage"] = json!({
            "input_tokens": 1000,
            "input_tokens_details": { "cached_tokens": 600 },
            "output_tokens": 300,
            "output_tokens_details": { "reasoning_tokens": 200 },
            "total_tokens": 1300
        });
        let usage = parse_response_object(&body, MODEL).usage;
        assert_eq!(usage.input_tokens, Some(400));
        assert_eq!(usage.cache_read_input_tokens, Some(600));
        assert_eq!(usage.output_tokens, Some(300));
        assert_eq!(usage.reasoning_tokens, Some(200));
        assert_eq!(usage.total_tokens(), Some(1300));
    }

    #[test]
    fn status_maps_to_finish_reason() {
        let with_status = |status: &str, reason: Option<&str>| {
            let mut body = completed(json!([]));
            body["status"] = json!(status);
            if let Some(reason) = reason {
                body["incomplete_details"] = json!({ "reason": reason });
            }
            parse_response_object(&body, MODEL)
        };
        assert_eq!(with_status("completed", None).finish_reason, "stop");
        assert_eq!(
            with_status("incomplete", Some("max_output_tokens")).finish_reason,
            "length"
        );
        let filtered = with_status("incomplete", Some("content_filter"));
        assert_eq!(filtered.finish_reason, "error");
        assert_eq!(
            filtered.content.as_deref(),
            Some("OpenAI response incomplete: content_filter")
        );
        assert_eq!(with_status("incomplete", None).finish_reason, "error");
    }

    #[test]
    fn a_failed_response_reports_its_error_code_and_message() {
        let body = json!({
            "status": "failed",
            "error": { "code": "server_error", "message": "The server had an error." },
            "output": []
        });
        let response = parse_response_object(&body, MODEL);
        assert_eq!(response.finish_reason, "error");
        assert_eq!(
            response.content.as_deref(),
            Some("server_error: The server had an error.")
        );
        assert!(OpenAIResponsesProvider::is_transient_error(
            response.content.as_deref()
        ));
    }

    #[test]
    fn unfinished_function_calls_are_not_returned() {
        let mut body = completed(json!([{
            "type": "function_call", "id": "fc_1", "call_id": "call_1",
            "name": "get_weather", "arguments": "{\"loc", "status": "incomplete"
        }]));
        body["status"] = json!("incomplete");
        body["incomplete_details"] = json!({ "reason": "max_output_tokens" });
        let response = parse_response_object(&body, MODEL);
        assert_eq!(response.finish_reason, "length");
        assert!(response.tool_calls.is_empty());
    }

    // ── HTTP ─────────────────────────────────────────────────────────────────

    /// Serve `raw_response` once on a local port and return the base URL.
    async fn serve_once(raw_response: &'static [u8]) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0u8; 16384];
            let _ = socket.read(&mut request).await;
            socket.write_all(raw_response).await.unwrap();
        });
        format!("http://{address}/v1")
    }

    fn provider_at(api_base: String) -> OpenAIResponsesProvider {
        OpenAIResponsesProvider::new(
            Some("test-key".to_string()),
            Some(api_base),
            Some(MODEL.to_string()),
            None,
            None,
        )
    }

    async fn chat_once(provider: &OpenAIResponsesProvider) -> LLMResponse {
        provider
            .chat(vec![user("hi")], None, None, 100, Some(0.2), None, None)
            .await
    }

    async fn stream_once(provider: &OpenAIResponsesProvider) -> LLMResponse {
        let no_callback: Option<fn(String) -> std::future::Ready<()>> = None;
        provider
            .chat_stream(
                vec![user("hi")],
                None,
                None,
                100,
                None,
                None,
                None,
                &no_callback,
                &None,
            )
            .await
    }

    #[tokio::test]
    async fn chat_parses_a_response_body() {
        let base = serve_once(
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n\
{\"status\":\"completed\",\"output\":[{\"type\":\"message\",\"id\":\"msg_1\",\"content\":[{\"type\":\"output_text\",\"text\":\"pong\"}]}]}",
        )
        .await;
        let response = chat_once(&provider_at(base)).await;
        assert_eq!(response.finish_reason, "stop");
        assert_eq!(response.content.as_deref(), Some("pong"));
    }

    #[tokio::test]
    async fn an_error_json_body_reports_the_api_message() {
        let base = serve_once(
            b"HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n\
{\"error\":{\"message\":\"Unsupported parameter: 'temperature'\",\"type\":\"invalid_request_error\"}}",
        )
        .await;
        let provider = provider_at(base);
        let response = chat_once(&provider).await;
        assert_eq!(response.finish_reason, "error");
        assert_eq!(
            response.content.as_deref(),
            Some("HTTP 400 Bad Request: Unsupported parameter: 'temperature'")
        );
    }

    #[tokio::test]
    async fn a_streamed_request_with_an_error_page_reports_the_status() {
        let base = serve_once(
            b"HTTP/1.1 502 Bad Gateway\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n<html>bad gateway</html>",
        )
        .await;
        let response = stream_once(&provider_at(base)).await;
        assert_eq!(response.finish_reason, "error");
        assert_eq!(
            response.content.as_deref(),
            Some("HTTP 502 Bad Gateway: <html>bad gateway</html>")
        );
        assert!(OpenAIResponsesProvider::is_transient_error(
            response.content.as_deref()
        ));
    }

    #[tokio::test]
    async fn a_streamed_request_is_parsed_from_its_events() {
        let base = serve_once(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n\
data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_1\",\"content\":[]}}\n\n\
data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"pong\"}\n\n\
data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[{\"type\":\"message\",\"id\":\"msg_1\",\"content\":[{\"type\":\"output_text\",\"text\":\"pong\"}]}]}}\n\n",
        )
        .await;
        let response = stream_once(&provider_at(base)).await;
        assert_eq!(response.finish_reason, "stop");
        assert_eq!(response.content.as_deref(), Some("pong"));
    }

    #[test]
    fn the_endpoint_defaults_to_api_openai_com() {
        let provider = OpenAIResponsesProvider::new(Some("k".into()), None, None, None, None);
        assert_eq!(
            provider.responses_url,
            "https://api.openai.com/v1/responses"
        );
        assert_eq!(provider.get_default_model(), "gpt-6-luna");
        let custom = provider_at("https://api.openai.com/v1/".to_string());
        assert_eq!(custom.responses_url, "https://api.openai.com/v1/responses");
    }
}
