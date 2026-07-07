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
