use std::sync::Arc;
use std::time::Duration;

use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use utoipa::OpenApi;
use utoipa::ToSchema;
use utoipa_swagger_ui::SwaggerUi;
use uuid::Uuid;

use crate::agent::agent_loop::AgentLoop;

pub struct ApiServer {
    pub agent_loop: Arc<AgentLoop>,
    pub host: String,
    pub port: u16,
    pub session_id: String,
    pub model_name: String,
    pub timeout: u64,
}

#[derive(Clone)]
struct AppState {
    agent_loop: Arc<AgentLoop>,
    session_id: String,
    model_name: String,
    timeout: Duration,
}

impl From<ApiServer> for AppState {
    fn from(server: ApiServer) -> Self {
        Self {
            agent_loop: server.agent_loop,
            session_id: server.session_id,
            model_name: server.model_name,
            timeout: Duration::from_secs(server.timeout),
        }
    }
}

#[derive(Debug, Deserialize, ToSchema)]
struct ChatCompletionRequest {
    messages: Vec<ChatMessage>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    stream: Option<bool>,
    #[serde(default)]
    user: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
struct ChatMessage {
    role: String,
    #[schema(value_type = String)]
    content: ChatMessageContent,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ChatMessageContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

#[derive(Debug, Deserialize)]
struct ContentPart {
    #[serde(rename = "type")]
    part_type: String,
    #[serde(default)]
    text: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
struct ChatCompletionResponse {
    id: String,
    object: String,
    created: i64,
    model: String,
    choices: Vec<ChatCompletionChoice>,
    usage: Usage,
}

#[derive(Debug, Serialize, ToSchema)]
struct ChatCompletionChoice {
    index: u32,
    message: AssistantMessage,
    finish_reason: String,
}

#[derive(Debug, Serialize, ToSchema)]
struct AssistantMessage {
    role: String,
    content: String,
}

#[derive(Debug, Serialize, ToSchema)]
struct Usage {
    prompt_tokens: u32,
    completion_tokens: u32,
    total_tokens: u32,
}

#[derive(OpenApi)]
#[openapi(
    paths(health, chat_completions),
    components(schemas(
        ChatCompletionRequest,
        ChatCompletionResponse,
        ChatCompletionChoice,
        AssistantMessage,
        ChatMessage,
        Usage,
    )),
    tags((name = "chat", description = "OpenAI-compatible chat completions API"))
)]
struct ApiDoc;

struct ApiError {
    status: StatusCode,
    message: String,
    error_type: Option<String>,
}

impl ApiError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
            error_type: None,
        }
    }

    fn request_timeout(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::REQUEST_TIMEOUT,
            message: message.into(),
            error_type: Some("timeout".to_string()),
        }
    }

    fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: message.into(),
            error_type: Some("server_error".to_string()),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(error_json(
                self.status.as_u16(),
                &self.message,
                self.error_type,
            )),
        )
            .into_response()
    }
}

fn error_json(status: u16, message: &str, err_type: Option<String>) -> serde_json::Value {
    let err_type = err_type.unwrap_or_else(|| "invalid_request_error".to_string());
    serde_json::json!({"error": {"message": message, "type": err_type, "code": status}})
}

fn content_as_string(content: &ChatMessageContent) -> Option<String> {
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

fn extract_last_user_message(messages: &[ChatMessage]) -> Option<String> {
    messages
        .iter()
        .rev()
        .filter(|message| message.role == "user")
        .find_map(|message| content_as_string(&message.content))
        .filter(|content| !content.trim().is_empty())
}

#[utoipa::path(
    get,
    path = "/health",
    responses((status = 200, description = "Server is healthy"))
)]
async fn health() -> StatusCode {
    StatusCode::OK
}

#[utoipa::path(
    post,
    path = "/v1/chat/completions",
    request_body = ChatCompletionRequest,
    responses(
        (status = 200, description = "Chat completion response", body = ChatCompletionResponse),
        (status = 400, description = "Invalid request"),
        (status = 408, description = "Request timed out"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "chat"
)]
async fn chat_completions(
    State(state): State<Arc<AppState>>,
    Json(request): Json<ChatCompletionRequest>,
) -> Result<Json<ChatCompletionResponse>, ApiError> {
    if request.stream.unwrap_or(false) {
        return Err(ApiError::bad_request(
            "Streaming is not supported. Set stream to false or omit it.",
        ));
    }

    let content = extract_last_user_message(&request.messages).ok_or_else(|| {
        ApiError::bad_request("Request must include at least one non-empty user message.")
    })?;

    let session_id = request
        .user
        .as_deref()
        .unwrap_or(state.session_id.as_str());
    let chat_id = request.user.as_deref().unwrap_or("default");
    let model = request
        .model
        .as_deref()
        .unwrap_or(state.model_name.as_str())
        .to_string();

    let agent_loop = Arc::clone(&state.agent_loop);
    let timeout = state.timeout;
    let process = async move {
        agent_loop
            .process_direct(
                content.as_str(),
                Some(session_id),
                Some("api"),
                Some(chat_id),
                None,
                None,
                None,
                None,
            )
            .await
    };

    let outbound = tokio::time::timeout(timeout, process)
        .await
        .map_err(|_| {
            ApiError::request_timeout(format!(
                "Request timed out after {} seconds.",
                timeout.as_secs()
            ))
        })?
        .ok_or_else(|| ApiError::internal("Agent did not produce a response."))?;

    Ok(Json(ChatCompletionResponse {
        id: format!("chatcmpl-{}", Uuid::new_v4()),
        object: "chat.completion".to_string(),
        created: Utc::now().timestamp(),
        model,
        choices: vec![ChatCompletionChoice {
            index: 0,
            message: AssistantMessage {
                role: "assistant".to_string(),
                content: outbound.content,
            },
            finish_reason: "stop".to_string(),
        }],
        usage: Usage {
            prompt_tokens: 0,
            completion_tokens: 0,
            total_tokens: 0,
        },
    }))
}

async fn shutdown_signal() {
    if tokio::signal::ctrl_c().await.is_ok() {
        log::info!("Shutdown signal received, stopping API server...");
    }
}

pub async fn create_api_server(server: ApiServer) -> std::io::Result<()> {
    let addr = format!("{}:{}", server.host, server.port);
    let agent_loop = Arc::clone(&server.agent_loop);

    agent_loop.connect_mcp().await;

    let state = Arc::new(AppState::from(server));
    let app = Router::new()
        .route("/health", get(health))
        .route("/v1/chat/completions", post(chat_completions))
        .merge(SwaggerUi::new("/swagger-ui").url("/api-docs/openapi.json", ApiDoc::openapi()))
        .with_state(state);

    let listener = TcpListener::bind(&addr).await?;
    log::info!("API server listening on http://{addr}");
    log::info!("Swagger UI available at http://{addr}/swagger-ui");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    agent_loop.close_mcp().await;
    log::info!("MCP connections closed.");

    Ok(())
}
