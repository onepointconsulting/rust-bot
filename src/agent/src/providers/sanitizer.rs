use serde_json::Value;

/// Normalize to a provider-safe 9-char alphanumeric form.
/// Equivalent to the Python function:
/// def _normalize_tool_call_id(tool_call_id: Any) -> Any:
///     if not isinstance(tool_call_id, str):
///         return tool_call_id
///     if len(tool_call_id) == 9 and tool_call_id.isalnum():
///         return tool_call_id
///     return hashlib.sha1(tool_call_id.encode()).hexdigest()[:9]
pub fn normalize_tool_call_id(tool_call_id: &Value) -> Value {
    if let Some(s) = tool_call_id.as_str() {
        if s.len() == 9 && s.chars().all(|c| c.is_ascii_alphanumeric()) {
            // Already normalized, return as String
            Value::String(s.to_string())
        } else {
            // Hash with sha1 and take first 9 hex chars
            use sha1::{Digest, Sha1};
            let mut hasher = Sha1::new();
            hasher.update(s.as_bytes());
            let result = hasher.finalize();
            let hexed = hex::encode(result);
            Value::String(hexed[..9].to_string())
        }
    } else {
        // Not a string, return as is (clone)
        tool_call_id.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_tool_call_id_ok() {
        let tool_call_id = Value::String("123456789".to_string());
        let result = normalize_tool_call_id(&tool_call_id);
        assert_eq!(result, Value::String("123456789".to_string()));
    }

    #[test]
    fn test_normalize_tool_call_id_too_long() {
        let tool_call_id = Value::String("tralalalalalala".to_string());
        let result = normalize_tool_call_id(&tool_call_id);
        println!("result: {}", serde_json::to_string_pretty(&result).unwrap());
        assert!(result.is_string());
        assert_eq!(result.as_str().unwrap().len(), 9);
    }
}
