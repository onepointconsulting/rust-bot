pub const EXPANDED_STORAGE_KEY: &str = "rust-bot-websockets-chat-expanded";
pub const SIDEBAR_OPEN_STORAGE_KEY: &str = "rust-bot-websockets-chat-sidebar-open";
pub const CHAT_OPEN_STORAGE_KEY: &str = "rust-bot-websockets-chat-chat-open";
/// Survives tab close and browser restart so reconnects reuse the same
/// gateway session (`websocket:{chat_id}.jsonl`) instead of minting a new one.
pub const CHAT_ID_STORAGE_KEY: &str = "rust-bot-websockets-chat-chat-id";
