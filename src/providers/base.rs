use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use futures::FutureExt;

#[derive(Debug, Clone, PartialEq)]
pub struct ToolCallRequest {
    /// A tool call request from the LLM.
    pub id: String,
    pub name: String,
    pub arguments: HashMap<String, serde_json::Value>,
    pub extra_content: Option<HashMap<String, serde_json::Value>>,
    pub provider_specific_fields: Option<HashMap<String, serde_json::Value>>,
    pub function_provider_specific_fields: Option<HashMap<String, serde_json::Value>>,
}

impl ToolCallRequest {
    /// Serialize to an OpenAI-style tool_call payload.
    pub fn to_openai_tool_call(&self) -> serde_json::Value {
        let mut function = serde_json::Map::new();
        function.insert(
            "name".to_string(),
            serde_json::Value::String(self.name.clone()),
        );
        // arguments field must be a JSON string representing the arguments dict
        function.insert(
            "arguments".to_string(),
            serde_json::Value::String(
                serde_json::to_string_pretty(&self.arguments).unwrap_or_else(|_| "{}".to_string()),
            ),
        );

        if let Some(ref func_provider_fields) = self.function_provider_specific_fields {
            function.insert(
                "provider_specific_fields".to_string(),
                serde_json::Value::Object(func_provider_fields.clone().into_iter().collect()),
            );
        }

        let mut tool_call = serde_json::Map::new();
        tool_call.insert("id".to_string(), serde_json::Value::String(self.id.clone()));
        tool_call.insert(
            "type".to_string(),
            serde_json::Value::String("function".to_string()),
        );
        tool_call.insert("function".to_string(), serde_json::Value::Object(function));

        if let Some(ref ext) = self.extra_content {
            tool_call.insert(
                "extra_content".to_string(),
                serde_json::Value::Object(ext.clone().into_iter().collect()),
            );
        }
        if let Some(ref provider_fields) = self.provider_specific_fields {
            tool_call.insert(
                "provider_specific_fields".to_string(),
                serde_json::Value::Object(provider_fields.clone().into_iter().collect()),
            );
        }

        serde_json::Value::Object(tool_call)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct LLMResponse {
    /// Response content from the LLM provider.
    pub content: Option<String>,
    /// Tool calls proposed by the LLM.
    pub tool_calls: Vec<ToolCallRequest>,
    /// Finish reason, such as "stop" or "tool_calls".
    pub finish_reason: String,
    /// Usage metrics, e.g., token counts.
    pub usage: HashMap<String, i64>,
    /// Providers' extra reasoning content, e.g., for Kimi or DeepSeek-R1.
    pub reasoning_content: Option<String>,
    /// Extended blocks, e.g., for Anthropic's "thinking".
    pub thinking_blocks: Option<Vec<HashMap<String, serde_json::Value>>>,
}

impl LLMResponse {
    /// Construct a new LLMResponse with defaults.
    pub fn new() -> Self {
        Self {
            content: None,
            tool_calls: Vec::new(),
            finish_reason: "stop".to_string(),
            usage: HashMap::new(),
            reasoning_content: None,
            thinking_blocks: None,
        }
    }

    /// Check if response contains tool calls.
    pub fn has_tool_calls(&self) -> bool {
        !self.tool_calls.is_empty()
    }
}

struct GenerationSettings {
    temperature: f32,
    max_tokens: usize,
    reasoning_effort: Option<String>,
}

impl GenerationSettings {
    pub fn new() -> Self {
        Self {
            temperature: 0.7,
            max_tokens: 4096,
            reasoning_effort: None,
        }
    }
}

pub trait LLMProvider {
    /// Default retry delays (in seconds) for chat requests.
    const CHAT_RETRY_DELAYS: &'static [u64] = &[1, 2, 4];

    /// Markers that signal a transient error and should trigger a retry.
    const TRANSIENT_ERROR_MARKERS: &'static [&'static str] = &[
        "429",
        "rate limit",
        "500",
        "502",
        "503",
        "504",
        "overloaded",
        "timeout",
        "timed out",
        "connection",
        "server error",
        "temporarily unavailable",
    ];

    /// Required method to initialize an LLMProvider.
    /// Equivalent Python signature:
    ///     def __init__(self, api_key: str | None = None, api_base: str | None = None):
    ///         self.api_key = api_key
    ///         self.api_base = api_base
    ///         self.generation: GenerationSettings = GenerationSettings()
    fn new(api_key: Option<String>, api_base: Option<String>) -> Self
    where
        Self: Sized;

    /// Returns a reference to the provider's API key, if set.
    fn api_key(&self) -> Option<String>;

    /// Returns a reference to the provider's API base, if set.
    fn api_base(&self) -> Option<String>;

    /// Returns a reference to the provider's GenerationSettings.
    fn generation_settings(&self) -> &GenerationSettings;

    /// Returns a mutable reference to the provider's GenerationSettings.
    fn generation_settings_mut(&mut self) -> &mut GenerationSettings;

    /// Sanitize message content: fix empty blocks, strip internal _meta fields.
    /// Equivalent to the python static method `_sanitize_empty_content`.
    /// Accepts an array of messages (as serde_json::Value, expected to be array of objects), and returns a new sanitized array.
    fn sanitize_empty_content(messages: &[serde_json::Value]) -> Vec<serde_json::Value> {
        let mut result = Vec::new();

        for msg in messages.iter() {
            let content = msg.get("content");

            fn get_str<'a>(v: &'a serde_json::Value, key: &str) -> Option<&'a str> {
                v.get(key).and_then(|s| s.as_str())
            }

            if let Some(content_str) = content.and_then(|v| v.as_str()) {
                if content_str.is_empty() {
                    // Clone whole msg, replace "content"
                    let mut clean = msg.clone();
                    let is_assistant = get_str(msg, "role") == Some("assistant");
                    let has_tool_calls = msg
                        .get("tool_calls")
                        .map(|tc| !tc.is_null())
                        .unwrap_or(false);
                    if is_assistant && has_tool_calls {
                        clean["content"] = serde_json::Value::Null;
                    } else {
                        clean["content"] = serde_json::Value::String("(empty)".to_string());
                    }
                    result.push(clean);
                    continue;
                }
            }

            if let Some(content_arr) = content.and_then(|v| v.as_array()) {
                let mut new_items = Vec::new();
                let mut changed = false;
                for item in content_arr.iter() {
                    if let Some(item_obj) = item.as_object() {
                        let typ = item_obj.get("type").and_then(|v| v.as_str());
                        let text = item_obj.get("text").and_then(|v| v.as_str());
                        if typ
                            .map(|t| t == "text" || t == "input_text" || t == "output_text")
                            .unwrap_or(false)
                            && (text.is_none() || text == Some(""))
                        {
                            changed = true;
                            continue;
                        }
                        if item_obj.contains_key("_meta") {
                            let filtered = item_obj
                                .iter()
                                .filter(|(k, _)| *k != "_meta")
                                .map(|(k, v)| (k.clone(), v.clone()))
                                .collect::<serde_json::Map<_, _>>();
                            new_items.push(serde_json::Value::Object(filtered));
                            changed = true;
                        } else {
                            new_items.push(item.clone());
                        }
                    } else {
                        new_items.push(item.clone());
                    }
                }
                if changed {
                    let mut clean = msg.clone();
                    if !new_items.is_empty() {
                        clean["content"] = serde_json::Value::Array(new_items);
                    } else {
                        let is_assistant = get_str(msg, "role") == Some("assistant");
                        let has_tool_calls = msg
                            .get("tool_calls")
                            .map(|tc| !tc.is_null())
                            .unwrap_or(false);
                        if is_assistant && has_tool_calls {
                            clean["content"] = serde_json::Value::Null;
                        } else {
                            clean["content"] = serde_json::Value::String("(empty)".to_string());
                        }
                    }
                    result.push(clean);
                    continue;
                }
            }

            if let Some(content_obj) = content.and_then(|v| v.as_object()) {
                let mut clean = msg.clone();
                clean["content"] =
                    serde_json::Value::Array(vec![serde_json::Value::Object(content_obj.clone())]);
                result.push(clean);
                continue;
            }

            // No changes needed, just push as is.
            result.push(msg.clone());
        }
        result
    }

    /// Keeps only provider-safe message keys and normalizes assistant content.
    ///
    fn sanitize_request_messages(
        messages: &[serde_json::Value],
        allowed_keys: &std::collections::HashSet<String>,
    ) -> Vec<serde_json::Value> {
        let mut sanitized = Vec::with_capacity(messages.len());

        for msg in messages {
            let obj = match msg.as_object() {
                Some(o) => o,
                None => {
                    sanitized.push(msg.clone());
                    continue;
                }
            };
            let mut clean = serde_json::Map::new();
            for (k, v) in obj.iter() {
                if allowed_keys.contains(k) {
                    clean.insert(k.clone(), v.clone());
                }
            }
            let is_assistant = clean
                .get("role")
                .and_then(|v| v.as_str())
                .map(|r| r == "assistant")
                .unwrap_or(false);

            let has_content = clean.contains_key("content");
            if is_assistant && !has_content {
                clean.insert("content".to_string(), serde_json::Value::Null);
            }
            sanitized.push(serde_json::Value::Object(clean));
        }
        sanitized
    }

    /// Send a chat completion request.
    ///
    /// # Arguments
    /// * `messages` - List of message maps with "role" and "content".
    /// * `tools` - Optional vector of tool definition maps.
    /// * `model` - Optional model identifier (provider-specific).
    /// * `max_tokens` - Maximum tokens in response.
    /// * `temperature` - Sampling temperature.
    /// * `reasoning_effort` - Optional reasoning effort string.
    /// * `tool_choice` - Tool selection strategy ("auto", "required", or specific tool map/string).
    ///
    /// # Returns
    /// An LLMResponse containing the result.
    ///
    /// # Errors
    /// Should be implemented by the LLMProvider for actual backend.
    #[allow(unused_variables)]
    async fn chat(
        &self,
        messages: Vec<serde_json::Value>,
        tools: Option<Vec<serde_json::Value>>,
        model: Option<String>,
        max_tokens: usize,
        temperature: f32,
        reasoning_effort: Option<String>,
        tool_choice: Option<serde_json::Value>,
    ) -> LLMResponse;


    fn get_default_model(&self) -> String;
    

    fn is_transient_error(content: Option<&str>) -> bool {
        let err = content.unwrap_or("").to_lowercase();
        Self::TRANSIENT_ERROR_MARKERS
            .iter()
            .any(|marker| err.contains(marker))
    }

    /// Replace image_url blocks with text placeholder. Returns None if no images found.
    /// Rough equivalent of the Python static method _strip_image_content.
    fn strip_image_content(
        messages: &[serde_json::Value],
    ) -> Option<Vec<serde_json::Value>> {
        let mut found = false;
        let mut result = Vec::with_capacity(messages.len());

        for msg in messages.iter() {
            let content = msg.get("content");
            if let Some(serde_json::Value::Array(blocks)) = content {
                let mut new_content = Vec::with_capacity(blocks.len());
                for b in blocks {
                    if let serde_json::Value::Object(obj) = b {
                        if obj
                            .get("type")
                            .and_then(|t| t.as_str())
                            .map(|t| t == "image_url")
                            .unwrap_or(false)
                        {
                            let path = obj
                                .get("_meta")
                                .and_then(|meta| {
                                    if let serde_json::Value::Object(meta_obj) = meta {
                                        meta_obj.get("path").and_then(|p| p.as_str())
                                    } else {
                                        None
                                    }
                                })
                                .unwrap_or("");
                            let placeholder = if path.is_empty() {
                                "[image omitted]".to_string()
                            } else {
                                format!("[image: {}]", path)
                            };
                            new_content.push(serde_json::json!({
                                "type": "text",
                                "text": placeholder,
                            }));
                            found = true;
                        } else {
                            new_content.push(b.clone());
                        }
                    } else {
                        new_content.push(b.clone());
                    }
                }
                let mut new_msg = msg.clone();
                new_msg["content"] = serde_json::Value::Array(new_content);
                result.push(new_msg);
            } else {
                result.push(msg.clone());
            }
        }
        if found { Some(result) } else { None }
    }

    async fn safe_chat(
        &self,
        messages: Vec<serde_json::Value>,
        tools: Option<Vec<serde_json::Value>>,
        model: Option<String>,
        max_tokens: usize,
        temperature: f32,
        reasoning_effort: Option<String>,
        tool_choice: Option<serde_json::Value>,
    ) -> LLMResponse
    {

        match AssertUnwindSafe(
            self.chat(messages, tools, model, max_tokens, temperature, reasoning_effort, tool_choice)
        )
        .catch_unwind()
        .await
        {
            Ok(resp) => resp,
            Err(panic_info) => {
                let err_msg = if let Some(s) = panic_info.downcast_ref::<&str>() {
                    s.to_string()
                } else if let Some(s) = panic_info.downcast_ref::<String>() {
                    s.clone()
                } else {
                    "unknown panic".to_string()
                };
                LLMResponse {
                    content: Some(format!("Error calling LLM: {err_msg}")),
                    finish_reason: "error".to_string(),
                    tool_calls: Vec::new(),
                    usage: HashMap::new(),
                    reasoning_content: None,
                    thinking_blocks: None,
                }
            }
        }
    }

    /// Stream a chat completion, calling `on_content_delta` for each text chunk.
    ///
    /// The default implementation falls back to a non-streaming call and delivers the
    /// full content as a single delta. Providers that support native streaming should override this method.
    ///
    /// # Arguments
    /// * `messages` - List of message maps with "role" and "content".
    /// * `tools` - Optional vector of tool definition maps.
    /// * `model` - Optional model identifier (provider-specific).
    /// * `max_tokens` - Maximum tokens in response.
    /// * `temperature` - Sampling temperature.
    /// * `reasoning_effort` - Optional reasoning effort string.
    /// * `tool_choice` - Tool selection strategy ("auto", "required", or specific tool map/string).
    /// * `on_content_delta` - Optional async function taking a string that will be called with content delta text.
    ///
    /// # Returns
    /// An LLMResponse containing the result.
    ///
    /// # Notes
    /// This dummy implementation can be overridden by providers supporting real streaming.
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
        on_content_delta: Option<F>,
    ) -> LLMResponse
    where
        F: Fn(String) -> Fut + Send + Sync,
        Fut: std::future::Future<Output = ()> + Send,
    {
        let response = self.chat(
            messages,
            tools,
            model,
            max_tokens,
            temperature,
            reasoning_effort,
            tool_choice,
        ).await;

        if let Some(on_delta) = on_content_delta {
            if let Some(ref content) = response.content {
                on_delta(content.clone()).await;
            }
        }

        response
    }

    #[allow(unused_variables)]
    async fn safe_chat_stream<F, Fut>(
        &self,
        messages: Vec<serde_json::Value>,
        tools: Option<Vec<serde_json::Value>>,
        model: Option<String>,
        max_tokens: usize,
        temperature: f32,
        reasoning_effort: Option<String>,
        tool_choice: Option<serde_json::Value>,
        on_content_delta: Option<F>,
    ) -> LLMResponse
    where
        F: Fn(String) -> Fut + Send + Sync,
        Fut: std::future::Future<Output = ()> + Send,
    {
        match AssertUnwindSafe(
            self.chat_stream(messages, tools, model, max_tokens, temperature,
                reasoning_effort, tool_choice, on_content_delta)
        )
        .catch_unwind()
        .await
        {
            Ok(resp) => resp,
            Err(panic_info) => {
                let err_msg = if let Some(s) = panic_info.downcast_ref::<&str>() {
                    s.to_string()
                } else if let Some(s) = panic_info.downcast_ref::<String>() {
                    s.clone()
                } else {
                    "unknown panic".to_string()
                };
                LLMResponse {
                    content: Some(format!("Error calling LLM: {err_msg}")),
                    finish_reason: "error".to_string(),
                    tool_calls: Vec::new(),
                    usage: HashMap::new(),
                    reasoning_content: None,
                    thinking_blocks: None,
                }
            }
        }

    }

    /// Calls chat() with retry logic on transient provider failures.
    ///
    /// Parameters default to self.generation when not explicitly passed,
    /// so callers do not need to thread temperature / max_tokens / reasoning_effort through every layer.
    async fn chat_with_retry(
        &self,
        messages: Vec<serde_json::Value>,
        tools: Option<Vec<serde_json::Value>>,
        model: Option<String>,
        max_tokens: Option<usize>,
        temperature: Option<f32>,
        reasoning_effort: Option<String>,
        tool_choice: Option<serde_json::Value>,
    ) -> LLMResponse {
        // Fallback to default values from generation_settings if not specified.
        let gs = self.generation_settings();
        let max_tokens = max_tokens.unwrap_or(gs.max_tokens);
        let temperature = temperature.unwrap_or(gs.temperature);
        let reasoning_effort = reasoning_effort.or_else(|| gs.reasoning_effort.clone());

        // Helper closure for calling self.chat and handling .await
        async fn call_safe_chat<T: LLMProvider + ?Sized>(
            provider: &T,
            messages: Vec<serde_json::Value>,
            tools: Option<Vec<serde_json::Value>>,
            model: Option<String>,
            max_tokens: usize,
            temperature: f32,
            reasoning_effort: Option<String>,
            tool_choice: Option<serde_json::Value>,
        ) -> LLMResponse {
            provider.safe_chat(
                messages,
                tools,
                model,
                max_tokens,
                temperature,
                reasoning_effort,
                tool_choice,
            )
            .await
        }

        // The retry loop.
        let mut last_response = None;
        for (attempt, delay) in Self::CHAT_RETRY_DELAYS.iter().enumerate() {
            let response = call_safe_chat(
                self,
                messages.clone(),
                tools.clone(),
                model.clone(),
                max_tokens,
                temperature,
                reasoning_effort.clone(),
                tool_choice.clone(),
            )
            .await;

            // If finish_reason is not "error", return response.
            if response.finish_reason != "error" {
                return response;
            }

            // If the error is NOT transient, attempt to strip image content, else return.
            if !Self::is_transient_error(response.content.as_deref()) {
                // Attempt to strip image content and retry just once if possible.
                if let Some(stripped) = Self::strip_image_content(&messages) {
                    log::warn!(
                        "Non-transient LLM error with image content, retrying without images"
                    );
                    // Retry immediately with stripped messages.
                    return call_safe_chat(
                        self,
                        stripped,
                        tools.clone(),
                        model.clone(),
                        max_tokens,
                        temperature,
                        reasoning_effort.clone(),
                        tool_choice.clone(),
                    )
                    .await;
                }
                // All else failed, return last response.
                return response;
            }
            // Otherwise, transient error; log and sleep before retrying.
            log::warn!(
                "LLM transient error (attempt {}/{}) retrying in {}s: {}",
                attempt + 1,
                Self::CHAT_RETRY_DELAYS.len(),
                delay,
                response.content.as_deref().unwrap_or("").get(..120).unwrap_or(""),
            );

            // Sleep the retry delay.
            tokio::time::sleep(std::time::Duration::from_secs(*delay)).await;

            last_response = Some(response);
        }
        // Last attempt after retries exhausted
        call_safe_chat(
            self,
            messages,
            tools,
            model,
            max_tokens,
            temperature,
            reasoning_effort,
            tool_choice,
        )
        .await
    }
    
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::HashSet, fs};

    struct TestLLMProvider {
        api_key: Option<String>,
        api_base: Option<String>,
        generation: GenerationSettings,
    }

    impl LLMProvider for TestLLMProvider {
        fn new(api_key: Option<String>, api_base: Option<String>) -> Self {
            Self {
                api_key,
                api_base,
                generation: GenerationSettings::new(),
            }
        }

        fn api_key(&self) -> Option<String> {
            return self.api_key.clone();
        }

        fn api_base(&self) -> Option<String> {
            return self.api_base.clone();
        }

        fn generation_settings(&self) -> &GenerationSettings {
            return &self.generation;
        }

        fn generation_settings_mut(&mut self) -> &mut GenerationSettings {
            return &mut self.generation;
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
        ) -> LLMResponse {
            LLMResponse {
                content: Some("Hello, world!".to_string()),
                finish_reason: "stop".to_string(),
                tool_calls: Vec::new(),
                usage: std::collections::HashMap::new(),
                reasoning_content: None,
                thinking_blocks: None,
            }
        }

        fn get_default_model(&self) -> String {
            return "test".to_string();
        }

        async fn chat_stream<F, Fut>(
            &self,
            messages: Vec<serde_json::Value>,
            tools: Option<Vec<serde_json::Value>>,
            model: Option<String>,
            max_tokens: usize,
            temperature: f32,
            reasoning_effort: Option<String>,
            tool_choice: Option<serde_json::Value>,
            on_content_delta: Option<F>,
        ) -> LLMResponse
        where
            F: Fn(String) -> Fut + Send + Sync,
            Fut: std::future::Future<Output = ()> + Send,
        {
            if let Some(on_delta) = on_content_delta {
                on_delta("Hello, ".to_string()).await;
                on_delta("world!".to_string()).await;
            }
            let response = LLMResponse {
                content: Some("Hello, world!".to_string()),
                finish_reason: "stop".to_string(),
                tool_calls: Vec::new(),
                usage: std::collections::HashMap::new(),
                reasoning_content: None,
                thinking_blocks: None,
            };
            response
        }
    }

    fn create_tool_call_request() -> ToolCallRequest {
        let mut extract_content = HashMap::new();
        extract_content.insert(
            "content".to_string(),
            serde_json::Value::String("Hello, world!".to_string()),
        );
        let mut provider_specific_fields = HashMap::new();
        provider_specific_fields.insert(
            "top_k".to_string(),
            serde_json::Value::Number(serde_json::Number::from(10)),
        );
        ToolCallRequest {
            id: "123".to_string(),
            name: "test".to_string(),
            arguments: HashMap::new(),
            extra_content: Some(extract_content),
            provider_specific_fields: Some(provider_specific_fields),
            function_provider_specific_fields: None,
        }
    }

    fn create_llm_response() -> LLMResponse {
        LLMResponse::new()
    }

    fn create_generation_settings() -> GenerationSettings {
        GenerationSettings::new()
    }

    fn create_test_llm_provider() -> TestLLMProvider {
        TestLLMProvider::new(
            Some("test".to_string()),
            Some("https://test.com".to_string()),
        )
    }

    #[test]
    fn test_to_openai_tool_call() {
        let tool_call_request = create_tool_call_request();
        let result = tool_call_request.to_openai_tool_call();
        println!("result: {}", result);
        assert!(result.is_object());
        assert!(result.get("id").unwrap().is_string());
        assert!(result.get("type").unwrap().is_string());
        assert!(result.get("function").unwrap().is_object());
        assert!(result.get("extra_content").unwrap().is_object());
        assert!(result.get("provider_specific_fields").unwrap().is_object());
    }

    #[test]
    fn test_has_tool_calls_false() {
        let llm_response = create_llm_response();
        assert!(!llm_response.has_tool_calls());
    }

    #[test]
    fn test_create_generation_settings() {
        let generation_settings = create_generation_settings();
        assert_eq!(generation_settings.temperature, 0.7);
        assert_eq!(generation_settings.max_tokens, 4096);
        assert!(generation_settings.reasoning_effort.is_none());
    }

    #[test]
    fn test_create_test_llm_provider() {
        let llm_provider = create_test_llm_provider();
        assert_eq!(llm_provider.api_key(), Some("test".to_string()));
        assert_eq!(
            llm_provider.api_base(),
            Some("https://test.com".to_string())
        );
        assert_eq!(llm_provider.generation_settings().temperature, 0.7);
        assert_eq!(llm_provider.generation_settings().max_tokens, 4096);
        assert!(
            llm_provider
                .generation_settings()
                .reasoning_effort
                .is_none()
        );
    }

    #[test]
    fn test_sanitize_empty_content() {
        let messages = vec![serde_json::json!(
            {
                "role": "assistant",
                "type": "message",
                "content": ""
            }
        )];
        let result = TestLLMProvider::sanitize_empty_content(&messages);
        println!("result: {}", serde_json::to_string_pretty(&result).unwrap());
        assert!(!result.is_empty());
        assert_eq!(
            result[0].get("content").unwrap(),
            &serde_json::Value::String("(empty)".to_string())
        );
    }

    #[test]
    fn test_sanitize_type_without_text() {
        let messages = vec![serde_json::json!(
            {
                "role": "assistant",
                "type": "message",
                "content": [{"type": "text"}]
            }
        )];
        let result = TestLLMProvider::sanitize_empty_content(&messages);
        println!("result: {}", serde_json::to_string_pretty(&result).unwrap());
        assert!(!result.is_empty());
        assert_eq!(
            result[0].get("content").unwrap(),
            &serde_json::Value::String("(empty)".to_string())
        );
    }

    #[test]
    fn test_sanitize_type_with_text() {
        let messages = vec![serde_json::json!(
            {
                "role": "assistant",
                "type": "message",
                "content": [{"type": "text", "text": "Hello, world!"}]
            }
        )];
        let result = TestLLMProvider::sanitize_empty_content(&messages);
        println!("result: {}", serde_json::to_string_pretty(&result).unwrap());
        assert!(!result.is_empty());
        assert_eq!(
            result[0].get("content").unwrap(),
            &serde_json::Value::Array(vec![
                serde_json::json!({ "type": "text", "text": "Hello, world!" })
            ])
        );
    }

    #[test]
    fn test_sanitize_request_messages() {
        let allowed_keys = HashSet::from(
            ["role", "content", "tool_calls", "tool_call_id", "name"]
                .iter()
                .map(|s| s.to_string())
                .collect::<HashSet<String>>(),
        );
        let messages = vec![
            serde_json::json!({
                "role": "user",
                "content": "Hello",
                "_meta": "should be stripped"
            }),
            serde_json::json!({
                "role": "assistant",
                "content": "Hi there",
                "secret": "should be stripped"
            }),
        ];
        let res = TestLLMProvider::sanitize_request_messages(&messages, &allowed_keys);
        assert_eq!(res.len(), 2);
    }

    #[test]
    fn test_is_transient_error() {
        assert!(TestLLMProvider::is_transient_error(Some(
            "429: Too Many Requests"
        )));
        assert!(TestLLMProvider::is_transient_error(Some(
            "rate limit exceeded"
        )));
        assert!(TestLLMProvider::is_transient_error(Some(
            "500: Internal Server Error"
        )));
        assert!(TestLLMProvider::is_transient_error(Some(
            "502: Bad Gateway"
        )));
        assert!(TestLLMProvider::is_transient_error(Some(
            "503: Service Unavailable"
        )));
        assert!(TestLLMProvider::is_transient_error(Some(
            "504: Gateway Timeout"
        )));
    }

    #[test]
    fn test_is_transient_error_false() {
        assert!(!TestLLMProvider::is_transient_error(None));
        assert!(!TestLLMProvider::is_transient_error(Some("banana error")));
    }

    #[test]
    fn test_strip_image_content() {
        let messages = vec![serde_json::json!(
            {
                "role": "assistant",
                "type": "message",
                "content": [{"type": "image_url", "_meta": {
                    "path": "https://test.com/image.png"
                }}]
            }
        )];
        let result = TestLLMProvider::strip_image_content(&messages);
        println!("result: {}", serde_json::to_string_pretty(&result.clone().unwrap()).unwrap());
        assert!(result.is_some());
        assert_eq!(result.unwrap().len(), 1);
    }

    
    #[test]
    fn test_strip_image_content_no_images() {
        let messages = vec![serde_json::json!(
            {
                "role": "assistant",
                "type": "message",
                "content": [{"type": "text", "text": "Hello, world!"}]
            }
        )];
        let result = TestLLMProvider::strip_image_content(&messages);
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_safe_chat() {
        let messages = vec![serde_json::json!(
            {
                "role": "assistant",
                "type": "message",
                "content": [{"type": "text", "text": "Hello, world!"}]
            }
        )];
        let tools = vec![serde_json::json!(
            {
                "name": "test_tool",
                "description": "Test tool",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "test_param": {
                            "type": "string",
                            "description": "Test parameter"
                        }
                    }
                }
            }
        )];
        let llm_provider = create_test_llm_provider();
        let result = llm_provider.safe_chat(
            messages,
            Some(tools),
            None,
            4096,
            0.0,
            None,
            None,
        ).await;
        println!("result: {}", result.content.unwrap());
    }

    #[tokio::test]
    async fn test_chat_stream() {
        let messages = vec![serde_json::json!(
            {
                "role": "assistant",
                "type": "message",
                "content": [{"type": "text", "text": "Hello, world!"}]
            }
        )];
        let llm_provider = create_test_llm_provider();
        let result = llm_provider.chat_stream(
            messages,
            None,
            None,
            4096,
            0.0,
            None,
            None,
            None::<fn(String) -> std::future::Ready<()>>,
        ).await;
        assert_eq!(result.content, Some("Hello, world!".to_string()));
    }

    #[tokio::test]
    async fn test_chat_stream_with_on_content_delta() {
        let messages = vec![serde_json::json!(
            {
                "role": "assistant",
                "type": "message",
                "content": [{"type": "text", "text": "Hello, world!"}]
            }
        )];
        let llm_provider = create_test_llm_provider();
        let result = llm_provider.chat_stream(
            messages,
            None,
            None,
            4096,
            0.0,
            None,
            None,
            Some(|content| async move {
                println!("content: {}", content);
            }),
        ).await;
        assert_eq!(result.content, Some("Hello, world!".to_string()));
    }

    #[tokio::test]
    async fn test_safe_chat_stream() {
        let messages = vec![serde_json::json!(
            {
                "role": "user",
                "type": "message",
                "content": [{"type": "text", "text": "Hello, world!"}]
            }
        )];
    
        let llm_provider = create_test_llm_provider();
        let result = llm_provider.safe_chat_stream(
            messages,
            None,
            None,
            4096,
            0.0,
            None,
            None,
            None::<fn(String) -> std::future::Ready<()>>,
        ).await;
        assert_eq!(result.content, Some("Hello, world!".to_string()));
    }

    #[tokio::test]
    async fn test_safe_chat_stream_with_on_content_delta() {
        let messages = vec![serde_json::json!(
            {
                "role": "user",
                "type": "message",
                "content": [{"type": "text", "text": "Hello, world!"}]
            }
        )];
        let llm_provider = create_test_llm_provider();
        let result = llm_provider.safe_chat_stream(
            messages,
            None,
            None,
            4096,
            0.0,
            None,
            None,
            Some(|content| async move {
                println!("content: {}", content);
            }),
        ).await;
        assert_eq!(result.content, Some("Hello, world!".to_string()));
    }
}
