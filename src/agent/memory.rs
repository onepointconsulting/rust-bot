/// Memory system for persistent agent memory.
use std::sync::LazyLock;
use std::io::Write;

pub static SAVE_MEMORY_TOOL: LazyLock<Vec<serde_json::Value>> = LazyLock::new(|| {
    vec![serde_json::json!({
        "type": "function",
        "function": {
            "name": "save_memory",
            "description": "Save the memory consolidation result to persistent storage.",
            "parameters": {
                "type": "object",
                "properties": {
                    "history_entry": {
                        "type": "string",
                        "description": "A paragraph summarizing key events/decisions/topics. Start with [YYYY-MM-DD HH:MM]. Include detail useful for grep search."
                    },
                    "memory_update": {
                        "type": "string",
                        "description": "Full updated long-term memory as markdown. Include all existing facts plus new ones. Return unchanged if nothing new."
                    }
                },
                "required": ["history_entry", "memory_update"]
            }
        }
    })]
});

pub static TOOL_CHOICE_ERROR_MARKERS: LazyLock<Vec<&str>> = LazyLock::new(|| {
    vec!["tool_choice", "toolchoice", "does not support", "should be [\"none\", \"auto\"]"]
});

fn ensure_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        other => serde_json::to_string(other).unwrap_or_else(|_| String::from("null")),
    }
}

fn normalize_save_memory_args(args: &serde_json::Value) -> Option<serde_json::Map<String, serde_json::Value>> {
    let args_value = match args {
        serde_json::Value::String(s) => {
            match serde_json::from_str::<serde_json::Value>(s) {
                Ok(v) => v,
                Err(_) => return None,
            }
        },
        _ => args.clone(),
    };

    match &args_value {
        serde_json::Value::Array(arr) => {
            if let Some(serde_json::Value::Object(map)) = arr.get(0) {
                return Some(map.clone());
            } else {
                return None;
            }
        }
        serde_json::Value::Object(map) => Some(map.clone()),
        _ => None,
    }
}

fn is_tool_choice_unsupported(content: Option<&str>) -> bool {
    match content {
        Some(content) => TOOL_CHOICE_ERROR_MARKERS.iter().any(|marker| content.contains(marker)),
        None => false,
    }
}

pub const MAX_FAILURES_BEFORE_RAW_ARCHIVE: usize = 3;

/// Two-layer memory: MEMORY.md (long-term facts) + HISTORY.md (grep-searchable log).
struct MemoryStore {
    memory_dir: std::path::PathBuf,
    memory_file: std::path::PathBuf,
    history_file: std::path::PathBuf,
    consecutive_failures: usize,
}

impl MemoryStore {

    pub fn new(workspace: std::path::PathBuf) -> Self {
        let memory_dir = workspace.join("memory");
        if !memory_dir.clone().exists() {
            std::fs::create_dir_all(&memory_dir).unwrap();
        }
        let memory_path = memory_dir.join("MEMORY.md");
        let history_path = memory_dir.join("HISTORY.md");
        Self { memory_dir, memory_file: memory_path, history_file: history_path, consecutive_failures: 0 }
    }

    pub fn read_long_term(&self) -> String {
        if self.memory_file.exists() {
            match std::fs::read_to_string(&self.memory_file) {
                Ok(content) => content,
                Err(_) => String::new(),
            }
        } else {
            String::new()
        }
    }

    pub fn write_long_term(&self, content: &str) {
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .open(&self.memory_file)
        {
            let _ = file.write_all(content.as_bytes());
        }
    }

