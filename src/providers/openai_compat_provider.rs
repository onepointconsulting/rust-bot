use crate::providers::{
    base::{GenerationSettings, LLMProvider, LLMResponse, LLMUsage, ToolCallRequest},
    cache_control::apply_cache_control,
    registry::ProviderSpec,
};
use async_openai::error::OpenAIError;
use async_openai::types::chat::ImageUrl;
use async_openai::types::chat::{
    ChatCompletionMessageToolCall, ChatCompletionMessageToolCalls,
    ChatCompletionRequestAssistantMessageArgs, ChatCompletionRequestAssistantMessageContent,
    ChatCompletionRequestAssistantMessageContentPart, ChatCompletionRequestMessage,
    ChatCompletionRequestMessageContentPartImage, ChatCompletionRequestMessageContentPartText,
    ChatCompletionRequestSystemMessageArgs, ChatCompletionRequestSystemMessageContent,
    ChatCompletionRequestSystemMessageContentPart, ChatCompletionRequestToolMessage,
    ChatCompletionRequestToolMessageContent, ChatCompletionRequestUserMessageArgs,
    ChatCompletionRequestUserMessageContent, ChatCompletionRequestUserMessageContentPart,
    ChatCompletionStreamOptions, ChatCompletionToolChoiceOption, ChatCompletionTools,
    CompletionUsage, CreateChatCompletionRequest, CreateChatCompletionRequestArgs,
    CreateChatCompletionResponse, FinishReason, FunctionCall,
};
use async_openai::{Client, config::OpenAIConfig, types::chat::ReasoningEffort};
use futures::StreamExt;
use std::collections::HashMap;

struct OpenAICombinedResponse {
    response: CreateChatCompletionResponse,
    raw_json: serde_json::Value,
}

fn maybe_mapping(value: &serde_json::Value) -> Option<&serde_json::Map<String, serde_json::Value>> {
    value.as_object()
}

pub struct OpenAICompatProvider {
    api_key: Option<String>,
    api_base: Option<String>,
    default_model: Option<String>,
    extra_headers: HashMap<String, String>,
    spec: Option<ProviderSpec>,
    generation: GenerationSettings,
    client: Client<OpenAIConfig>,
    /// Raw reqwest client used to bypass async-openai's strict typed
    /// deserializer when the API returns unknown enum variants (e.g.
    /// `service_tier: "standard"` from Anthropic's OpenAI-compat endpoint).
    http_client: reqwest::Client,
    chat_completions_url: String,
}

impl OpenAICompatProvider {
    const DEFAULT_MODEL: &str = "gpt-5-mini";

    // Allowed message keys for OpenAI-compatible messages
    const ALLOWED_MSG_KEYS: &[&str] = &[
        "role",
        "content",
        "tool_calls",
        "tool_call_id",
        "name",
        "reasoning_content",
        "extra_content",
    ];

    // Alphanumeric characters (ASCII letters + digits)
    const ALNUM: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";

    // Standard tool call keys
    const STANDARD_TC_KEYS: &[&str] = &["id", "type", "index", "function"];

    // Standard function call keys
    const STANDARD_FN_KEYS: &[&str] = &["name", "arguments"];
    const ARG_PARSE_ERROR_KEY: &str = "__args_json_parse_error";
    const ARG_PARSE_RAW_KEY: &str = "__args_json_raw";
    const ARG_PARSE_RAW_LIMIT: usize = 400;

    // Default OpenRouter headers as a static map
    fn default_openrouter_headers() -> std::collections::HashMap<&'static str, &'static str> {
        let mut m = std::collections::HashMap::new();
        m.insert(
            "HTTP-Referer",
            "https://github.com/onepointconsulting/rust-bot.git",
        );
        m.insert("X-OpenRouter-Title", "rust-bot");
        m.insert("X-OpenRouter-Categories", "cli-agent,personal-agent");
        m
    }

    /// Generates a 9-character alphanumeric ID compatible with all providers (incl. Mistral).
    fn short_tool_id() -> String {
        use rand::RngExt;

        let mut rng = rand::rng();
        (0..9)
            .map(|_| {
                let idx = rng.random_range(0..Self::ALNUM.len());
                Self::ALNUM.chars().nth(idx).unwrap()
            })
            .collect()
    }

    fn parse_tool_arguments(arguments_json: &str) -> HashMap<String, serde_json::Value> {
        match serde_json::from_str(arguments_json) {
            Ok(args) => args,
            Err(err) => {
                log::error!(
                    "Failed to parse tool arguments: {}. Arguments length: {}",
                    err,
                    arguments_json.len()
                );
                let mut args = HashMap::new();
                let raw: String = arguments_json
                    .chars()
                    .take(Self::ARG_PARSE_RAW_LIMIT)
                    .collect();
                args.insert(
                    Self::ARG_PARSE_ERROR_KEY.to_string(),
                    serde_json::Value::String(err.to_string()),
                );
                args.insert(
                    Self::ARG_PARSE_RAW_KEY.to_string(),
                    serde_json::Value::String(raw),
                );
                args
            }
        }
    }

    /// Get a value from a serde_json::Value::Object or struct field, returning None if absent.
    ///
    /// If `obj` is an Object, tries to get the key as in Python's dict access.
    /// For anything else (including structs), it attempts to get a field via serde_json pointer, but returns None if not found.
    pub fn get_value(obj: &serde_json::Value, key: &str) -> Option<serde_json::Value> {
        if let Some(map) = obj.as_object() {
            map.get(key).cloned()
        } else {
            // Fallback for struct-like objects, match Python getattr(obj, key, None)
            obj.get(key).cloned()
        }
    }

    /// Try to coerce `value` to a serde_json::Map; return None if not possible or empty.
    ///
    /// This mimics the Python helper for extracting a dict/map from:
    /// - a serde_json Object (`Value::Object`)
    /// - an object with a "model_dump" method (if callable in Python, here: if value["model_dump"] is a function, call and check result)
    pub fn coerce_map(
        value: &serde_json::Value,
    ) -> Option<serde_json::Map<String, serde_json::Value>> {
        // If value is null, return None
        if value.is_null() {
            return None;
        }

        // If value is already a Map/Object, return it if not empty
        if let Some(map) = value.as_object() {
            if !map.is_empty() {
                return Some(map.clone());
            } else {
                return None;
            }
        }

        // If value has a "model_dump" key and it's a callable like in Python,
        // we can simulate by checking if value["model_dump"] is a function,
        // but in Rust/serde_json we can't call arbitrary functions.
        // Instead, we handle the case where value is an object with a field "model_dump" that's an object
        if let Some(obj) = value.as_object() {
            if let Some(model_dump_value) = obj.get("model_dump") {
                // If "model_dump" itself is an object, try returning it non-empty
                if let Some(dumped) = model_dump_value.as_object() {
                    if !dumped.is_empty() {
                        return Some(dumped.clone());
                    }
                }
                // If "model_dump" is a function placeholder, we can't handle it -- out of scope
            }
        }

        None
    }

