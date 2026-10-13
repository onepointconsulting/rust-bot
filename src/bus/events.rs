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
    /// Stable id of this submission, `<channel>:<chat_id>:<platform message id>`,
    /// used to drop a redelivered message instead of running a second turn.
    /// `None` for messages without a stable platform id (and for internally
    /// generated ones); those are never deduplicated.
    #[serde(default)]
    pub request_id: Option<String>,
}

impl InboundMessage {
    /// Request id for a message whose adapter stored the platform message id
    /// under `message_id` in `metadata`; `None` when it did not (ids are never
    /// invented, because a made-up id would defeat redelivery detection).
    pub fn request_id_from_metadata(
        channel: &str,
        chat_id: &str,
        metadata: &HashMap<String, serde_json::Value>,
    ) -> Option<String> {
        let platform_id = match metadata.get("message_id")? {
            serde_json::Value::String(id) if !id.is_empty() => id.clone(),
            serde_json::Value::Number(id) => id.to_string(),
            _ => return None,
        };
        Some(Self::namespaced_request_id(channel, chat_id, &platform_id))
    }

    /// Build a namespaced request id from a platform message id.
    ///
    /// The namespace keeps ids of different channels and chats apart.
    pub fn namespaced_request_id(channel: &str, chat_id: &str, platform_id: &str) -> String {
        format!("{channel}:{chat_id}:{platform_id}")
    }

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

#[cfg(test)]
mod request_id_tests {
    use super::*;
    use serde_json::json;

    fn metadata(value: serde_json::Value) -> HashMap<String, serde_json::Value> {
        HashMap::from([("message_id".to_string(), value)])
    }

    #[test]
    fn request_id_is_namespaced_by_channel_and_chat() {
        let id = InboundMessage::request_id_from_metadata("telegram", "42", &metadata(json!("m7")));
        assert_eq!(id.as_deref(), Some("telegram:42:m7"));
        let other = InboundMessage::request_id_from_metadata("slack", "42", &metadata(json!("m7")));
        assert_ne!(id, other);
    }

    #[test]
    fn numeric_platform_ids_are_accepted() {
        let id = InboundMessage::request_id_from_metadata("telegram", "42", &metadata(json!(99)));
        assert_eq!(id.as_deref(), Some("telegram:42:99"));
    }

    #[test]
    fn missing_or_unusable_platform_ids_yield_no_request_id() {
        let none = |meta| InboundMessage::request_id_from_metadata("c", "x", &meta);
        assert_eq!(none(HashMap::new()), None);
        assert_eq!(none(metadata(json!(""))), None);
        assert_eq!(none(metadata(json!(null))), None);
        assert_eq!(none(metadata(json!({"nested": 1}))), None);
    }

    #[test]
    fn old_serialized_messages_without_request_id_still_deserialize() {
        let message: InboundMessage = serde_json::from_value(json!({
            "channel": "c", "sender_id": "u", "chat_id": "x", "content": "hi",
            "timestamp": "2026-01-01T00:00:00Z"
        }))
        .unwrap();
        assert_eq!(message.request_id, None);
    }
}
