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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatEntry {
    pub id: u64,
    pub role: Role,
    pub content: String,
    #[serde(default)]
    pub attachments: Vec<ImageAttachment>,
}

/// A message assembled by the composer, ready to be sent.
#[derive(Debug, Clone, Default)]
pub struct OutgoingMessage {
    pub text: String,
    pub attachments: Vec<ImageAttachment>,
}
