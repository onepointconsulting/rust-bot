use std::collections::HashMap;
use crate::providers::registry::ProviderSpec;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OpenAICompatProvider {
    api_key: Option<String>,
    api_base: Option<String>,
    default_model: Option<String>,
    extra_headers: HashMap<String, String>,
    spec: Option<ProviderSpec>
}

impl OpenAICompatProvider {

    // Allowed message keys for OpenAI-compatible messages
    const ALLOWED_MSG_KEYS: &[&str] = &[
        "role", "content", "tool_calls", "tool_call_id", "name",
        "reasoning_content", "extra_content",
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
        m.insert("HTTP-Referer", "https://github.com/HKUDS/nanobot");
        m.insert("X-OpenRouter-Title", "nanobot");
        m.insert("X-OpenRouter-Categories", "cli-agent,personal-agent");
        m
    }
    
    pub fn new(api_key: Option<String>, api_base: Option<String>, default_model: Option<String>, extra_headers: HashMap<String, String>, spec: Option<ProviderSpec>) -> Self {
        Self { api_key, api_base, default_model, extra_headers, spec }
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
}