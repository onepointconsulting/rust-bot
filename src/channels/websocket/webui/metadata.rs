//! Shared WebUI metadata keys

pub const WEBUI_TURN_METADATA_KEY: &str = "webui_turn_id";
pub const WEBUI_SYSTEM_COMMAND_TURN_PREFIX: &str = "webui-system:";
pub const WEBSOCKET_TURN_OWNER_METADATA_KEY: &str = "_websocket_turn_owner";
pub const WEBUI_MESSAGE_SOURCE_METADATA_KEY: &str = "_webui_message_source";
/// JWT `sub` of the connection that sent this user turn. Stamped onto
/// inbound metadata, the transcript user row, and the `user` fan-out event.
pub const USER_ID_METADATA_KEY: &str = "user_id";
/// Prefix of the turn id given to the main agent's turn on a background
/// sub-agent's result (a system message): that turn has no user message of its
/// own, and its replies must not be mistaken for the answer to a pending one.
pub const WEBUI_SYSTEM_TURN_PREFIX: &str = "system-";

/// Whether outbound `metadata` belongs to a turn on a sub-agent's result: its
/// WebUI turn id carries the [`WEBUI_SYSTEM_TURN_PREFIX`].
pub fn is_system_turn_metadata(
    metadata: &std::collections::HashMap<String, serde_json::Value>,
) -> bool {
    metadata
        .get(WEBUI_TURN_METADATA_KEY)
        .and_then(serde_json::Value::as_str)
        .is_some_and(|turn_id| turn_id.starts_with(WEBUI_SYSTEM_TURN_PREFIX))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn turn_metadata(turn_id: serde_json::Value) -> HashMap<String, serde_json::Value> {
        HashMap::from([(WEBUI_TURN_METADATA_KEY.to_string(), turn_id)])
    }

    #[test]
    fn a_system_turn_id_is_recognized() {
        assert!(is_system_turn_metadata(&turn_metadata(serde_json::json!(
            "system-0123abcd"
        ))));
    }

    #[test]
    fn a_user_turn_id_is_not_a_system_turn() {
        assert!(!is_system_turn_metadata(&turn_metadata(serde_json::json!(
            "59c922fe-f0aa"
        ))));
    }

    #[test]
    fn metadata_without_a_usable_turn_id_is_not_a_system_turn() {
        assert!(!is_system_turn_metadata(&HashMap::new()));
        assert!(!is_system_turn_metadata(&turn_metadata(serde_json::json!(
            7
        ))));
    }
}
