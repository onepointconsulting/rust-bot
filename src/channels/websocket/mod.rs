pub mod registry;
pub mod runtime;
pub mod types;
pub mod webui;

/// Canonical channel name: config section key, `BaseChannel::name()`, media
/// subdirectory, session-key prefix (`websocket:{chat_id}`), and outbound
/// bus routing.
pub const CHANNEL_NAME: &str = "websocket";

/// Session key for a websocket chat (`websocket:{chat_id}`), matching
/// `SessionManager`'s `{channel}:{chat_id}` convention.
pub(crate) fn get_session_id(chat_id: &str) -> String {
    format!("{CHANNEL_NAME}:{chat_id}")
}
