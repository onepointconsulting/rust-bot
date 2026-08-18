//! OpenAI-compatible chat completion request/response types.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::command::types::ChatCommand;

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({
    "messages": [
        {
            "role": "user",
            "content": [
                { "type": "text", "text": "What's in this?" },
                { "type": "image_url", "image_url": { "url": "https://example.com/a.png" } }
            ]
        }
    ],
    "model": "default",
    "stream": false,
    "user": "my-session"
}))]
pub struct ChatCompletionRequest {
    pub messages: Vec<ChatMessage>,
    #[serde(default)]
    #[schema(example = "default")]
    pub model: Option<String>,
    #[serde(default)]
    #[schema(example = false, default = false)]
    pub stream: Option<bool>,
    #[serde(default)]
    #[schema(example = "my-session")]
    pub user: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({
    "role": "user",
    "content": [
        { "type": "text", "text": "What's in this?" },
        { "type": "image_url", "image_url": { "url": "https://example.com/a.png" } }
    ]
}))]
pub struct ChatMessage {
    #[schema(example = "user")]
    pub role: String,
    #[schema(value_type = String, example = "What can you help me with?")]
    pub content: ChatMessageContent,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum ChatMessageContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

#[derive(Debug, Deserialize)]
pub struct ContentPart {
    #[serde(rename = "type")]
    pub part_type: String,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub image_url: Option<ImageUrl>,
}

#[derive(Debug, Deserialize)]
pub struct ImageUrl {
    pub url: String,
    #[serde(default)]
    pub detail: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ChatCompletionResponse {
    pub id: String,
    pub object: String,
    pub created: i64,
    pub model: String,
    pub choices: Vec<ChatCompletionChoice>,
    pub usage: Usage,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ChatCompletionChoice {
    pub index: u32,
    pub message: AssistantMessage,
    pub finish_reason: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AssistantMessage {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
}

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({
    "command": "new",
    "session_id": "my-session"
}))]
pub struct ChatCommandRequest {
    #[serde(default)]
    #[schema(example = "new")]
    pub command: ChatCommand,

