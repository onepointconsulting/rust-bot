use serde_json::Value;

/// Return the single non-null branch for nullable unions.
fn extract_nullable_branch(options: Value) -> Option<(Value, bool)> {
    if !options.is_array() {
        return None;
    }
    let mut non_null: Vec<Value> = Vec::new();
    let mut saw_null = false;
    for option in options.as_array().unwrap() {
        if !option.is_object() {
            return None;
        }
        if let Some(type_value) = option.get("type") {
            if let Some(type_str) = type_value.as_str() && type_str == "null" {
                saw_null = true;
                continue;
            }
        }
        non_null.push(option.clone());
    }
    if saw_null && non_null.len() == 1 {
        return Some((non_null[0].clone(), true));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── positive cases ────────────────────────────────────────────────────────

    #[test]
    fn test_nullable_string_returns_string_branch() {
        // Typical JSON Schema nullable string: anyOf: [{type:string},{type:null}]
        let options = json!([{"type": "string"}, {"type": "null"}]);
        let result = extract_nullable_branch(options);
        assert!(result.is_some());
        let (branch, is_nullable) = result.unwrap();
        assert_eq!(branch, json!({"type": "string"}));
        assert!(is_nullable);
    }

    #[test]
    fn test_nullable_object_returns_object_branch() {
        let options = json!([{"type": "null"}, {"type": "object", "properties": {}}]);
        let (branch, is_nullable) = extract_nullable_branch(options).unwrap();
        assert_eq!(branch["type"], "object");
        assert!(is_nullable);
    }

    #[test]
    fn test_null_branch_can_appear_first_or_last() {
        // null first
        let opts_null_first = json!([{"type": "null"}, {"type": "integer"}]);
        let (b1, _) = extract_nullable_branch(opts_null_first).unwrap();
        assert_eq!(b1["type"], "integer");

        // null last
        let opts_null_last = json!([{"type": "integer"}, {"type": "null"}]);
        let (b2, _) = extract_nullable_branch(opts_null_last).unwrap();
        assert_eq!(b2["type"], "integer");
    }

    // ── negative cases ────────────────────────────────────────────────────────

    #[test]
    fn test_non_array_input_returns_none() {
        assert!(extract_nullable_branch(json!({"type": "string"})).is_none());
        assert!(extract_nullable_branch(json!("string")).is_none());
        assert!(extract_nullable_branch(json!(null)).is_none());
    }

    #[test]
    fn test_no_null_branch_returns_none() {
        // Two non-null branches — not a simple nullable union.
        let options = json!([{"type": "string"}, {"type": "integer"}]);
        assert!(extract_nullable_branch(options).is_none());
    }

    #[test]
    fn test_multiple_non_null_branches_returns_none() {
        // null + two non-null types — ambiguous, should return None.
        let options = json!([{"type": "string"}, {"type": "integer"}, {"type": "null"}]);
        assert!(extract_nullable_branch(options).is_none());
    }

    #[test]
    fn test_array_containing_non_object_returns_none() {
        // A bare string in the array is not a valid JSON Schema type object.
        let options = json!(["string", {"type": "null"}]);
        assert!(extract_nullable_branch(options).is_none());
    }

    #[test]
    fn test_only_null_branch_returns_none() {
        let options = json!([{"type": "null"}]);
        assert!(extract_nullable_branch(options).is_none());
    }
}
