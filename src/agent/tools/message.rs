use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::Value;

use crate::agent::tools::base::Tool;
use crate::bus::events::OutboundMessage;
use crate::bus::queue::MessageBus;
use crate::utils::helpers::strip_think;

pub type SendCallback =
    Arc<dyn Fn(OutboundMessage) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// Inbound WebUI flag copied onto owner-bound outbounds so
/// `WebSocketChannel::send` persists the delivery to the transcript.
const WEBUI_METADATA_KEY: &str = "webui";
/// Same key `WebUiTranscriptRecorder::annotate_turn` reads for `turn_id`.
const WEBUI_TURN_ID_KEY: &str = "webui_turn_id";

/// Tool for sending messages (and optional media) to a chat channel.
pub struct MessageTool {
    send_callback: Mutex<Option<SendCallback>>,
    default_channel: Mutex<String>,
    default_chat_id: Mutex<String>,
    default_message_id: Mutex<Option<String>>,
    /// `webui` / `webui_turn_id` from the inbound turn, if any. Copied onto
    /// owner-bound sends so the WebUI transcript records the delivery;
    /// cross-chat sends must not inherit them. Replaced each turn from
    /// inbound metadata — not inferred from `channel == "websocket"`.
    inbound_webui_metadata: Mutex<HashMap<String, Value>>,
    pub sent_in_turn: Mutex<bool>,
    /// Last outbound delivered back to the owning chat this turn (for sync callers).
    last_owner_outbound: Mutex<Option<OutboundMessage>>,
}

impl MessageTool {
    pub fn new(
        send_callback: Option<SendCallback>,
        default_channel: impl Into<String>,
        default_chat_id: impl Into<String>,
        default_message_id: Option<String>,
    ) -> Self {
        Self {
            send_callback: Mutex::new(send_callback),
            default_channel: Mutex::new(default_channel.into()),
            default_chat_id: Mutex::new(default_chat_id.into()),
            default_message_id: Mutex::new(default_message_id),
            inbound_webui_metadata: Mutex::new(HashMap::new()),
            sent_in_turn: Mutex::new(false),
            last_owner_outbound: Mutex::new(None),
        }
    }

    /// Set the current message context.
    pub fn set_context(&self, channel: &str, chat_id: &str, message_id: Option<&str>) {
        *self
            .default_channel
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = channel.to_string();
        *self
            .default_chat_id
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = chat_id.to_string();
        *self
            .default_message_id
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = message_id.map(str::to_string);
    }

    /// Snapshot inbound WebUI keys for this turn. Empty when the inbound
    /// message was not a WebUI turn, so a later CLI/Telegram send cannot
    /// leak a stale `webui: true` onto the outbound.
    pub fn set_inbound_webui_metadata(&self, inbound: &HashMap<String, Value>) {
        *self
            .inbound_webui_metadata
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = extract_webui_outbound_metadata(inbound);
    }

    /// Set the callback for sending messages.
    pub fn set_send_callback(&self, callback: SendCallback) {
        *self.send_callback.lock().unwrap_or_else(|e| e.into_inner()) = Some(callback);
    }

    /// Reset per-turn send tracking.
    pub fn start_turn(&self) {
        *self.sent_in_turn.lock().unwrap_or_else(|e| e.into_inner()) = false;
        *self
            .last_owner_outbound
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// Outbound already delivered to the owning chat this turn, if any.
    pub fn take_delivered_outbound(&self) -> Option<OutboundMessage> {
        if !*self.sent_in_turn.lock().unwrap_or_else(|e| e.into_inner()) {
            return None;
        }
        self.last_owner_outbound
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }

    pub fn create_send_callback(bus: Arc<MessageBus>) -> SendCallback {
        Arc::new(move |msg| {
            let bus = Arc::clone(&bus);
            Box::pin(async move {
                let _ = bus.publish_outbound(msg);
            })
        })
    }
}

pub const MESSAGE_TOOL_NAME: &str = "message";

#[async_trait]
impl Tool for MessageTool {
    fn name(&self) -> String {
        MESSAGE_TOOL_NAME.to_string()
    }

    fn description(&self) -> String {
        "Send a message to the user, optionally with file attachments. \
         This is the ONLY way to deliver files (images, documents, audio, video) to the user. \
         Use the 'media' parameter with file paths to attach files. \
         Do NOT use read_file to send files — that only reads content for your own analysis."
            .to_string()
    }