    #[serde(default)]
    #[schema(example = "my-session")]
    pub session_id: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ChatCommandResponse {
    pub command: ChatCommand,
    pub response: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SessionSummary {
    pub key: String,
    pub created_at: String,
    pub updated_at: String,
    pub path: String,
    pub title: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SessionsListResponse {
    pub sessions: Vec<SessionSummary>,
}

impl SessionsListResponse {
    pub fn from_session_entries(entries: &[serde_json::Value]) -> Self {
        let sessions = entries
            .iter()
            .filter_map(|entry| SessionSummary::from_entry(entry))
            .collect();
        Self { sessions }
    }
}

impl SessionSummary {
    fn from_entry(entry: &serde_json::Value) -> Option<Self> {
        let key = entry.get("key")?.as_str()?.to_string();
        Some(Self {
            created_at: entry
                .get("created_at")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            updated_at: entry
                .get("updated_at")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            path: entry
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            title: entry
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            key,
        })
    }
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct ChatLoginRequest {
    pub email: String,
    pub password: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ChatLoginResponse {
    pub token: String,
}

/// Text and image references extracted from a single user message.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UserTurn {
    pub text: String,
    pub image_urls: Vec<String>,
}

fn turn_from_content(content: &ChatMessageContent) -> UserTurn {
    match content {
        ChatMessageContent::Text(text) => UserTurn {
            text: text.clone(),
            image_urls: Vec::new(),
        },
        ChatMessageContent::Parts(parts) => {
            let texts: Vec<&str> = parts
                .iter()
                .filter(|part| part.part_type == "text")
                .filter_map(|part| part.text.as_deref())
                .collect();
            let image_urls: Vec<String> = parts
                .iter()
                .filter(|part| part.part_type == "image_url")
                .filter_map(|part| part.image_url.as_ref())
                .map(|image_url| image_url.url.clone())
                .collect();
            UserTurn {
                text: texts.join("\n"),
                image_urls,
            }
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ExamplePromptsResponse {
    pub prompts: Vec<String>,
}

/// Extract the text and image URLs from the last non-empty user message.
///
/// A turn is considered non-empty when it has non-blank text or at least one
/// image reference (an image-only turn is valid).
pub fn extract_last_user_turn(messages: &[ChatMessage]) -> Option<UserTurn> {
    messages
        .iter()
        .rev()
        .filter(|message| message.role == "user")
        .map(|message| turn_from_content(&message.content))
        .find(|turn| !turn.text.trim().is_empty() || !turn.image_urls.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sessions_list_response_marks_current_session() {
        let entries = vec![
            json!({
                "key": "cli:direct",
                "created_at": "2026-01-01T00:00:00Z",
                "updated_at": "2026-06-01T00:00:00Z",
                "path": "/tmp/cli:direct.jsonl",
            }),
            json!({
                "key": "other",
                "created_at": "2026-01-02T00:00:00Z",
                "updated_at": "2026-06-02T00:00:00Z",
                "path": "/tmp/other.jsonl",
            }),
        ];
        let response = SessionsListResponse::from_session_entries(&entries);
        assert_eq!(response.sessions.len(), 2);
        assert_eq!(response.sessions[0].key, "cli:direct");
        assert_eq!(response.sessions[0].created_at, "2026-01-01T00:00:00Z");
        assert_eq!(response.sessions[0].updated_at, "2026-06-01T00:00:00Z");
        assert_eq!(response.sessions[0].path, "/tmp/cli:direct.jsonl");
        assert_eq!(response.sessions[0].title, "");
        assert_eq!(response.sessions[1].key, "other");
        assert_eq!(response.sessions[1].created_at, "2026-01-02T00:00:00Z");
        assert_eq!(response.sessions[1].updated_at, "2026-06-02T00:00:00Z");
        assert_eq!(response.sessions[1].title, "");
    }

    #[test]
    fn sessions_list_response_forwards_title() {
        let entries = vec![json!({
            "key": "websocket:chat-1",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-06-01T00:00:00Z",
            "path": "/tmp/chat.jsonl",
            "title": "Fix the login bug",
        })];
        let response = SessionsListResponse::from_session_entries(&entries);
        assert_eq!(response.sessions[0].title, "Fix the login bug");
    }

    // ── multimodal content ─────────────────────────────────────────────────

    fn user_message(content: serde_json::Value) -> ChatMessage {
        serde_json::from_value(json!({ "role": "user", "content": content })).unwrap()
    }

    #[test]
    fn chat_message_deserializes_plain_string_content() {
        let msg = user_message(json!("hello"));
        assert!(matches!(msg.content, ChatMessageContent::Text(ref t) if t == "hello"));
    }

    #[test]
    fn chat_message_deserializes_multimodal_content() {
        let msg = user_message(json!([
            { "type": "text", "text": "What's in this?" },
            { "type": "image_url", "image_url": { "url": "https://example.com/a.png" } },
            { "type": "image_url", "image_url": { "url": "data:image/png;base64,AA==" } },
        ]));
        let turn = turn_from_content(&msg.content);
        assert_eq!(turn.text, "What's in this?");
        assert_eq!(
            turn.image_urls,
            vec![
                "https://example.com/a.png".to_string(),
                "data:image/png;base64,AA==".to_string(),
            ]
        );
    }

    #[test]
    fn extract_last_user_turn_returns_text_only_message() {
        let messages = vec![user_message(json!("hello there"))];
        let turn = extract_last_user_turn(&messages).expect("turn");
        assert_eq!(turn.text, "hello there");
        assert!(turn.image_urls.is_empty());
    }

    #[test]
    fn extract_last_user_turn_accepts_image_only_message() {
        let messages = vec![user_message(json!([
            { "type": "image_url", "image_url": { "url": "https://example.com/a.png" } },
        ]))];
        let turn = extract_last_user_turn(&messages).expect("turn");
        assert_eq!(turn.text, "");
        assert_eq!(
            turn.image_urls,
            vec!["https://example.com/a.png".to_string()]
        );
    }

    #[test]
    fn extract_last_user_turn_ignores_blank_and_non_user_messages() {
        let messages = vec![
            user_message(json!("first message")),
            serde_json::from_value::<ChatMessage>(
                json!({ "role": "assistant", "content": "an answer" }),
            )
            .unwrap(),
            user_message(json!("   ")),
        ];
        let turn = extract_last_user_turn(&messages).expect("turn");
        assert_eq!(turn.text, "first message");
    }

    #[test]
    fn extract_last_user_turn_returns_none_when_no_user_content() {
        let messages = vec![user_message(json!("   "))];
        assert!(extract_last_user_turn(&messages).is_none());
    }
}
