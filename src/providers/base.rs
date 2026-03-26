use std::collections::HashMap;

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
    temperature: f64,
    max_tokens: u32,
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
        messages: &Vec<serde_json::Map<String, serde_json::Value>>,
        allowed_keys: &std::collections::HashSet<String>,
    ) -> Vec<serde_json::Map<String, serde_json::Value>> {
        let mut sanitized = Vec::with_capacity(messages.len());

        for msg in messages {
            let mut clean = serde_json::Map::new();
            for (k, v) in msg.iter() {
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
            sanitized.push(clean);
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
        messages: Vec<std::collections::HashMap<String, serde_json::Value>>,
        tools: Option<Vec<std::collections::HashMap<String, serde_json::Value>>>,
        model: Option<String>,
        max_tokens: usize,
        temperature: f32,
        reasoning_effort: Option<String>,
        tool_choice: Option<serde_json::Value>,
    ) -> LLMResponse;
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
            messages: Vec<std::collections::HashMap<String, serde_json::Value>>,
            tools: Option<Vec<std::collections::HashMap<String, serde_json::Value>>>,
            model: Option<String>,
            max_tokens: usize,
            temperature: f32,
            reasoning_effort: Option<String>,
            tool_choice: Option<serde_json::Value>,
        ) -> LLMResponse {
            LLMResponse::new()
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
        let messages_values = vec![
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
        let messages = messages_values.iter()
            .map(|v| v.as_object().unwrap().clone())
            .collect();
        let res = TestLLMProvider::sanitize_request_messages(&messages, &allowed_keys);
        assert_eq!(res.len(), 2);
    }
}