    /// Extracts (extra_content, provider_specific_fields, fn_provider_specific_fields)
    /// from a tool call (tc). This mimics the Python _extract_tc_extras.
    ///
    /// Returns a tuple of three Option<serde_json::Map<...>> corresponding to:
    /// (extra_content, provider_specific_fields, fn_provider_specific_fields)
    pub fn extract_tc_extras(
        tc: &serde_json::Value,
    ) -> (
        Option<serde_json::Map<String, serde_json::Value>>,
        Option<serde_json::Map<String, serde_json::Value>>,
        Option<serde_json::Map<String, serde_json::Value>>,
    ) {
        // Helper: get value by key
        fn get<'a>(obj: &'a serde_json::Value, key: &str) -> Option<&'a serde_json::Value> {
            if let Some(map) = obj.as_object() {
                map.get(key)
            } else {
                obj.get(key)
            }
        }

        // Helper: coerce serde_json::Value to Option<Map>
        fn coerce_dict(
            value: &serde_json::Value,
        ) -> Option<serde_json::Map<String, serde_json::Value>> {
            OpenAICompatProvider::coerce_map(value)
        }

        // 1. extra_content extraction
        let extra_content = get(tc, "extra_content").and_then(|v| coerce_dict(v));

        // Try to get tc as a dict (serde_json::Map)
        let tc_dict = coerce_dict(tc);

        let mut prov: Option<serde_json::Map<String, serde_json::Value>> = None;
        let mut fn_prov: Option<serde_json::Map<String, serde_json::Value>> = None;

        if let Some(ref dict) = tc_dict {
            // Collect leftover non-standard keys (excluding extra_content, and non-null)
            let mut leftover = serde_json::Map::new();
            for (k, v) in dict.iter() {
                if !OpenAICompatProvider::STANDARD_TC_KEYS.contains(&k.as_str())
                    && k != "extra_content"
                    && !v.is_null()
                {
                    leftover.insert(k.clone(), v.clone());
                }
            }
            if !leftover.is_empty() {
                prov = Some(leftover);
            }

            if let Some(fn_value) = dict.get("function") {
                if let Some(fn_map) = coerce_dict(fn_value) {
                    let mut fn_leftover = serde_json::Map::new();
                    for (k, v) in fn_map.iter() {
                        if !OpenAICompatProvider::STANDARD_FN_KEYS.contains(&k.as_str())
                            && !v.is_null()
                        {
                            fn_leftover.insert(k.clone(), v.clone());
                        }
                    }
                    if !fn_leftover.is_empty() {
                        fn_prov = Some(fn_leftover);
                    }
                }
            }
        } else {
            // Fallback if tc was not already a map
            prov = get(tc, "provider_specific_fields").and_then(|v| coerce_dict(v));
            if let Some(fn_obj) = get(tc, "function") {
                fn_prov = get(fn_obj, "provider_specific_fields").and_then(|v| coerce_dict(v));
            }
        }

        (extra_content, prov, fn_prov)
    }

    fn uses_openrouter_attribution(spec: Option<&ProviderSpec>, api_base: Option<&str>) -> bool {
        // Apply Rust-bot attribution headers to OpenRouter requests by default.
        if let Some(spec) = spec {
            if spec.name == "openrouter" {
                return true;
            }
        }
        if let Some(base) = api_base {
            return base.to_lowercase().contains("openrouter");
        }
        false
    }

    fn setup_env(&self, api_key_option: Option<String>, api_base: Option<String>) {
        let spec_option = self.spec.as_ref();
        if spec_option.is_none() || spec_option.unwrap().env_key.is_empty() {
            return;
        }
        if let Some(spec) = spec_option {
            let api_key = api_key_option.unwrap_or_else(|| {
                log::error!(
                    "Provider spec '{}' requires an API key (env: {}), but none was supplied. \
                     Check your configuration.",
                    spec.name,
                    spec.env_key
                );
                panic!(
                    "Missing API key for provider spec '{}' (expected env var '{}')",
                    spec.name, spec.env_key
                );
            });
            // SAFETY: single-threaded at provider init time; no other threads read this env var concurrently.
            if spec.is_gateway {
                unsafe {
                    std::env::set_var(&spec.env_key, &api_key);
                }
            } else if std::env::var_os(&spec.env_key).is_none() {
                unsafe {
                    std::env::set_var(&spec.env_key, &api_key);
                }
            }

            let effective_base_str = api_base
                .as_ref()
                .or_else(|| spec.default_api_base.as_ref())
                .map(|s| s.as_str())
                .unwrap_or_default();

            for (env_name, env_val) in &spec.env_extras {
                let resolved = env_val
                    .replace("{api_key}", api_key.as_str())
                    .replace("{api_base}", effective_base_str);
                if std::env::var_os(env_name).is_none() {
                    unsafe {
                        std::env::set_var(env_name, resolved);
                    }
                }
            }
        }
    }

    fn sanitize_messages(messages: &[serde_json::Value]) -> Vec<serde_json::Value> {
        // Strip non-standard keys, normalize tool_call IDs.

        // Prepare allowed_keys as a HashSet<String>.
        let allowed_keys: std::collections::HashSet<String> =
            OpenAICompatProvider::ALLOWED_MSG_KEYS
                .iter()
                .map(|s| s.to_string())
                .collect();

        // Sanitize: strip nonstandard keys.
        let mut sanitized =
            <Self as LLMProvider>::sanitize_request_messages(messages, &allowed_keys);

        // id_map: maps original id string -> normalized id string
        let mut id_map: HashMap<String, String> = HashMap::new();

        // Use normalize_tool_call_id from sanitizer.rs
        fn map_id(
            id_map: &mut HashMap<String, String>,
            value: &serde_json::Value,
        ) -> serde_json::Value {
            // If not a string, return as is
            if let Some(s) = value.as_str() {
                // Use crate::providers::sanitizer::normalize_tool_call_id for normalization
                // Don't import at this scope, just call fully qualified
                let v = id_map.entry(s.to_string()).or_insert_with(|| {
                    let normalized = crate::providers::sanitizer::normalize_tool_call_id(
                        &serde_json::Value::String(s.to_string()),
                    );
                    normalized.as_str().unwrap_or(s).to_string()
                });
                serde_json::Value::String(v.clone())
            } else {
                value.clone()
            }
        }

        for clean in sanitized.iter_mut() {
            // tool_calls normalization
            if let Some(tool_calls) = clean.get_mut("tool_calls") {
                if let Some(tc_arr) = tool_calls.as_array_mut() {
                    let mut normalized = Vec::with_capacity(tc_arr.len());
                    for tc in tc_arr.drain(..) {
                        if let Some(mut tc_obj) = tc.as_object().cloned() {
                            // Normalize id
                            let id_val =
                                tc_obj.get("id").cloned().unwrap_or(serde_json::Value::Null);
                            tc_obj.insert("id".to_string(), map_id(&mut id_map, &id_val));
                            normalized.push(serde_json::Value::Object(tc_obj));
                        } else {
                            // Not an object, leave as-is
                            normalized.push(tc);
                        }
                    }
                    *tool_calls = serde_json::Value::Array(normalized);
                }
            }
            // tool_call_id normalization
            if let Some(val) = clean.get_mut("tool_call_id") {
                if !val.is_null() {
                    *val = map_id(&mut id_map, val);
                }
            }
        }

        sanitized
    }

    /// Extract non-empty text strings from content-part arrays.
    ///
    /// Used after [`apply_cache_control`] rewrites string content into
    /// `[{"type":"text","text":"...","cache_control":...}]`.
    fn content_text_parts(blocks: &[serde_json::Value]) -> Vec<String> {
        blocks
            .iter()
            .filter_map(|block| {
                let typ = block.get("type").and_then(|t| t.as_str());
                if typ.is_some() && typ != Some("text") {
                    return None;
                }
                let text = block.get("text").and_then(|t| t.as_str()).unwrap_or("");
                if text.is_empty() {
                    None
                } else {
                    Some(text.to_string())
                }
            })
            .collect()
    }

    /// Convert a JSON `content` value into typed system-message content.
    ///
    /// Supports both plain strings and content-part arrays.
    fn system_message_content(
        content: Option<&serde_json::Value>,
    ) -> ChatCompletionRequestSystemMessageContent {
        match content {
            Some(serde_json::Value::String(text)) => {
                ChatCompletionRequestSystemMessageContent::Text(text.clone())
            }
            Some(serde_json::Value::Array(blocks)) => {
                let parts: Vec<ChatCompletionRequestSystemMessageContentPart> =
                    Self::content_text_parts(blocks)
                        .into_iter()
                        .map(|text| {
                            ChatCompletionRequestSystemMessageContentPart::Text(
                                ChatCompletionRequestMessageContentPartText { text },
                            )
                        })
                        .collect();
                if parts.is_empty() {
                    ChatCompletionRequestSystemMessageContent::Text(String::new())
                } else {
                    ChatCompletionRequestSystemMessageContent::Array(parts)
                }
            }
            _ => ChatCompletionRequestSystemMessageContent::Text(String::new()),
        }
    }

    /// Convert a JSON `content` value into typed assistant-message content.
    ///
    /// Returns `None` when content is empty so tool-call-only assistant turns
    /// can omit `content`. Supports both plain strings and content-part arrays.
    fn assistant_message_content(
        content: Option<&serde_json::Value>,
    ) -> Option<ChatCompletionRequestAssistantMessageContent> {
        match content {
            Some(serde_json::Value::String(text)) if !text.is_empty() => Some(
                ChatCompletionRequestAssistantMessageContent::Text(text.clone()),
            ),
            Some(serde_json::Value::Array(blocks)) => {
                let parts: Vec<ChatCompletionRequestAssistantMessageContentPart> =
                    Self::content_text_parts(blocks)
                        .into_iter()
                        .map(|text| {
                            ChatCompletionRequestAssistantMessageContentPart::Text(
                                ChatCompletionRequestMessageContentPartText { text },
                            )
                        })
                        .collect();
                if parts.is_empty() {
                    None
                } else {
                    Some(ChatCompletionRequestAssistantMessageContent::Array(parts))
                }
            }
            _ => None,
        }
    }

    fn build_request(
        &self,
        messages: &[serde_json::Value],
        tools: Option<Vec<serde_json::Value>>,
        model: Option<String>,
        max_tokens: usize,
        temperature: Option<f32>,
        reasoning_effort: Option<String>,
        tool_choice: Option<serde_json::Value>,
    ) -> CreateChatCompletionRequestArgs {
        let mut model_name = model.unwrap_or_else(|| self.get_default_model());
        let spec_option = self.spec.as_ref();
        let mut messages = messages;
        let (cached_messages, cached_tools) = if let Some(spec) = spec_option {
            if spec.supports_prompt_caching {
                let (new_messages, new_tools) =
                    apply_cache_control(&messages, tools.as_ref().map(|t| t.as_slice()));
                (Some(new_messages), new_tools)
            } else {
                (None, None)
            }
        } else {
            (None, None)
        };
        if let Some(ref cached) = cached_messages {
            messages = cached.as_slice();
        }
        if let Some(spec) = spec_option {
            if spec.strip_model_prefix {
                model_name = model_name
                    .rsplitn(2, '/')
                    .next()
                    .unwrap_or(&model_name)
                    .to_string();
            }
        }
        let sanitized_messages = Self::sanitize_messages(
            OpenAICompatProvider::sanitize_empty_content(messages).as_slice(),
        );
        let chat_messages: Vec<ChatCompletionRequestMessage> = sanitized_messages
            .iter()
            .filter_map(|msg| {
                let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("user");
                let content_str = msg
                    .get("content")
                    .and_then(|c| c.as_str())
                    .unwrap_or("")
                    .to_string();
                match role {
                    "system" => ChatCompletionRequestSystemMessageArgs::default()
                        .content(Self::system_message_content(msg.get("content")))
                        .build()
                        .ok()
                        .map(Into::into),
                    "assistant" => {
                        let mut builder = ChatCompletionRequestAssistantMessageArgs::default();
                        if let Some(content) = Self::assistant_message_content(msg.get("content")) {
                            builder.content(content);
                        }
                        if let Some(tcs_val) = msg.get("tool_calls").and_then(|v| v.as_array()) {
                            let tcs: Vec<ChatCompletionMessageToolCalls> = tcs_val
                                .iter()
                                .filter_map(|tc| {
                                    let id = tc.get("id")?.as_str()?.to_string();
                                    let func = tc.get("function")?;
                                    let name = func.get("name")?.as_str()?.to_string();
                                    let arguments = func
                                        .get("arguments")
                                        .map(|a| {
                                            a.as_str()
                                                .map(str::to_string)
                                                .unwrap_or_else(|| a.to_string())
                                        })
                                        .unwrap_or_default();
                                    Some(ChatCompletionMessageToolCalls::Function(
                                        ChatCompletionMessageToolCall {
                                            id,
                                            function: FunctionCall { name, arguments },
                                        },
                                    ))
                                })
                                .collect();
                            if !tcs.is_empty() {
                                builder.tool_calls(tcs);
                            }
                        }
                        builder.build().ok().map(Into::into)
                    }
                    "tool" => {
                        let tool_content = match msg.get("content") {
                            Some(c) if c.is_string() => c.as_str().unwrap_or("").to_string(),
                            Some(c) => c.to_string(),
                            None => String::new(),
                        };
                        let tool_call_id = msg
                            .get("tool_call_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        Some(ChatCompletionRequestMessage::Tool(
                            ChatCompletionRequestToolMessage {
                                content: ChatCompletionRequestToolMessageContent::Text(tool_content),
                                tool_call_id,
                            },
                        ))
                    }
                    _ => {
                        let raw_content = msg.get("content");
                        let user_content =
                            if let Some(arr) = raw_content.and_then(|c| c.as_array()) {
                                // Multimodal: build a typed content array, dropping _meta.
                                let parts: Vec<ChatCompletionRequestUserMessageContentPart> = arr
                                    .iter()
                                    .filter_map(|block| {
                                        match block.get("type").and_then(|t| t.as_str()) {
                                            Some("text") => {
                                                let text = block
                                                    .get("text")
                                                    .and_then(|t| t.as_str())
                                                    .unwrap_or("")
                                                    .to_string();
                                                Some(ChatCompletionRequestUserMessageContentPart::Text(
                                                    ChatCompletionRequestMessageContentPartText { text },
                                                ))
                                            }
                                            Some("image_url") => {
                                                let url = block
                                                    .get("image_url")
                                                    .and_then(|u| u.get("url"))
                                                    .and_then(|u| u.as_str())
                                                    .unwrap_or("")
                                                    .to_string();
                                                if url.is_empty() {
                                                    return None;
                                                }
                                                Some(ChatCompletionRequestUserMessageContentPart::ImageUrl(
                                                    ChatCompletionRequestMessageContentPartImage {
                                                        image_url: ImageUrl { url, detail: None },
                                                    },
                                                ))
                                            }
                                            _ => None,
                                        }
                                    })
                                    .collect();
                                if parts.is_empty() {
                                    ChatCompletionRequestUserMessageContent::Text(String::new())
                                } else {
                                    ChatCompletionRequestUserMessageContent::Array(parts)
                                }
                            } else {
                                ChatCompletionRequestUserMessageContent::Text(content_str)
                            };
                        ChatCompletionRequestUserMessageArgs::default()
                            .content(user_content)
                            .build()
                            .ok()
                            .map(Into::into)
                    }
                }
            })
            .collect();
        let mut request = CreateChatCompletionRequestArgs::default();
        request.model(model_name);
        request.messages(chat_messages);
        request.max_tokens(max_tokens as u32);
        if let Some(temperature) = temperature {
            log::info!("temperature: {}", temperature);
            request.temperature(temperature);
        }

        // Prefer cache-annotated tools over the originals; deserialize from JSON.
        let effective_tools = cached_tools.or(tools);
        if let Some(tool_list) = effective_tools {
            let typed_tools: Vec<ChatCompletionTools> = tool_list
                .into_iter()
                .filter_map(|t| serde_json::from_value(t).ok())
                .collect();
            if !typed_tools.is_empty() {
                request.tools(typed_tools);
            }
        }

        if let Some(effort) = reasoning_effort {
            if let Ok(typed_effort) =
                serde_json::from_value::<ReasoningEffort>(serde_json::Value::String(effort))
            {
                request.reasoning_effort(typed_effort);
            }
        }
        if let Some(tc) = tool_choice {
            if let Ok(typed_tc) = serde_json::from_value::<ChatCompletionToolChoiceOption>(tc) {
                request.tool_choice(typed_tc);
            }
        }

        request
    }

    /// Drop or reshape fields that `async-openai`'s typed response structs
    /// reject. OpenAI-compatible gateways (Requesty, Anthropic, Vertex/Gemini)
    /// often emit extra citation / routing metadata that is unused by
    /// [`Self::parse_response`] but still fails serde.
    fn sanitize_compat_response(json: &mut serde_json::Value) {
        if let Some(obj) = json.as_object_mut() {
            // Anthropic's OpenAI-compat endpoint returns `"standard"`.
            obj.remove("service_tier");
        }

        let Some(choices) = json.get_mut("choices").and_then(|c| c.as_array_mut()) else {
            return;
        };
        for choice in choices {
            for key in ["message", "delta"] {
                let Some(container) = choice.get_mut(key).and_then(|m| m.as_object_mut()) else {
                    continue;
                };
                Self::sanitize_message_annotations(container);
            }
        }
    }

    /// `async-openai` only accepts `annotations[].type == "url_citation"` with
    /// a nested `url_citation` object. Requesty/Gemini grounding often sends
    /// `"type": "annotation"` (or a flattened citation), which otherwise
    /// errors as `unknown variant 'annotation', expected 'url_citation'`.
    fn sanitize_message_annotations(message: &mut serde_json::Map<String, serde_json::Value>) {
        let Some(annotations) = message
            .get_mut("annotations")
            .and_then(|a| a.as_array_mut())
        else {
            return;
        };

        let before = annotations.len();
        annotations.retain(Self::is_typed_url_citation);
        let dropped = before.saturating_sub(annotations.len());
        if dropped > 0 {
            log::debug!(
                "Dropped {dropped} incompatible message annotation(s) from OpenAI-compat response"
            );
        }
        if annotations.is_empty() {
            message.remove("annotations");
        }
    }

    fn is_typed_url_citation(ann: &serde_json::Value) -> bool {
        if ann.get("type").and_then(|t| t.as_str()) != Some("url_citation") {
            return false;
        }
        let Some(citation) = ann.get("url_citation") else {
            return false;
        };
        citation.get("url").is_some()
            && citation.get("title").is_some()
            && citation.get("start_index").is_some()
            && citation.get("end_index").is_some()
    }

    /// POST the request as raw JSON and deserialize after
    /// [`Self::sanitize_compat_response`], so unknown gateway fields do not
    /// fail typed parsing.
    async fn chat_raw(
        &self,
        request: &CreateChatCompletionRequest,
    ) -> Result<OpenAICombinedResponse, String> {
        let body = serde_json::to_value(request).map_err(|e| e.to_string())?;
        let resp = self
            .http_client
            .post(&self.chat_completions_url)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| e.to_string())?;

        let status = resp.status();
        let mut json: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;

        if !status.is_success() {
            return Err(format!("HTTP {status}: {json}"));
        }

        // Strip / coerce fields that async-openai's typed structs cannot
        // deserialize (unknown `service_tier` values, Gemini/Requesty
        // citation annotations with `type: "annotation"`, etc.).
        Self::sanitize_compat_response(&mut json);

        let response = serde_json::from_value(json.clone())
            .map_err(|e| format!("failed to deserialize api response: {e}"))?;
        Ok(OpenAICombinedResponse {
            response,
            raw_json: json,
        })
    }

    fn json_u32(value: &serde_json::Value, key: &str) -> Option<u32> {
        let n = value.get(key)?;
        n.as_u64()
            .map(|n| n as u32)
            .or_else(|| n.as_f64().map(|f| f as u32))
    }

    fn json_f64(value: &serde_json::Value, key: &str) -> Option<f64> {
        let n = value.get(key)?;
        n.as_f64().or_else(|| n.as_u64().map(|n| n as f64))
    }

    /// Map an OpenAI-compat `usage` object onto [`LLMUsage`].
    ///
    /// OpenAI `prompt_tokens` already includes cache hits. Those are stored as
    /// `cache_read_input_tokens` and subtracted from `input_tokens` so
    /// [`LLMUsage::prompt_tokens`] does not double-count.
    fn parse_usage(usage: &serde_json::Value) -> LLMUsage {
        let prompt = Self::json_u32(usage, "prompt_tokens")
            .or_else(|| Self::json_u32(usage, "input_tokens"));
        let output = Self::json_u32(usage, "completion_tokens")
            .or_else(|| Self::json_u32(usage, "output_tokens"));

        let details = usage.get("prompt_tokens_details");
        let cached = details
            .and_then(|d| Self::json_u32(d, "cached_tokens"))
            .or_else(|| Self::json_u32(usage, "cache_read_input_tokens"))
            .unwrap_or(0);
        let cache_write = details
            .and_then(|d| Self::json_u32(d, "cache_write_tokens"))
            .or_else(|| Self::json_u32(usage, "cache_creation_input_tokens"))
            .unwrap_or(0);
        let reasoning = usage
            .get("completion_tokens_details")
            .and_then(|d| Self::json_u32(d, "reasoning_tokens"))
            .filter(|&n| n > 0);

        let input_tokens = prompt.map(|p| p.saturating_sub(cached).saturating_sub(cache_write));

        let cost_details = usage.get("cost_details");
        let split_input =
            cost_details.and_then(|d| Self::json_f64(d, "upstream_inference_prompt_cost"));
        let split_output =
            cost_details.and_then(|d| Self::json_f64(d, "upstream_inference_completions_cost"));
        let (input_cost, output_cost) =
            match (split_input, split_output, Self::json_f64(usage, "cost")) {
                (Some(input), Some(output), _) => (Some(input), Some(output)),
                (Some(input), None, _) => (Some(input), None),
                (None, Some(output), _) => (None, Some(output)),
                (None, None, Some(total)) => (Some(total), None),
                (None, None, None) => (None, None),
            };

        LLMUsage {
            input_tokens,
            output_tokens: output,
            cache_creation_input_tokens: (cache_write > 0).then_some(cache_write),
            cache_read_input_tokens: (cached > 0).then_some(cached),
            reasoning_tokens: reasoning,
            input_cost,
            output_cost,
        }
    }

    fn parse_completion_usage(usage: &CompletionUsage) -> LLMUsage {
        match serde_json::to_value(usage) {
            Ok(value) => Self::parse_usage(&value),
            Err(_) => LLMUsage {
                input_tokens: Some(usage.prompt_tokens),
                output_tokens: Some(usage.completion_tokens),
                ..LLMUsage::new()
            },
        }
    }

    fn parse_response(combined_response: OpenAICombinedResponse) -> LLMResponse {
        let response = combined_response.response;
        if response.choices.is_empty() {
            return LLMResponse {
                content: Some("Error: API returned empty choices.".to_string()),
                finish_reason: "error".to_string(),
                tool_calls: vec![],
                usage: LLMUsage::new(),
                reasoning_content: None,
                thinking_blocks: None,
            };
        }

        let choice0 = &response.choices[0];
        let mut content = choice0.message.content.clone();
        let reasoning_content = Self::extract_reasoning_content(&combined_response.raw_json);

        let mut finish_reason = choice0
            .finish_reason
            .map(|r| {
                serde_json::to_value(r)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_else(|| "stop".to_string())
            })
            .unwrap_or_else(|| "stop".to_string());

        // Collect tool calls across all choices (mirrors the Python loop)
        let mut raw_tool_calls = vec![];
        for ch in &response.choices {
            if let Some(tcs) = &ch.message.tool_calls {
                if !tcs.is_empty() {
                    raw_tool_calls.extend(tcs.iter());
                    if matches!(
                        ch.finish_reason,
                        Some(FinishReason::ToolCalls) | Some(FinishReason::Stop)
                    ) {
                        finish_reason = serde_json::to_value(ch.finish_reason)
                            .ok()
                            .and_then(|v| v.as_str().map(str::to_string))
                            .unwrap_or(finish_reason);
                    }
                }
            }
            if content.is_none() {
                content = ch.message.content.clone();
            }
        }

        // Parse tool calls
        let tool_calls = raw_tool_calls
            .into_iter()
            .filter_map(|tc| {
                let ChatCompletionMessageToolCalls::Function(tc) = tc else {
                    return None;
                };
                let args = Self::parse_tool_arguments(&tc.function.arguments);
                Some(ToolCallRequest {
                    id: Self::short_tool_id(),
                    name: tc.function.name.clone(),
                    arguments: args,
                    extra_content: None,
                    provider_specific_fields: None,
                    function_provider_specific_fields: None,
                })
            })
            .collect();

        let usage = combined_response
            .raw_json
            .get("usage")
            .map(Self::parse_usage)
            .or_else(|| response.usage.as_ref().map(Self::parse_completion_usage))
            .unwrap_or_else(LLMUsage::new);

        LLMResponse {
            content,
            tool_calls,
            finish_reason,
            usage,
            reasoning_content,
            thinking_blocks: None,
        }
    }

    /// Extract `reasoning_content` from the raw API JSON.
    ///
    /// Mirrors the Python parse path: prefer `message.reasoning_content`, then
    /// fall back to `message.reasoning` (via text extraction), then scan other choices.
    fn extract_reasoning_content(raw_json: &serde_json::Value) -> Option<String> {
        let response_map = maybe_mapping(raw_json)?;
        let choices = response_map.get("choices")?.as_array()?;
        if choices.is_empty() {
            return None;
        }

        let empty_map = serde_json::Map::new();
        let choice0 = maybe_mapping(&choices[0]).unwrap_or(&empty_map);
        let msg0 = choice0
            .get("message")
            .and_then(maybe_mapping)
            .unwrap_or(&empty_map);

        let mut reasoning_content = msg0
            .get("reasoning_content")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());

        if reasoning_content.is_none() {
            if let Some(reasoning) = msg0.get("reasoning") {
                reasoning_content =
                    Self::extract_text_content(reasoning.clone()).filter(|s| !s.is_empty());
            }
        }

        if reasoning_content.is_none() {
            for ch in choices {
                let ch_map = maybe_mapping(ch).unwrap_or(&empty_map);
                let m = ch_map
                    .get("message")
                    .and_then(maybe_mapping)
                    .unwrap_or(&empty_map);
                if let Some(rc) = m
                    .get("reasoning_content")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                {
                    reasoning_content = Some(rc.to_string());
                    break;
                }
            }
        }

        if let Some(reasoning_content) = reasoning_content.clone() {
            log::info!("reasoning_content: {}", reasoning_content);
        }
        reasoning_content
    }

    fn extract_text_content(value: serde_json::Value) -> Option<String> {
        if value.is_string() {
            Some(value.as_str().unwrap_or("").to_string())
        } else if value.is_array() {
            let mut parts: Vec<String> = vec![];
            for item in value.as_array().unwrap() {
                let item_map_option = maybe_mapping(item);
                if let Some(item_map) = item_map_option {
                    let text_option = item_map.get("text");
                    if let Some(text) = text_option
                        && text.is_string()
                    {
                        parts.push(text.as_str().unwrap_or("").to_string());
                        continue;
                    }
                }
                if item.is_string() {
                    parts.push(item.as_str().unwrap_or("").to_string());
                    continue;
                }
            }
            Some(parts.join(""))
        } else {
            None
        }
    }

    fn parse_stream_response(
        content: String,
        finish_reason: String,
        raw_tool_calls: Vec<ChatCompletionMessageToolCalls>,
        usage: LLMUsage,
    ) -> LLMResponse {
        let tool_calls = raw_tool_calls
            .into_iter()
            .filter_map(|tc| {
                let ChatCompletionMessageToolCalls::Function(tc) = tc else {
                    return None;
                };
                let args = Self::parse_tool_arguments(&tc.function.arguments);
                Some(ToolCallRequest {
                    id: Self::short_tool_id(),
                    name: tc.function.name.clone(),
                    arguments: args,
                    extra_content: None,
                    provider_specific_fields: None,
                    function_provider_specific_fields: None,
                })
            })
            .collect();

        LLMResponse {
            content: if content.is_empty() {
                None
            } else {
                Some(content)
            },
            finish_reason,
            tool_calls,
            usage,
            reasoning_content: None,
            thinking_blocks: None,
        }
    }

    /// Normalize streaming chunks so repeated snapshots or overlaps do not
    /// duplicate already-emitted content.
    fn non_overlapping_suffix<'a>(existing: &str, incoming: &'a str) -> &'a str {
        if existing.is_empty() || incoming.is_empty() {
            return incoming;
        }
        let max_overlap = existing.len().min(incoming.len());
        for overlap in (1..=max_overlap).rev() {
            let existing_start = existing.len() - overlap;
            if !existing.is_char_boundary(existing_start) || !incoming.is_char_boundary(overlap) {
                continue;
            }
            if existing[existing_start..] == incoming[..overlap] {
                return &incoming[overlap..];
            }
        }
        incoming
    }

    /// Pull a provider `error.message` out of a raw error string or JSON body.
    ///
    /// OpenRouter (and similar gateways) often return
    /// `{"error":{"message":"...","code":404}}` as a stream event that
    /// async-openai cannot deserialize as a chat chunk. The SDK then wraps
    /// that JSON in `OpenAIError::JSONDeserialize`.
    fn extract_api_error_message(raw: &str) -> String {
        if let Some(msg) = Self::api_error_message_from_json(raw) {
            return msg;
        }
        if let Some(start) = raw.find('{') {
            if let Some(msg) = Self::api_error_message_from_json(&raw[start..]) {
                return msg;
            }
        }
        raw.to_string()
    }

    fn api_error_message_from_json(raw: &str) -> Option<String> {
        let json: serde_json::Value = serde_json::from_str(raw).ok()?;
        let error = json.get("error")?;
        error
            .get("message")
            .and_then(|m| m.as_str())
            .map(str::to_string)
            .or_else(|| error.as_str().map(str::to_string))
            .filter(|s| !s.is_empty())
    }

    fn llm_response_from_openai_error(err: OpenAIError) -> LLMResponse {
        let message = match &err {
            OpenAIError::JSONDeserialize(_, content) => Self::extract_api_error_message(content),
            OpenAIError::ApiError(api) => api.message.clone(),
            other => Self::extract_api_error_message(&other.to_string()),
        };
        LLMResponse {
            content: Some(message),
            finish_reason: "error".to_string(),
            tool_calls: Vec::new(),
            usage: LLMUsage::new(),
            reasoning_content: None,
            thinking_blocks: None,
        }
    }
}

