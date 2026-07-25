//! OpenAI-compatible chat completion request/response types.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::command::types::ChatCommand;

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({
    "messages": [
        {
            "role": "user",
            "content": "What can you help me with?"
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
    "content": "What can you help me with?"
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
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
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
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SessionsListResponse {
    pub sessions: Vec<SessionSummary>
}

impl SessionsListResponse {
    pub fn from_session_entries(
        entries: &[serde_json::Value],
    ) -> Self {
        let sessions = entries
            .iter()
            .filter_map(|entry| SessionSummary::from_entry(entry))
            .collect();
        Self {
            sessions
        }
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

pub fn content_as_string(content: &ChatMessageContent) -> Option<String> {
    match content {
        ChatMessageContent::Text(text) => Some(text.clone()),
        ChatMessageContent::Parts(parts) => {
            let texts: Vec<&str> = parts
                .iter()
                .filter(|part| part.part_type == "text")
                .filter_map(|part| part.text.as_deref())
                .collect();
            if texts.is_empty() {
                None
            } else {
                Some(texts.join("\n"))
            }
        }
    }
}

pub fn extract_last_user_message(messages: &[ChatMessage]) -> Option<String> {
    messages
        .iter()
        .rev()
        .filter(|message| message.role == "user")
        .find_map(|message| content_as_string(&message.content))
        .filter(|content| !content.trim().is_empty())
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
        assert_eq!(response.sessions[1].key, "other");
        assert_eq!(response.sessions[1].created_at, "2026-01-02T00:00:00Z");
        assert_eq!(response.sessions[1].updated_at, "2026-06-02T00:00:00Z");
    }
}
