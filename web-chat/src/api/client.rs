//! Thin client for the rust-bot OpenAPI REST surface
//! (`/v1/login`, `/v1/chat/completions`, `/v1/chat/commands`).
//!
//! Requests are relative (e.g. `/v1/login`) so the app works unmodified
//! whether it's served by `rust-bot api --web-root` (same origin) or
//! proxied during `trunk serve` (see `Trunk.toml`).

use gloo_net::http::Request;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub struct ApiError {
    pub message: String,
}

impl ApiError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

#[derive(Debug, Serialize)]
struct LoginRequest<'a> {
    email: &'a str,
    password: &'a str,
}

#[derive(Debug, Deserialize)]
struct LoginResponse {
    token: String,
}

#[derive(Debug, Serialize)]
struct ChatMessage<'a> {
    role: &'a str,
    content: ChatMessageContent<'a>,
}

/// Mirrors the server's OpenAI-compatible multimodal content shape: either a
/// plain string, or an array of `text` / `image_url` parts.
#[derive(Debug, Serialize)]
#[serde(untagged)]
enum ChatMessageContent<'a> {
    Text(&'a str),
    Parts(Vec<ContentPart<'a>>),
}

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
enum ContentPart<'a> {
    #[serde(rename = "text")]
    Text { text: &'a str },
    #[serde(rename = "image_url")]
    ImageUrl { image_url: ImageUrlRef<'a> },
}

#[derive(Debug, Serialize)]
struct ImageUrlRef<'a> {
    url: &'a str,
}

#[derive(Debug, Serialize)]
struct ChatCompletionRequest<'a> {
    messages: Vec<ChatMessage<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    user: Option<&'a str>,
}

#[derive(Debug, Deserialize)]
struct ChatCompletionResponse {
    choices: Vec<ChatCompletionChoice>,
}

#[derive(Debug, Deserialize)]
struct ChatCompletionChoice {
    message: AssistantMessage,
}

#[derive(Debug, Deserialize)]
struct AssistantMessage {
    content: String,
}

#[derive(Debug, Serialize)]
struct ChatCommandRequest<'a> {
    command: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<&'a str>,
}

#[derive(Debug, Deserialize)]
struct ChatCommandResponse {
    response: String,
}

#[derive(Debug, Deserialize)]
struct ExamplePromptsResponse {
    prompts: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ErrorBody {
    error: ErrorDetail,
}

#[derive(Debug, Deserialize)]
struct ErrorDetail {
    message: String,
}

async fn error_from_response(resp: gloo_net::http::Response) -> ApiError {
    let status = resp.status();
    match resp.json::<ErrorBody>().await {
        Ok(body) => ApiError::new(body.error.message),
        Err(_) => ApiError::new(format!("Request failed with status {status}")),
    }
}

/// Authenticate with email/password and return a freshly minted JWT.
pub async fn login(email: &str, password: &str) -> Result<String, ApiError> {
    let resp = Request::post("/v1/login")
        .json(&LoginRequest { email, password })
        .map_err(|e| ApiError::new(e.to_string()))?
        .send()
        .await
        .map_err(|e| ApiError::new(e.to_string()))?;

    if !resp.ok() {
        return Err(error_from_response(resp).await);
    }

    resp.json::<LoginResponse>()
        .await
        .map(|body| body.token)
        .map_err(|e| ApiError::new(e.to_string()))
}

/// Send a chat message (with optional image attachments) and return the
/// assistant's reply text.
///
/// When `image_urls` is empty, `content` is serialized as a plain string for
/// backwards compatibility. Otherwise it's serialized as a multimodal array
/// of `text` / `image_url` parts (each `image_url` is either an `http(s)://`
/// URL or a `data:image/...;base64,...` URL).
pub async fn send_chat_message(
    token: &str,
    session_id: &str,
    message: &str,
    image_urls: &[String],
) -> Result<String, ApiError> {
    let content = if image_urls.is_empty() {
        ChatMessageContent::Text(message)
    } else {
        let mut parts = Vec::with_capacity(image_urls.len() + 1);
        if !message.trim().is_empty() {
            parts.push(ContentPart::Text { text: message });
        }
        for url in image_urls {
            parts.push(ContentPart::ImageUrl {
                image_url: ImageUrlRef { url },
            });
        }
        ChatMessageContent::Parts(parts)
    };

    let request = ChatCompletionRequest {
        messages: vec![ChatMessage {
            role: "user",
            content,
        }],
        user: Some(session_id),
    };

    let resp = Request::post("/v1/chat/completions")
        .header("Authorization", &format!("Bearer {token}"))
        .json(&request)
        .map_err(|e| ApiError::new(e.to_string()))?
        .send()
        .await
        .map_err(|e| ApiError::new(e.to_string()))?;

    if !resp.ok() {
        return Err(error_from_response(resp).await);
    }

    let body = resp
        .json::<ChatCompletionResponse>()
        .await
        .map_err(|e| ApiError::new(e.to_string()))?;

    body.choices
        .into_iter()
        .next()
        .map(|choice| choice.message.content)
        .ok_or_else(|| ApiError::new("The server returned no response choices."))
}

/// Start a new conversation on the server by issuing the `new` chat command.
pub async fn start_new_session(token: &str, session_id: &str) -> Result<String, ApiError> {
    let request = ChatCommandRequest {
        command: "new",
        session_id: Some(session_id),
    };

    let resp = Request::post("/v1/chat/commands")
        .header("Authorization", &format!("Bearer {token}"))
        .json(&request)
        .map_err(|e| ApiError::new(e.to_string()))?
        .send()
        .await
        .map_err(|e| ApiError::new(e.to_string()))?;

    if !resp.ok() {
        return Err(error_from_response(resp).await);
    }

    resp.json::<ChatCommandResponse>()
        .await
        .map(|body| body.response)
        .map_err(|e| ApiError::new(e.to_string()))
}

/// Fetch the example prompts configured for the current agent, shown as
/// clickable suggestions in an empty chat session.
pub async fn fetch_example_prompts(token: &str) -> Result<Vec<String>, ApiError> {
    let resp = Request::get("/v1/example-prompts")
        .header("Authorization", &format!("Bearer {token}"))
        .send()
        .await
        .map_err(|e| ApiError::new(e.to_string()))?;

    if !resp.ok() {
        return Err(error_from_response(resp).await);
    }

    resp.json::<ExamplePromptsResponse>()
        .await
        .map(|body| body.prompts)
        .map_err(|e| ApiError::new(e.to_string()))
}
