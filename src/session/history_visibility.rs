//! Visibility helpers for persisted session history messages.

use serde_json::Value;

use crate::session::{
    automation_turns::{has_message_value, is_automation_history_message},
    keys::HIDDEN_HISTORY_KEY,
};

fn has_hidden_history_marker(message: &Value) -> bool {
    has_message_value(message, HIDDEN_HISTORY_KEY)
}

/// True for persisted messages that should not be shown as chat turns.
pub fn is_hidden_history_message(message: &Value) -> bool {
    has_hidden_history_marker(message) || is_automation_history_message(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Map, Value, json};

    fn message_with_marker(marker: Option<Value>) -> Value {
        let mut map = Map::new();
        map.insert("role".into(), json!("user"));
        if let Some(marker) = marker {
            map.insert(HIDDEN_HISTORY_KEY.to_string(), marker);
        }
        Value::Object(map)
    }

    #[test]
    fn empty_message_and_missing_marker_are_not_hidden() {
        assert!(!has_hidden_history_marker(&json!({})));
        assert!(!has_hidden_history_marker(&message_with_marker(None)));
    }

    #[test]
    fn non_object_message_is_not_hidden() {
        assert!(!has_hidden_history_marker(&json!(null)));
        assert!(!has_hidden_history_marker(&json!("user")));
        assert!(!has_hidden_history_marker(&json!([])));
    }

    #[test]
    fn exact_true_marker_is_hidden() {
        assert!(has_hidden_history_marker(&message_with_marker(Some(
            json!(true)
        ))));
    }

    #[test]
    fn object_marker_is_hidden() {
        assert!(has_hidden_history_marker(&message_with_marker(Some(
            json!({})
        ))));
        assert!(has_hidden_history_marker(&message_with_marker(Some(
            json!({"reason": "system"})
        ))));
    }

    #[test]
    fn other_marker_values_are_not_hidden() {
        assert!(!has_hidden_history_marker(&message_with_marker(Some(
            json!(false)
        ))));
        assert!(!has_hidden_history_marker(&message_with_marker(Some(
            json!(null)
        ))));
        assert!(!has_hidden_history_marker(&message_with_marker(Some(
            json!(1)
        ))));
        assert!(!has_hidden_history_marker(&message_with_marker(Some(
            json!("true")
        ))));
        assert!(!has_hidden_history_marker(&message_with_marker(Some(
            json!([])
        ))));
    }
}