impl LLMProvider for OpenAICompatProvider {
    fn new(
        api_key: Option<String>,
        api_base: Option<String>,
        default_model: Option<String>,
        extra_headers: Option<HashMap<String, String>>,
        spec: Option<ProviderSpec>,
    ) -> Self {
        // Defaults based on Python signature.
        // In the Rust implementation, additional fields like default_model, extra_headers, spec,
        // client, etc., must be handled in the struct definition, but are omitted/handled elsewhere for clarity.
        let default_model =
            default_model.unwrap_or_else(|| OpenAICompatProvider::DEFAULT_MODEL.to_string());
        let extra_headers: std::collections::HashMap<String, String> =
            extra_headers.unwrap_or_default();
        let spec: Option<ProviderSpec> = spec.clone();

        // Compute effective_base.
        let effective_base = api_base
            .clone()
            .or_else(|| spec.as_ref().and_then(|s| s.default_api_base.clone()))
            .or(None);

        // Default headers.
        use uuid::Uuid;
        let mut default_headers: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        default_headers.insert(
            "x-session-affinity".to_string(),
            Uuid::new_v4().simple().to_string(),
        );

        if Self::uses_openrouter_attribution(spec.as_ref(), effective_base.as_deref()) {
            // _DEFAULT_OPENROUTER_HEADERS must be defined on struct
            for (k, v) in Self::default_openrouter_headers() {
                default_headers.insert(k.to_string(), v.to_string());
            }
        }

        // Merge extra_headers if any
        for (k, v) in extra_headers.iter() {
            default_headers.insert(k.clone(), v.clone());
        }

        // Build OpenAIConfig with api_key, optional base URL, and all merged headers.
        let mut config = OpenAIConfig::new();
        if let Some(ref key) = api_key {
            config = config.with_api_key(key);
        }
        if let Some(ref base) = effective_base {
            config = config.with_api_base(base);
        }
        for (k, v) in &default_headers {
            use reqwest::header::HeaderName;
            let header_name = HeaderName::from_bytes(k.as_bytes()).unwrap_or_else(|e| {
                log::error!("Invalid HTTP header name '{}': {}", k, e);
                panic!("Invalid HTTP header name '{}': {}", k, e);
            });
            config = config
                .with_header(header_name, v.as_str())
                .unwrap_or_else(|e| {
                    log::error!("Invalid HTTP header value for '{}': {}", k, e);
                    panic!("Invalid HTTP header value for '{}': {}", k, e);
                });
        }
        let client = Client::with_config(config);

        // Build a raw reqwest client with the same headers for the fallback
        // JSON path that strips unknown fields before typed deserialization.
        let mut header_map = reqwest::header::HeaderMap::new();
        if let Some(ref key) = api_key {
            use reqwest::header::{AUTHORIZATION, HeaderValue};
            if let Ok(val) = HeaderValue::from_str(&format!("Bearer {key}")) {
                header_map.insert(AUTHORIZATION, val);
            }
        }
        for (k, v) in &default_headers {
            use reqwest::header::{HeaderName, HeaderValue};
            if let (Ok(name), Ok(val)) = (
                HeaderName::from_bytes(k.as_bytes()),
                HeaderValue::from_str(v),
            ) {
                header_map.insert(name, val);
            }
        }
        let http_client = reqwest::Client::builder()
            .default_headers(header_map)
            .build()
            .unwrap_or_default();

        let chat_completions_url = format!(
            "{}/chat/completions",
            effective_base
                .as_deref()
                .unwrap_or("https://api.openai.com/v1")
                .trim_end_matches('/')
        );

        let provider = Self {
            api_key,
            api_base: effective_base,
            default_model: Some(default_model),
            extra_headers,
            spec: spec,
            generation: GenerationSettings::new(),
            client,
            http_client,
            chat_completions_url,
        };

        // Setup environment if appropriate.
        provider.setup_env(provider.api_key.clone(), provider.api_base.clone());

        provider
    }

