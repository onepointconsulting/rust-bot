use std::collections::HashMap;
use std::env;
use std::time::Duration;

use crate::providers::{
    base::{GenerationSettings, LLMProvider, LLMResponse, LLMUsage, ToolCallRequest},
    registry::ProviderSpec,
};
use adk_anthropic::{
    AccumulatingStream, Anthropic, ContentBlockDelta, Error as AdkError, Message,
    MessageStreamEvent,
};
use futures::StreamExt;
use rand::seq::IndexedRandom;
use regex::Regex;
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use serde_json::json;
use std::sync::OnceLock;
use tokio::time::timeout;

const ALNUM: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";

/// Upper bound on the SSE byte buffer; guards against a stream that never emits
/// a complete `\n\n`-terminated event from growing memory without bound.
const MAX_SSE_BUFFER_BYTES: usize = 1024 * 1024;

/// Claude 5 models reject legacy `temperature` and `thinking.type.enabled`.
const CLAUDE_5_MODEL_PREFIXES: &[&str] = &["claude-sonnet-5", "claude-opus-5", "claude-haiku-5"];

fn gen_tool_id() -> String {
    let mut rng = rand::rng();
    let suffix: String = (0..22)
        .map(|_| *ALNUM.choose(&mut rng).unwrap() as char)
        .collect();
    format!("toolu_{suffix}")
}

pub struct AnthropicProvider {
    api_key: Option<String>,
    api_base: Option<String>,
    default_model: Option<String>,
    extra_headers: HashMap<String, String>,
    spec: Option<ProviderSpec>,
    generation: GenerationSettings,
    /// Raw HTTP client with Anthropic defaults plus merged `extra_headers`, used for all requests.
    http_client: reqwest::Client,
    messages_url: String,
}

impl AnthropicProvider {
    pub const DEFAULT_MODEL: &str = "claude-sonnet-4-6";
    pub const DEFAULT_API_BASE: &str = "https://api.anthropic.com";
    const ANTHROPIC_API_VERSION: &str = "2023-06-01";

    fn build_request_headers(api_key: &str, extra_headers: &HashMap<String, String>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        headers.insert(
            HeaderName::from_static("x-api-key"),
            HeaderValue::from_str(api_key).unwrap_or_else(|e| {
                log::error!("Invalid Anthropic API key header value: {e}");
                panic!("Invalid Anthropic API key header value: {e}");
            }),
        );
        headers.insert(
            HeaderName::from_static("anthropic-version"),
            HeaderValue::from_static(Self::ANTHROPIC_API_VERSION),
        );

        for (key, value) in extra_headers {
            let header_name = HeaderName::from_bytes(key.as_bytes()).unwrap_or_else(|e| {
                log::error!("Invalid HTTP header name '{key}': {e}");
                panic!("Invalid HTTP header name '{key}': {e}");
            });
            let header_value = HeaderValue::from_str(value).unwrap_or_else(|e| {
                log::error!("Invalid HTTP header value for '{key}': {e}");
                panic!("Invalid HTTP header value for '{key}': {e}");
            });
            headers.insert(header_name, header_value);
        }

        headers
    }

    fn build_http_client(
        api_key: &str,
        extra_headers: &HashMap<String, String>,
    ) -> reqwest::Client {
        reqwest::Client::builder()
            .default_headers(Self::build_request_headers(api_key, extra_headers))
            .build()
            .unwrap_or_else(|e| {
                log::error!("Failed to build Anthropic HTTP client: {e}");
                panic!("Failed to build Anthropic HTTP client: {e}");
            })
    }

    fn strip_prefix(model: &str) -> String {
        model
            .strip_prefix("anthropic/")
            .unwrap_or(model)
            .to_string()
    }

    fn convert_tool_choice(
        tool_choice: Option<serde_json::Value>,
        thinking_enabled: bool,
    ) -> Option<serde_json::Value> {
        if thinking_enabled {
            return Some(json!({ "type": "auto" }));
        }

        let Some(tc) = tool_choice else {
            return Some(json!({ "type": "auto" }));
        };

        if tc.as_str() == Some("auto") {
            return Some(json!({ "type": "auto" }));
        }

        if tc.as_str() == Some("required") {
            return Some(json!({ "type": "any" }));
        }

        if tc.as_str() == Some("none") {
            return None;
        }

        if let Some(name) = tc
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(|n| n.as_str())
        {
            return Some(json!({ "type": "tool", "name": name }));
        }

        Some(json!({ "type": "auto" }))
    }

    /// Convert the application's de-facto OpenAI message format to native Anthropic Messages API format.
    /// Returns `(system, messages)`: the extracted `system` prompt and the remaining user/assistant turns.
    fn convert_messages(
        &self,
        messages: Vec<serde_json::Value>,
    ) -> (serde_json::Value, Vec<serde_json::Value>) {
        let mut system: serde_json::Value = serde_json::Value::String("".to_string());
        let mut raw: Vec<serde_json::Value> = Vec::new();

        for msg in messages {
            let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
            let content = msg.get("content");
            if role == "system" {
                system = match content {
                    Some(v) if v.is_string() || v.is_array() => v.clone(),
                    Some(v) if v.is_null() => serde_json::Value::String(String::new()),
                    Some(v) => serde_json::Value::String(v.to_string()),
                    None => serde_json::Value::String(String::new()),
                };
                continue;
            }
            if role == "tool" {
                let block = Self::tool_result_block(&msg);
                if let Some(last_msg) = raw.last_mut() {
                    if last_msg.get("role").and_then(|v| v.as_str()) == Some("user") {
                        if let Some(content) = last_msg.get_mut("content") {
                            if let Some(content_array) = content.as_array_mut() {
                                content_array.push(block);
                            } else {
                                let prev_c = content.as_str().unwrap_or("").to_string();
                                *content = json!([
                                    { "type": "text", "text": prev_c },
                                    block,
                                ]);
                            }
                        }
                    } else {
                        raw.push(serde_json::json!({
                            "role": "user",
                            "content": [block],
                        }));
                    }
                }
                continue;
            }

            if role == "assistant" {
                raw.push(json!({
                    "role": "assistant",
                    "content": Self::assistant_blocks(&msg),
                }));
                continue;
            }

            if role == "user" {
                raw.push(json!({
                    "role": "user",
                    "content": Self::convert_user_content(&msg),
                }));
                continue;
            }
        }
        return (system, AnthropicProvider::merge_consecutive(raw));
    }

    fn convert_tools(tools: Option<Vec<serde_json::Value>>) -> Option<Vec<serde_json::Value>> {
        let tools = tools?;
        if tools.is_empty() {
            return None;
        }

        let default_schema = json!({"type": "object", "properties": {}});
        let mut result = Vec::new();
        for tool in tools {
            let func = tool.get("function").unwrap_or(&tool);
            let mut entry = json!({
                "name": func.get("name").and_then(|v| v.as_str()).unwrap_or(""),
                "input_schema": func
                    .get("parameters")
                    .cloned()
                    .unwrap_or_else(|| default_schema.clone()),
            });
            if let Some(desc) = func
                .get("description")
                .and_then(|d| d.as_str())
                .filter(|s| !s.is_empty())
            {
                entry["description"] = serde_json::Value::String(desc.to_string());
            }
            if tool.get("cache_control").is_some() {
                entry["cache_control"] = tool["cache_control"].clone();
            }
            result.push(entry);
        }
        Some(result)
    }

