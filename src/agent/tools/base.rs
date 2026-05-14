use async_trait::async_trait;
use std::collections::HashMap;

/// Mapping from JSON Schema primitive types to idiomatic Rust types.
///
/// This is the Rust equivalent of the Python `_TYPE_MAP`:
///
/// ```python
/// _TYPE_MAP = {
///     "string": str,
///     "integer": int,
///     "number": (int, float),
///     "boolean": bool,
///     "array": list,
///     "object": dict,
/// }
/// ```
pub fn json_type_map() -> HashMap<&'static str, &'static [&'static str]> {
    let mut map: HashMap<&'static str, &'static [&'static str]> = HashMap::new();

    // JSON string -> Rust owned UTF-8 string.
    map.insert("string", &["String"]);

    // JSON integer -> Rust integer types.
    map.insert("integer", &["i64", "i32", "u64", "u32", "usize"]);

    // JSON number -> Rust numeric types (ints and floats).
    map.insert("number", &["f64", "f32", "i64", "i32", "u64", "u32"]);

    // JSON boolean -> Rust bool.
    map.insert("boolean", &["bool"]);

    // JSON array -> Rust Vec of some element type.
    map.insert("array", &["Vec<T>"]);

    // JSON object -> map-like Rust types commonly used for JSON objects.
    map.insert(
        "object",
        &[
            "serde_json::Map<String, serde_json::Value>",
            "std::collections::HashMap<String, serde_json::Value>",
        ],
    );

    map
}

#[async_trait]
pub trait Tool: Send + Sync {
    /// Tool name used in function calls.
    fn name(&self) -> String;
    /// Description of what the tool does.
    fn description(&self) -> String;
    /// JSON Schema for tool parameters.
    fn parameters(&self) -> serde_json::Value;
    /*
    Execute the tool with given parameters.

        Args:

            **kwargs: Tool-specific parameters.

        Returns:

            String result of the tool execution.
    */
    async fn execute(&self, params: &serde_json::Value) -> String;

    /// Whether this tool is side-effect free and safe to parallelize.
    /// Defaults to `false` so that existing tools are always executed
    /// sequentially unless they explicitly opt in.
    fn exclusive(&self) -> bool {
        false
    }

    /// Whether this tool is read-only and safe to parallelize.
    fn read_only(&self) -> bool {
        false
    }

    /// Return `true` if this tool is safe to run concurrently with other
    /// concurrency-safe tools in the same LLM turn.
    ///
    /// Defaults to `false` so that existing tools are always executed
    /// sequentially unless they explicitly opt in.
    fn concurrency_safe(&self) -> bool {
        self.read_only() && !self.exclusive()
    }

    fn cast_params(&self, params: &serde_json::Value) -> serde_json::Value {
        let schema = self.parameters().clone();
        if schema
            .get("type")
            .unwrap_or(&serde_json::Value::String("object".to_string()))
            != "object"
        {
            return params.clone();
        }
        return self._cast_object(params.clone(), &schema);
    }

    fn _cast_object(
        &self,
        val: serde_json::Value,
        schema: &serde_json::Value,
    ) -> serde_json::Value {
        if !val.is_object() {
            return val;
        }
        let props = schema
            .get("properties")
            .and_then(|p| p.as_object())
            .cloned()
            .unwrap_or_default();
        let obj = val.as_object().cloned().unwrap_or_default();
        let mut result = serde_json::Map::new();

        // Python equivalent:
        // for key, value in obj.items():
        //     if key in props:
        //         result[key] = self._cast_value(value, props[key])
        //     else:
        //         result[key] = value
        for (key, value) in obj {
            if let Some(prop_schema) = props.get(&key) {
                result.insert(key, self._cast_value(value, prop_schema));
            } else {
                result.insert(key, value);
            }
        }
        serde_json::Value::Object(result)
    }

    /// Cast a raw JSON value to the Rust type implied by a JSON Schema fragment.
    ///
    /// This is the Rust equivalent of the Python helper:
    ///
    /// ```python
    /// def _cast_value(self, val: Any, schema: dict[str, Any]) -> Any:
    ///     ...
    /// ```
    ///
    /// The default implementation is a no-op passthrough; implementors can
    /// override this to perform real casting / validation.
    fn _cast_value(&self, val: serde_json::Value, schema: &serde_json::Value) -> serde_json::Value {
        let default_target = serde_json::Value::String("any".to_string());
        let target_type = schema
            .get("type")
            .unwrap_or(&default_target);
        if target_type == "boolean" && val.is_boolean() {
            return val;
        }
        if target_type == "integer" && val.is_number() {
            return val;
        }
        let _type_map = json_type_map();
        if let Some(target_type_str) = target_type.as_str() {
            if let Some(rust_type_name) = _type_map.get(target_type_str) {
                if !["boolean", "integer", "array", "object"].contains(&target_type_str) {
                    let type_matches = match rust_type_name.first().copied().unwrap_or("") {
                        "String" => val.is_string(),
                        "f64" | "f32" => val.is_number(),
                        "usize" | "u64" | "i64" | "i32" => val.is_number(),
                        _ => false,
                    };
                    if type_matches {
                        return val;
                    }
                }
            }
        }
        if target_type == "integer" && val.is_string() {
            if let Some(val_str) = val.as_str() {
                // Try to parse the string as an integer
                if let Ok(parsed) = val_str.parse::<i64>() {
                    return serde_json::Value::from(parsed);
                } else {
                    return val;
                }
            }
        }
        if target_type == "number" && val.is_string() {
            if let Some(val_str) = val.as_str() {
                // Try to parse the string as an integer
                if let Ok(parsed) = val_str.parse::<f64>() {
                    return serde_json::Value::from(parsed);
                } else {
                    return val;
                }
            }
        }
        if target_type == "string" {
            // If val is null, return as is; otherwise, ensure it's a string
            if val.is_null() {
                return val;
            } else {
                return serde_json::Value::String(val.to_string());
            }
        }
        if target_type == "boolean" && val.is_string() {
            if let Some(val_str) = val.as_str() {
                let val_lower = val_str.to_lowercase();
                if ["true", "1", "yes"].contains(&val_lower.as_str()) {
                    return serde_json::Value::Bool(true);
                }
                if ["false", "0", "no"].contains(&val_lower.as_str()) {
                    return serde_json::Value::Bool(false);
                }
                return val;
            }
        }
        if target_type == "array" && val.is_array() {
            let item_schema = schema.get("items");
            if let Some(item_schema) = item_schema {
                if let Some(array) = val.as_array() {
                    let casted_array: Vec<serde_json::Value> = array
                        .iter()
                        .map(|item| self._cast_value(item.clone(), item_schema))
                        .collect();
                    return serde_json::Value::Array(casted_array);
                }
            }
            return val;
        }
        if target_type == "object" && val.is_object() {
            return self._cast_object(val, schema);
        }
        val
    }

    fn validate_params(&self, params: &serde_json::Value) -> Vec<String> {
        if !params.is_object() {
            return vec!["params must be an object".to_string()];
        }
        let schema = self.parameters();
        let schema_type = schema
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("object");
        if schema_type != "object" {
            return vec![format!(
                "Schema must be object type, got {:?}",
                schema.get("type")
            )];
        }
        // Merge schema with explicit "type": "object"
        let mut schema_obj = schema.as_object().cloned().unwrap_or_default();
        schema_obj.insert(
            "type".to_string(),
            serde_json::Value::String("object".to_string()),
        );
        let merged_schema = serde_json::Value::Object(schema_obj);
        self._validate(params.clone(), &merged_schema, "")
    }

    fn _validate(
        &self,
        val: serde_json::Value,
        schema: &serde_json::Value,
        path: &str,
    ) -> Vec<String> {
        let t = schema.get("type").and_then(|v| v.as_str());
        let label = if path.is_empty() {
            "parameter".to_string()
        } else {
            path.to_string()
        };

        // Type checks (and early return on mismatch), mirroring your Python logic.
        match t {
            Some("integer") => {
                let is_integer = val.as_i64().is_some() || val.as_u64().is_some();
                if !val.is_number() || !is_integer {
                    return vec![format!("{} should be integer", label)];
                }
            }
            Some("number") => {
                if !val.is_number() {
                    return vec![format!("{} should be number", label)];
                }
            }
            Some("string") => {
                if !val.is_string() {
                    return vec![format!("{} should be string", label)];
                }
            }
            Some("boolean") => {
                if !val.is_boolean() {
                    return vec![format!("{} should be boolean", label)];
                }
            }
            Some("array") => {
                if !val.is_array() {
                    return vec![format!("{} should be array", label)];
                }
            }
            Some("object") => {
                if !val.is_object() {
                    return vec![format!("{} should be object", label)];
                }
            }
            _ => {
                // Unknown type: don't enforce type at this stage.
            }
        }

        let mut errors: Vec<String> = Vec::new();

        // enum
        if let Some(enum_vals) = schema.get("enum").and_then(|v| v.as_array()) {
            let ok = enum_vals.iter().any(|ev| ev == &val);
            if !ok {
                errors.push(format!(
                    "{} must be one of {}",
                    label,
                    serde_json::Value::Array(enum_vals.clone())
                ));
            }
        }

        // numeric constraints
        if t == Some("integer") || t == Some("number") {
            let val_f64 = val.as_f64();
            if let Some(val_f64) = val_f64 {
                if let Some(minv) = schema.get("minimum").and_then(|v| v.as_f64()) {
                    if val_f64 < minv {
                        errors.push(format!("{} must be >= {}", label, minv));
                    }
                }
                if let Some(maxv) = schema.get("maximum").and_then(|v| v.as_f64()) {
                    if val_f64 > maxv {
                        errors.push(format!("{} must be <= {}", label, maxv));
                    }
                }
            }
        }

        // string constraints
        if t == Some("string") {
            if let Some(s) = val.as_str() {
                let len = s.chars().count();
                if let Some(min_len) = schema.get("minLength").and_then(|v| v.as_u64()) {
                    if len < min_len as usize {
                        errors.push(format!("{} must be at least {} chars", label, min_len));
                    }
                }
                if let Some(max_len) = schema.get("maxLength").and_then(|v| v.as_u64()) {
                    if len > max_len as usize {
                        errors.push(format!("{} must be at most {} chars", label, max_len));
                    }
                }
            }
        }

        // object constraints + recursion
        if t == Some("object") {
            let props = schema
                .get("properties")
                .and_then(|p| p.as_object())
                .cloned()
                .unwrap_or_default();

            let required = schema
                .get("required")
                .and_then(|r| r.as_array())
                .cloned()
                .unwrap_or_default();

            if let Some(obj) = val.as_object() {
                for req_key in required {
                    if let Some(k) = req_key.as_str() {
                        if !obj.contains_key(k) {
                            let missing_path = if path.is_empty() {
                                k.to_string()
                            } else {
                                format!("{}.{}", path, k)
                            };
                            errors.push(format!("missing required {}", missing_path));
                        }
                    }
                }

                for (k, v) in obj {
                    if let Some(prop_schema) = props.get(k) {
                        let next_path = if path.is_empty() {
                            k.clone()
                        } else {
                            format!("{}.{}", path, k)
                        };
                        errors.extend(self._validate(v.clone(), prop_schema, &next_path));
                    }
                }
            }
        }

        // array recursion
        if t == Some("array") {
            if let Some(items_schema) = schema.get("items") {
                if let Some(arr) = val.as_array() {
                    for (i, item) in arr.iter().enumerate() {
                        let item_path = if path.is_empty() {
                            format!("[{}]", i)
                        } else {
                            format!("{}[{}]", path, i)
                        };
                        errors.extend(self._validate(item.clone(), items_schema, &item_path));
                    }
                }
            }
        }

        errors
    }

    /// Convert tool to OpenAI function schema format.
    fn to_schema(&self) -> serde_json::Value {
        let parameters: serde_json::Value = serde_json::from_str(&self.parameters().to_string())
            .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));
        serde_json::json!({
            "type": "function",
            "function": {
                "name": self.name(),
                "description": self.description(),
                "parameters": parameters,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::agent::tools::base::Tool;

    struct SampleTool;

    #[async_trait::async_trait]
    impl Tool for SampleTool {
        fn name(&self) -> String {
            "sample".to_string()
        }

        fn description(&self) -> String {
            "sample tool".to_string()
        }

        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "minLength": 2 },
                    "count": { "type": "integer", "minimum": 1, "maximum": 10 },
                    "mode": { "type": "string", "enum": ["fast", "full"] },
                    "meta": {
                        "type": "object",
                        "properties": {
                            "tag": { "type": "string" },
                            "flags": {
                                "type": "array",
                                "items": { "type": "string" }
                            }
                        },
                        "required": ["tag"]
                    }
                },
                "required": ["query", "count"]
            })
        }

        async fn execute(&self, _params: &serde_json::Value) -> String {
            "ok".to_string()
        }
    }

    #[test]
    fn sample_tool_schema_shape() {
        let tool = SampleTool;
        let schema = tool.to_schema();

        assert_eq!(schema["type"], "function");
        assert_eq!(schema["function"]["name"], "sample");
        assert_eq!(schema["function"]["description"], "sample tool");
        assert_eq!(schema["function"]["parameters"]["type"], "object");
    }

    #[test]
    fn test_validate_params_missing_required() {
        let tool = SampleTool;
        let params = serde_json::json!({
            "query": "hello"
        });
        let errors = tool.validate_params(&params);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0], "missing required count");
    }

    #[test]
    fn test_validate_params_type_and_range() {
        let tool = SampleTool;
        let params = serde_json::json!({
            "query": "hi", "count": 0
        });
        let errors = tool.validate_params(&params);
        assert!(errors.len() > 0);
        assert!(errors.iter().any(|e| e.contains("count must be >= 1")));

        let params = serde_json::json!({
            "query": "hi", "count": "2"
        });
        let errors = tool.validate_params(&params);
        assert!(errors.len() > 0);
        assert!(errors.iter().any(|e| e.contains("count should be integer")));
    }

    #[test]
    fn test_validate_params_enum_and_min_length() {
        let tool = SampleTool;
        let params = serde_json::json!({
            "query": "hi", "count": 2, "mode": "fool"
        });
        let errors = tool.validate_params(&params);
        assert!(errors.len() > 0);
        assert!(errors.iter().any(|e| e.contains("mode must be one of [\"fast\",\"full\"]")));
    }

    #[test]
    fn test_validate_params_nested_object_and_array() {
        let tool = SampleTool;
        let params = serde_json::json!({
            "query": "hi",
            "count": 2,
            "meta": {"flags": [1, "ok"]},
        });
        let errors = tool.validate_params(&params);
        assert!(errors.len() > 0);
        assert!(errors.iter().any(|e| e.contains("missing required meta.tag")));
        assert!(errors.iter().any(|e| e.contains("meta.flags[0] should be string")));
    }

    #[test]
    fn test_validate_params_ignores_unknown_fields() {
        let tool = SampleTool;
        let params = serde_json::json!({"query": "hi", "count": 2, "extra": "x"});
        let errors = tool.validate_params(&params);
        assert!(errors.len() == 0);
    }
}
