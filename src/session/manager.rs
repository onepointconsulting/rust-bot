use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde_json::{Map, Value, json};

use crate::utils::helpers::find_legal_message_start;

/// In-memory conversation session record.
pub struct Session {
    key: String,
    messages: Vec<Value>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    metadata: HashMap<String, Value>,
    last_consolidated: usize,
}

impl Session {

    pub fn new(key: String) -> Self {
        Self {
            key,
            messages: Vec::new(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            metadata: HashMap::new(),
            last_consolidated: 0,
        }
    }

    /// Append a chat-style message (`role`, `content`, RFC3339 `timestamp`) and bump `updated_at`.
    ///
    /// `extras` is merged **after** the base fields—same semantics as Python `**kwargs`:
    /// duplicate keys in `extras` override `role`, `content`, and `timestamp`.
    pub fn add_message(
        &mut self,
        role: impl Into<String>,
        content: impl Into<String>,
        extras: Map<String, Value>,
    ) {
        let mut msg = Map::new();
        msg.insert("role".into(), Value::String(role.into()));
        msg.insert("content".into(), Value::String(content.into()));
        msg.insert("timestamp".into(), Value::String(Utc::now().to_rfc3339()));
        msg.extend(extras);

        self.messages.push(Value::Object(msg));
        self.updated_at = Utc::now();
    }

    pub fn get_history(&self, max_messages: Option<usize>) -> Vec<Value> {
        let max_messages = max_messages.unwrap_or(500);
        let unconsolidated =
            self.messages[(self.last_consolidated).min(self.messages.len())..].to_vec();
        let mut sliced = if unconsolidated.len() > max_messages {
            unconsolidated[unconsolidated.len() - max_messages..].to_vec()
        } else {
            unconsolidated
        };

        // Avoid starting mid-turn when possible.
        for i in 0..sliced.len() {
            if let Some(role) = sliced[i].get("role").and_then(|v| v.as_str()) {
                if role == "user" {
                    sliced = sliced[i..].to_vec();
                    break;
                }
            }
        }

        let mut out: Vec<Value> = Vec::new();
        for message in sliced {
            let mut entry = json!({
                "role": message.get("role").and_then(|v| v.as_str()).unwrap_or(""),
                "content": message.get("content").and_then(|v| v.as_str()).unwrap_or(""),
                "timestamp": message.get("timestamp").and_then(|v| v.as_str()).unwrap_or(""),
            });
            for key in vec!["tool_calls", "tool_call_id", "name", "reasoning_content"] {
                if message.get(key).is_some() {
                    entry[key] = message.get(key).unwrap().clone();
                }
            }
            out.push(entry);
        }
        out
    }

    /// Clears conversation messages and resets consolidation cursor.
    pub fn clear(&mut self) {
        self.messages.clear();
        self.last_consolidated = 0;
        self.updated_at = Utc::now();
    }

    /// Keep a legal recent suffix, mirroring get_history boundary rules.
    pub fn retain_recent_legal_suffix(&mut self, max_messages: usize) {
        if max_messages == 0 {
            self.clear();
            return;
        }
        if self.messages.len() <= max_messages {
            return;
        }

        let prev_last = self.last_consolidated;
        let mut start_idx = self.messages.len() - max_messages;

        // If the cutoff lands mid-turn, extend backward to the nearest user turn.
        while start_idx > 0
            && self.messages[start_idx].get("role").and_then(|v| v.as_str()) != Some("user")
        {
            start_idx -= 1;
        }

        self.messages = self.messages[start_idx..].to_vec();
        let mut last_consolidated = prev_last.saturating_sub(start_idx);

        let legal_trim = find_legal_message_start(&self.messages);
        if legal_trim > 0 {
            self.messages = self.messages[legal_trim..].to_vec();
            last_consolidated = last_consolidated.saturating_sub(legal_trim);
        }

        self.last_consolidated = last_consolidated;
        self.updated_at = Utc::now();
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_message(role: &str, content: &str) -> Value {
        json!({
            "role": role,
            "content": content,
            "timestamp": "2026-01-01T00:00:00Z",
        })
    }

    #[test]
    fn get_history_empty_returns_empty_vec() {
        let session = Session::new("s1".into());
        assert!(session.get_history(None).is_empty());
        assert!(session.get_history(Some(10)).is_empty());
    }

    #[test]
    fn get_history_skips_suffix_before_last_consolidated() {
        let mut session = Session::new("s1".into());
        session
            .messages
            .push(fixture_message("user", "before"));
        session
            .messages
            .push(fixture_message("assistant", "middle"));
        session
            .messages
            .push(fixture_message("user", "after"));
        session.last_consolidated = 1;
        // Window is [assistant, user]; alignment keeps from first user only.
        let h = session.get_history(None);
        assert_eq!(h.len(), 1);
        assert_eq!(h[0]["content"], json!("after"));
    }

    #[test]
    fn get_history_max_messages_keeps_tail_of_window() {
        let mut session = Session::new("s1".into());
        session.messages.push(fixture_message("assistant", "m0"));
        session.messages.push(fixture_message("user", "m1"));
        session.messages.push(fixture_message("assistant", "m2"));
        session.messages.push(fixture_message("user", "m3"));
        session.messages.push(fixture_message("assistant", "m4"));
        let h = session.get_history(Some(2));
        // Tail last two: user m3, assistant m4 — first message is already user → both kept.
        assert_eq!(h.len(), 2);
        assert_eq!(h[0]["role"], json!("user"));
        assert_eq!(h[0]["content"], json!("m3"));
        assert_eq!(h[1]["role"], json!("assistant"));
        assert_eq!(h[1]["content"], json!("m4"));
    }

    #[test]
    fn get_history_alignment_drops_prefix_until_first_user() {
        let mut session = Session::new("s1".into());
        session
            .messages
            .push(fixture_message("assistant", "lead"));
        session
            .messages
            .push(fixture_message("user", "prompt"));
        session
            .messages
            .push(fixture_message("assistant", "reply"));
        let h = session.get_history(Some(50));
        assert_eq!(h.len(), 2);
        assert_eq!(h[0]["role"], json!("user"));
        assert_eq!(h[0]["content"], json!("prompt"));
        assert_eq!(h[1]["content"], json!("reply"));
    }

    #[test]
    fn get_history_forward_whitelisted_optional_fields() {
        let mut session = Session::new("s1".into());
        let mut m = Map::new();
        m.insert("role".into(), json!("assistant"));
        m.insert("content".into(), json!("x"));
        m.insert("timestamp".into(), json!("t"));
        m.insert("tool_call_id".into(), json!("id-9"));
        m.insert(
            "tool_calls".into(),
            json!([{"type": "function", "id": "c1"}]),
        );
        session.messages.push(Value::Object(m));

        let h = session.get_history(None);
        assert_eq!(h.len(), 1);
        assert_eq!(h[0]["tool_call_id"], json!("id-9"));
        assert_eq!(
            h[0]["tool_calls"],
            json!([{"type": "function", "id": "c1"}])
        );
        assert!(h[0].get("name").is_none());
    }

    #[test]
    fn test_add_message() {
        let session_key = "test".to_string();
        let mut session = Session::new(session_key.clone());
        session.add_message("user", "Hello, world!", Map::new());
        assert_eq!(session.messages.len(), 1);
        assert_eq!(
            session.messages[0].get("role").unwrap().as_str().unwrap(),
            "user"
        );
        assert_eq!(
            session.messages[0]
                .get("content")
                .unwrap()
                .as_str()
                .unwrap(),
            "Hello, world!"
        );
        assert_eq!(session.key, session_key);
    }

    #[test]
    fn clear_removes_messages_and_resets_last_consolidated() {
        let mut session = Session::new("k1".into());
        session.add_message("user", "a", Map::new());
        session.add_message("assistant", "b", Map::new());
        session.last_consolidated = 1;
        session.clear();
        assert!(session.messages.is_empty());
        assert_eq!(session.last_consolidated, 0);
        assert!(session.get_history(None).is_empty());
    }

    #[test]
    fn clear_preserves_key_and_metadata() {
        let mut session = Session::new("persist-key".into());
        session.metadata.insert("trace".into(), json!("v1"));
        session.add_message("user", "x", Map::new());
        session.clear();
        assert_eq!(session.key, "persist-key");
        assert_eq!(session.metadata.get("trace"), Some(&json!("v1")));
    }

    #[test]
    fn clear_bumps_updated_at() {
        let mut session = Session::new("k2".into());
        let before = session.updated_at;
        std::thread::sleep(std::time::Duration::from_millis(20));
        session.clear();
        assert!(
            session.updated_at > before,
            "expected updated_at to advance after clear"
        );
    }

    #[test]
    fn retain_recent_remaps_last_consolidated_after_prefix_drop() {
        let mut session = Session::new("k".into());
        session.messages.push(fixture_message("user", "u0"));
        session.messages.push(fixture_message("assistant", "a1"));
        session.messages.push(fixture_message("user", "u2"));
        session.messages.push(fixture_message("assistant", "a3"));
        session.messages.push(fixture_message("user", "u4"));
        session.last_consolidated = 3;
        let history_before = session.get_history(None);

        session.retain_recent_legal_suffix(2);
        assert_eq!(session.messages.len(), 3);
        assert_eq!(
            session.messages[0].get("content"),
            Some(&json!("u2")),
            "user-alignment should include the user before a mid-turn cut"
        );

        assert_eq!(session.last_consolidated, 1);
        let history_after = session.get_history(None);
        assert_eq!(history_before, history_after);
    }

    #[test]
    fn retain_recent_max_zero_behaves_like_clear() {
        let mut session = Session::new("k".into());
        session.messages.push(fixture_message("user", "x"));
        session.last_consolidated = 1;
        session.retain_recent_legal_suffix(0);
        assert!(session.messages.is_empty());
        assert_eq!(session.last_consolidated, 0);
    }

    /// Orphan tool after assistant-only prefix: `find_legal_message_start` can advance past the
    /// illegal prefix once that prefix is retained (here the count cap still keeps the whole vec,
    /// but legal trim drops everything before the first safe index).
    #[test]
    fn retain_recent_applies_legal_trim_for_assistant_led_suffix() {
        let mut orphan = Map::new();
        orphan.insert("role".into(), json!("tool"));
        orphan.insert("content".into(), json!("orphan"));
        orphan.insert("timestamp".into(), json!("t"));
        orphan.insert("tool_call_id".into(), json!("unknown-id"));

        let mut session = Session::new("k".into());
        session.messages.push(fixture_message("assistant", "lead-a"));
        session.messages.push(fixture_message("assistant", "lead-b"));
        session.messages.push(Value::Object(orphan));
        session.messages.push(fixture_message("user", "hi"));
        session.last_consolidated = 0;

        session.retain_recent_legal_suffix(3);
        assert_eq!(session.messages.len(), 1);
        assert_eq!(session.messages[0].get("content"), Some(&json!("hi")));
        assert_eq!(session.last_consolidated, 0);
    }
}
