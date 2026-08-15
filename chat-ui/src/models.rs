use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

/// An image attached to an outgoing (or previously sent) message.
///
/// `url` is either an `http(s)://` reference or a `data:image/...;base64,...`
/// URL produced client-side from a picked/dropped/pasted file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageAttachment {
    pub url: String,
    #[serde(default)]
    pub label: Option<String>,
}

/// A single tool invocation's lifecycle, as surfaced by the gateway's live
/// progress stream.
///
/// Mirrors the backend's `ToolEvent` shape (`src/bus/outbound_events.rs`) so
/// `websockets-chat` can deserialize gateway events directly into this type;
/// `chat-ui` has no dependency on the backend crate itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolEvent {
    pub name: String,
    pub status: String,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatEntry {
    pub id: u64,
    pub role: Role,
    pub content: String,
    #[serde(default)]
    pub attachments: Vec<ImageAttachment>,
    /// True while an assistant reply is still streaming in (websockets-chat
    /// only; web-chat never sets this).
    #[serde(default)]
    pub streaming: bool,
    /// Live tool-activity chips for this entry (websockets-chat only).
    #[serde(default)]
    pub tool_events: Option<Vec<ToolEvent>>,
    /// Streamed reasoning/thinking text for this entry (websockets-chat only).
    #[serde(default)]
    pub reasoning: Option<String>,
}

/// A message assembled by the composer, ready to be sent.
#[derive(Debug, Clone, Default)]
pub struct OutgoingMessage {
    pub text: String,
    pub attachments: Vec<ImageAttachment>,
}
