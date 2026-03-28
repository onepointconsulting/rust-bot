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

    /// Try to coerce `value` to a serde_json::Map; return None if not possible or empty.
    ///
    /// This mimics the Python helper for extracting a dict/map from:
    /// - a serde_json Object (`Value::Object`)
    /// - an object with a "model_dump" method (if callable in Python, here: if value["model_dump"] is a function, call and check result)
    pub fn coerce_map(value: &serde_json::Value) -> Option<serde_json::Map<String, serde_json::Value>> {
        use serde_json::{Value, Map};

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
}