    fn api_key(&self) -> Option<String> {
        self.api_key.clone()
    }

    fn api_base(&self) -> Option<String> {
        self.api_base.clone()
    }

    fn generation_settings(&self) -> &GenerationSettings {
        &self.generation
    }

    fn generation_settings_mut(&mut self) -> &mut GenerationSettings {
        &mut self.generation
    }

    fn extra_headers(&self) -> Option<HashMap<String, String>> {
        Some(self.extra_headers.clone())
    }

    fn spec(&self) -> Option<&ProviderSpec> {
        self.spec.as_ref()
    }

    fn get_default_model(&self) -> String {
        self.default_model
            .clone()
            .unwrap_or_else(|| OpenAICompatProvider::DEFAULT_MODEL.to_string())
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
    ) -> crate::providers::base::LLMResponse {
        let request_args = self.build_request(
            &messages,
            tools,
            model,
            max_tokens,
            temperature,
            reasoning_effort,
            tool_choice,
        );
        match request_args.build() {
            Ok(request) => match self.chat_raw(&request).await {
                Ok(response) => OpenAICompatProvider::parse_response(response),
                Err(e) => LLMResponse {
                    content: Some(e),
                    finish_reason: "error".to_string(),
                    tool_calls: Vec::new(),
                    usage: LLMUsage::new(),
                    reasoning_content: None,
                    thinking_blocks: None,
                },
            },
            Err(e) => OpenAICompatProvider::handle_error(Box::new(e)),
        }
    }

