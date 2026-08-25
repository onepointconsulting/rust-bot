use futures::FutureExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;

use crate::providers::registry::ProviderSpec;

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

impl fmt::Display for ToolCallRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let args = serde_json::to_string(&self.arguments).unwrap_or_else(|_| "{}".to_string());
        write!(
            f,
            "ToolCall {{ id: {}, name: {}, arguments: {}",
            self.id, self.name, args
        )?;
        if let Some(extra) = &self.extra_content {
            let extra = serde_json::to_string(extra).unwrap_or_else(|_| "{}".to_string());
            write!(f, ", extra_content: {extra}")?;
        }
        if let Some(fields) = &self.provider_specific_fields {
            let fields = serde_json::to_string(fields).unwrap_or_else(|_| "{}".to_string());
            write!(f, ", provider_specific_fields: {fields}")?;
        }
        if let Some(fields) = &self.function_provider_specific_fields {
            let fields = serde_json::to_string(fields).unwrap_or_else(|_| "{}".to_string());
            write!(f, ", function_provider_specific_fields: {fields}")?;
        }
        write!(f, " }}")
    }
}

// Not used right now
pub enum RetryMode {
    Standard,
    Persistent,
}

/// Token and cost totals for one provider call (or an accumulated run).
///
/// `input_tokens` is the provider's uncached prompt count. Cache write/read are
/// stored separately and folded into [`Self::prompt_tokens`]. `reasoning_tokens`
/// is a breakdown of output, not an extra addend — OpenAI already includes it in
/// `output_tokens`.
///
/// `None` means the provider did not report that field. Missing is distinct from
/// zero so `/status` and run totals can avoid treating "unknown" as "free".
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct LLMUsage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_cost: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_cost: Option<f64>,
}

impl LLMUsage {
    pub fn new() -> Self {
        Self::default()
    }

    /// Incoming tokens billed as prompt: uncached input plus cache write/read.
    /// `None` when `input_tokens` itself was not reported.
    pub fn prompt_tokens(&self) -> Option<u32> {
        Some(
            self.input_tokens?
                .saturating_add(self.cache_creation_input_tokens.unwrap_or(0))
                .saturating_add(self.cache_read_input_tokens.unwrap_or(0)),
        )
    }

    /// Prompt plus output. Reasoning is not added; it is already inside output.
    pub fn total_tokens(&self) -> Option<u32> {
        Some(self.prompt_tokens()?.saturating_add(self.output_tokens?))
    }

    pub fn total_cost(&self) -> Option<f64> {
        match (self.input_cost, self.output_cost) {
            (None, None) => None,
            (input, output) => Some(input.unwrap_or(0.0) + output.unwrap_or(0.0)),
        }
    }

    pub fn add(&mut self, other: &Self) {
        self.input_tokens = Self::add_usage(self.input_tokens, other.input_tokens);
        self.output_tokens = Self::add_usage(self.output_tokens, other.output_tokens);
        self.cache_creation_input_tokens = Self::add_usage(
            self.cache_creation_input_tokens,
            other.cache_creation_input_tokens,
        );
        self.cache_read_input_tokens =
            Self::add_usage(self.cache_read_input_tokens, other.cache_read_input_tokens);
        self.reasoning_tokens = Self::add_usage(self.reasoning_tokens, other.reasoning_tokens);
        self.input_cost = Self::add_cost(self.input_cost, other.input_cost);
        self.output_cost = Self::add_cost(self.output_cost, other.output_cost);
    }

    /// `None + None = None`; otherwise treat missing as zero so a known iteration
    /// is not wiped by a later call that omitted the field.
    fn add_usage(a: Option<u32>, b: Option<u32>) -> Option<u32> {
        match (a, b) {
            (None, None) => None,
            (a, b) => Some(a.unwrap_or(0).saturating_add(b.unwrap_or(0))),
        }
    }

    fn add_cost(a: Option<f64>, b: Option<f64>) -> Option<f64> {
        match (a, b) {
            (None, None) => None,
            (a, b) => Some(a.unwrap_or(0.0) + b.unwrap_or(0.0)),
        }
    }
}

impl fmt::Display for LLMUsage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.prompt_tokens(), self.output_tokens) {
            (Some(input), Some(output)) => write!(f, "{input} in / {output} out")?,
            (Some(input), None) => write!(f, "{input} in / ? out")?,
            (None, Some(output)) => write!(f, "? in / {output} out")?,
            (None, None) => write!(f, "tokens unknown")?,
        }
        if let Some(cost) = self.total_cost() {
            write!(f, " (${cost:.6})")?;
        }
        Ok(())
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
    pub usage: LLMUsage,
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
            usage: LLMUsage::new(),
            reasoning_content: None,
            thinking_blocks: None,
        }
    }

    /// Check if response contains tool calls.
    pub fn has_tool_calls(&self) -> bool {
        !self.tool_calls.is_empty()
    }
}

