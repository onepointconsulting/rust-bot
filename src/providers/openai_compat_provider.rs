use crate::providers::{
    base::{GenerationSettings, LLMProvider, LLMResponse, LLMUsage, ToolCallRequest},
    cache_control::apply_cache_control,
    registry::ProviderSpec,
};
use std::collections::HashMap;

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
    /// Shared reqwest client for chat completions (stream and non-stream).
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

    /// Keep non-empty text content parts, including extras such as `cache_control`.
    ///
    /// Used after [`apply_cache_control`] rewrites string content into
    /// `[{"type":"text","text":"...","cache_control":...}]`.
    fn text_content_parts(blocks: &[serde_json::Value]) -> Vec<serde_json::Value> {
        blocks
            .iter()
            .filter(|block| {
                let typ = block.get("type").and_then(|t| t.as_str());
                if typ.is_some() && typ != Some("text") {
                    return false;
                }
                !block
                    .get("text")
                    .and_then(|t| t.as_str())
                    .unwrap_or("")
                    .is_empty()
            })
            .cloned()
            .collect()
    }

    /// Convert a JSON `content` value into system-message content.
    ///
    /// Supports both plain strings and content-part arrays.
    fn system_message_content(content: Option<&serde_json::Value>) -> serde_json::Value {
        match content {
            Some(serde_json::Value::String(text)) => serde_json::Value::String(text.clone()),
            Some(serde_json::Value::Array(blocks)) => {
                let parts = Self::text_content_parts(blocks);
                if parts.is_empty() {
                    serde_json::Value::String(String::new())
                } else {
                    serde_json::Value::Array(parts)
                }
            }
            _ => serde_json::Value::String(String::new()),
        }
    }

    /// Convert a JSON `content` value into assistant-message content.
    ///
    /// Returns `None` when content is empty so tool-call-only assistant turns
    /// can omit `content`. Supports both plain strings and content-part arrays.
    fn assistant_message_content(content: Option<&serde_json::Value>) -> Option<serde_json::Value> {
        match content {
            Some(serde_json::Value::String(text)) if !text.is_empty() => {
                Some(serde_json::Value::String(text.clone()))
            }
            Some(serde_json::Value::Array(blocks)) => {
                let parts = Self::text_content_parts(blocks);
                if parts.is_empty() {
                    None
                } else {
                    Some(serde_json::Value::Array(parts))
                }
            }
            _ => None,
        }
    }

    fn user_message_content(content: Option<&serde_json::Value>) -> serde_json::Value {
        if let Some(arr) = content.and_then(|c| c.as_array()) {
            let parts: Vec<serde_json::Value> = arr
                .iter()
                .filter(|block| match block.get("type").and_then(|t| t.as_str()) {
                    Some("text") => true,
                    Some("image_url") => block
                        .get("image_url")
                        .and_then(|u| u.get("url"))
                        .and_then(|u| u.as_str())
                        .is_some_and(|url| !url.is_empty()),
                    _ => false,
                })
                .cloned()
                .collect();
            if parts.is_empty() {
                serde_json::Value::String(String::new())
            } else {
                serde_json::Value::Array(parts)
            }
        } else if let Some(text) = content.and_then(|c| c.as_str()) {
            serde_json::Value::String(text.to_string())
        } else {
            serde_json::Value::String(String::new())
        }
    }

    fn assistant_tool_calls(tool_calls: Option<&serde_json::Value>) -> Option<serde_json::Value> {
        let arr = tool_calls.and_then(|v| v.as_array())?;
        let tcs: Vec<serde_json::Value> = arr
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
                let mut obj = serde_json::json!({
                    "id": id,
                    "type": "function",
                    "function": { "name": name, "arguments": arguments }
                });
                if let Some(extra) = tc.get("extra_content") {
                    obj["extra_content"] = extra.clone();
                }
                Some(obj)
            })
            .collect();
        if tcs.is_empty() {
            None
        } else {
            Some(serde_json::Value::Array(tcs))
        }
    }

    fn map_request_message(msg: &serde_json::Value) -> Option<serde_json::Value> {
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("user");
        let mut out = serde_json::Map::new();
        out.insert("role".into(), serde_json::json!(role));

        for key in ["name", "reasoning_content", "extra_content"] {
            if let Some(value) = msg.get(key) {
                if !value.is_null() {
                    out.insert(key.to_string(), value.clone());
                }
            }
        }

        match role {
            "system" => {
                out.insert(
                    "content".into(),
                    Self::system_message_content(msg.get("content")),
                );
            }
            "assistant" => {
                if let Some(content) = Self::assistant_message_content(msg.get("content")) {
                    out.insert("content".into(), content);
                }
                if let Some(tcs) = Self::assistant_tool_calls(msg.get("tool_calls")) {
                    out.insert("tool_calls".into(), tcs);
                }
            }
            "tool" => {
                let tool_content = match msg.get("content") {
                    Some(c) if c.is_string() => c.clone(),
                    Some(c) => serde_json::Value::String(c.to_string()),
                    None => serde_json::Value::String(String::new()),
                };
                out.insert("content".into(), tool_content);
                out.insert(
                    "tool_call_id".into(),
                    serde_json::Value::String(
                        msg.get("tool_call_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                    ),
                );
            }
            _ => {
                out.insert(
                    "content".into(),
                    Self::user_message_content(msg.get("content")),
                );
            }
        }

        Some(serde_json::Value::Object(out))
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
    ) -> serde_json::Value {
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
        let chat_messages: Vec<serde_json::Value> = sanitized_messages
            .iter()
            .filter_map(Self::map_request_message)
            .collect();

        let mut request = serde_json::json!({
            "model": model_name,
            "messages": chat_messages,
            "max_tokens": max_tokens as u32,
        });
        if let Some(temperature) = temperature {
            log::info!("temperature: {}", temperature);
            request["temperature"] = serde_json::json!(temperature);
        }

        let effective_tools = cached_tools.or(tools);
        if let Some(tool_list) = effective_tools {
            if !tool_list.is_empty() {
                request["tools"] = serde_json::Value::Array(tool_list);
            }
        }

        if let Some(effort) = reasoning_effort {
            if !effort.is_empty() {
                request["reasoning_effort"] = serde_json::Value::String(effort);
            }
        }
        if let Some(tc) = tool_choice {
            request["tool_choice"] = tc;
        }

        request
    }

    fn apply_stream_flags(body: &mut serde_json::Value) {
        body["stream"] = serde_json::json!(true);
        body["stream_options"] = serde_json::json!({ "include_usage": true });
    }

    /// POST the request as raw JSON. The body is parsed as a `Value` so
    /// unknown gateway fields do not have to match a fixed response schema.
    async fn chat_raw(&self, request: &serde_json::Value) -> Result<serde_json::Value, String> {
        let resp = self
            .http_client
            .post(&self.chat_completions_url)
            .header("Content-Type", "application/json")
            .json(request)
            .send()
            .await
            .map_err(|e| e.to_string())?;

        let status = resp.status();
        let json: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;

        if !status.is_success() {
            return Err(format!("HTTP {status}: {json}"));
        }

        Ok(json)
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
    pub(crate) fn parse_usage(usage: &serde_json::Value) -> LLMUsage {
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

    fn parse_response(raw_json: &serde_json::Value) -> LLMResponse {
        let Some(choices) = raw_json
            .get("choices")
            .and_then(|c| c.as_array())
            .filter(|c| !c.is_empty())
        else {
            return LLMResponse {
                content: Some("Error: API returned empty choices.".to_string()),
                finish_reason: "error".to_string(),
                tool_calls: vec![],
                usage: LLMUsage::new(),
                reasoning_content: None,
                thinking_blocks: None,
            };
        };

        let empty_map = serde_json::Map::new();
        let choice0 = maybe_mapping(&choices[0]).unwrap_or(&empty_map);
        let mut content = choice0
            .get("message")
            .and_then(Self::extract_message_content);
        let mut finish_reason =
            Self::json_finish_reason(choice0).unwrap_or_else(|| "stop".to_string());

        let mut tool_calls = Vec::new();
        for ch in choices {
            let ch_map = maybe_mapping(ch).unwrap_or(&empty_map);
            let msg = ch_map.get("message").unwrap_or(&serde_json::Value::Null);
            if let Some(tcs) = msg.get("tool_calls").and_then(|v| v.as_array()) {
                if !tcs.is_empty() {
                    for tc in tcs {
                        if let Some(parsed) = Self::parse_json_tool_call(tc) {
                            tool_calls.push(parsed);
                        }
                    }
                    if let Some(reason) = Self::json_finish_reason(ch_map) {
                        if reason == "tool_calls" || reason == "stop" {
                            finish_reason = reason;
                        }
                    }
                }
            }
            if content.is_none() {
                content = Self::extract_message_content(msg);
            }
        }

        LLMResponse {
            content,
            tool_calls,
            finish_reason,
            usage: raw_json
                .get("usage")
                .map(Self::parse_usage)
                .unwrap_or_else(LLMUsage::new),
            reasoning_content: Self::extract_reasoning_content(raw_json),
            thinking_blocks: None,
        }
    }

    pub(crate) fn json_finish_reason(
        choice: &serde_json::Map<String, serde_json::Value>,
    ) -> Option<String> {
        choice
            .get("finish_reason")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    }

    fn extract_message_content(message: &serde_json::Value) -> Option<String> {
        Self::extract_text_content(message.get("content")?).filter(|s| !s.is_empty())
    }

    fn parse_json_tool_call(tc: &serde_json::Value) -> Option<ToolCallRequest> {
        if let Some(kind) = tc.get("type").and_then(|t| t.as_str()) {
            if kind != "function" {
                return None;
            }
        }
        let func = tc.get("function")?;
        let name = func.get("name")?.as_str()?.to_string();
        let arguments = match func.get("arguments") {
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(value) => serde_json::to_string(value).unwrap_or_default(),
            None => String::new(),
        };
        Some(ToolCallRequest {
            id: Self::short_tool_id(),
            name,
            arguments: Self::parse_tool_arguments(&arguments),
            extra_content: None,
            provider_specific_fields: None,
            function_provider_specific_fields: None,
        })
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
                reasoning_content = Self::extract_text_content(reasoning).filter(|s| !s.is_empty());
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

        if let Some(ref reasoning) = reasoning_content {
            log::info!("reasoning_content: {}", reasoning);
        }
        reasoning_content
    }

    pub(crate) fn extract_text_content(value: &serde_json::Value) -> Option<String> {
        if let Some(s) = value.as_str() {
            Some(s.to_string())
        } else if let Some(arr) = value.as_array() {
            let mut parts: Vec<String> = vec![];
            for item in arr {
                if let Some(item_map) = maybe_mapping(item) {
                    if let Some(text) = item_map.get("text").and_then(|t| t.as_str()) {
                        parts.push(text.to_string());
                        continue;
                    }
                }
                if let Some(s) = item.as_str() {
                    parts.push(s.to_string());
                }
            }
            Some(parts.join(""))
        } else {
            None
        }
    }

    pub(crate) fn parse_stream_response(
        content: String,
        finish_reason: String,
        tool_call_acc: std::collections::BTreeMap<u32, (Option<String>, String, String)>,
        usage: LLMUsage,
        reasoning_content: Option<String>,
    ) -> LLMResponse {
        let tool_calls = tool_call_acc
            .into_values()
            .filter_map(|(_id, name, arguments)| {
                if name.is_empty() {
                    return None;
                }
                Some(ToolCallRequest {
                    id: Self::short_tool_id(),
                    name,
                    arguments: Self::parse_tool_arguments(&arguments),
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
            reasoning_content: reasoning_content.filter(|s| !s.is_empty()),
            thinking_blocks: None,
        }
    }

    pub(crate) fn error_message_from_value(value: &serde_json::Value) -> Option<String> {
        let error = value.get("error")?;
        error
            .get("message")
            .and_then(|m| m.as_str())
            .map(str::to_string)
            .or_else(|| error.as_str().map(str::to_string))
            .filter(|s| !s.is_empty())
    }

    /// Normalize streaming chunks so repeated snapshots or full replays do not
    /// duplicate already-emitted content.
    ///
    /// Only two gateway quirks are stripped:
    /// - cumulative snapshot: `incoming` starts with everything already seen
    /// - exact replay: `incoming` is already the trailing text
    ///
    /// Accidental suffix/prefix overlaps ("Hel" + "lo") are true deltas and
    /// must be kept. Longest-partial-overlap would emit "Helo".
    pub(crate) fn non_overlapping_suffix<'a>(existing: &str, incoming: &'a str) -> &'a str {
        if existing.is_empty() || incoming.is_empty() {
            return incoming;
        }
        if incoming.starts_with(existing) {
            return &incoming[existing.len()..];
        }
        if existing.ends_with(incoming) {
            return "";
        }
        incoming
    }

    /// Pull a provider `error.message` out of a raw error string or JSON body.
    ///
    /// OpenRouter (and similar gateways) often return
    /// `{"error":{"message":"...","code":404}}` as a stream event or HTTP body.
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
        let request = self.build_request(
            &messages,
            tools,
            model,
            max_tokens,
            temperature,
            reasoning_effort,
            tool_choice,
        );
        match self.chat_raw(&request).await {
            Ok(response) => OpenAICompatProvider::parse_response(&response),
            Err(e) => LLMResponse {
                content: Some(e),
                finish_reason: "error".to_string(),
                tool_calls: Vec::new(),
                usage: LLMUsage::new(),
                reasoning_content: None,
                thinking_blocks: None,
            },
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
        on_progress: &Option<crate::providers::base::BoxedProgressCallback>,
    ) -> LLMResponse
    where
        F: Fn(String) -> Fut + Send + Sync,
        Fut: std::future::Future<Output = ()> + Send,
    {
        let mut body = self.build_request(
            &messages,
            tools,
            model,
            max_tokens,
            temperature,
            reasoning_effort,
            tool_choice,
        );
        Self::apply_stream_flags(&mut body);

        let response = match self
            .http_client
            .post(&self.chat_completions_url)
            .header("Content-Type", "application/json")
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .json(&body)
            .send()
            .await
        {
            Ok(response) => response,
            Err(e) => {
                return LLMResponse {
                    content: Some(e.to_string()),
                    finish_reason: "error".to_string(),
                    tool_calls: Vec::new(),
                    usage: LLMUsage::new(),
                    reasoning_content: None,
                    thinking_blocks: None,
                };
            }
        };

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            let message = Self::extract_api_error_message(&body);
            return LLMResponse {
                content: Some(if message.is_empty() {
                    format!("HTTP {status}: {body}")
                } else {
                    message
                }),
                finish_reason: "error".to_string(),
                tool_calls: Vec::new(),
                usage: LLMUsage::new(),
                reasoning_content: None,
                thinking_blocks: None,
            };
        }

        let (idle_timeout, idle_timeout_s) =
            crate::providers::openai_compat_stream::stream_idle_timeout();
        crate::providers::openai_compat_stream::consume_sse_byte_stream(
            response.bytes_stream(),
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
    fn test_non_overlapping_suffix_keeps_boundary_letter_overlap() {
        let suffix = OpenAICompatProvider::non_overlapping_suffix("Hel", "lo");
        assert_eq!(suffix, "lo");
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
        assert_eq!(converted, serde_json::json!("You are a helpful assistant."));
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
        assert_eq!(
            converted,
            serde_json::json!([{
                "type": "text",
                "text": "Cached system prompt",
                "cache_control": { "type": "ephemeral" }
            }])
        );
    }

    #[test]
    fn test_system_message_content_skips_empty_and_non_text_parts() {
        let content = serde_json::json!([
            { "type": "text", "text": "" },
            { "type": "image_url", "image_url": { "url": "https://example.com/x.png" } },
            { "type": "text", "text": "Keep me" }
        ]);
        let converted = OpenAICompatProvider::system_message_content(Some(&content));
        assert_eq!(
            converted,
            serde_json::json!([{ "type": "text", "text": "Keep me" }])
        );
    }

    #[test]
    fn test_assistant_message_content_from_string() {
        let content = serde_json::json!("Hello from assistant");
        let converted = OpenAICompatProvider::assistant_message_content(Some(&content));
        assert_eq!(converted, Some(serde_json::json!("Hello from assistant")));
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
        assert_eq!(
            converted,
            Some(serde_json::json!([{
                "type": "text",
                "text": "Cached assistant reply",
                "cache_control": { "type": "ephemeral" }
            }]))
        );
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
    fn parse_response_empty_choices_is_error() {
        let json = serde_json::json!({
            "id": "chatcmpl-test",
            "choices": []
        });
        let response = OpenAICompatProvider::parse_response(&json);
        assert_eq!(response.finish_reason, "error");
        assert_eq!(
            response.content.as_deref(),
            Some("Error: API returned empty choices.")
        );
        assert!(response.tool_calls.is_empty());
        assert!(response.reasoning_content.is_none());
    }

    #[test]
    fn parse_response_missing_choices_is_error() {
        let json = serde_json::json!({ "id": "chatcmpl-test" });
        let response = OpenAICompatProvider::parse_response(&json);
        assert_eq!(response.finish_reason, "error");
        assert_eq!(
            response.content.as_deref(),
            Some("Error: API returned empty choices.")
        );
    }

    #[test]
    fn parse_response_string_content_and_reasoning_content() {
        let json = serde_json::json!({
            "choices": [{
                "finish_reason": "stop",
                "message": {
                    "role": "assistant",
                    "content": "hello",
                    "reasoning_content": "think first"
                }
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 4
            }
        });
        let response = OpenAICompatProvider::parse_response(&json);
        assert_eq!(response.content.as_deref(), Some("hello"));
        assert_eq!(response.reasoning_content.as_deref(), Some("think first"));
        assert_eq!(response.finish_reason, "stop");
        assert_eq!(response.usage.input_tokens, Some(10));
        assert_eq!(response.usage.output_tokens, Some(4));
    }

    #[test]
    fn parse_response_falls_back_to_message_reasoning_string() {
        let json = serde_json::json!({
            "choices": [{
                "finish_reason": "stop",
                "message": {
                    "role": "assistant",
                    "content": "answer",
                    "reasoning": "step by step"
                }
            }]
        });
        let response = OpenAICompatProvider::parse_response(&json);
        assert_eq!(response.content.as_deref(), Some("answer"));
        assert_eq!(response.reasoning_content.as_deref(), Some("step by step"));
    }

    #[test]
    fn parse_response_falls_back_to_message_reasoning_text_parts() {
        let json = serde_json::json!({
            "choices": [{
                "finish_reason": "stop",
                "message": {
                    "role": "assistant",
                    "content": "answer",
                    "reasoning": [
                        {"type": "text", "text": "let me "},
                        {"type": "text", "text": "think"}
                    ]
                }
            }]
        });
        let response = OpenAICompatProvider::parse_response(&json);
        assert_eq!(response.reasoning_content.as_deref(), Some("let me think"));
    }

    #[test]
    fn parse_response_concatenates_array_content_parts() {
        let json = serde_json::json!({
            "choices": [{
                "finish_reason": "stop",
                "message": {
                    "role": "assistant",
                    "content": [
                        {"type": "text", "text": "Hello "},
                        "world"
                    ]
                }
            }]
        });
        let response = OpenAICompatProvider::parse_response(&json);
        assert_eq!(response.content.as_deref(), Some("Hello world"));
    }

    #[test]
    fn parse_response_parses_function_tool_calls_with_string_arguments() {
        let json = serde_json::json!({
            "choices": [{
                "finish_reason": "tool_calls",
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {
                            "name": "lookup",
                            "arguments": "{\"q\":\"rust\"}"
                        }
                    }]
                }
            }]
        });
        let response = OpenAICompatProvider::parse_response(&json);
        assert_eq!(response.finish_reason, "tool_calls");
        assert!(response.content.is_none());
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].name, "lookup");
        assert_eq!(
            response.tool_calls[0]
                .arguments
                .get("q")
                .and_then(|v| v.as_str()),
            Some("rust")
        );
        assert_eq!(response.tool_calls[0].id.len(), 9);
    }

    #[test]
    fn parse_response_stringifies_object_tool_call_arguments() {
        let json = serde_json::json!({
            "choices": [{
                "finish_reason": "tool_calls",
                "message": {
                    "role": "assistant",
                    "tool_calls": [{
                        "type": "function",
                        "function": {
                            "name": "lookup",
                            "arguments": {"q": "rust"}
                        }
                    }]
                }
            }]
        });
        let response = OpenAICompatProvider::parse_response(&json);
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].name, "lookup");
        assert_eq!(
            response.tool_calls[0]
                .arguments
                .get("q")
                .and_then(|v| v.as_str()),
            Some("rust")
        );
    }

    #[test]
    fn parse_response_ignores_unknown_gateway_fields() {
        let mut json = sample_compat_response(serde_json::json!([{
            "type": "annotation",
            "url": "https://arxiv.org/abs/1234.5678",
            "title": "Agentic AI",
            "start_index": 0,
            "end_index": 12
        }]));
        json.as_object_mut()
            .unwrap()
            .insert("service_tier".into(), serde_json::json!("standard"));

        let response = OpenAICompatProvider::parse_response(&json);
        assert_eq!(response.content.as_deref(), Some("Here are some papers."));
        assert_eq!(response.finish_reason, "stop");
        assert!(response.tool_calls.is_empty());
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
    fn parse_stream_response_keeps_captured_usage() {
        let usage = LLMUsage {
            input_tokens: Some(12),
            output_tokens: Some(3),
            ..LLMUsage::new()
        };
        let response = OpenAICompatProvider::parse_stream_response(
            "hello".into(),
            "stop".into(),
            std::collections::BTreeMap::new(),
            usage,
            Some("think".into()),
        );
        assert_eq!(response.content.as_deref(), Some("hello"));
        assert_eq!(response.reasoning_content.as_deref(), Some("think"));
        assert_eq!(response.usage.input_tokens, Some(12));
        assert_eq!(response.usage.output_tokens, Some(3));
    }

    fn test_provider() -> OpenAICompatProvider {
        OpenAICompatProvider::new(
            Some("test-key".to_string()),
            Some("https://example.com/v1".to_string()),
            Some("gpt-test".to_string()),
            None,
            None,
        )
    }

    fn caching_provider() -> OpenAICompatProvider {
        OpenAICompatProvider::new(
            Some("test-key".to_string()),
            Some("https://example.com/v1".to_string()),
            Some("gpt-test".to_string()),
            None,
            Some(ProviderSpec {
                supports_prompt_caching: true,
                ..ProviderSpec::default()
            }),
        )
    }

    fn weather_tool() -> serde_json::Value {
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "get_weather",
                "description": "Get the current weather",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "location": { "type": "string" }
                    }
                }
            }
        })
    }

    #[test]
    fn build_request_omits_temperature_when_none() {
        let request = test_provider().build_request(
            &[serde_json::json!({ "role": "user", "content": "hi" })],
            None,
            Some("gpt-test".to_string()),
            16,
            None,
            None,
            None,
        );
        assert!(
            request.get("temperature").is_none(),
            "temperature should be omitted, got {request}"
        );
    }

    #[test]
    fn build_request_includes_temperature_when_some() {
        let request = test_provider().build_request(
            &[serde_json::json!({ "role": "user", "content": "hi" })],
            None,
            Some("gpt-test".to_string()),
            16,
            Some(0.5),
            None,
            None,
        );
        assert_eq!(request.get("temperature"), Some(&serde_json::json!(0.5)));
    }

    #[test]
    fn build_request_includes_tools_and_tool_choice() {
        let request = test_provider().build_request(
            &[serde_json::json!({ "role": "user", "content": "weather?" })],
            Some(vec![weather_tool()]),
            Some("gpt-test".to_string()),
            32,
            None,
            None,
            Some(serde_json::json!({
                "type": "function",
                "function": { "name": "get_weather" }
            })),
        );
        assert_eq!(request["tools"], serde_json::json!([weather_tool()]));
        assert_eq!(
            request["tool_choice"],
            serde_json::json!({
                "type": "function",
                "function": { "name": "get_weather" }
            })
        );
    }

    #[test]
    fn build_request_keeps_multimodal_image_url() {
        let request = test_provider().build_request(
            &[serde_json::json!({
                "role": "user",
                "content": [
                    { "type": "text", "text": "what is this?" },
                    {
                        "type": "image_url",
                        "image_url": { "url": "https://example.com/cat.png" }
                    }
                ]
            })],
            None,
            Some("gpt-test".to_string()),
            16,
            None,
            None,
            None,
        );
        assert_eq!(
            request["messages"][0]["content"],
            serde_json::json!([
                { "type": "text", "text": "what is this?" },
                {
                    "type": "image_url",
                    "image_url": { "url": "https://example.com/cat.png" }
                }
            ])
        );
    }

    #[test]
    fn build_request_preserves_cache_control_on_system_and_tools() {
        let request = caching_provider().build_request(
            &[
                serde_json::json!({ "role": "system", "content": "You are helpful." }),
                serde_json::json!({ "role": "user", "content": "hi" }),
            ],
            Some(vec![weather_tool()]),
            Some("gpt-test".to_string()),
            16,
            None,
            None,
            None,
        );
        assert_eq!(
            request["messages"][0]["content"],
            serde_json::json!([{
                "type": "text",
                "text": "You are helpful.",
                "cache_control": { "type": "ephemeral" }
            }])
        );
        assert_eq!(
            request["tools"][0]["cache_control"],
            serde_json::json!({ "type": "ephemeral" })
        );
    }

    #[test]
    fn build_request_includes_reasoning_effort_and_extra_keys() {
        let request = test_provider().build_request(
            &[
                serde_json::json!({ "role": "user", "content": "think" }),
                serde_json::json!({
                    "role": "assistant",
                    "content": "ok",
                    "reasoning_content": "step by step",
                    "extra_content": { "google": { "thought": true } }
                }),
            ],
            None,
            Some("gpt-test".to_string()),
            16,
            None,
            Some("high".to_string()),
            None,
        );
        assert_eq!(request["reasoning_effort"], serde_json::json!("high"));
        assert_eq!(
            request["messages"][1]["reasoning_content"],
            serde_json::json!("step by step")
        );
        assert_eq!(
            request["messages"][1]["extra_content"],
            serde_json::json!({ "google": { "thought": true } })
        );
    }

    #[test]
    fn apply_stream_flags_sets_stream_and_include_usage() {
        let mut body = test_provider().build_request(
            &[serde_json::json!({ "role": "user", "content": "hi" })],
            None,
            Some("gpt-test".to_string()),
            16,
            None,
            None,
            None,
        );
        OpenAICompatProvider::apply_stream_flags(&mut body);
        assert_eq!(body["stream"], serde_json::json!(true));
        assert_eq!(
            body["stream_options"],
            serde_json::json!({ "include_usage": true })
        );
    }
}
