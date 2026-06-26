use crate::providers::{
    base::{GenerationSettings, LLMProvider, LLMResponse, ToolCallRequest},
    cache_control::apply_cache_control,
    registry::ProviderSpec,
};
use async_openai::types::chat::{
    ChatCompletionMessageToolCall, ChatCompletionMessageToolCalls,
    ChatCompletionRequestAssistantMessageArgs, ChatCompletionRequestMessage,
    ChatCompletionRequestMessageContentPartImage, ChatCompletionRequestMessageContentPartText,
    ChatCompletionRequestSystemMessageArgs, ChatCompletionRequestToolMessage,
    ChatCompletionRequestToolMessageContent, ChatCompletionRequestUserMessageArgs,
    ChatCompletionRequestUserMessageContent, ChatCompletionRequestUserMessageContentPart,
    ChatCompletionToolChoiceOption, ChatCompletionTools, CreateChatCompletionRequest,
    CreateChatCompletionRequestArgs, CreateChatCompletionResponse, FinishReason, FunctionCall,
};
use async_openai::types::chat::ImageUrl;
use async_openai::{Client, config::OpenAIConfig, types::chat::ReasoningEffort};
use futures::StreamExt;
use std::collections::HashMap;

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
                    "Failed to parse tool arguments: {}. Arguments length: {}", err, arguments_json.len()
                );
                let mut args = HashMap::new();
                let raw: String = arguments_json.chars().take(Self::ARG_PARSE_RAW_LIMIT).collect();
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

    fn build_request(
        &self,
        messages: &[serde_json::Value],
        tools: Option<Vec<serde_json::Value>>,
        model: Option<String>,
        max_tokens: usize,
        temperature: f32,
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
                        .content(content_str)
                        .build()
                        .ok()
                        .map(Into::into),
                    "assistant" => {
                        let mut builder = ChatCompletionRequestAssistantMessageArgs::default();
                        if !content_str.is_empty() {
                            builder.content(content_str);
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
        request.temperature(temperature);

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

    /// POST the request as raw JSON and deserialize with unknown `service_tier`
    /// variants stripped out.  This works around async-openai's strict `ServiceTier`
    /// enum that rejects values like `"standard"` returned by Anthropic's
    /// OpenAI-compatible endpoint.
    async fn chat_raw(
        &self,
        request: &CreateChatCompletionRequest,
    ) -> Result<CreateChatCompletionResponse, String> {
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

        // Strip fields that async-openai's typed structs cannot deserialize
        // (e.g. `service_tier: "standard"` from Anthropic).
        if let Some(obj) = json.as_object_mut() {
            obj.remove("service_tier");
        }

        serde_json::from_value(json).map_err(|e| format!("failed to deserialize api response: {e}"))
    }

    fn parse_response(response: CreateChatCompletionResponse) -> LLMResponse {
        if response.choices.is_empty() {
            return LLMResponse {
                content: Some("Error: API returned empty choices.".to_string()),
                finish_reason: "error".to_string(),
                tool_calls: vec![],
                usage: HashMap::new(),
                reasoning_content: None,
                thinking_blocks: None,
            };
        }
  
        let choice0 = &response.choices[0];
        let mut content = choice0.message.content.clone();
        let mut finish_reason = choice0.finish_reason
            .map(|r| serde_json::to_value(r).ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_else(|| "stop".to_string()))
            .unwrap_or_else(|| "stop".to_string());
  
        // Collect tool calls across all choices (mirrors the Python loop)
        let mut raw_tool_calls = vec![];
        for ch in &response.choices {
            if let Some(tcs) = &ch.message.tool_calls {
                if !tcs.is_empty() {
                    raw_tool_calls.extend(tcs.iter());
                    if matches!(ch.finish_reason, Some(FinishReason::ToolCalls) | Some(FinishReason::Stop)) {
                        finish_reason = serde_json::to_value(ch.finish_reason)
                            .ok().and_then(|v| v.as_str().map(str::to_string))
                            .unwrap_or(finish_reason);
                    }
                }
            }
            if content.is_none() {
                content = ch.message.content.clone();
            }
        }
  
        // Parse tool calls
        let tool_calls = raw_tool_calls.into_iter().filter_map(|tc| {
            let ChatCompletionMessageToolCalls::Function(tc) = tc else { return None; };
            let args = Self::parse_tool_arguments(&tc.function.arguments);
            Some(ToolCallRequest {
                id: Self::short_tool_id(),
                name: tc.function.name.clone(),
                arguments: args,
                extra_content: None,
                provider_specific_fields: None,
                function_provider_specific_fields: None,
            })
        }).collect();
        
        let mut usage_map = HashMap::new();
        match response.usage {
            Some(usage) => {
                usage_map.insert("prompt_tokens".to_string(), usage.prompt_tokens as u64);
                usage_map.insert("completion_tokens".to_string(), usage.completion_tokens as u64);
                usage_map.insert("total_tokens".to_string(), usage.total_tokens as u64);
            }
            None => {
                usage_map.insert("prompt_tokens".to_string(), 0);
                usage_map.insert("completion_tokens".to_string(), 0);
                usage_map.insert("total_tokens".to_string(), 0);
            }
        }
  
        LLMResponse {
            content,
            tool_calls,
            finish_reason,
            usage: usage_map.clone(),
            reasoning_content: None,
            thinking_blocks: None,
        }
    }

    fn parse_stream_response(
        content: String,
        finish_reason: String,
        raw_tool_calls: Vec<ChatCompletionMessageToolCalls>,
    ) -> LLMResponse {
        let tool_calls = raw_tool_calls.into_iter().filter_map(|tc| {
            let ChatCompletionMessageToolCalls::Function(tc) = tc else { return None; };
            let args = Self::parse_tool_arguments(&tc.function.arguments);
            Some(ToolCallRequest {
                id: Self::short_tool_id(),
                name: tc.function.name.clone(),
                arguments: args,
                extra_content: None,
                provider_specific_fields: None,
                function_provider_specific_fields: None,
            })
        }).collect();

        LLMResponse {
            content: if content.is_empty() { None } else { Some(content) },
            finish_reason,
            tool_calls,
            usage: HashMap::new(),
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
        let extra_headers: std::collections::HashMap<String, String> = extra_headers.unwrap_or_default();
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
        temperature: f32,
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
                    usage: HashMap::new(),
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
        temperature: f32,
        reasoning_effort: Option<String>,
        tool_choice: Option<serde_json::Value>,
        on_content_delta: &Option<F>,
    ) -> LLMResponse
    where
        F: Fn(String) -> Fut + Send + Sync,
        Fut: std::future::Future<Output = ()> + Send,
    {

        let request_args = self.build_request(
            &messages,
            tools,
            model,
            max_tokens,
            temperature,
            reasoning_effort,
            tool_choice,
        );
        log::debug!("chat stream request: {:?}", request_args);
        match request_args.build() {
            Ok(request) => {
                match self.client.chat().create_stream(request).await {
                    Ok(mut stream) => {
                        let mut content_buf = String::new();
                        let mut finish_reason = "stop".to_string();
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
                            if let Ok(chunk) = chunk {
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
                                            let entry = tool_call_acc
                                                .entry(tc.index)
                                                .or_insert_with(|| (None, String::new(), String::new()));
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
                        return Self::parse_stream_response(content_buf, finish_reason, raw_tool_calls);
                    }
                    Err(e) => {
                        return OpenAICompatProvider::handle_error(Box::new(e));
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
        assert!(args.get(OpenAICompatProvider::ARG_PARSE_ERROR_KEY).is_none());
    }

    #[test]
    fn test_parse_tool_arguments_malformed_json_adds_markers() {
        let malformed = r#"{"path":"a.txt","content":"hi""#;
        let args = OpenAICompatProvider::parse_tool_arguments(malformed);
        assert!(args.get(OpenAICompatProvider::ARG_PARSE_ERROR_KEY).is_some());
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
}