    /// Anthropic requires alternating user/assistant roles.
    fn merge_consecutive(messages: Vec<serde_json::Value>) -> Vec<serde_json::Value> {
        let mut merged: Vec<serde_json::Value> = Vec::new();
        for msg in messages {
            if let Some(last) = merged.last_mut()
                && last.get("role") == msg.get("role")
            {
                let mut prev_c = last.get("content").cloned().unwrap_or(json!([]));
                let mut cur_c = msg.get("content").cloned().unwrap_or(json!([]));

                if prev_c.is_string() {
                    prev_c = json!([{
                        "type": "text",
                        "text": prev_c.as_str().unwrap_or(""),
                    }]);
                }
                if cur_c.is_string() {
                    cur_c = json!([{
                        "type": "text",
                        "text": cur_c.as_str().unwrap_or(""),
                    }]);
                }
                if cur_c.is_array() {
                    if let (Some(prev_arr), Some(cur_arr)) =
                        (prev_c.as_array_mut(), cur_c.as_array())
                    {
                        prev_arr.extend(cur_arr.iter().cloned());
                    }
                }
                last["content"] = prev_c;
            } else {
                merged.push(msg);
            }
        }
        merged
    }

    fn tool_result_block(msg: &serde_json::Value) -> serde_json::Value {
        let content = msg.get("content");
        let tool_use_id = msg
            .get("tool_call_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let block_content = match content {
            Some(v) if v.is_string() || v.is_array() => v.clone(),
            Some(v) => serde_json::Value::String(v.to_string()),
            None => serde_json::Value::String(String::new()),
        };

        json!({
            "type": "tool_result",
            "tool_use_id": tool_use_id,
            "content": block_content,
        })
    }

    fn parse_tool_input(arguments: &serde_json::Value) -> serde_json::Value {
        if let Some(s) = arguments.as_str() {
            serde_json::from_str(s).unwrap_or_else(|_| json!({}))
        } else if arguments.is_object() {
            arguments.clone()
        } else {
            json!({})
        }
    }

    fn assistant_blocks(msg: &serde_json::Value) -> serde_json::Value {
        let mut blocks: Vec<serde_json::Value> = Vec::new();
        let content = msg.get("content");

        if let Some(thinking_blocks) = msg.get("thinking_blocks").and_then(|v| v.as_array()) {
            for tb in thinking_blocks {
                if tb.get("type").and_then(|v| v.as_str()) == Some("thinking") {
                    blocks.push(json!({
                        "type": "thinking",
                        "thinking": tb.get("thinking").and_then(|v| v.as_str()).unwrap_or(""),
                        "signature": tb.get("signature").and_then(|v| v.as_str()).unwrap_or(""),
                    }));
                }
            }
        }

        match content {
            Some(v) if v.is_string() => {
                if let Some(text) = v.as_str() {
                    if !text.is_empty() {
                        blocks.push(json!({ "type": "text", "text": text }));
                    }
                }
            }
            Some(v) if v.is_array() => {
                for item in v.as_array().unwrap() {
                    if item.is_object() {
                        blocks.push(item.clone());
                    } else {
                        blocks.push(json!({ "type": "text", "text": item.to_string() }));
                    }
                }
            }
            _ => {}
        }

        if let Some(tool_calls) = msg.get("tool_calls").and_then(|v| v.as_array()) {
            for tc in tool_calls {
                let Some(tc) = tc.as_object() else {
                    continue;
                };
                let func = tc.get("function");
                let args = func
                    .and_then(|f| f.get("arguments"))
                    .map(Self::parse_tool_input)
                    .unwrap_or_else(|| json!({}));
                let id = tc
                    .get("id")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(gen_tool_id);
                blocks.push(json!({
                    "type": "tool_use",
                    "id": id,
                    "name": func
                        .and_then(|f| f.get("name"))
                        .and_then(|v| v.as_str())
                        .unwrap_or(""),
                    "input": args,
                }));
            }
        }

        if blocks.is_empty() {
            json!([{ "type": "text", "text": "" }])
        } else {
            serde_json::Value::Array(blocks)
        }
    }

    fn convert_user_content(msg: &serde_json::Value) -> serde_json::Value {
        let content = msg.get("content");

        match content {
            None => json!("(empty)"),
            Some(v) if v.is_null() => json!("(empty)"),
            Some(v) if v.is_string() => {
                let text = v.as_str().unwrap_or("");
                if text.is_empty() {
                    json!("(empty)")
                } else {
                    v.clone()
                }
            }
            Some(v) if v.is_array() => {
                let mut result: Vec<serde_json::Value> = Vec::new();
                for item in v.as_array().unwrap() {
                    if let Some(obj) = item.as_object() {
                        if obj.get("type").and_then(|t| t.as_str()) == Some("image_url") {
                            if let Some(converted) = Self::convert_image_block(item) {
                                result.push(converted);
                            }
                            continue;
                        }
                        result.push(item.clone());
                    } else {
                        result.push(json!({ "type": "text", "text": item.to_string() }));
                    }
                }
                if result.is_empty() {
                    json!("(empty)")
                } else {
                    serde_json::Value::Array(result)
                }
            }
            Some(v) => serde_json::Value::String(v.to_string()),
        }
    }

    fn convert_image_block(block: &serde_json::Value) -> Option<serde_json::Value> {
        let url = block
            .get("image_url")
            .and_then(|iu| {
                iu.get("url")
                    .and_then(|u| u.as_str())
                    .or_else(|| iu.as_str())
            })
            .unwrap_or("");
        if url.is_empty() {
            return None;
        }

        static DATA_IMAGE_RE: OnceLock<Regex> = OnceLock::new();
        let re = DATA_IMAGE_RE
            .get_or_init(|| Regex::new(r"(?s)^data:(image/\w+);base64,(.+)$").unwrap());
        if let Some(caps) = re.captures(url) {
            return Some(json!({
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": caps.get(1).unwrap().as_str(),
                    "data": caps.get(2).unwrap().as_str(),
                }
            }));
        }

