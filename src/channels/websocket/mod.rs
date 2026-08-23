pub mod registry;
pub mod runtime;
pub mod types;
pub mod webui;

/// Session key for a websocket chat (`websocket:{chat_id}`), matching
/// `SessionManager`'s `{channel}:{chat_id}` convention.
pub(crate) fn get_session_id(chat_id: &str) -> String {
    format!("websocket:{chat_id}")
}
