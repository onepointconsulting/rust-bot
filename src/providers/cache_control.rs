//! Prompt-cache helpers shared by OpenAI-compatible and Anthropic-style APIs.

use serde_json::{Value, json};

/// Injects `cache_control` markers for prompt caching (e.g. Anthropic-style ephemeral blocks).
///
/// Translation of the Python staticmethod `_apply_cache_control`.
///
/// `messages` and `tools` are expected as slices of [`serde_json::Value`] (arrays of objects).
/// Returns owned `(new_messages, new_tools)`.
pub fn apply_cache_control(
    messages: &[Value],
    tools: Option<&[Value]>,
) -> (Vec<Value>, Option<Vec<Value>>) {
    let cache_marker = json!({"type": "ephemeral"});

    fn mark_message(msg: &Value, cache_marker: &Value) -> Value {
        let mut msg_obj = match msg.as_object() {
            Some(map) => map.clone(),
            None => return msg.clone(),
        };
        match msg_obj.get("content") {
            Some(Value::String(content)) => {
                msg_obj.insert(
                    "content".to_string(),
                    Value::Array(vec![json!({
                        "type": "text",
                        "text": content,
                        "cache_control": cache_marker
                    })]),
                );
                Value::Object(msg_obj)
            }
            Some(Value::Array(arr)) if !arr.is_empty() => {
                let mut nc = arr.clone();
                if let Some(last) = nc.last_mut() {
                    let mut last_obj = match last.as_object() {
                        Some(m) => m.clone(),
                        None => return msg.clone(),
                    };
                    last_obj.insert("cache_control".to_string(), cache_marker.clone());
                    *last = Value::Object(last_obj);
                }
                msg_obj.insert("content".to_string(), Value::Array(nc));
                Value::Object(msg_obj)
            }
            _ => msg.clone(),
        }
    }

    let mut new_messages: Vec<Value> = messages.iter().cloned().collect();

    if let Some(first) = new_messages.get_mut(0) {
        if first.get("role").and_then(|v| v.as_str()) == Some("system") {
            *first = mark_message(first, &cache_marker);
        }
    }
    if new_messages.len() >= 3 {
        let idx = new_messages.len() - 2;
        let target = &mut new_messages[idx];
        *target = mark_message(target, &cache_marker);
    }

    let new_tools = tools.map(|tools_slice| {
        let mut new_vec: Vec<Value> = tools_slice.iter().cloned().collect();
        if let Some(last) = new_vec.last_mut() {
            if let Some(obj) = last.as_object() {
                let mut last_obj = obj.clone();
                last_obj.insert("cache_control".to_string(), cache_marker.clone());
                *last = Value::Object(last_obj);
            }
        }
        new_vec
    });

    (new_messages, new_tools)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_apply_cache_control() {
        let messages = vec![
            json!({
                "role": "system",
                "content": "You are a helpful assistant."
            }),
            json!({
                "role": "user",
                "content": "Hello, how are you?"
            }),
            json!({
                "role": "assistant",
                "content": "I am a helpful assistant."
            }),
        ];
        let tools = vec![json!({
            "name": "test",
            "description": "Test tool",
            "parameters": { "type": "object", "properties": { "key": { "type": "string" } } }
        })];
        let tools_option = Some(tools.as_slice());
        let (new_messages, new_tools) = apply_cache_control(&messages, tools_option);
        println!(
            "new_messages: {}",
            serde_json::to_string_pretty(&new_messages).unwrap()
        );
        println!(
            "new_tools: {}",
            serde_json::to_string_pretty(&new_tools).unwrap()
        );
        assert_eq!(new_messages.len(), 3);
        assert_eq!(new_tools.unwrap().len(), 1);
        assert_eq!(
            new_messages[0],
            json!({
              "role": "system",
              "content": [
                {
                  "type": "text",
                  "text": "You are a helpful assistant.",
                  "cache_control": {
                    "type": "ephemeral"
                  }
                }
              ]
            })
        );
        assert_eq!(
            new_messages[1],
            json!({
              "role": "user",
              "content": [
                {
                  "type": "text",
                  "text": "Hello, how are you?",
                  "cache_control": {
                    "type": "ephemeral"
                  }
                }
              ]
            })
        );
        assert_eq!(
            new_messages[2],
            json!({
              "role": "assistant",
              "content": "I am a helpful assistant."
            })
        );
    }
}