        Some(json!({
            "type": "image",
            "source": {
                "type": "url",
                "url": url,
            }
        }))
    }

    fn build_args(
        &self,
        messages: &[serde_json::Value],
        tools: Option<Vec<serde_json::Value>>,
        model: Option<String>,
        max_tokens: usize,
        temperature: Option<f32>,
        reasoning_effort: Option<String>,
        tool_choice: Option<serde_json::Value>,
        supports_caching: bool,
    ) -> HashMap<String, serde_json::Value> {
        let model_name =
            AnthropicProvider::strip_prefix(&model.unwrap_or_else(|| self.get_default_model()));
        let (mut system, mut anthropic_msgs) =
            self.convert_messages(AnthropicProvider::sanitize_empty_content(messages));
        let mut anthropic_tools = Self::convert_tools(tools);

        if supports_caching {
            (system, anthropic_msgs, anthropic_tools) =
                self.apply_cache_control(system, anthropic_msgs, anthropic_tools);
        }
        let max_tokens = std::cmp::max(1, max_tokens);
        let thinking_enabled = if let Some(reasoning_effort) = reasoning_effort.clone() {
            !reasoning_effort.is_empty()
        } else {
            false
        };

        let mut args = HashMap::new();
        args.insert(
            "model".to_string(),
            serde_json::Value::String(model_name.clone()),
        );
        args.insert(
            "messages".to_string(),
            serde_json::Value::Array(anthropic_msgs),
        );
        args.insert(
            "max_tokens".to_string(),
            serde_json::Value::Number(serde_json::Number::from(max_tokens as u32)),
        );
        log::debug!("anthropic max_tokens: {}", max_tokens);

        let system_is_truthy = match &system {
            serde_json::Value::String(s) => !s.is_empty(),
            serde_json::Value::Array(a) => !a.is_empty(),
            _ => false,
        };
        if system_is_truthy {
            args.insert("system".to_string(), system);
        }

        if Self::uses_adaptive_thinking(&model_name) {
            if thinking_enabled {
                args.insert(
                    "thinking".to_string(),
                    serde_json::json!({"type": "adaptive"}),
                );
                if let Some(reasoning_effort) = reasoning_effort.as_ref()
                    && reasoning_effort != "adaptive"
                {
                    args.insert(
                        "output_config".to_string(),
                        serde_json::json!({"effort": reasoning_effort}),
                    );
                }
            } else if let Some(temperature) = temperature
                && Self::supports_temperature(&model_name)
            {
                args.insert("temperature".to_string(), serde_json::json!(temperature));
            }
        } else if let Some(reasoning_effort) = reasoning_effort.clone()
            && reasoning_effort == "adaptive"
        {
            args.insert(
                "thinking".to_string(),
                serde_json::json!({"type": "adaptive"}),
            );
            if Self::supports_temperature(&model_name) {
                args.insert("temperature".to_string(), serde_json::json!(1.0));
            }
        } else if let Some(reasoning_effort) = reasoning_effort
            && thinking_enabled
        {
            let budget_map = serde_json::json!({"low": 1024, "medium": 4096, "high": std::cmp::max(8192, max_tokens)});
            let budget_default = serde_json::Value::Number(serde_json::Number::from(4096u32));
            let budget = budget_map
                .get(reasoning_effort.to_lowercase().as_str())
                .unwrap_or(&budget_default);
            let budget_tokens = budget.as_u64().unwrap_or(4096) as usize;
            args.insert(
                "thinking".to_string(),
                serde_json::json!({"type": "enabled", "budget_tokens": budget}),
            );
            args.insert(
                "max_tokens".to_string(),
                serde_json::Value::Number(serde_json::Number::from(std::cmp::max(
                    max_tokens,
                    budget_tokens + 4096,
                ) as u32)),
            );
            if Self::supports_temperature(&model_name) {
                args.insert("temperature".to_string(), serde_json::json!(1.0));
            }
        } else if let Some(temperature) = temperature
            && Self::supports_temperature(&model_name)
        {
            args.insert("temperature".to_string(), serde_json::json!(temperature));
        }

        if let Some(anthropic_tools) = anthropic_tools
            && !anthropic_tools.is_empty()
        {
            args.insert(
                "tools".to_string(),
                serde_json::Value::Array(anthropic_tools),
            );
            let tc = Self::convert_tool_choice(tool_choice, thinking_enabled);
            if let Some(tc) = tc {
                args.insert("tool_choice".to_string(), tc);
            }
        }

        if !self.extra_headers.is_empty() {
            args.insert(
                "extra_headers".to_string(),
                serde_json::Value::Object(
                    self.extra_headers
                        .clone()
                        .into_iter()
                        .map(|(k, v)| (k, serde_json::Value::String(v)))
                        .collect(),
                ),
            );
        }

        args
    }

    fn is_claude_5_family(model: &str) -> bool {
        CLAUDE_5_MODEL_PREFIXES
            .iter()
            .any(|prefix| model.starts_with(prefix))
    }

    fn supports_temperature(model: &str) -> bool {
        !Self::is_claude_5_family(model)
    }

    fn uses_adaptive_thinking(model: &str) -> bool {
        Self::is_claude_5_family(model)
    }

    fn format_api_error(status: u16, body: &str) -> String {
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(body) {
            if let Some(message) = json
                .get("error")
                .and_then(|error| error.get("message"))
                .and_then(|message| message.as_str())
            {
                return format!("Anthropic API error ({status}): {message}");
            }
        }

        let trimmed = body.trim();
        if trimmed.is_empty() {
            format!("Anthropic API error ({status})")
        } else {
            format!("Anthropic API error ({status}): {trimmed}")
        }
    }

    fn apply_cache_control(
        &self,
        system_param: serde_json::Value,
        messages: Vec<serde_json::Value>,
        tools: Option<Vec<serde_json::Value>>,
    ) -> (
        serde_json::Value,
        Vec<serde_json::Value>,
        Option<Vec<serde_json::Value>>,
    ) {
        let marker = serde_json::json!({"type": "ephemeral"});

        let system = if system_param.is_string() && !system_param.as_str().unwrap_or("").is_empty()
        {
            serde_json::json!([{
                "type": "text",
                "text": system_param.as_str().unwrap_or(""),
                "cache_control": marker.clone(),
            }])
        } else if let Some(system) = system_param.as_array() {
            let mut system = system.clone();
            if let Some(last) = system.last_mut() {
                last["cache_control"] = marker.clone();
            }
            serde_json::Value::Array(system)
        } else {
            system_param
        };

        let mut new_msgs = messages;
        let new_msgs_len = new_msgs.len();
        if new_msgs_len >= 3 {
            let c = new_msgs[new_msgs_len - 2].get("content").cloned();
            if let Some(c) = c {
                if c.is_string() {
                    new_msgs[new_msgs_len - 2]["content"] = serde_json::json!([
                        {"type": "text", "text": c, "cache_control": marker}
                    ]);
                } else if let Some(c) = c.as_array()
                    && !c.is_empty()
                {
                    let mut nc = c.clone();
                    let last_option = nc.last_mut();
                    if let Some(last) = last_option {
                        last["cache_control"] = marker.clone();
                    }
                    new_msgs[new_msgs_len - 2]["content"] = serde_json::Value::Array(nc);
                }
            }
        }

        let new_tools = match tools {
            None => None,
            Some(tools) if tools.is_empty() => Some(tools),
            Some(tools) => {
                let mut new_tools = tools.clone();
                for idx in Self::tool_cache_marker_indices(Some(tools)).unwrap_or_default() {
                    new_tools[idx]["cache_control"] = marker.clone();
                }
                Some(new_tools)
            }
        };
        (system, new_msgs, new_tools)
    }

    fn parse_usage(usage: &serde_json::Value) -> LLMUsage {
        let input_tokens = usage
            .get("input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;
        let cache_creation = usage
            .get("cache_creation_input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;
        let cache_read = usage
            .get("cache_read_input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;
        let output_tokens = usage
            .get("output_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;

        let mut usage = LLMUsage::new();
        usage.input_tokens = Some(input_tokens);
        usage.output_tokens = Some(output_tokens);
        if cache_creation > 0 {
            usage.cache_creation_input_tokens = Some(cache_creation);
        }
        if cache_read > 0 {
            usage.cache_read_input_tokens = Some(cache_read);
        }
        usage
    }

    fn parse_json_response(content: serde_json::Value) -> LLMResponse {
        let mut content_parts: Vec<String> = Vec::new();
        let mut tool_calls: Vec<ToolCallRequest> = Vec::new();
        let mut thinking_blocks: Vec<HashMap<String, serde_json::Value>> = Vec::new();

        if let Some(blocks) = content.get("content").and_then(|c| c.as_array()) {
            for block in blocks {
                let block_type = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
                if block_type == "text" {
                    let text = block.get("text").and_then(|t| t.as_str()).unwrap_or("");
                    content_parts.push(text.to_string());
                } else if block_type == "tool_use" {
                    let block_id = block.get("id").and_then(|t| t.as_str()).unwrap_or("");
                    let block_name = block.get("name").and_then(|t| t.as_str()).unwrap_or("");
                    let default_input = serde_json::Map::new();
                    let block_input = block
                        .get("input")
                        .and_then(|t| t.as_object())
                        .unwrap_or_else(|| &default_input);
                    tool_calls.push(ToolCallRequest {
                        id: block_id.to_string(),
                        name: block_name.to_string(),
                        arguments: block_input.clone().into_iter().collect(),
                        extra_content: None,
                        provider_specific_fields: None,
                        function_provider_specific_fields: None,
                    });
                } else if block_type == "thinking" {
                    let signature = block
                        .get("signature")
                        .and_then(|t| t.as_str())
                        .unwrap_or("");
                    let mut value = HashMap::new();
                    value.insert(
                        "type".to_string(),
                        serde_json::Value::String("thinking".to_string()),
                    );
                    value.insert(
                        "thinking".to_string(),
                        serde_json::Value::String(
                            block
                                .get("thinking")
                                .and_then(|t| t.as_str())
                                .unwrap_or("")
                                .to_string(),
                        ),
                    );
                    value.insert(
                        "signature".to_string(),
                        serde_json::Value::String(signature.to_string()),
                    );
                    thinking_blocks.push(value);
                }
            }
        }

        let stop_map = serde_json::json!({"tool_use": "tool_calls", "end_turn": "stop", "max_tokens": "length"});
        let default_stop = serde_json::Value::String("stop".to_string());
        let finish_reason = stop_map
            .get(
                content
                    .get("stop_reason")
                    .and_then(|t| t.as_str())
                    .unwrap_or(""),
            )
            .unwrap_or(&default_stop)
            .as_str()
            .unwrap_or("stop");

        let usage = content
            .get("usage")
            .map(Self::parse_usage)
            .unwrap_or_else(LLMUsage::new);

        let joined = content_parts.join("");
        LLMResponse {
            content: if joined.is_empty() {
                None
            } else {
                Some(joined)
            },
            finish_reason: finish_reason.to_string(),
            tool_calls,
            usage,
            reasoning_content: None,
            thinking_blocks: if thinking_blocks.is_empty() {
                None
            } else {
                Some(thinking_blocks)
            },
        }
    }

    async fn parse_response(response: Result<reqwest::Response, reqwest::Error>) -> LLMResponse {
        match response {
            Ok(response) => {
                let status = response.status();
                if !status.is_success() {
                    let body = response.text().await.unwrap_or_default();
                    log::error!(
                        "Anthropic API request failed ({}): {}",
                        status.as_u16(),
                        body.chars().take(500).collect::<String>()
                    );
                    return Self::handle_error(Self::format_api_error(status.as_u16(), &body));
                }
                match response.json::<serde_json::Value>().await {
                    Ok(content) => Self::parse_json_response(content),
                    Err(e) => Self::handle_error(format!(
                        "Anthropic API response parse error ({}): {e}",
                        e.status()
                            .unwrap_or(http::StatusCode::INTERNAL_SERVER_ERROR)
                            .as_u16()
                    )),
                }
            }
            Err(e) => Self::handle_error(format!(
                "Anthropic API transport error ({}): {e}",
                e.status()
                    .unwrap_or(http::StatusCode::INTERNAL_SERVER_ERROR)
                    .as_u16()
            )),
        }
    }

    fn handle_error(message: impl ToString) -> LLMResponse {
        LLMResponse {
            content: Some(message.to_string()),
            finish_reason: "error".to_string(),
            tool_calls: Vec::new(),
            usage: LLMUsage::new(),
            reasoning_content: None,
            thinking_blocks: None,
        }
    }

    fn stream_stalled_error(idle_timeout_s: u64) -> LLMResponse {
        Self::handle_error(format!(
            "Error calling LLM: stream stalled for more than {idle_timeout_s} seconds"
        ))
    }

    fn parse_message(message: Message) -> LLMResponse {
        match serde_json::to_value(message) {
            Ok(value) => Self::parse_json_response(value),
            Err(e) => Self::handle_error(format!("Failed to serialize Anthropic message: {e}")),
        }
    }

    fn take_sse_message_event(
        buffer: &mut Vec<u8>,
    ) -> Option<Result<MessageStreamEvent, AdkError>> {
        let split_pos = buffer.windows(2).position(|window| window == b"\n\n")?;
        // Decode only the complete event block; the `\n\n` boundary falls on
        // whole bytes, so no multi-byte UTF-8 sequence is split here.
        let raw = String::from_utf8_lossy(&buffer[..split_pos]).into_owned();
        buffer.drain(..split_pos + 2);
        Self::parse_sse_event(&raw)
    }

    fn parse_sse_event(raw: &str) -> Option<Result<MessageStreamEvent, AdkError>> {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed == "event: ping" {
            return Some(Ok(MessageStreamEvent::Ping));
        }

        let data = raw
            .lines()
            .filter_map(|line| line.strip_prefix("data:").map(str::trim))
            .collect::<Vec<_>>()
            .join("\n");
        if data.is_empty() {
            return Some(Ok(MessageStreamEvent::Ping));
        }

        Some(serde_json::from_str::<MessageStreamEvent>(&data).map_err(AdkError::from))
    }

    fn sse_event_stream<S, B>(
        byte_stream: S,
    ) -> impl futures::Stream<Item = Result<MessageStreamEvent, AdkError>> + Send
    where
        S: futures::Stream<Item = Result<B, reqwest::Error>> + Send + Unpin + 'static,
        B: AsRef<[u8]>,
    {
        futures::stream::unfold(
            (byte_stream, Vec::<u8>::new()),
            |(mut byte_stream, mut buffer)| async move {
                loop {
                    if let Some(result) = Self::take_sse_message_event(&mut buffer) {
                        return Some((result, (byte_stream, buffer)));
                    }

                    if buffer.len() > MAX_SSE_BUFFER_BYTES {
                        buffer.clear();
                        return Some((
                            Err(AdkError::streaming(
                                format!(
                                    "SSE buffer exceeded {MAX_SSE_BUFFER_BYTES} bytes without a complete event"
                                ),
                                None::<Box<dyn std::error::Error + Send + Sync>>,
                            )),
                            (byte_stream, buffer),
                        ));
                    }

                    match byte_stream.next().await {
                        None => {
                            if buffer.iter().all(u8::is_ascii_whitespace) {
                                return None;
                            }
                            // Flush a trailing event that was not terminated by `\n\n`.
                            buffer.extend_from_slice(b"\n\n");
                            return Self::take_sse_message_event(&mut buffer)
                                .map(|result| (result, (byte_stream, buffer)));
                        }
                        Some(Ok(chunk)) => {
                            buffer.extend_from_slice(chunk.as_ref());
                        }
                        Some(Err(e)) => {
                            return Some((
                                Err(AdkError::streaming(
                                    format!("Error in HTTP stream: {e}"),
                                    Some(Box::new(e)),
                                )),
                                (byte_stream, buffer),
                            ));
                        }
                    }
                }
            },
        )
    }

    async fn consume_stream<F, Fut>(
        mut acc_stream: AccumulatingStream,
        message_rx: tokio::sync::oneshot::Receiver<Result<Message, AdkError>>,
        idle_timeout: Duration,
        idle_timeout_s: u64,
        on_content_delta: &Option<F>,
    ) -> LLMResponse
    where
        F: Fn(String) -> Fut + Send + Sync,
        Fut: std::future::Future<Output = ()> + Send,
    {
        while let Some(item) = match timeout(idle_timeout, acc_stream.next()).await {
            Ok(item) => item,
            Err(_) => return Self::stream_stalled_error(idle_timeout_s),
        } {
            match item {
                Err(e) => return Self::handle_error(e.to_string()),
                Ok(event) => {
                    if let Some(cb) = on_content_delta.as_ref()
                        && let MessageStreamEvent::ContentBlockDelta(delta) = event
                        && let ContentBlockDelta::TextDelta(text_delta) = delta.delta
                    {
                        log::info!("Received content delta: {}", text_delta.text);
                        cb(text_delta.text).await;
                    }
                }
            }
        }

        match timeout(idle_timeout, message_rx).await {
            Err(_) => Self::stream_stalled_error(idle_timeout_s),
            Ok(Ok(Ok(message))) => Self::parse_message(message),
            Ok(Ok(Err(e))) => Self::handle_error(e.to_string()),
            Ok(Err(_)) => Self::handle_error("Anthropic stream ended without a final message"),
        }
    }
}

impl LLMProvider for AnthropicProvider {
    fn new(
        api_key: Option<String>,
        api_base: Option<String>,
        default_model: Option<String>,
        extra_headers: Option<HashMap<String, String>>,
        spec: Option<ProviderSpec>,
    ) -> Self {
        let extra_headers = extra_headers.unwrap_or_default();
        let effective_base = api_base
            .clone()
            .unwrap_or_else(|| Self::DEFAULT_API_BASE.to_string());

        // `Anthropic::new` only resolves the API key here (env-var fallback and
        // `file://` indirection); the client itself is not retained because all
        // requests go through `http_client`, which carries `extra_headers`.
        let resolved_key = Anthropic::new(api_key.clone())
            .expect("Failed to create Anthropic client")
            .api_key()
            .to_string();
        let http_client = Self::build_http_client(&resolved_key, &extra_headers);
        let messages_url = format!("{}/v1/messages", effective_base.trim_end_matches('/'));

        Self {
            api_key,
            api_base: Some(effective_base),
            default_model: Some(default_model.unwrap_or_else(|| Self::DEFAULT_MODEL.to_string())),
            extra_headers,
            spec,
            generation: GenerationSettings::new(),
            http_client,
            messages_url,
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

    async fn chat(
        &self,
        messages: Vec<serde_json::Value>,
        tools: Option<Vec<serde_json::Value>>,
        model: Option<String>,
        max_tokens: usize,
        temperature: Option<f32>,
        reasoning_effort: Option<String>,
        tool_choice: Option<serde_json::Value>,
    ) -> LLMResponse {
        let args = self.build_args(
            &messages,
            tools,
            model,
            max_tokens,
            temperature,
            reasoning_effort,
            tool_choice,
            true,
        );
        let mut body = args;
        body.remove("extra_headers");
        let response = self
            .http_client
            .post(&self.messages_url)
            .json(&body)
            .send()
            .await;
        Self::parse_response(response).await
    }

    async fn chat_stream<F, Fut>(
        &self,
        messages: Vec<serde_json::Value>,
        tools: Option<Vec<serde_json::Value>>,
        model: Option<String>,
        max_tokens: usize,
        temperature: Option<f32>,
        reasoning_effort: Option<String>,
        tool_choice: Option<serde_json::Value>,
        on_content_delta: &Option<F>,
    ) -> LLMResponse
    where
        F: Fn(String) -> Fut + Send + Sync,
        Fut: std::future::Future<Output = ()> + Send,
    {
        log::info!("Starting Anthropic chat stream");
        let idle_timeout_s: u64 = env::var("RUSTBOT_STREAM_IDLE_TIMEOUT_S")
            .unwrap_or_else(|_| "90".to_string())
            .parse()
            .unwrap_or(90);
        let idle_timeout = Duration::from_secs(idle_timeout_s);

        let mut body = self.build_args(
            &messages,
            tools,
            model,
            max_tokens,
            temperature,
            reasoning_effort,
            tool_choice,
            true,
        );
        body.remove("extra_headers");
        body.insert("stream".to_string(), json!(true));

        let response = match self
            .http_client
            .post(&self.messages_url)
            .header(ACCEPT, "text/event-stream")
            .json(&serde_json::Value::Object(body.into_iter().collect()))
            .send()
            .await
        {
            Ok(response) => response,
            Err(e) => return AnthropicProvider::handle_error(e.to_string()),
        };

        if !response.status().is_success() {
            log::error!("Response status: {:?}", response.status());
            log::error!(
                "API Key is set: {:?}",
                self.api_key.as_ref().is_some() && !self.api_key.as_ref().unwrap().is_empty()
            );
            return AnthropicProvider::parse_response(Ok(response)).await;
        }

        let event_stream = AnthropicProvider::sse_event_stream(response.bytes_stream());
        let (acc_stream, message_rx) = AccumulatingStream::new(event_stream);
        AnthropicProvider::consume_stream(
            acc_stream,
            message_rx,
            idle_timeout,
            idle_timeout_s,
            on_content_delta,
        )
        .await
    }

    fn get_default_model(&self) -> String {
        self.default_model
            .clone()
            .unwrap_or_else(|| Self::DEFAULT_MODEL.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remove_anthropic_prefix_test() {
        let result = AnthropicProvider::strip_prefix("anthropic/claude");
        println!("result: {}", result);
        assert_eq!(result, "claude");
    }

    #[test]
    fn keeps_original_when_no_prefix_test() {
        let result = AnthropicProvider::strip_prefix("claude");

        println!("result is: {}", result);
        assert_eq!(result, "claude");
    }

    #[test]

    fn empty_string_test() {
        let result = AnthropicProvider::strip_prefix("");
        println!("result is: {}", result);
        assert_eq!(result, "");
    }

    #[test]
    fn only_prefix_test() {
        let result = AnthropicProvider::strip_prefix("anthropic/");
        println!("result is: {}", result);
        assert_eq!(result, "");
    }

    #[test]
    fn returns_auto_when_thinking_enabled() {
        let result = AnthropicProvider::convert_tool_choice(
            Some(serde_json::json!({ "type": "auto" })),
            true,
        );

        println!(
            "result is: {}",
            serde_json::to_string_pretty(&result).unwrap()
        );
        assert_eq!(result, Some(serde_json::json!({ "type": "auto" })));
    }

    #[test]
    fn returns_auto_when_tool_choice_is_none() {
        let result = AnthropicProvider::convert_tool_choice(None, false);

        assert_eq!(result, Some(serde_json::json!({ "type": "auto" })));
    }

    #[test]
    fn converts_auto_string() {
        let result = AnthropicProvider::convert_tool_choice(Some(serde_json::json!("auto")), false);

        assert_eq!(result, Some(serde_json::json!({ "type": "auto" })));
    }

    #[test]

    fn converts_required_to_any() {
        let result =
            AnthropicProvider::convert_tool_choice(Some(serde_json::json!("required")), false);
        assert_eq!(result, Some(serde_json::json!({ "type": "any" })));
    }

    #[test]

    fn converts_none_string_to_none() {
        let result = AnthropicProvider::convert_tool_choice(Some(serde_json::json!("none")), false);
        assert_eq!(result, None);
    }

    #[test]
    fn converts_function_name_to_tool_type() {
        let input = serde_json::json!({
            "function": {
                "name": "search_web"
            }
        });

        let result = AnthropicProvider::convert_tool_choice(Some(input), false);

        assert_eq!(
            result,
            Some(serde_json::json!({
                "type": "tool",
                "name": "search_web"
            }))
        );
    }

    #[test]
    fn falls_back_to_auto_for_unknown_input() {
        let result = AnthropicProvider::convert_tool_choice(
            Some(serde_json::json!({ "type": "unknown" })),
            false,
        );
        assert_eq!(result, Some(serde_json::json!({ "type": "auto" })));
    }

    #[test]
    fn build_request_headers_includes_anthropic_defaults() {
        let headers = AnthropicProvider::build_request_headers("sk-ant-test", &HashMap::new());

        assert_eq!(headers.get("x-api-key").unwrap(), "sk-ant-test");
        assert_eq!(headers.get("anthropic-version").unwrap(), "2023-06-01");
        assert_eq!(headers.get("content-type").unwrap(), "application/json");
        assert_eq!(headers.get("accept").unwrap(), "application/json");
    }

    #[test]
    fn tool_result_block_preserves_string_content() {
        let msg = json!({
            "role": "tool",
            "tool_call_id": "toolu_abc123",
            "content": "search results here"
        });

        let block = AnthropicProvider::tool_result_block(&msg);

        assert_eq!(
            block,
            json!({
                "type": "tool_result",
                "tool_use_id": "toolu_abc123",
                "content": "search results here"
            })
        );
    }

    #[test]
    fn tool_result_block_preserves_array_content() {
        let msg = json!({
            "role": "tool",
            "tool_call_id": "toolu_xyz",
            "content": [{ "type": "text", "text": "hello" }]
        });

        let block = AnthropicProvider::tool_result_block(&msg);

        assert_eq!(
            block,
            json!({
                "type": "tool_result",
                "tool_use_id": "toolu_xyz",
                "content": [{ "type": "text", "text": "hello" }]
            })
        );
    }

    #[test]
    fn tool_result_block_stringifies_non_string_content() {
        let msg = json!({
            "role": "tool",
            "tool_call_id": "toolu_1",
            "content": { "key": "value" }
        });

        let block = AnthropicProvider::tool_result_block(&msg);

        assert_eq!(block["type"], "tool_result");
        assert_eq!(block["tool_use_id"], "toolu_1");
        assert!(block["content"].is_string());
    }

    #[test]
    fn tool_result_block_empty_when_no_content() {
        let msg = json!({
            "role": "tool",
            "tool_call_id": "toolu_1"
        });

        let block = AnthropicProvider::tool_result_block(&msg);

        assert_eq!(
            block,
            json!({
                "type": "tool_result",
                "tool_use_id": "toolu_1",
                "content": ""
            })
        );
    }

    #[test]
    fn tool_result_block_defaults_tool_use_id() {
        let msg = json!({
            "role": "tool",
            "content": "done"
        });

        let block = AnthropicProvider::tool_result_block(&msg);

        assert_eq!(block["tool_use_id"], "");
    }

    #[test]
    fn assistant_blocks_empty_fallback() {
        let blocks = AnthropicProvider::assistant_blocks(&json!({ "role": "assistant" }));
        assert_eq!(blocks, json!([{ "type": "text", "text": "" }]));
    }

    #[test]
    fn assistant_blocks_text_and_tool_use() {
        let msg = json!({
            "role": "assistant",
            "content": "Hello",
            "tool_calls": [{
                "id": "call_1",
                "function": {
                    "name": "search",
                    "arguments": "{\"q\":\"rust\"}"
                }
            }]
        });

        let blocks = AnthropicProvider::assistant_blocks(&msg);

        assert_eq!(blocks[0], json!({ "type": "text", "text": "Hello" }));
        assert_eq!(blocks[1]["type"], "tool_use");
        assert_eq!(blocks[1]["id"], "call_1");
        assert_eq!(blocks[1]["name"], "search");
        assert_eq!(blocks[1]["input"], json!({ "q": "rust" }));
    }

    #[test]
    fn assistant_blocks_thinking_block() {
        let msg = json!({
            "role": "assistant",
            "thinking_blocks": [{
                "type": "thinking",
                "thinking": "hmm",
                "signature": "sig123"
            }]
        });

        let blocks = AnthropicProvider::assistant_blocks(&msg);

        assert_eq!(
            blocks[0],
            json!({
                "type": "thinking",
                "thinking": "hmm",
                "signature": "sig123"
            })
        );
    }

    #[test]
    fn convert_user_content_empty_string() {
        let msg = json!({ "role": "user", "content": "" });
        assert_eq!(
            AnthropicProvider::convert_user_content(&msg),
            json!("(empty)")
        );
    }

    #[test]
    fn convert_user_content_plain_text() {
        let msg = json!({ "role": "user", "content": "hello" });
        assert_eq!(
            AnthropicProvider::convert_user_content(&msg),
            json!("hello")
        );
    }

    #[test]
    fn convert_user_content_converts_image_url_block() {
        let msg = json!({
            "role": "user",
            "content": [
                {
                    "type": "image_url",
                    "image_url": { "url": "data:image/png;base64,abc123" }
                },
                { "type": "text", "text": "what is this?" }
            ]
        });

        let content = AnthropicProvider::convert_user_content(&msg);
        assert_eq!(content[0]["type"], "image");
        assert_eq!(content[0]["source"]["type"], "base64");
        assert_eq!(content[0]["source"]["media_type"], "image/png");
        assert_eq!(content[0]["source"]["data"], "abc123");
        assert_eq!(
            content[1],
            json!({ "type": "text", "text": "what is this?" })
        );
    }

    #[test]
    fn convert_user_content_http_image_url() {
        let msg = json!({
            "role": "user",
            "content": [{
                "type": "image_url",
                "image_url": { "url": "https://example.com/a.png" }
            }]
        });

        let content = AnthropicProvider::convert_user_content(&msg);
        assert_eq!(content[0]["source"]["type"], "url");
        assert_eq!(content[0]["source"]["url"], "https://example.com/a.png");
    }

    #[test]
    fn convert_user_content_empty_array() {
        let msg = json!({ "role": "user", "content": [] });
        assert_eq!(
            AnthropicProvider::convert_user_content(&msg),
            json!("(empty)")
        );
    }

    #[test]
    fn convert_tools_returns_none_for_empty_input() {
        assert_eq!(AnthropicProvider::convert_tools(None), None);
        assert_eq!(AnthropicProvider::convert_tools(Some(vec![])), None);
    }

    #[test]
    fn convert_tools_converts_openai_function_tool() {
        let tools = vec![json!({
            "type": "function",
            "function": {
                "name": "search",
                "description": "Search the web",
                "parameters": {
                    "type": "object",
                    "properties": { "query": { "type": "string" } }
                }
            }
        })];

        let result = AnthropicProvider::convert_tools(Some(tools)).unwrap();

        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["name"], "search");
        assert_eq!(result[0]["description"], "Search the web");
        assert_eq!(
            result[0]["input_schema"],
            json!({
                "type": "object",
                "properties": { "query": { "type": "string" } }
            })
        );
    }

    #[test]
    fn convert_tools_uses_default_schema_and_skips_empty_description() {
        let tools = vec![json!({
            "type": "function",
            "function": {
                "name": "noop",
                "description": ""
            }
        })];

        let result = AnthropicProvider::convert_tools(Some(tools)).unwrap();

        assert_eq!(result[0]["name"], "noop");
        assert_eq!(
            result[0]["input_schema"],
            json!({ "type": "object", "properties": {} })
        );
        assert!(result[0].get("description").is_none());
    }

    #[test]
    fn convert_tools_reads_cache_control_from_tool_not_function() {
        let tools = vec![json!({
            "type": "function",
            "cache_control": { "type": "ephemeral" },
            "function": {
                "name": "cached_tool",
                "parameters": { "type": "object", "properties": {} }
            }
        })];

        let result = AnthropicProvider::convert_tools(Some(tools)).unwrap();

        assert_eq!(result[0]["cache_control"], json!({ "type": "ephemeral" }));
    }

    #[test]
    fn convert_tools_falls_back_to_tool_when_function_missing() {
        let tools = vec![json!({
            "name": "direct_tool",
            "description": "No wrapper",
            "parameters": { "type": "object", "properties": { "x": { "type": "number" } } }
        })];

        let result = AnthropicProvider::convert_tools(Some(tools)).unwrap();

        assert_eq!(result[0]["name"], "direct_tool");
        assert_eq!(result[0]["description"], "No wrapper");
    }

    #[test]
    fn merge_consecutive_merges_same_role_string_content() {
        let msgs = vec![
            json!({ "role": "user", "content": "hello" }),
            json!({ "role": "user", "content": " world" }),
        ];

        let merged = AnthropicProvider::merge_consecutive(msgs);

        assert_eq!(merged.len(), 1);
        assert_eq!(
            merged[0]["content"],
            json!([
                { "type": "text", "text": "hello" },
                { "type": "text", "text": " world" },
            ])
        );
    }

    #[test]
    fn merge_consecutive_merges_same_role_array_content() {
        let msgs = vec![
            json!({
                "role": "user",
                "content": [{ "type": "text", "text": "a" }]
            }),
            json!({
                "role": "user",
                "content": [{ "type": "tool_result", "tool_use_id": "1", "content": "ok" }]
            }),
        ];

        let merged = AnthropicProvider::merge_consecutive(msgs);

        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0]["content"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn merge_consecutive_keeps_alternating_roles() {
        let msgs = vec![
            json!({ "role": "user", "content": "hi" }),
            json!({ "role": "assistant", "content": [{ "type": "text", "text": "hey" }] }),
            json!({ "role": "user", "content": "again" }),
        ];

        let merged = AnthropicProvider::merge_consecutive(msgs);

        assert_eq!(merged.len(), 3);
    }

    #[test]
    fn build_request_headers_merges_extra_headers() {
        let mut extra = HashMap::new();
        extra.insert("X-Custom-Header".to_string(), "custom-value".to_string());

        let headers = AnthropicProvider::build_request_headers("sk-ant-test", &extra);

        assert_eq!(headers.get("X-Custom-Header").unwrap(), "custom-value");
        assert_eq!(headers.get("x-api-key").unwrap(), "sk-ant-test");
    }

    // --- parse_usage ---

    #[test]
    fn parse_usage_basic_token_counts() {
        let usage = json!({
            "input_tokens": 100,
            "output_tokens": 50
        });

        let result = AnthropicProvider::parse_usage(&usage);

        assert_eq!(result.prompt_tokens().unwrap(), 100);
        assert_eq!(result.output_tokens.unwrap(), 50);
        assert_eq!(result.total_tokens().unwrap(), 150);
    }

    #[test]
    fn parse_usage_includes_cache_creation_tokens() {
        let usage = json!({
            "input_tokens": 100,
            "output_tokens": 50,
            "cache_creation_input_tokens": 200
        });

        let result = AnthropicProvider::parse_usage(&usage);

        assert_eq!(result.prompt_tokens().unwrap(), 300); // 100 + 200
        assert_eq!(result.total_tokens().unwrap(), 350);
        assert_eq!(result.cache_creation_input_tokens.unwrap(), 200);
        assert!(result.cache_read_input_tokens.is_none());
    }

    #[test]
    fn parse_usage_includes_cache_read_tokens_and_normalises_cached_tokens() {
        let usage = json!({
            "input_tokens": 100,
            "output_tokens": 50,
            "cache_read_input_tokens": 400
        });

        let result = AnthropicProvider::parse_usage(&usage);

        assert_eq!(result.prompt_tokens().unwrap(), 500); // 100 + 400
        assert_eq!(result.total_tokens().unwrap(), 550);
        assert_eq!(result.cache_read_input_tokens.unwrap(), 400);
        assert!(result.cache_creation_input_tokens.is_none());
    }

    #[test]
    fn parse_usage_with_both_cache_fields() {
        let usage = json!({
            "input_tokens": 10,
            "output_tokens": 20,
            "cache_creation_input_tokens": 30,
            "cache_read_input_tokens": 40
        });

        let result = AnthropicProvider::parse_usage(&usage);

        assert_eq!(result.prompt_tokens().unwrap(), 80); // 10 + 30 + 40
        assert_eq!(result.output_tokens.unwrap(), 20);
        assert_eq!(result.total_tokens().unwrap(), 100);
        assert_eq!(result.cache_creation_input_tokens.unwrap(), 30);
        assert_eq!(result.cache_read_input_tokens.unwrap(), 40);
    }

    #[test]
    fn parse_usage_zero_cache_fields_are_omitted() {
        let usage = json!({
            "input_tokens": 5,
            "output_tokens": 5,
            "cache_creation_input_tokens": 0,
            "cache_read_input_tokens": 0
        });

        let result = AnthropicProvider::parse_usage(&usage);

        assert_eq!(result.prompt_tokens().unwrap(), 5);
        assert!(result.cache_creation_input_tokens.is_none());
        assert!(result.cache_read_input_tokens.is_none());
    }

    #[test]
    fn parse_usage_handles_missing_fields_gracefully() {
        let result = AnthropicProvider::parse_usage(&json!({}));

        assert_eq!(result.prompt_tokens().unwrap(), 0);
        assert_eq!(result.output_tokens.unwrap(), 0);
        assert_eq!(result.total_tokens().unwrap(), 0);
    }

    // --- parse_json_response ---

    #[test]
    fn parse_json_response_plain_text_block() {
        let body = json!({
            "content": [{ "type": "text", "text": "Hello, world!" }],
            "stop_reason": "end_turn"
        });

        let resp = AnthropicProvider::parse_json_response(body);

        assert_eq!(resp.content, Some("Hello, world!".to_string()));
        assert_eq!(resp.finish_reason, "stop");
        assert!(resp.tool_calls.is_empty());
        assert!(resp.thinking_blocks.is_none());
    }

    #[test]
    fn parse_json_response_concatenates_multiple_text_blocks() {
        let body = json!({
            "content": [
                { "type": "text", "text": "Hello" },
                { "type": "text", "text": ", world!" }
            ],
            "stop_reason": "end_turn"
        });

        let resp = AnthropicProvider::parse_json_response(body);

        assert_eq!(resp.content, Some("Hello, world!".to_string()));
    }

    #[test]
    fn parse_json_response_empty_content_array_yields_none() {
        let body = json!({
            "content": [],
            "stop_reason": "end_turn"
        });

        let resp = AnthropicProvider::parse_json_response(body);

        assert!(resp.content.is_none());
    }

    #[test]
    fn parse_json_response_tool_use_block() {
        let body = json!({
            "content": [{
                "type": "tool_use",
                "id": "toolu_abc123",
                "name": "get_weather",
                "input": { "city": "London", "units": "celsius" }
            }],
            "stop_reason": "tool_use"
        });

        let resp = AnthropicProvider::parse_json_response(body);

        assert_eq!(resp.finish_reason, "tool_calls");
        assert_eq!(resp.tool_calls.len(), 1);
        let call = &resp.tool_calls[0];
        assert_eq!(call.id, "toolu_abc123");
        assert_eq!(call.name, "get_weather");
        assert_eq!(call.arguments["city"], json!("London"));
        assert_eq!(call.arguments["units"], json!("celsius"));
        assert!(resp.content.is_none());
    }

    #[test]
    fn parse_json_response_tool_use_with_non_object_input_defaults_to_empty() {
        let body = json!({
            "content": [{
                "type": "tool_use",
                "id": "toolu_x",
                "name": "noop",
                "input": null
            }],
            "stop_reason": "tool_use"
        });

        let resp = AnthropicProvider::parse_json_response(body);

        assert_eq!(resp.tool_calls.len(), 1);
        assert!(resp.tool_calls[0].arguments.is_empty());
    }

    #[test]
    fn parse_json_response_thinking_block() {
        let body = json!({
            "content": [{
                "type": "thinking",
                "thinking": "Let me reason through this...",
                "signature": "sig_abc"
            }],
            "stop_reason": "end_turn"
        });

        let resp = AnthropicProvider::parse_json_response(body);

        let blocks = resp.thinking_blocks.unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], json!("thinking"));
        assert_eq!(
            blocks[0]["thinking"],
            json!("Let me reason through this...")
        );
        assert_eq!(blocks[0]["signature"], json!("sig_abc"));
        assert!(resp.content.is_none());
    }

    #[test]
    fn parse_json_response_thinking_block_missing_signature_defaults_to_empty_string() {
        let body = json!({
            "content": [{
                "type": "thinking",
                "thinking": "Hmm..."
            }],
            "stop_reason": "end_turn"
        });

        let resp = AnthropicProvider::parse_json_response(body);

        let blocks = resp.thinking_blocks.unwrap();
        assert_eq!(blocks[0]["signature"], json!(""));
    }

    #[test]
    fn parse_json_response_mixed_blocks() {
        let body = json!({
            "content": [
                { "type": "thinking", "thinking": "reasoning...", "signature": "s1" },
                { "type": "text", "text": "The answer is 42." },
                { "type": "tool_use", "id": "toolu_1", "name": "lookup", "input": { "q": "x" } }
            ],
            "stop_reason": "tool_use"
        });

        let resp = AnthropicProvider::parse_json_response(body);

        assert_eq!(resp.content, Some("The answer is 42.".to_string()));
        assert_eq!(resp.tool_calls.len(), 1);
        assert_eq!(resp.thinking_blocks.as_ref().unwrap().len(), 1);
        assert_eq!(resp.finish_reason, "tool_calls");
    }

    #[test]
    fn parse_json_response_stop_reason_mapping() {
        let cases = [
            ("end_turn", "stop"),
            ("tool_use", "tool_calls"),
            ("max_tokens", "length"),
            ("unknown_reason", "stop"), // unmapped → default "stop"
            ("", "stop"),               // missing → default "stop"
        ];
        for (stop_reason, expected_finish) in cases {
            let body = json!({ "stop_reason": stop_reason, "content": [] });
            let resp = AnthropicProvider::parse_json_response(body);
            assert_eq!(
                resp.finish_reason, expected_finish,
                "stop_reason={stop_reason:?}"
            );
        }
    }

    #[test]
    fn parse_json_response_unknown_block_types_are_ignored() {
        let body = json!({
            "content": [
                { "type": "redacted_thinking", "data": "opaque" },
                { "type": "text", "text": "visible" }
            ],
            "stop_reason": "end_turn"
        });

        let resp = AnthropicProvider::parse_json_response(body);

        assert_eq!(resp.content, Some("visible".to_string()));
        assert!(resp.thinking_blocks.is_none());
    }

    #[test]
    fn parse_json_response_with_usage() {
        let body = json!({
            "content": [{ "type": "text", "text": "hi" }],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 10,
                "output_tokens": 5,
                "cache_read_input_tokens": 20
            }
        });

        let resp = AnthropicProvider::parse_json_response(body);

        assert_eq!(resp.usage.prompt_tokens().unwrap(), 30); // 10 + 20
        assert_eq!(resp.usage.output_tokens.unwrap(), 5);
        assert_eq!(resp.usage.cache_read_input_tokens.unwrap(), 20);
    }

    #[test]
    fn parse_json_response_missing_usage_field_returns_empty_map() {
        let body = json!({
            "content": [{ "type": "text", "text": "hi" }],
            "stop_reason": "end_turn"
        });

        let resp = AnthropicProvider::parse_json_response(body);

        assert!(resp.usage.prompt_tokens().is_none());
    }

    #[test]
    fn supports_temperature_for_claude_4_models() {
        assert!(AnthropicProvider::supports_temperature("claude-sonnet-4-6"));
        assert!(AnthropicProvider::supports_temperature("claude-opus-4-6"));
        assert!(AnthropicProvider::supports_temperature("claude-haiku-4-5"));
    }

    #[test]
    fn supports_temperature_rejects_claude_5_models() {
        assert!(!AnthropicProvider::supports_temperature("claude-sonnet-5"));
        assert!(!AnthropicProvider::supports_temperature("claude-opus-5"));
        assert!(!AnthropicProvider::supports_temperature("claude-haiku-5"));
    }

    #[test]
    fn uses_adaptive_thinking_for_claude_5_models() {
        assert!(AnthropicProvider::uses_adaptive_thinking("claude-sonnet-5"));
        assert!(!AnthropicProvider::uses_adaptive_thinking(
            "claude-sonnet-4-6"
        ));
    }

    #[test]
    fn format_api_error_extracts_anthropic_message() {
        let body = r#"{"type":"error","error":{"type":"invalid_request_error","message":"`temperature` is deprecated for this model."}}"#;
        let message = AnthropicProvider::format_api_error(400, body);
        assert_eq!(
            message,
            "Anthropic API error (400): `temperature` is deprecated for this model."
        );
    }

    #[test]
    fn format_api_error_falls_back_to_raw_body() {
        let message = AnthropicProvider::format_api_error(502, "bad gateway");
        assert_eq!(message, "Anthropic API error (502): bad gateway");
    }

    #[test]
    fn parse_sse_event_parses_text_delta() {
        let raw = "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n";
        let event = AnthropicProvider::parse_sse_event(raw).unwrap().unwrap();
        match event {
            MessageStreamEvent::ContentBlockDelta(delta) => {
                assert_eq!(
                    delta.delta,
                    ContentBlockDelta::TextDelta(adk_anthropic::TextDelta::new("Hi".to_string()))
                );
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn parse_sse_event_treats_ping_as_no_op() {
        let event = AnthropicProvider::parse_sse_event("event: ping\n\n")
            .unwrap()
            .unwrap();
        assert_eq!(event, MessageStreamEvent::Ping);
    }

    #[tokio::test]
    async fn sse_event_stream_reassembles_utf8_split_across_chunks() {
        // "café" — the 'é' is two UTF-8 bytes (0xC3 0xA9); split the event so
        // the boundary lands in the middle of that sequence to exercise the
        // byte-buffer reassembly.
        let event = "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"café\"}}\n\n";
        let bytes = event.as_bytes().to_vec();
        let e_acute_start = bytes
            .windows(2)
            .position(|w| w == [0xC3, 0xA9])
            .expect("é byte sequence present");
        let split = e_acute_start + 1;

        let chunks: Vec<Result<Vec<u8>, reqwest::Error>> =
            vec![Ok(bytes[..split].to_vec()), Ok(bytes[split..].to_vec())];

        let mut stream = Box::pin(AnthropicProvider::sse_event_stream(futures::stream::iter(
            chunks,
        )));

        let event = stream.next().await.unwrap().unwrap();
        match event {
            MessageStreamEvent::ContentBlockDelta(delta) => assert_eq!(
                delta.delta,
                ContentBlockDelta::TextDelta(adk_anthropic::TextDelta::new("café".to_string()))
            ),
            other => panic!("unexpected event: {other:?}"),
        }
        assert!(stream.next().await.is_none());
    }

    #[test]
    fn build_args_omits_temperature_when_none() {
        let provider = AnthropicProvider::new(
            Some("sk-ant-test".to_string()),
            None,
            Some("claude-sonnet-4-6".to_string()),
            None,
            None,
        );
        let args = provider.build_args(
            &[serde_json::json!({ "role": "user", "content": "hi" })],
            None,
            Some("claude-sonnet-4-6".to_string()),
            16,
            None,
            None,
            None,
            false,
        );
        assert!(
            !args.contains_key("temperature"),
            "temperature should be omitted, got {args:?}"
        );
    }

    #[test]
    fn build_args_includes_temperature_when_some() {
        let provider = AnthropicProvider::new(
            Some("sk-ant-test".to_string()),
            None,
            Some("claude-sonnet-4-6".to_string()),
            None,
            None,
        );
        let args = provider.build_args(
            &[serde_json::json!({ "role": "user", "content": "hi" })],
            None,
            Some("claude-sonnet-4-6".to_string()),
            16,
            Some(0.5),
            None,
            None,
            false,
        );
        assert_eq!(args.get("temperature"), Some(&serde_json::json!(0.5)));
    }
}