    fn set_tool_context(&self, channel: &str, chat_id: &str, message_id: Option<&str>) {
        self.set_context(channel, chat_id, message_id);
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "content": {
                    "type": "string",
                    "description": "The message content to send",
                },
                "channel": {
                    "type": "string",
                    "description": "Optional: target channel (telegram, discord, etc.)",
                },
                "chat_id": {
                    "type": "string",
                    "description": "Optional: target chat/user ID",
                },
                "media": {
                    "type": "array",
                    "description": "Optional: list of file paths to attach (images, audio, documents)",
                    "items": {
                        "type": "string",
                    },
                },
            },
            "required": ["content"],
        })
    }

    async fn execute(&self, params: &Value) -> String {
        let mut content = params.get("content").and_then(Value::as_str).unwrap_or("");
        if content.is_empty() {
            return "Error: missing required parameter 'content'".to_string();
        }
        let stripped = strip_think(content);
        content = stripped.as_str();

        let default_channel = self
            .default_channel
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let default_chat_id = self
            .default_chat_id
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();

        let channel = params
            .get("channel")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| default_channel.clone());
        let chat_id = params
            .get("chat_id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| default_chat_id.clone());

        // Only inherit the default message_id when targeting the same channel+chat.
        // Cross-chat sends must not carry the original message_id, because some
        // channels use it to determine the target conversation via their Reply API,
        // which would route the message to the wrong chat entirely.
        let going_back_to_owner = channel == default_channel && chat_id == default_chat_id;
        let message_id: Option<String> = if going_back_to_owner {
            self.default_message_id
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        } else {
            None
        };

        if channel.is_empty() || chat_id.is_empty() {
            return "Error: No target channel/chat specified".to_string();
        } else {
            log::debug!("MessageTool: sending message to channel: {channel}, chat_id: {chat_id}");
        }

        let callback = self
            .send_callback
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let Some(callback) = callback else {
            return "Error: Message sending not configured".to_string();
        };

        let media: Vec<String> = params
            .get("media")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();

        let mut metadata = HashMap::new();
        if let Some(ref message_id) = message_id {
            metadata.insert("message_id".to_string(), Value::String(message_id.clone()));
        }
        if going_back_to_owner {
            metadata.extend(
                self.inbound_webui_metadata
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone(),
            );
        }
        let outbound = OutboundMessage {
            channel: channel.clone(),
            chat_id: chat_id.clone(),
            content: content.to_string(),
            reply_to: None,
            media: media.clone(),
            metadata,
            event: None,
        };

        let owner_outbound = going_back_to_owner.then(|| outbound.clone());
        callback(outbound).await;

        if let Some(owner_outbound) = owner_outbound {
            *self
                .last_owner_outbound
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(owner_outbound);
            *self.sent_in_turn.lock().unwrap_or_else(|e| e.into_inner()) = true;
        }

        let media_info = if media.is_empty() {
            String::new()
        } else {
            format!(" with {} attachments", media.len())
        };

        format!("Message sent to {channel}:{chat_id}{media_info}")
    }
}

