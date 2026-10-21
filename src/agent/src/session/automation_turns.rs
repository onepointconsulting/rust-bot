use serde_json::Value;

use crate::session::AUTOMATION_HISTORY_KEY;

/// True for hidden automation trigger records in session history.
pub fn is_automation_history_message(message: &Value) -> bool {
    has_message_value(message, AUTOMATION_HISTORY_KEY)
}

pub fn has_message_value(message: &Value, key: &str) -> bool {
    let Value::Object(map) = message else {
        return false;
    };
    if map.is_empty() {
        return false;
    }
    matches!(
        map.get(key),
        Some(Value::Bool(true)) | Some(Value::Object(_))
    )
}