    #[allow(unused_variables)]
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
        let mut request_args = self.build_request(
            &messages,
            tools,
            model,
            max_tokens,
            temperature,
            reasoning_effort,
            tool_choice,
        );
        request_args.stream_options(ChatCompletionStreamOptions {
            include_usage: Some(true),
            include_obfuscation: None,
        });
        // TODO: Uncomment this when we have a way to log the request
        // log::debug!("chat stream request: {:?}", request_args);
        match request_args.build() {
            Ok(request) => {
                match self.client.chat().create_stream(request).await {
                    Ok(mut stream) => {
                        let mut content_buf = String::new();
                        let mut finish_reason = "stop".to_string();
                        let mut usage = LLMUsage::new();
                        // Streaming tool calls arrive as fragments keyed by `index`:
                        // the first fragment for an index carries the id + function
                        // name; later fragments for the same index carry only pieces
                        // of the JSON `arguments` string that must be concatenated.
                        // Accumulate by index here and assemble complete tool calls
                        // once the stream finishes. A BTreeMap preserves index order.
                        let mut tool_call_acc: std::collections::BTreeMap<
                            u32,
                            (Option<String>, String, String),
                        > = std::collections::BTreeMap::new();
                        let cb = on_content_delta.as_ref();
                        while let Some(chunk) = stream.next().await {
                            let chunk = match chunk {
                                Ok(chunk) => chunk,
                                Err(e) => {
                                    return Self::llm_response_from_openai_error(e);
                                }
                            };
                            if let Some(ref chunk_usage) = chunk.usage {
                                usage = Self::parse_completion_usage(chunk_usage);
                            }
                            for choice in &chunk.choices {
                                if let Some(delta_content) = &choice.delta.content {
                                    let normalized = Self::non_overlapping_suffix(
                                        &content_buf,
                                        delta_content.as_str(),
                                    );
                                    if !normalized.is_empty() {
                                        content_buf.push_str(normalized);
                                        if let Some(cb) = cb {
                                            cb(normalized.to_string()).await;
                                        }
                                    }
                                }
                                if let Some(ref tcs) = choice.delta.tool_calls {
                                    for tc in tcs {
                                        // (id, name, arguments) accumulated per index.
                                        let entry =
                                            tool_call_acc.entry(tc.index).or_insert_with(|| {
                                                (None, String::new(), String::new())
                                            });
                                        if let Some(id) = &tc.id {
                                            if !id.is_empty() {
                                                entry.0 = Some(id.clone());
                                            }
                                        }
                                        if let Some(func) = &tc.function {
                                            if let Some(name) = &func.name {
                                                if !name.is_empty() {
                                                    entry.1 = name.clone();
                                                }
                                            }
                                            if let Some(args) = &func.arguments {
                                                entry.2.push_str(args);
                                            }
                                        }
                                    }
                                }
                                if let Some(reason) = choice.finish_reason {
                                    finish_reason = serde_json::to_value(reason)
                                        .ok()
                                        .and_then(|v| v.as_str().map(str::to_string))
                                        .unwrap_or(finish_reason.clone());
                                }
                            }
                        }
                        let raw_tool_calls: Vec<ChatCompletionMessageToolCalls> = tool_call_acc
                            .into_values()
                            .map(|(id, name, arguments)| {
                                ChatCompletionMessageToolCalls::Function(
                                    ChatCompletionMessageToolCall {
                                        id: id.unwrap_or_else(Self::short_tool_id),
                                        function: FunctionCall { name, arguments },
                                    },
                                )
                            })
                            .collect();
                        return Self::parse_stream_response(
                            content_buf,
                            finish_reason,
                            raw_tool_calls,
                            usage,
                        );
                    }
                    Err(e) => {
                        return Self::llm_response_from_openai_error(e);
                    }
                }
            }
            Err(e) => {
                return OpenAICompatProvider::handle_error(Box::new(e));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_short_tool_id() {
        let id = OpenAICompatProvider::short_tool_id();
        assert_eq!(id.len(), 9);
        assert!(id.chars().all(|c| OpenAICompatProvider::ALNUM.contains(c)));
    }

    #[test]
    fn test_get_value() {
        let obj = serde_json::json!({ "key": "value" });
        let value = OpenAICompatProvider::get_value(&obj, "key");
        assert_eq!(value, Some(serde_json::json!("value")));
    }

    #[test]
    fn test_coerce_null() {
        let value = serde_json::json!(null);
        let result = OpenAICompatProvider::coerce_map(&value);
        assert_eq!(result, None);
    }

    #[test]
    fn test_coerce_object() {
        let value = serde_json::json!({ "key": "value" });
        let result = OpenAICompatProvider::coerce_map(&value);
        let check = serde_json::json!({ "key": "value" })
            .as_object()
            .unwrap()
            .clone();
        assert_eq!(result, Some(check));
    }

    #[test]
    fn test_extract_tc_extras() {
        let tc = serde_json::json!({ "id": "123", "type": "function", "index": 0, "provider_specific_fields": { "key": "value" }, "function": { "name": "test", "arguments": { "key": "value" }, "provider_specific_fields": { "key": "value" } } });
        let (extra_content, prov, fn_prov) = OpenAICompatProvider::extract_tc_extras(&tc);
        assert_eq!(extra_content, None);
        let prov_check = OpenAICompatProvider::coerce_map(
            &serde_json::json!({ "provider_specific_fields": { "key": "value" } }),
        )
        .unwrap();
        println!("prov: {:?}", prov);
        println!("prov_check: {:?}", prov_check);
        assert_eq!(prov, Some(prov_check));
        let fn_prov_check = OpenAICompatProvider::coerce_map(
            &serde_json::json!({ "provider_specific_fields": { "key": "value" } }),
        )
        .unwrap();
        assert_eq!(fn_prov, Some(fn_prov_check));
    }

    #[test]
    fn test_uses_openrouter_attribution_false() {
        let spec = ProviderSpec::default();
        assert!(!OpenAICompatProvider::uses_openrouter_attribution(
            Some(&spec),
            None
        ));
    }

    #[test]
    fn test_uses_openrouter_attribution_true() {
        let spec = ProviderSpec {
            name: "openrouter".to_string(),
            keywords: Vec::new(),
            env_key: "".to_string(),
            display_name: "".to_string(),
            backend: "".to_string(),
            env_extras: Vec::new(),
            is_gateway: false,
            is_local: false,
            detect_by_key_prefix: "".to_string(),
            detect_by_base_keyword: "".to_string(),
            default_api_base: Some("".to_string()),
            strip_model_prefix: false,
            model_overrides: Vec::new(),
            is_oauth: false,
            is_direct: false,
            supports_prompt_caching: false,
            supports_max_completion_tokens: false,
        };
        assert!(OpenAICompatProvider::uses_openrouter_attribution(
            Some(&spec),
            None
        ));
    }

    #[test]
    fn test_sanitize_messages() {
        let messages = vec![
            serde_json::json!({
                "role": "system",
                "content": "You are a helpful assistant.",
                "foo": "bar"
            }),
            serde_json::json!({
                "role": "user",
                "content": "Hello, how are you?",
                "foo": "bar"
            }),
        ];
        let sanitized = OpenAICompatProvider::sanitize_messages(&messages);
        assert_eq!(sanitized.len(), 2);
        assert_eq!(
            sanitized[0],
            serde_json::json!({ "role": "system", "content": "You are a helpful assistant." })
        );
        assert_eq!(
            sanitized[1],
            serde_json::json!({ "role": "user", "content": "Hello, how are you?" })
        );
    }

    #[test]
    fn test_parse_tool_arguments_valid_json() {
        let args = OpenAICompatProvider::parse_tool_arguments(r#"{"path":"a.txt","content":"hi"}"#);
        assert_eq!(
            args.get("path").and_then(serde_json::Value::as_str),
            Some("a.txt")
        );
        assert_eq!(
            args.get("content").and_then(serde_json::Value::as_str),
            Some("hi")
        );
        assert!(
            args.get(OpenAICompatProvider::ARG_PARSE_ERROR_KEY)
                .is_none()
        );
    }

    #[test]
    fn test_parse_tool_arguments_malformed_json_adds_markers() {
        let malformed = r#"{"path":"a.txt","content":"hi""#;
        let args = OpenAICompatProvider::parse_tool_arguments(malformed);
        assert!(
            args.get(OpenAICompatProvider::ARG_PARSE_ERROR_KEY)
                .is_some()
        );
        assert_eq!(
            args.get(OpenAICompatProvider::ARG_PARSE_RAW_KEY)
                .and_then(serde_json::Value::as_str),
            Some(malformed)
        );
    }

    #[test]
    fn test_non_overlapping_suffix_keeps_true_delta() {
        let suffix = OpenAICompatProvider::non_overlapping_suffix("Hello ", "world");
        assert_eq!(suffix, "world");
    }

    #[test]
    fn test_non_overlapping_suffix_trims_cumulative_snapshot() {
        let suffix = OpenAICompatProvider::non_overlapping_suffix("Hello", "Hello world");
        assert_eq!(suffix, " world");
    }

    #[test]
    fn test_non_overlapping_suffix_handles_repeated_chunk() {
        let suffix = OpenAICompatProvider::non_overlapping_suffix("Hello world", "world");
        assert_eq!(suffix, "");
    }

    #[test]
    fn test_system_message_content_from_string() {
        let content = serde_json::json!("You are a helpful assistant.");
        let converted = OpenAICompatProvider::system_message_content(Some(&content));
        assert_eq!(
            converted,
            ChatCompletionRequestSystemMessageContent::Text(
                "You are a helpful assistant.".to_string()
            )
        );
    }

    #[test]
    fn test_system_message_content_from_text_array_with_cache_control() {
        let content = serde_json::json!([
            {
                "type": "text",
                "text": "Cached system prompt",
                "cache_control": { "type": "ephemeral" }
            }
        ]);
        let converted = OpenAICompatProvider::system_message_content(Some(&content));
        match converted {
            ChatCompletionRequestSystemMessageContent::Array(parts) => {
                assert_eq!(parts.len(), 1);
                match &parts[0] {
                    ChatCompletionRequestSystemMessageContentPart::Text(part) => {
                        assert_eq!(part.text, "Cached system prompt");
                    }
                }
            }
            other => panic!("expected Array content, got {other:?}"),
        }
    }

    #[test]
    fn test_system_message_content_skips_empty_and_non_text_parts() {
        let content = serde_json::json!([
            { "type": "text", "text": "" },
            { "type": "image_url", "image_url": { "url": "https://example.com/x.png" } },
            { "type": "text", "text": "Keep me" }
        ]);
        let converted = OpenAICompatProvider::system_message_content(Some(&content));
        match converted {
            ChatCompletionRequestSystemMessageContent::Array(parts) => {
                assert_eq!(parts.len(), 1);
                match &parts[0] {
                    ChatCompletionRequestSystemMessageContentPart::Text(part) => {
                        assert_eq!(part.text, "Keep me");
                    }
                }
            }
            other => panic!("expected Array content, got {other:?}"),
        }
    }

    #[test]
    fn test_assistant_message_content_from_string() {
        let content = serde_json::json!("Hello from assistant");
        let converted = OpenAICompatProvider::assistant_message_content(Some(&content));
        assert_eq!(
            converted,
            Some(ChatCompletionRequestAssistantMessageContent::Text(
                "Hello from assistant".to_string()
            ))
        );
    }

    #[test]
    fn test_assistant_message_content_from_text_array_with_cache_control() {
        let content = serde_json::json!([
            {
                "type": "text",
                "text": "Cached assistant reply",
                "cache_control": { "type": "ephemeral" }
            }
        ]);
        let converted = OpenAICompatProvider::assistant_message_content(Some(&content));
        match converted {
            Some(ChatCompletionRequestAssistantMessageContent::Array(parts)) => {
                assert_eq!(parts.len(), 1);
                match &parts[0] {
                    ChatCompletionRequestAssistantMessageContentPart::Text(part) => {
                        assert_eq!(part.text, "Cached assistant reply");
                    }
                    other => panic!("expected Text part, got {other:?}"),
                }
            }
            other => panic!("expected Array content, got {other:?}"),
        }
    }

    #[test]
    fn test_assistant_message_content_empty_returns_none() {
        assert_eq!(
            OpenAICompatProvider::assistant_message_content(Some(&serde_json::json!(""))),
            None
        );
        assert_eq!(
            OpenAICompatProvider::assistant_message_content(Some(&serde_json::json!([]))),
            None
        );
        assert_eq!(OpenAICompatProvider::assistant_message_content(None), None);
    }

    fn sample_compat_response(annotations: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "id": "chatcmpl-test",
            "object": "chat.completion",
            "created": 1,
            "model": "vertex/gemini-3.7-flash@eu",
            "choices": [{
                "index": 0,
                "finish_reason": "stop",
                "message": {
                    "role": "assistant",
                    "content": "Here are some papers.",
                    "annotations": annotations
                }
            }]
        })
    }

    #[test]
    fn sanitize_compat_response_drops_gemini_annotation_variant() {
        let mut json = sample_compat_response(serde_json::json!([{
            "type": "annotation",
            "url": "https://arxiv.org/abs/1234.5678",
            "title": "Agentic AI",
            "start_index": 0,
            "end_index": 12
        }]));

        let before = serde_json::from_value::<CreateChatCompletionResponse>(json.clone());
        let err = before.expect_err("unsanitized Requesty/Gemini annotations should fail");
        assert!(
            err.to_string().contains("annotation"),
            "expected unknown annotation variant, got {err}"
        );

        json.as_object_mut()
            .unwrap()
            .insert("service_tier".into(), serde_json::json!("standard"));
        OpenAICompatProvider::sanitize_compat_response(&mut json);
        assert!(json.get("service_tier").is_none());
        assert!(json["choices"][0]["message"].get("annotations").is_none());

        let parsed = serde_json::from_value::<CreateChatCompletionResponse>(json)
            .expect("sanitized response should deserialize");
        assert_eq!(
            parsed.choices[0].message.content.as_deref(),
            Some("Here are some papers.")
        );
    }

    #[test]
    fn sanitize_compat_response_keeps_openai_url_citations() {
        let mut json = sample_compat_response(serde_json::json!([{
            "type": "url_citation",
            "url_citation": {
                "url": "https://arxiv.org/abs/1234.5678",
                "title": "Agentic AI",
                "start_index": 0,
                "end_index": 12
            }
        }]));

        OpenAICompatProvider::sanitize_compat_response(&mut json);
        let parsed = serde_json::from_value::<CreateChatCompletionResponse>(json)
            .expect("valid url_citation annotations should deserialize");
        assert_eq!(
            parsed.choices[0]
                .message
                .annotations
                .as_ref()
                .map(|a| a.len()),
            Some(1)
        );
    }

    #[test]
    fn extract_api_error_message_from_openrouter_json() {
        let raw =
            r#"{"error":{"message":"No endpoints found that support image input","code":404}}"#;
        assert_eq!(
            OpenAICompatProvider::extract_api_error_message(raw),
            "No endpoints found that support image input"
        );
    }

    #[test]
    fn extract_api_error_message_from_deserialize_wrapper() {
        let raw = r#"failed to deserialize api response: error:missing field content:{"error":{"message":"No endpoints found that support image input","code":404}}"#;
        assert_eq!(
            OpenAICompatProvider::extract_api_error_message(raw),
            "No endpoints found that support image input"
        );
    }

    #[test]
    fn llm_response_from_json_deserialize_error_uses_api_message() {
        let body =
            r#"{"error":{"message":"No endpoints found that support image input","code":404}}"#;
        let err = OpenAIError::JSONDeserialize(
            serde_json::from_str::<serde_json::Value>("not json").unwrap_err(),
            body.to_string(),
        );
        let response = OpenAICompatProvider::llm_response_from_openai_error(err);
        assert_eq!(response.finish_reason, "error");
        assert_eq!(
            response.content.as_deref(),
            Some("No endpoints found that support image input")
        );
    }

    #[test]
    fn parse_usage_basic_token_counts() {
        let usage = serde_json::json!({
            "prompt_tokens": 100,
            "completion_tokens": 25,
            "total_tokens": 125
        });
        let result = OpenAICompatProvider::parse_usage(&usage);
        assert_eq!(result.input_tokens, Some(100));
        assert_eq!(result.output_tokens, Some(25));
        assert_eq!(result.prompt_tokens(), Some(100));
        assert_eq!(result.total_tokens(), Some(125));
        assert!(result.cache_read_input_tokens.is_none());
        assert!(result.input_cost.is_none());
    }

    #[test]
    fn parse_usage_subtracts_cached_tokens_from_input() {
        let usage = serde_json::json!({
            "prompt_tokens": 500,
            "completion_tokens": 10,
            "prompt_tokens_details": {
                "cached_tokens": 400,
                "cache_write_tokens": 20
            },
            "completion_tokens_details": {
                "reasoning_tokens": 6
            }
        });
        let result = OpenAICompatProvider::parse_usage(&usage);
        assert_eq!(result.input_tokens, Some(80));
        assert_eq!(result.cache_read_input_tokens, Some(400));
        assert_eq!(result.cache_creation_input_tokens, Some(20));
        assert_eq!(result.reasoning_tokens, Some(6));
        assert_eq!(result.prompt_tokens(), Some(500));
        assert_eq!(result.total_tokens(), Some(510));
    }

    #[test]
    fn parse_usage_reads_openrouter_cost_split() {
        let usage = serde_json::json!({
            "prompt_tokens": 10,
            "completion_tokens": 5,
            "cost": 0.03,
            "cost_details": {
                "upstream_inference_prompt_cost": 0.01,
                "upstream_inference_completions_cost": 0.02
            }
        });
        let result = OpenAICompatProvider::parse_usage(&usage);
        assert_eq!(result.input_cost, Some(0.01));
        assert_eq!(result.output_cost, Some(0.02));
        assert_eq!(result.total_cost(), Some(0.03));
    }

    #[test]
    fn parse_usage_total_cost_without_split_goes_to_input_cost() {
        let usage = serde_json::json!({
            "prompt_tokens": 10,
            "completion_tokens": 5,
            "cost": 0.004
        });
        let result = OpenAICompatProvider::parse_usage(&usage);
        assert_eq!(result.input_cost, Some(0.004));
        assert!(result.output_cost.is_none());
        assert_eq!(result.total_cost(), Some(0.004));
    }

    #[test]
    fn parse_usage_missing_object_leaves_fields_none() {
        let result = OpenAICompatProvider::parse_usage(&serde_json::json!({}));
        assert!(result.input_tokens.is_none());
        assert!(result.output_tokens.is_none());
        assert!(result.prompt_tokens().is_none());
        assert!(result.total_cost().is_none());
    }

    #[test]
    fn parse_completion_usage_maps_typed_openai_usage() {
        let usage = CompletionUsage {
            prompt_tokens: 80,
            completion_tokens: 20,
            total_tokens: 100,
            ..Default::default()
        };
        let result = OpenAICompatProvider::parse_completion_usage(&usage);
        assert_eq!(result.input_tokens, Some(80));
        assert_eq!(result.output_tokens, Some(20));
        assert_eq!(result.total_tokens(), Some(100));
    }

    #[test]
    fn parse_stream_response_keeps_captured_usage() {
        let usage = LLMUsage {
            input_tokens: Some(12),
            output_tokens: Some(3),
            ..LLMUsage::new()
        };
        let response = OpenAICompatProvider::parse_stream_response(
            "hello".into(),
            "stop".into(),
            vec![],
            usage,
        );
        assert_eq!(response.content.as_deref(), Some("hello"));
        assert_eq!(response.usage.input_tokens, Some(12));
        assert_eq!(response.usage.output_tokens, Some(3));
    }

    #[test]
    fn build_request_omits_temperature_when_none() {
        let provider = OpenAICompatProvider::new(
            Some("test-key".to_string()),
            Some("https://example.com/v1".to_string()),
            Some("gpt-test".to_string()),
            None,
            None,
        );
        let request = provider
            .build_request(
                &[serde_json::json!({ "role": "user", "content": "hi" })],
                None,
                Some("gpt-test".to_string()),
                16,
                None,
                None,
                None,
            )
            .build()
            .expect("request should build");
        let json = serde_json::to_value(&request).expect("serialize request");
        assert!(
            json.get("temperature").is_none(),
            "temperature should be omitted, got {json}"
        );
    }

    #[test]
    fn build_request_includes_temperature_when_some() {
        let provider = OpenAICompatProvider::new(
            Some("test-key".to_string()),
            Some("https://example.com/v1".to_string()),
            Some("gpt-test".to_string()),
            None,
            None,
        );
        let request = provider
            .build_request(
                &[serde_json::json!({ "role": "user", "content": "hi" })],
                None,
                Some("gpt-test".to_string()),
                16,
                Some(0.5),
                None,
                None,
            )
            .build()
            .expect("request should build");
        let json = serde_json::to_value(&request).expect("serialize request");
        assert_eq!(json.get("temperature"), Some(&serde_json::json!(0.5)));
    }
}
