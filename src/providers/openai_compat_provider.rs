use async_openai::{Client, config::OpenAIConfig};
use crate::providers::{base::{LLMProvider, GenerationSettings}, registry::ProviderSpec};
use std::collections::HashMap;

pub struct OpenAICompatProvider {
    api_key: Option<String>,
    api_base: Option<String>,
    default_model: Option<String>,
    extra_headers: HashMap<String, String>,
    spec: Option<ProviderSpec>,
    generation: GenerationSettings,
    client: Client<OpenAIConfig>,
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
        use serde_json::{Map, Value};

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
        // Apply Nanobot attribution headers to OpenRouter requests by default.
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
                unsafe { std::env::set_var(&spec.env_key, &api_key); }
            } else if std::env::var_os(&spec.env_key).is_none() {
                unsafe { std::env::set_var(&spec.env_key, &api_key); }
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
                    unsafe { std::env::set_var(env_name, resolved); }
                }
            }
        }
    }
}

impl LLMProvider for OpenAICompatProvider {
    fn new(
        api_key: Option<String>,
        api_base: Option<String>,
        default_model: Option<String>,
        extra_headers: Option<HashMap<String, String>>,
        spec: Option<ProviderSpec>
    ) -> Self {
        // Defaults based on Python signature.
        // In the Rust implementation, additional fields like default_model, extra_headers, spec,
        // client, etc., must be handled in the struct definition, but are omitted/handled elsewhere for clarity.
        let default_model = default_model.unwrap_or_else(|| OpenAICompatProvider::DEFAULT_MODEL.to_string());
        let extra_headers: std::collections::HashMap<String, String> =
            extra_headers.unwrap_or_else(|| std::collections::HashMap::new());
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
            config = config.with_header(header_name, v.as_str()).unwrap_or_else(|e| {
                log::error!("Invalid HTTP header value for '{}': {}", k, e);
                panic!("Invalid HTTP header value for '{}': {}", k, e);
            });
        }
        let client = Client::with_config(config);

        let provider = Self {
            api_key,
            api_base: effective_base,
            default_model: Some(default_model),
            extra_headers,
            spec: spec,
            generation: GenerationSettings::new(),
            client,
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
        _messages: Vec<serde_json::Value>,
        _tools: Option<Vec<serde_json::Value>>,
        _model: Option<String>,
        _max_tokens: usize,
        _temperature: f32,
        _reasoning_effort: Option<String>,
        _tool_choice: Option<serde_json::Value>,
    ) -> crate::providers::base::LLMResponse {
        unimplemented!("OpenAICompatProvider::chat is not yet implemented")
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
        };
        assert!(OpenAICompatProvider::uses_openrouter_attribution(
            Some(&spec),
            None
        ));
    }
}