pub struct GenerationSettings {
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

/// Default retry delays (in seconds) for chat requests.
const CHAT_RETRY_DELAYS: &[u64] = &[1, 2, 4];

/// Markers that signal a transient error and should trigger a retry.
const TRANSIENT_ERROR_MARKERS: &[&str] = &[
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

/// Markers that the selected model cannot accept image / vision input.
/// These are capability mismatches, not transient failures — retrying with
/// the image stripped hides the real error from the user.
const UNSUPPORTED_IMAGE_INPUT_MARKERS: &[&str] = &[
    "no endpoints found that support image",
    "does not support image",
    "doesn't support image",
    "image input is not supported",
    "images are not supported",
    "does not support vision",
    "doesn't support vision",
    "vision is not supported",
    "support image input",
];

/// True when `content` is an LLM error saying the model cannot take images.
pub fn is_unsupported_image_input_error(content: Option<&str>) -> bool {
    let err = content.unwrap_or("").to_lowercase();
    UNSUPPORTED_IMAGE_INPUT_MARKERS
        .iter()
        .any(|marker| err.contains(marker))
}

/// A dyn-safe streaming callback: receives one content delta per token and
/// returns a boxed `Send` future.
///
/// The `Send + Sync` bounds are required so the closure can be stored behind an
/// `Arc` and called from async provider code that may span threads.
pub type BoxedStreamCallback =
    Box<dyn Fn(String) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// A dyn-compatible subset of [`LLMProvider`].
///
/// `LLMProvider` cannot be used as `dyn LLMProvider` because it contains:
/// - Associated constants (`CHAT_RETRY_DELAYS`, `TRANSIENT_ERROR_MARKERS`)
/// - Static methods with no `&self` receiver (`sanitize_empty_content`, etc.)
/// - Generic async methods (`chat_stream<F, Fut>`, etc.)
///
/// `LLMProviderDyn` exposes only the methods needed for runtime dispatch:
/// the core `&self` accessors and the three non-generic async call methods.
/// A blanket impl covers any `T: LLMProvider + Send + Sync` automatically,
/// so callers store `Arc<dyn LLMProviderDyn>` and construct it from any concrete provider.
#[async_trait::async_trait]
pub trait LLMProviderDyn: Send + Sync {
    fn api_key(&self) -> Option<String>;
    fn api_base(&self) -> Option<String>;
    fn extra_headers(&self) -> Option<HashMap<String, String>>;
    fn generation_settings(&self) -> &GenerationSettings;
    fn generation_settings_mut(&mut self) -> &mut GenerationSettings;
    fn spec(&self) -> Option<&ProviderSpec>;
    fn get_default_model(&self) -> String;

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

    async fn safe_chat(
        &self,
        messages: Vec<serde_json::Value>,
        tools: Option<Vec<serde_json::Value>>,
        model: Option<String>,
        max_tokens: usize,
        temperature: f32,
        reasoning_effort: Option<String>,
        tool_choice: Option<serde_json::Value>,
    ) -> LLMResponse;

    async fn chat_with_retry(
        &self,
        messages: Vec<serde_json::Value>,
        tools: Option<Vec<serde_json::Value>>,
        model: Option<String>,
        max_tokens: Option<usize>,
        temperature: Option<f32>,
        reasoning_effort: Option<String>,
        tool_choice: Option<serde_json::Value>,
    ) -> LLMResponse;

    /// Streaming counterpart of [`chat_with_retry`] that is safe to call
    /// through `Arc<dyn LLMProviderDyn>`.
    ///
    /// `on_content_delta` is invoked once per text token as it arrives from the
    /// provider.  Providers that do not implement real streaming (i.e. those
    /// relying on the default `chat_stream` base implementation) will invoke the
    /// callback exactly once with the full response content.
    ///
    /// The callback's `Send + Sync` bounds and the `Send` bound on the returned
    /// future are required so the closure can be held across `.await` points in
    /// provider code that may run on a multi-threaded executor.
    async fn chat_stream_with_retry_boxed(
        &self,
        messages: Vec<serde_json::Value>,
        tools: Option<Vec<serde_json::Value>>,
        model: Option<String>,
        max_tokens: Option<usize>,
        temperature: Option<f32>,
        reasoning_effort: Option<String>,
        tool_choice: Option<serde_json::Value>,
        on_content_delta: Option<BoxedStreamCallback>,
    ) -> LLMResponse;
}

#[async_trait::async_trait]
impl<T: LLMProvider + Send + Sync> LLMProviderDyn for T {
    fn api_key(&self) -> Option<String> {
        LLMProvider::api_key(self)
    }
    fn api_base(&self) -> Option<String> {
        LLMProvider::api_base(self)
    }
    fn extra_headers(&self) -> Option<HashMap<String, String>> {
        LLMProvider::extra_headers(self)
    }
    fn generation_settings(&self) -> &GenerationSettings {
        LLMProvider::generation_settings(self)
    }
    fn generation_settings_mut(&mut self) -> &mut GenerationSettings {
        LLMProvider::generation_settings_mut(self)
    }
    fn spec(&self) -> Option<&ProviderSpec> {
        LLMProvider::spec(self)
    }
    fn get_default_model(&self) -> String {
        LLMProvider::get_default_model(self)
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
        LLMProvider::chat(
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

    async fn safe_chat(
        &self,
        messages: Vec<serde_json::Value>,
        tools: Option<Vec<serde_json::Value>>,
        model: Option<String>,
        max_tokens: usize,
        temperature: f32,
        reasoning_effort: Option<String>,
        tool_choice: Option<serde_json::Value>,
    ) -> LLMResponse {
        LLMProvider::safe_chat(
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
        LLMProvider::chat_with_retry(
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

    async fn chat_stream_with_retry_boxed(
        &self,
        messages: Vec<serde_json::Value>,
        tools: Option<Vec<serde_json::Value>>,
        model: Option<String>,
        max_tokens: Option<usize>,
        temperature: Option<f32>,
        reasoning_effort: Option<String>,
        tool_choice: Option<serde_json::Value>,
        on_content_delta: Option<BoxedStreamCallback>,
    ) -> LLMResponse {
        // `BoxedStreamCallback` satisfies the bounds of `safe_chat_stream_with_retry`:
        //   F  = Box<dyn Fn(String) -> Pin<Box<dyn Future<Output=()> + Send>> + Send + Sync>
        //      → implements Fn(String) -> Fut + Send + Sync  ✓
        //   Fut = Pin<Box<dyn Future<Output=()> + Send>>
        //      → implements Future<Output=()> + Send          ✓
        LLMProvider::safe_chat_stream_with_retry(
            self,
            messages,
            tools,
            model,
            max_tokens,
            temperature,
            reasoning_effort,
            tool_choice,
            &on_content_delta,
        )
        .await
    }
}

pub trait LLMProvider: Send + Sync {
    /// Required method to initialize an LLMProvider.
    /// Equivalent Python signature:
    ///     def __init__(self, api_key: str | None = None, api_base: str | None = None):
    ///         self.api_key = api_key
    ///         self.api_base = api_base
    ///         self.generation: GenerationSettings = GenerationSettings()
    fn new(
        api_key: Option<String>,
        api_base: Option<String>,
        default_model: Option<String>,
        extra_headers: Option<HashMap<String, String>>,
        spec: Option<ProviderSpec>,
    ) -> Self
    where
        Self: Sized;

    /// Returns a reference to the provider's API key, if set.
    fn api_key(&self) -> Option<String>;

    /// Returns a reference to the provider's API base, if set.
    fn api_base(&self) -> Option<String>;

    fn extra_headers(&self) -> Option<HashMap<String, String>>;

    /// Returns a reference to the provider's GenerationSettings.
    fn generation_settings(&self) -> &GenerationSettings;

    /// Returns a mutable reference to the provider's GenerationSettings.
    fn generation_settings_mut(&mut self) -> &mut GenerationSettings;

    /// Returns a reference to the provider's ProviderSpec, if set.
    fn spec(&self) -> Option<&ProviderSpec>;

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

    /// Extract tool name from either OpenAI or Anthropic-style tool schemas.
    fn tool_name(tool: &serde_json::Value) -> String {
        if let Some(name) = tool.get("name").and_then(|v| v.as_str()) {
            return name.to_string();
        }
        if let Some(fname) = tool
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(|n| n.as_str())
        {
            return fname.to_string();
        }
        String::new()
    }

    /// Return cache marker indices: builtin/MCP boundary and tail index.
    fn tool_cache_marker_indices(tools: Option<Vec<serde_json::Value>>) -> Option<Vec<usize>> {
        let tools = tools?;
        if tools.is_empty() {
            return Some(Vec::new());
        }

        let tail_idx = tools.len() - 1;
        let mut last_builtin_idx: Option<usize> = None;
        for i in (0..=tail_idx).rev() {
            if !Self::tool_name(&tools[i]).starts_with("mcp_") {
                last_builtin_idx = Some(i);
                break;
            }
        }

        let mut ordered_unique = Vec::new();
        for idx in [last_builtin_idx, Some(tail_idx)] {
            if let Some(idx) = idx {
                if !ordered_unique.contains(&idx) {
                    ordered_unique.push(idx);
                }
            }
        }
        Some(ordered_unique)
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
    fn chat(
        &self,
        messages: Vec<serde_json::Value>,
        tools: Option<Vec<serde_json::Value>>,
        model: Option<String>,
        max_tokens: usize,
        temperature: f32,
        reasoning_effort: Option<String>,
        tool_choice: Option<serde_json::Value>,
    ) -> impl std::future::Future<Output = LLMResponse> + Send;

    fn get_default_model(&self) -> String;

    fn is_transient_error(content: Option<&str>) -> bool {
        let err = content.unwrap_or("").to_lowercase();
        TRANSIENT_ERROR_MARKERS
            .iter()
            .any(|marker| err.contains(marker))
    }

    /// Strip images and retry only when the failure is not a vision-capability
    /// mismatch. Returns `None` when the caller should surface the error as-is.
    fn retry_messages_without_images(
        content: Option<&str>,
        messages: &[serde_json::Value],
    ) -> Option<Vec<serde_json::Value>> {
        if is_unsupported_image_input_error(content) {
            log::warn!(
                "Model does not support image input; returning error instead of retrying without images"
            );
            return None;
        }
        Self::strip_image_content(messages)
    }

    /// Replace image_url blocks with text placeholder. Returns None if no images found.
    /// Rough equivalent of the Python static method _strip_image_content.
    fn strip_image_content(messages: &[serde_json::Value]) -> Option<Vec<serde_json::Value>> {
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

    fn safe_chat(
        &self,
        messages: Vec<serde_json::Value>,
        tools: Option<Vec<serde_json::Value>>,
        model: Option<String>,
        max_tokens: usize,
        temperature: f32,
        reasoning_effort: Option<String>,
        tool_choice: Option<serde_json::Value>,
    ) -> impl std::future::Future<Output = LLMResponse> + Send {
        async move {
            match AssertUnwindSafe(self.chat(
                messages,
                tools,
                model,
                max_tokens,
                temperature,
                reasoning_effort,
                tool_choice,
            ))
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
                        usage: LLMUsage::new(),
                        reasoning_content: None,
                        thinking_blocks: None,
                    }
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
    fn chat_stream<F, Fut>(
        &self,
        messages: Vec<serde_json::Value>,
        tools: Option<Vec<serde_json::Value>>,
        model: Option<String>,
        max_tokens: usize,
        temperature: f32,
        reasoning_effort: Option<String>,
        tool_choice: Option<serde_json::Value>,
        on_content_delta: &Option<F>,
    ) -> impl std::future::Future<Output = LLMResponse> + Send
    where
        F: Fn(String) -> Fut + Send + Sync,
        Fut: std::future::Future<Output = ()> + Send,
    {
        async move {
            let response = self
                .chat(
                    messages,
                    tools,
                    model,
                    max_tokens,
                    temperature,
                    reasoning_effort,
                    tool_choice,
                )
                .await;

            if let Some(on_delta) = on_content_delta {
                if let Some(ref content) = response.content {
                    on_delta(content.clone()).await;
                }
            }

            response
        }
    }

    #[allow(unused_variables)]
    fn safe_chat_stream<F, Fut>(
        &self,
        messages: Vec<serde_json::Value>,
        tools: Option<Vec<serde_json::Value>>,
        model: Option<String>,
        max_tokens: usize,
        temperature: f32,
        reasoning_effort: Option<String>,
        tool_choice: Option<serde_json::Value>,
        on_content_delta: &Option<F>,
    ) -> impl std::future::Future<Output = LLMResponse> + Send
    where
        F: Fn(String) -> Fut + Send + Sync,
        Fut: std::future::Future<Output = ()> + Send,
    {
        async move {
            match AssertUnwindSafe(self.chat_stream(
                messages,
                tools,
                model,
                max_tokens,
                temperature,
                reasoning_effort,
                tool_choice,
                on_content_delta,
            ))
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
                        usage: LLMUsage::new(),
                        reasoning_content: None,
                        thinking_blocks: None,
                    }
                }
            }
        }
    }

    /// Calls chat() with retry logic on transient provider failures.
    ///
    /// Parameters default to self.generation when not explicitly passed,
    /// so callers do not need to thread temperature / max_tokens / reasoning_effort through every layer.
    fn chat_with_retry(
        &self,
        messages: Vec<serde_json::Value>,
        tools: Option<Vec<serde_json::Value>>,
        model: Option<String>,
        max_tokens: Option<usize>,
        temperature: Option<f32>,
        reasoning_effort: Option<String>,
        tool_choice: Option<serde_json::Value>,
    ) -> impl std::future::Future<Output = LLMResponse> + Send {
        async move {
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
                provider
                    .safe_chat(
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
            for (attempt, delay) in CHAT_RETRY_DELAYS.iter().enumerate() {
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
                    if let Some(stripped) =
                        Self::retry_messages_without_images(response.content.as_deref(), &messages)
                    {
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
                    "LLM transient error with {} (attempt {}/{}) retrying in {}s: {}",
                    model.clone().unwrap_or("unknown model".to_string()),
                    attempt + 1,
                    CHAT_RETRY_DELAYS.len(),
                    delay,
                    response
                        .content
                        .as_deref()
                        .unwrap_or("")
                        .get(..120)
                        .unwrap_or(""),
                );

                // Sleep the retry delay.
                tokio::time::sleep(std::time::Duration::from_secs(*delay)).await;
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

    fn safe_chat_stream_with_retry<F, Fut>(
        &self,
        messages: Vec<serde_json::Value>,
        tools: Option<Vec<serde_json::Value>>,
        model: Option<String>,
        max_tokens: Option<usize>,
        temperature: Option<f32>,
        reasoning_effort: Option<String>,
        tool_choice: Option<serde_json::Value>,
        on_content_delta: &Option<F>,
    ) -> impl std::future::Future<Output = LLMResponse> + Send
    where
        F: Fn(String) -> Fut + Send + Sync,
        Fut: std::future::Future<Output = ()> + Send,
    {
        async move {
            let gs = self.generation_settings();
            let max_tokens = max_tokens.unwrap_or(gs.max_tokens);
            let temperature = temperature.unwrap_or(gs.temperature);
            let reasoning_effort = reasoning_effort.or_else(|| gs.reasoning_effort.clone());

            for (attempt, delay) in CHAT_RETRY_DELAYS.iter().enumerate() {
                let response = self
                    .safe_chat_stream(
                        messages.clone(),
                        tools.clone(),
                        model.clone(),
                        max_tokens,
                        temperature,
                        reasoning_effort.clone(),
                        tool_choice.clone(),
                        on_content_delta,
                    )
                    .await;
                log::info!("LLM stream response: {:?}", response);
                // Successful stream response: return immediately.
                if response.finish_reason != "error" {
                    return response;
                }

                if !Self::is_transient_error(response.content.as_deref()) {
                    // Attempt to strip image content and retry just once if possible.
                    if let Some(stripped) =
                        Self::retry_messages_without_images(response.content.as_deref(), &messages)
                    {
                        log::warn!(
                            "Non-transient LLM error with image content, retrying without images"
                        );
                        // Retry immediately with stripped messages.
                        return self
                            .safe_chat_stream(
                                stripped,
                                tools.clone(),
                                model.clone(),
                                max_tokens,
                                temperature,
                                reasoning_effort.clone(),
                                tool_choice.clone(),
                                on_content_delta,
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
                    CHAT_RETRY_DELAYS.len(),
                    delay,
                    response
                        .content
                        .as_deref()
                        .unwrap_or("")
                        .get(..120)
                        .unwrap_or(""),
                );

                // Sleep the retry delay.
                tokio::time::sleep(std::time::Duration::from_secs(*delay)).await;
            }
            self.safe_chat_stream(
                messages,
                tools,
                model,
                max_tokens,
                temperature,
                reasoning_effort,
                tool_choice,
                on_content_delta,
            )
            .await
        }
    }

    fn handle_error(e: Box<dyn std::error::Error>) -> crate::providers::base::LLMResponse {
        return crate::providers::base::LLMResponse {
            content: Some(e.to_string()),
            finish_reason: "error".to_string(),
            tool_calls: Vec::new(),
            usage: LLMUsage::new(),
            reasoning_content: None,
            thinking_blocks: None,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn prompt_tokens_sums_input_and_cache_not_output() {
        let usage = LLMUsage {
            input_tokens: Some(100),
            output_tokens: Some(50),
            cache_creation_input_tokens: Some(20),
            cache_read_input_tokens: Some(30),
            ..LLMUsage::new()
        };
        assert_eq!(usage.prompt_tokens(), Some(150));
        assert_eq!(usage.total_tokens(), Some(200));
    }

    #[test]
    fn prompt_tokens_none_when_input_missing() {
        let usage = LLMUsage {
            output_tokens: Some(10),
            ..LLMUsage::new()
        };
        assert!(usage.prompt_tokens().is_none());
        assert!(usage.total_tokens().is_none());
    }

    #[test]
    fn total_tokens_does_not_double_count_reasoning() {
        let usage = LLMUsage {
            input_tokens: Some(10),
            output_tokens: Some(40),
            reasoning_tokens: Some(25),
            ..LLMUsage::new()
        };
        assert_eq!(usage.total_tokens(), Some(50));
    }

    #[test]
    fn add_treats_missing_as_zero_once_either_side_is_known() {
        let mut acc = LLMUsage {
            input_tokens: Some(10),
            output_tokens: Some(5),
            input_cost: Some(0.01),
            ..LLMUsage::new()
        };
        acc.add(&LLMUsage {
            input_tokens: Some(3),
            output_tokens: Some(2),
            cache_read_input_tokens: Some(7),
            output_cost: Some(0.02),
            ..LLMUsage::new()
        });
        assert_eq!(acc.input_tokens, Some(13));
        assert_eq!(acc.output_tokens, Some(7));
        assert_eq!(acc.cache_read_input_tokens, Some(7));
        assert_eq!(acc.prompt_tokens(), Some(20));
        assert_eq!(acc.input_cost, Some(0.01));
        assert_eq!(acc.output_cost, Some(0.02));
        assert_eq!(acc.total_cost(), Some(0.03));
    }

    #[test]
    fn add_none_plus_none_stays_none() {
        let mut acc = LLMUsage::new();
        acc.add(&LLMUsage::new());
        assert!(acc.input_tokens.is_none());
        assert!(acc.total_cost().is_none());
    }

    struct TestLLMProvider {
        api_key: Option<String>,
        api_base: Option<String>,
        generation: GenerationSettings,
    }

    impl LLMProvider for TestLLMProvider {
        fn new(
            api_key: Option<String>,
            api_base: Option<String>,
            _default_model: Option<String>,
            _extra_headers: Option<HashMap<String, String>>,
            _spec: Option<ProviderSpec>,
        ) -> Self {
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

        fn extra_headers(&self) -> Option<HashMap<String, String>> {
            return None;
        }

        fn spec(&self) -> Option<&ProviderSpec> {
            return None;
        }

        async fn chat(
            &self,
            _messages: Vec<serde_json::Value>,
            _tools: Option<Vec<serde_json::Value>>,
            _model: Option<String>,
            _max_tokens: usize,
            _temperature: f32,
            _reasoning_effort: Option<String>,
            _tool_choice: Option<serde_json::Value>,
        ) -> LLMResponse {
            LLMResponse {
                content: Some("Hello, world!".to_string()),
                finish_reason: "stop".to_string(),
                tool_calls: Vec::new(),
                usage: LLMUsage::new(),
                reasoning_content: None,
                thinking_blocks: None,
            }
        }

        fn get_default_model(&self) -> String {
            return "test".to_string();
        }

        async fn chat_stream<F, Fut>(
            &self,
            _messages: Vec<serde_json::Value>,
            _tools: Option<Vec<serde_json::Value>>,
            _model: Option<String>,
            _max_tokens: usize,
            _temperature: f32,
            _reasoning_effort: Option<String>,
            _tool_choice: Option<serde_json::Value>,
            on_content_delta: &Option<F>,
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
                usage: LLMUsage::new(),
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
            Some("test".to_string()),
            None,
            None,
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
    fn test_tool_call_request_display() {
        let tool_call_request = create_tool_call_request();
        let s = tool_call_request.to_string();
        assert!(s.contains("id: 123"));
        assert!(s.contains("name: test"));
        assert!(s.contains("extra_content:"));
        assert!(s.contains("provider_specific_fields:"));
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
        assert_eq!(
            LLMProvider::api_key(&llm_provider),
            Some("test".to_string())
        );
        assert_eq!(
            LLMProvider::api_base(&llm_provider),
            Some("https://test.com".to_string())
        );
        assert_eq!(
            LLMProvider::generation_settings(&llm_provider).temperature,
            0.7
        );
        assert_eq!(
            LLMProvider::generation_settings(&llm_provider).max_tokens,
            4096
        );
        assert!(
            LLMProvider::generation_settings(&llm_provider)
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
    fn test_is_unsupported_image_input_error() {
        assert!(is_unsupported_image_input_error(Some(
            "No endpoints found that support image input"
        )));
        assert!(is_unsupported_image_input_error(Some(
            "This model does not support image input"
        )));
        assert!(is_unsupported_image_input_error(Some(
            "failed to deserialize api response: error:x content:{\"error\":{\"message\":\"No endpoints found that support image input\",\"code\":404}}"
        )));
        assert!(!is_unsupported_image_input_error(None));
        assert!(!is_unsupported_image_input_error(Some("invalid image")));
        assert!(!is_unsupported_image_input_error(Some(
            "image omitted due to size"
        )));
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
        println!(
            "result: {}",
            serde_json::to_string_pretty(&result.clone().unwrap()).unwrap()
        );
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
        let result = LLMProvider::safe_chat(
            &llm_provider,
            messages,
            Some(tools),
            None,
            4096,
            0.0,
            None,
            None,
        )
        .await;
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
        let result = llm_provider
            .chat_stream(
                messages,
                None,
                None,
                4096,
                0.0,
                None,
                None,
                &None::<fn(String) -> std::future::Ready<()>>,
            )
            .await;
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
        let result = llm_provider
            .chat_stream(
                messages,
                None,
                None,
                4096,
                0.0,
                None,
                None,
                &Some(|content| async move {
                    println!("content: {}", content);
                }),
            )
            .await;
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
        let result = llm_provider
            .safe_chat_stream(
                messages,
                None,
                None,
                4096,
                0.0,
                None,
                None,
                &None::<fn(String) -> std::future::Ready<()>>,
            )
            .await;
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
        let result = llm_provider
            .safe_chat_stream(
                messages,
                None,
                None,
                4096,
                0.0,
                None,
                None,
                &Some(|content| async move {
                    println!("content: {}", content);
                }),
            )
            .await;
        assert_eq!(result.content, Some("Hello, world!".to_string()));
    }

    #[tokio::test]
    async fn test_safe_chat_stream_with_retry_does_not_retry_successful_image_request() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct CountingProvider {
            generation: GenerationSettings,
            stream_calls: AtomicUsize,
        }

        impl LLMProvider for CountingProvider {
            fn new(
                _api_key: Option<String>,
                _api_base: Option<String>,
                _default_model: Option<String>,
                _extra_headers: Option<HashMap<String, String>>,
                _spec: Option<ProviderSpec>,
            ) -> Self {
                Self {
                    generation: GenerationSettings::new(),
                    stream_calls: AtomicUsize::new(0),
                }
            }

            fn api_key(&self) -> Option<String> {
                None
            }

            fn api_base(&self) -> Option<String> {
                None
            }

            fn generation_settings(&self) -> &GenerationSettings {
                &self.generation
            }

            fn generation_settings_mut(&mut self) -> &mut GenerationSettings {
                &mut self.generation
            }

            fn extra_headers(&self) -> Option<HashMap<String, String>> {
                None
            }

            fn spec(&self) -> Option<&ProviderSpec> {
                None
            }

            async fn chat(
                &self,
                _messages: Vec<serde_json::Value>,
                _tools: Option<Vec<serde_json::Value>>,
                _model: Option<String>,
                _max_tokens: usize,
                _temperature: f32,
                _reasoning_effort: Option<String>,
                _tool_choice: Option<serde_json::Value>,
            ) -> LLMResponse {
                LLMResponse {
                    content: Some("ok".to_string()),
                    finish_reason: "stop".to_string(),
                    tool_calls: Vec::new(),
                    usage: LLMUsage::new(),
                    reasoning_content: None,
                    thinking_blocks: None,
                }
            }

            fn get_default_model(&self) -> String {
                "test".to_string()
            }

            async fn chat_stream<F, Fut>(
                &self,
                _messages: Vec<serde_json::Value>,
                _tools: Option<Vec<serde_json::Value>>,
                _model: Option<String>,
                _max_tokens: usize,
                _temperature: f32,
                _reasoning_effort: Option<String>,
                _tool_choice: Option<serde_json::Value>,
                _on_content_delta: &Option<F>,
            ) -> LLMResponse
            where
                F: Fn(String) -> Fut + Send + Sync,
                Fut: std::future::Future<Output = ()> + Send,
            {
                self.stream_calls.fetch_add(1, Ordering::SeqCst);
                LLMResponse {
                    content: Some("ok".to_string()),
                    finish_reason: "stop".to_string(),
                    tool_calls: Vec::new(),
                    usage: LLMUsage::new(),
                    reasoning_content: None,
                    thinking_blocks: None,
                }
            }
        }

        let provider = CountingProvider::new(None, None, None, None, None);
        let messages = vec![serde_json::json!({
            "role": "user",
            "content": [
                {
                    "type": "text",
                    "text": "Describe this"
                },
                {
                    "type": "image_url",
                    "image_url": { "url": "data:image/png;base64,AA==" },
                    "_meta": { "path": "/tmp/test.png" }
                }
            ]
        })];

        let _ = provider
            .safe_chat_stream_with_retry(
                messages,
                None,
                None,
                None,
                None,
                None,
                None,
                &None::<fn(String) -> std::future::Ready<()>>,
            )
            .await;

        assert_eq!(provider.stream_calls.load(Ordering::SeqCst), 1);
    }

    fn image_user_message() -> serde_json::Value {
        serde_json::json!({
            "role": "user",
            "content": [
                {
                    "type": "text",
                    "text": "Describe this"
                },
                {
                    "type": "image_url",
                    "image_url": { "url": "data:image/png;base64,AA==" },
                    "_meta": { "path": "/tmp/test.png" }
                }
            ]
        })
    }

    #[tokio::test]
    async fn test_safe_chat_stream_with_retry_does_not_strip_unsupported_image_input() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct VisionErrorProvider {
            generation: GenerationSettings,
            stream_calls: AtomicUsize,
        }

        impl LLMProvider for VisionErrorProvider {
            fn new(
                _api_key: Option<String>,
                _api_base: Option<String>,
                _default_model: Option<String>,
                _extra_headers: Option<HashMap<String, String>>,
                _spec: Option<ProviderSpec>,
            ) -> Self {
                Self {
                    generation: GenerationSettings::new(),
                    stream_calls: AtomicUsize::new(0),
                }
            }

            fn api_key(&self) -> Option<String> {
                None
            }

            fn api_base(&self) -> Option<String> {
                None
            }

            fn generation_settings(&self) -> &GenerationSettings {
                &self.generation
            }

            fn generation_settings_mut(&mut self) -> &mut GenerationSettings {
                &mut self.generation
            }

            fn extra_headers(&self) -> Option<HashMap<String, String>> {
                None
            }

            fn spec(&self) -> Option<&ProviderSpec> {
                None
            }

            async fn chat(
                &self,
                _messages: Vec<serde_json::Value>,
                _tools: Option<Vec<serde_json::Value>>,
                _model: Option<String>,
                _max_tokens: usize,
                _temperature: f32,
                _reasoning_effort: Option<String>,
                _tool_choice: Option<serde_json::Value>,
            ) -> LLMResponse {
                unimplemented!()
            }

            fn get_default_model(&self) -> String {
                "test".to_string()
            }

            async fn chat_stream<F, Fut>(
                &self,
                _messages: Vec<serde_json::Value>,
                _tools: Option<Vec<serde_json::Value>>,
                _model: Option<String>,
                _max_tokens: usize,
                _temperature: f32,
                _reasoning_effort: Option<String>,
                _tool_choice: Option<serde_json::Value>,
                _on_content_delta: &Option<F>,
            ) -> LLMResponse
            where
                F: Fn(String) -> Fut + Send + Sync,
                Fut: std::future::Future<Output = ()> + Send,
            {
                self.stream_calls.fetch_add(1, Ordering::SeqCst);
                LLMResponse {
                    content: Some("No endpoints found that support image input".to_string()),
                    finish_reason: "error".to_string(),
                    tool_calls: Vec::new(),
                    usage: LLMUsage::new(),
                    reasoning_content: None,
                    thinking_blocks: None,
                }
            }
        }

        let provider = VisionErrorProvider::new(None, None, None, None, None);
        let result = provider
            .safe_chat_stream_with_retry(
                vec![image_user_message()],
                None,
                None,
                None,
                None,
                None,
                None,
                &None::<fn(String) -> std::future::Ready<()>>,
            )
            .await;

        assert_eq!(provider.stream_calls.load(Ordering::SeqCst), 1);
        assert_eq!(result.finish_reason, "error");
        assert_eq!(
            result.content.as_deref(),
            Some("No endpoints found that support image input")
        );
    }

    fn openai_tool(name: &str) -> serde_json::Value {
        serde_json::json!({
            "type": "function",
            "function": { "name": name }
        })
    }

    #[test]
    fn tool_cache_marker_indices_returns_none_for_missing_tools() {
        assert_eq!(TestLLMProvider::tool_cache_marker_indices(None), None);
    }

    #[test]
    fn tool_cache_marker_indices_returns_empty_for_empty_tools() {
        assert_eq!(
            TestLLMProvider::tool_cache_marker_indices(Some(vec![])),
            Some(vec![])
        );
    }

    #[test]
    fn tool_cache_marker_indices_marks_boundary_and_tail() {
        let tools = vec![
            openai_tool("search"),
            openai_tool("mcp_github_search"),
            openai_tool("mcp_github_read"),
        ];

        assert_eq!(
            TestLLMProvider::tool_cache_marker_indices(Some(tools)),
            Some(vec![0, 2])
        );
    }

    #[test]
    fn tool_cache_marker_indices_deduplicates_single_tool() {
        let tools = vec![openai_tool("search")];

        assert_eq!(
            TestLLMProvider::tool_cache_marker_indices(Some(tools)),
            Some(vec![0])
        );
    }

    #[test]
    fn tool_cache_marker_indices_tail_only_when_all_mcp() {
        let tools = vec![openai_tool("mcp_a"), openai_tool("mcp_b")];

        assert_eq!(
            TestLLMProvider::tool_cache_marker_indices(Some(tools)),
            Some(vec![1])
        );
    }

    #[test]
    fn tool_name_reads_openai_and_anthropic_schemas() {
        assert_eq!(TestLLMProvider::tool_name(&openai_tool("search")), "search");
        assert_eq!(
            TestLLMProvider::tool_name(&serde_json::json!({
                "name": "direct",
                "input_schema": { "type": "object", "properties": {} }
            })),
            "direct"
        );
    }
}