fn extract_webui_outbound_metadata(inbound: &HashMap<String, Value>) -> HashMap<String, Value> {
    let mut out = HashMap::new();
    if inbound.get(WEBUI_METADATA_KEY).and_then(Value::as_bool) != Some(true) {
        return out;
    }
    out.insert(WEBUI_METADATA_KEY.to_string(), Value::Bool(true));
    if let Some(turn_id) = inbound.get(WEBUI_TURN_ID_KEY).cloned().filter(|value| {
        value
            .as_str()
            .is_some_and(|turn_id| !turn_id.trim().is_empty())
    }) {
        out.insert(WEBUI_TURN_ID_KEY.to_string(), turn_id);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a callback that records every `OutboundMessage` it receives.
    fn capturing_callback() -> (SendCallback, Arc<Mutex<Vec<OutboundMessage>>>) {
        let captured: Arc<Mutex<Vec<OutboundMessage>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&captured);
        let callback: SendCallback = Arc::new(move |msg: OutboundMessage| {
            let sink = Arc::clone(&sink);
            Box::pin(async move {
                sink.lock().unwrap().push(msg);
            })
        });
        (callback, captured)
    }

    fn tool_with_capture(
        default_channel: &str,
        default_chat_id: &str,
        default_message_id: Option<String>,
    ) -> (MessageTool, Arc<Mutex<Vec<OutboundMessage>>>) {
        let (callback, captured) = capturing_callback();
        let tool = MessageTool::new(
            Some(callback),
            default_channel,
            default_chat_id,
            default_message_id,
        );
        (tool, captured)
    }

    // ── metadata ──────────────────────────────────────────────────────────────

    #[test]
    fn name_is_message() {
        let tool = MessageTool::new(None, "", "", None);
        assert_eq!(tool.name(), "message");
    }

    #[test]
    fn parameters_require_content_only() {
        let tool = MessageTool::new(None, "", "", None);
        let params = tool.parameters();
        assert_eq!(params["required"], serde_json::json!(["content"]));
        assert!(params["properties"]["media"].is_object());
    }

    // ── context setters ─────────────────────────────────────────────────────────

    #[test]
    fn set_context_updates_defaults() {
        let tool = MessageTool::new(None, "cli", "direct", None);
        tool.set_context("telegram", "chat-42", Some("msg-1"));

        assert_eq!(*tool.default_channel.lock().unwrap(), "telegram");
        assert_eq!(*tool.default_chat_id.lock().unwrap(), "chat-42");
        assert_eq!(
            *tool.default_message_id.lock().unwrap(),
            Some("msg-1".to_string())
        );
    }

    #[test]
    fn set_tool_context_via_trait_delegates_to_set_context() {
        let tool = MessageTool::new(None, "cli", "direct", Some("old".to_string()));
        Tool::set_tool_context(&tool, "discord", "guild-7", None);

        assert_eq!(*tool.default_channel.lock().unwrap(), "discord");
        assert_eq!(*tool.default_chat_id.lock().unwrap(), "guild-7");
        assert_eq!(*tool.default_message_id.lock().unwrap(), None);
    }

    #[test]
    fn start_turn_resets_sent_in_turn() {
        let tool = MessageTool::new(None, "cli", "direct", None);
        *tool.sent_in_turn.lock().unwrap() = true;
        *tool.last_owner_outbound.lock().unwrap() = Some(OutboundMessage {
            channel: "cli".into(),
            chat_id: "direct".into(),
            content: "stale".into(),
            reply_to: None,
            media: vec![],
            metadata: HashMap::new(),
            event: None,
        });
        tool.start_turn();
        assert!(!*tool.sent_in_turn.lock().unwrap());
        assert!(tool.last_owner_outbound.lock().unwrap().is_none());
    }

    #[test]
    fn set_send_callback_replaces_none() {
        let tool = MessageTool::new(None, "cli", "direct", None);
        assert!(tool.send_callback.lock().unwrap().is_none());
        let (callback, _captured) = capturing_callback();
        tool.set_send_callback(callback);
        assert!(tool.send_callback.lock().unwrap().is_some());
    }

    // ── execute: error paths ────────────────────────────────────────────────────

    #[tokio::test]
    async fn execute_missing_content_returns_error() {
        let (tool, _captured) = tool_with_capture("cli", "direct", None);
        let result = tool.execute(&serde_json::json!({})).await;
        assert_eq!(result, "Error: missing required parameter 'content'");
    }

    #[tokio::test]
    async fn execute_without_target_returns_error() {
        let (tool, _captured) = tool_with_capture("", "", None);
        let result = tool.execute(&serde_json::json!({ "content": "hi" })).await;
        assert_eq!(result, "Error: No target channel/chat specified");
    }

    #[tokio::test]
    async fn execute_without_callback_returns_error() {
        let tool = MessageTool::new(None, "cli", "direct", None);
        let result = tool.execute(&serde_json::json!({ "content": "hi" })).await;
        assert_eq!(result, "Error: Message sending not configured");
    }

    // ── execute: happy paths ────────────────────────────────────────────────────

    #[tokio::test]
    async fn execute_sends_to_default_context_and_marks_sent_in_turn() {
        let (tool, captured) = tool_with_capture("cli", "direct", Some("owner-msg".to_string()));

        let result = tool
            .execute(&serde_json::json!({ "content": "hello" }))
            .await;

        assert_eq!(result, "Message sent to cli:direct");
        let sent = captured.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].channel, "cli");
        assert_eq!(sent[0].chat_id, "direct");
        assert_eq!(sent[0].content, "hello");
        // Same channel+chat as the default ⇒ message_id is carried in metadata.
        assert_eq!(
            sent[0].metadata.get("message_id"),
            Some(&Value::String("owner-msg".to_string()))
        );
        assert!(*tool.sent_in_turn.lock().unwrap());
        let delivered = tool.take_delivered_outbound().expect("owner outbound");
        assert_eq!(delivered.content, "hello");
        assert!(tool.take_delivered_outbound().is_none());
        assert!(sent[0].metadata.get(WEBUI_METADATA_KEY).is_none());
    }

    #[tokio::test]
    async fn execute_owner_send_copies_inbound_webui_keys() {
        let (tool, captured) = tool_with_capture("websocket", "chat-1", None);
        tool.set_inbound_webui_metadata(&HashMap::from([
            (WEBUI_METADATA_KEY.to_string(), Value::Bool(true)),
            (
                WEBUI_TURN_ID_KEY.to_string(),
                Value::String("turn-abc".to_string()),
            ),
        ]));

        tool.execute(&serde_json::json!({ "content": "here's the image" }))
            .await;

        let sent = captured.lock().unwrap();
        assert_eq!(
            sent[0].metadata.get(WEBUI_METADATA_KEY),
            Some(&Value::Bool(true))
        );
        assert_eq!(
            sent[0].metadata.get(WEBUI_TURN_ID_KEY),
            Some(&Value::String("turn-abc".to_string()))
        );
    }

    #[tokio::test]
    async fn execute_websocket_channel_without_inbound_webui_does_not_invent_the_flag() {
        let (tool, captured) = tool_with_capture("websocket", "chat-1", None);

        tool.execute(&serde_json::json!({ "content": "hi" })).await;

        let sent = captured.lock().unwrap();
        assert!(sent[0].metadata.get(WEBUI_METADATA_KEY).is_none());
        assert!(sent[0].metadata.get(WEBUI_TURN_ID_KEY).is_none());
    }

    #[tokio::test]
    async fn execute_cross_chat_does_not_copy_inbound_webui_keys() {
        let (tool, captured) = tool_with_capture("websocket", "chat-1", None);
        tool.set_inbound_webui_metadata(&HashMap::from([
            (WEBUI_METADATA_KEY.to_string(), Value::Bool(true)),
            (
                WEBUI_TURN_ID_KEY.to_string(),
                Value::String("turn-abc".to_string()),
            ),
        ]));

        tool.execute(&serde_json::json!({
            "content": "hi there",
            "channel": "telegram",
            "chat_id": "other-chat",
        }))
        .await;

        let sent = captured.lock().unwrap();
        assert!(sent[0].metadata.get(WEBUI_METADATA_KEY).is_none());
        assert!(sent[0].metadata.get(WEBUI_TURN_ID_KEY).is_none());
    }

    #[test]
    fn set_inbound_webui_metadata_clears_when_inbound_is_not_webui() {
        let tool = MessageTool::new(None, "websocket", "chat-1", None);
        tool.set_inbound_webui_metadata(&HashMap::from([
            (WEBUI_METADATA_KEY.to_string(), Value::Bool(true)),
            (
                WEBUI_TURN_ID_KEY.to_string(),
                Value::String("turn-abc".to_string()),
            ),
        ]));
        tool.set_inbound_webui_metadata(&HashMap::new());
        assert!(tool.inbound_webui_metadata.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn execute_strips_think_tags_from_content() {
        let (tool, captured) = tool_with_capture("cli", "direct", None);

        tool.execute(&serde_json::json!({
            "content": "<think>secret reasoning</think>visible answer",
        }))
        .await;

        let sent = captured.lock().unwrap();
        assert_eq!(sent[0].content, "visible answer");
    }

    #[tokio::test]
    async fn execute_cross_chat_drops_message_id_and_skips_sent_in_turn() {
        let (tool, captured) = tool_with_capture("cli", "direct", Some("owner-msg".to_string()));

        let result = tool
            .execute(&serde_json::json!({
                "content": "hi there",
                "channel": "telegram",
                "chat_id": "other-chat",
            }))
            .await;

        assert_eq!(result, "Message sent to telegram:other-chat");
        let sent = captured.lock().unwrap();
        assert_eq!(sent[0].channel, "telegram");
        assert_eq!(sent[0].chat_id, "other-chat");
        // Cross-chat send must not carry the owner's message_id.
        assert!(sent[0].metadata.get("message_id").is_none());
        // Not going back to the owner ⇒ sent_in_turn stays false.
        assert!(!*tool.sent_in_turn.lock().unwrap());
        assert!(tool.take_delivered_outbound().is_none());
    }

    #[tokio::test]
    async fn execute_reports_media_count_and_forwards_paths() {
        let (tool, captured) = tool_with_capture("cli", "direct", None);

        let result = tool
            .execute(&serde_json::json!({
                "content": "see attached",
                "media": ["a.png", "b.pdf"],
            }))
            .await;

        assert_eq!(result, "Message sent to cli:direct with 2 attachments");
        let sent = captured.lock().unwrap();
        assert_eq!(
            sent[0].media,
            vec!["a.png".to_string(), "b.pdf".to_string()]
        );
    }

    #[tokio::test]
    async fn execute_blank_channel_param_falls_back_to_default() {
        let (tool, captured) = tool_with_capture("cli", "direct", None);

        let result = tool
            .execute(&serde_json::json!({
                "content": "hi",
                "channel": "",
                "chat_id": "",
            }))
            .await;

        assert_eq!(result, "Message sent to cli:direct");
        assert_eq!(captured.lock().unwrap().len(), 1);
    }
}