    pub fn append_history(&self, entry: &str) {
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.history_file)
        {
            let to_write = format!("{}\n\n", entry.trim_end());
            let _ = file.write_all(to_write.as_bytes());
        }
    }

    pub fn get_memory_context(&self) -> String {
        let long_term = self.read_long_term();
        format!("Long-term memory:\n{}", long_term)
    }

    /// Formats a list of message dicts as a pretty string, in the style of the Python _format_messages staticmethod.
    /// 
    /// - `messages`: &[serde_json::Value] (each should be an object with keys: content, timestamp, role, tools_used)
    /// 
    /// Returns a single String, one message per line.
    fn format_messages(messages: &[serde_json::Value]) -> String {
        let mut lines = Vec::new();

        for message in messages {
            let content = message.get("content").and_then(|v| v.as_str());
            if content.is_none() || content.unwrap().is_empty() {
                continue;
            }
            let content = content.unwrap();

            // Determine tools_used formatting.
            let tools_used = match message.get("tools_used") {
                Some(serde_json::Value::Array(arr)) if !arr.is_empty() => {
                    let joined = arr.iter()
                        .filter_map(|v| v.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!(" [tools: {}]", joined)
                }
                _ => String::new(),
            };

            // Format timestamp
            let timestamp = message
                .get("timestamp")
                .and_then(|v| v.as_str())
                .map(|s| {
                    // Truncate to 16 chars if possible
                    if s.len() > 16 { &s[..16] } else { s }
                })
                .unwrap_or("?");

            let role = message
                .get("role")
                .and_then(|v| v.as_str())
                .map(|s| s.to_uppercase())
                .unwrap_or_else(|| "UNKNOWN".to_owned());

            lines.push(format!(
                "[{}] {}{}: {}",
                timestamp, role, tools_used, content
            ));
        }

        lines.join("\n")
    }

    
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn create_memory_store() -> MemoryStore {
        let workspace = PathBuf::from("docs/workspace_sample1");
        MemoryStore::new(workspace)
    }

    #[test]
    fn test_ensure_text() {
        let value = serde_json::json!({
            "history_entry": "2026-03-25 10:00:00",
            "memory_update": "Full updated long-term memory as markdown. Include all existing facts plus new ones. Return unchanged if nothing new."
        });
        let result = ensure_text(&value);
        assert_eq!(result, "{\"history_entry\":\"2026-03-25 10:00:00\",\"memory_update\":\"Full updated long-term memory as markdown. Include all existing facts plus new ones. Return unchanged if nothing new.\"}");
    }

    #[test]
    fn test_normalize_save_memory_args() {
        let args = serde_json::json!({
            "history_entry": "2026-03-25 10:00:00",
            "memory_update": "Full updated long-term memory as markdown. Include all existing facts plus new ones. Return unchanged if nothing new."
        });
        let result = normalize_save_memory_args(&args);
        assert!(result.is_some());
        let result = result.unwrap();
        assert_eq!(result["history_entry"], "2026-03-25 10:00:00");
        assert_eq!(result["memory_update"], "Full updated long-term memory as markdown. Include all existing facts plus new ones. Return unchanged if nothing new.");
    }

    #[test]
    fn test_is_tool_choice_unsupported_false() {
        let result = is_tool_choice_unsupported(None);
        assert!(!result);
    }

    #[test]
    fn test_is_tool_choice_unsupported_true() {
        let result = is_tool_choice_unsupported(Some("tool_choice"));
        assert!(result);
    }

    #[test]
    fn test_read_long_term() {
        let memory_store = create_memory_store();
        let entry = "The agent initialized a new workspace and created initial memory files.";
        memory_store.write_long_term(entry);
        let result = memory_store.read_long_term();
        println!("result: {}", result);
        assert!(result.contains(entry));
        let result2 = memory_store.get_memory_context();
        assert!(result2.contains(entry));
    }

    #[test]
    fn test_append_history() {
        let memory_store = create_memory_store();
        let messages = vec![
            serde_json::json!({
                "content": "The agent initialized a new workspace and created initial memory files.",
                "timestamp": "2026-03-25 10:00:00",
                "role": "user",
                "tools_used": []
            }),
        ];
        let entry = MemoryStore::format_messages(&messages);
        memory_store.append_history(entry.as_str());
        assert!(memory_store.history_file.exists());
        let result = std::fs::read_to_string(&memory_store.history_file).unwrap();
        println!("result: {}", result);
        assert!(result.contains(entry.as_str()));
    }

    #[test]
    fn test_format_messages() {
        let messages = vec![
            serde_json::json!({
                "content": "Hello, world!",
                "timestamp": "2026-03-25 10:00:00",
                "role": "user",
                "tools_used": []
            }),
            serde_json::json!({
                "content": "I am a robot.",
                "timestamp": "2026-03-25 10:00:01",
                "role": "user",
                "tools_used": []
            })
        ];
        let result = MemoryStore::format_messages(&messages);
        println!("result: {}", result);
        assert!(result.contains("Hello, world!"));
        assert!(result.contains("I am a robot."));
    }
}