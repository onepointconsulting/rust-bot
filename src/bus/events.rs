use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::collections::HashMap;

use super::outbound_events::OutboundEvent;

/// Message received from a chat channel.
#[derive(Debug, Clone, serde::Serialize, Deserialize)]
pub struct InboundMessage {
    /// Channel: telegram, discord, slack, whatsapp
    pub channel: String,
    /// User identifier
    pub sender_id: String,
    /// Chat/channel identifier
    pub chat_id: String,
    /// Message text
    pub content: String,
    pub timestamp: DateTime<Utc>,
    /// Media URLs
    #[serde(default)]
    pub media: Vec<String>,
    /// Channel-specific data
    #[serde(default)]
    pub metadata: HashMap<String, serde_json::Value>,
    /// Optional override for thread-scoped sessions
    #[serde(default)]
    pub session_key_override: Option<String>,
}

impl InboundMessage {
    /// Unique key for session identification.
    pub fn session_key(&self) -> String {
        self.session_key_override
            .clone()
            .unwrap_or_else(|| format!("{}:{}", self.channel, self.chat_id))
    }
}

/// Message to send to a chat channel.
///
/// Display text lives in `content`. Optional typed control/event data lives in
/// `event`; channel-specific odds and ends stay in `metadata` for now.
#[derive(Debug, Clone)]
pub struct OutboundMessage {
    pub channel: String,
    pub chat_id: String,
    pub content: String,
    pub reply_to: Option<String>,
    pub media: Vec<String>,
    pub metadata: HashMap<String, serde_json::Value>,
    pub event: Option<OutboundEvent>,
}

impl OutboundMessage {
    pub const TOKEN_USAGE_KEY: &str = "token_usage";
}