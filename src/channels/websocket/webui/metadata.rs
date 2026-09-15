/// Shared WebUI metadata keys

pub const WEBUI_TURN_METADATA_KEY: &str = "webui_turn_id";
pub const WEBUI_SYSTEM_COMMAND_TURN_PREFIX: &str = "webui-system:";
pub const WEBSOCKET_TURN_OWNER_METADATA_KEY: &str = "_websocket_turn_owner";
pub const WEBUI_MESSAGE_SOURCE_METADATA_KEY: &str = "_webui_message_source";
/// JWT `sub` of the connection that sent this user turn. Stamped onto
/// inbound metadata, the transcript user row, and the `user` fan-out event.
pub const USER_ID_METADATA_KEY: &str = "user_id";
