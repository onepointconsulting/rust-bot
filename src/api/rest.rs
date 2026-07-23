use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    Json, Router,
    extract::State,
    http::{Request, StatusCode, header::AUTHORIZATION},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use chrono::Utc;
use serde::Deserialize;
use tokio::net::TcpListener;
use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};
use utoipa::{Modify, OpenApi};
use utoipa_swagger_ui::SwaggerUi;
use uuid::Uuid;

use crate::config::schema::JwtConfig;
use crate::security::jwt::{validate_jwt_token, JwtValidationOpts};
use crate::{
    agent::agent_loop::AgentLoop,
    api::types::{ChatCommandRequest, ChatCommandResponse},
    bus::events::OutboundMessage,
    command::types::ChatCommand,
};

use super::types::{
    AssistantMessage, ChatCompletionChoice, ChatCompletionRequest, ChatCompletionResponse,
    ChatMessage, SessionSummary, SessionsListResponse, Usage, extract_last_user_message,
};

pub struct ApiServer {
    pub agent_loop: Arc<AgentLoop>,
    pub host: String,
    pub port: u16,
    pub session_id: String,
    pub model_name: String,
    pub timeout: u64,
    pub jwt: JwtConfig,
}

#[derive(Clone)]
struct AppState {
    agent_loop: Arc<AgentLoop>,
    session_id: String,
    model_name: String,
    timeout: Duration,
    /// When `Some`, JWT auth is required on protected routes.
    jwt_auth: Option<JwtAuthState>,
}

#[derive(Clone)]
struct JwtAuthState {
    public_key_pem: Arc<Vec<u8>>,
    opts: JwtValidationOpts,
}

impl From<ApiServer> for AppState {
    fn from(server: ApiServer) -> Self {
        let jwt_auth = if server.jwt.enabled {
            let public_key_pem = std::fs::read(&server.jwt.public_key_path).unwrap_or_else(|e| {
                panic!(
                    "JWT enabled but failed to read public key '{}': {e}",
                    server.jwt.public_key_path
                );
            });
            if server.jwt.aud.trim().is_empty() {
                panic!("JWT enabled but api.jwt.aud is empty");
            }
            Some(JwtAuthState {
                public_key_pem: Arc::new(public_key_pem),
                opts: JwtValidationOpts {
                    iss: server.jwt.iss.clone(),
                    aud: server.jwt.aud.clone(),
                },
            })
        } else {
            None
        };

        Self {
            agent_loop: server.agent_loop,
            session_id: server.session_id,
            model_name: server.model_name,
            timeout: Duration::from_secs(server.timeout),
            jwt_auth,
        }
    }
}

struct SecurityAddon;

impl Modify for SecurityAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        if let Some(components) = openapi.components.as_mut() {
            components.add_security_scheme(
                "bearerAuth",
                SecurityScheme::Http(
                    HttpBuilder::new()
                        .scheme(HttpAuthScheme::Bearer)
                        .bearer_format("JWT")
                        .build(),
                ),
            );
        }
    }
}

#[derive(OpenApi)]
#[openapi(
    paths(health, chat_completions, chat_commands, list_sessions),
    components(schemas(
        ChatCompletionRequest,
        ChatCompletionResponse,
        ChatCompletionChoice,
        AssistantMessage,
        ChatMessage,
        Usage,
        ChatCommandRequest,
        ChatCommandResponse,
        ChatCommand,
        SessionSummary,
        SessionsListResponse,
    )),
    modifiers(&SecurityAddon),
    tags((name = "chat", description = "OpenAI-compatible chat completions API"))
)]
struct ApiDoc;

#[derive(Debug, Deserialize)]
struct SessionsQuery;

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

    fn unauthorized(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            message: message.into(),
            error_type: Some("unauthorized".to_string()),
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

async fn jwt_auth_middleware(
    State(state): State<Arc<AppState>>,
    request: Request<axum::body::Body>,
    next: Next,
) -> Result<Response, ApiError> {
    let Some(jwt_auth) = state.jwt_auth.as_ref() else {
        return Ok(next.run(request).await);
    };

    let auth_header = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| ApiError::unauthorized("Missing Authorization header"))?;

    let token = auth_header
        .strip_prefix("Bearer ")
        .or_else(|| auth_header.strip_prefix("bearer "))
        .ok_or_else(|| {
            ApiError::unauthorized("Authorization header must use Bearer scheme")
        })?
        .trim();

    if token.is_empty() {
        return Err(ApiError::unauthorized("Bearer token is empty"));
    }

    validate_jwt_token(token, jwt_auth.public_key_pem.as_slice(), &jwt_auth.opts).map_err(
        |err| ApiError::unauthorized(format!("Invalid JWT: {err}")),
    )?;

    Ok(next.run(request).await)
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
        (status = 401, description = "Unauthorized"),
        (status = 408, description = "Request timed out"),
        (status = 500, description = "Internal server error"),
    ),
    security(("bearerAuth" = [])),
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

    let outbound = await_agent_outbound(process, timeout).await?;
    Ok(Json(build_chat_completion_response(outbound, model)))
}

/// Await an agent response with a timeout, translating bus/timeout failures into `ApiError`.
async fn await_agent_outbound(
    process: impl Future<Output = Option<OutboundMessage>>,
    timeout: Duration,
) -> Result<OutboundMessage, ApiError> {
    tokio::time::timeout(timeout, process)
        .await
        .map_err(|_| {
            ApiError::request_timeout(format!(
                "Request timed out after {} seconds.",
                timeout.as_secs()
            ))
        })?
        .ok_or_else(|| ApiError::internal("Agent did not produce a response."))
}

fn build_chat_completion_response(outbound: OutboundMessage, model: String) -> ChatCompletionResponse {
    ChatCompletionResponse {
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
    }
}

#[utoipa::path(
    post,
    path = "/v1/chat/commands",
    request_body = ChatCommandRequest,
    responses(
        (status = 200, description = "Command response", body = ChatCommandResponse),
        (status = 400, description = "Invalid request"),
        (status = 401, description = "Unauthorized"),
        (status = 408, description = "Request timed out"),
        (status = 500, description = "Internal server error"),
    ),
    security(("bearerAuth" = [])),
    tag = "chat"
)]
async fn chat_commands(
    State(state): State<Arc<AppState>>,
    Json(request): Json<ChatCommandRequest>,
) -> Result<Json<ChatCommandResponse>, ApiError> {
    let command = request.command;
    let command_text = command.to_string();
    let session_id = request
        .session_id
        .unwrap_or_else(|| state.session_id.clone());

    let agent_loop = Arc::clone(&state.agent_loop);
    let timeout = state.timeout;
    let process = async move {
        agent_loop
            .process_direct(
                command_text.as_str(),
                Some(session_id.as_str()),
                Some("api"),
                Some("default"),
                None,
                None,
                None,
                None,
            )
            .await
    };

    let outbound = await_agent_outbound(process, timeout).await?;
    Ok(Json(ChatCommandResponse {
        command,
        response: outbound.content,
    }))
}

#[utoipa::path(
    get,
    path = "/v1/sessions",
    responses(
        (status = 200, description = "List of persisted sessions", body = SessionsListResponse),
        (status = 401, description = "Unauthorized"),
    ),
    security(("bearerAuth" = [])),
    tag = "chat"
)]
async fn list_sessions(
    State(state): State<Arc<AppState>>,
) -> Json<SessionsListResponse> {
    let session_manager = state
        .agent_loop
        .session_manager
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let entries = session_manager.list_sessions();
    Json(SessionsListResponse::from_session_entries(
        &entries
    ))
}

async fn shutdown_signal() {
    if tokio::signal::ctrl_c().await.is_ok() {
        log::info!("Shutdown signal received, stopping API server...");
    }
}

pub async fn create_api_server(server: ApiServer) -> std::io::Result<()> {
    let addr = format!("{}:{}", server.host, server.port);
    let agent_loop = Arc::clone(&server.agent_loop);
    let jwt_enabled = server.jwt.enabled;

    agent_loop.connect_mcp().await;

    let state = Arc::new(AppState::from(server));

    let protected = Router::new()
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/chat/commands", post(chat_commands))
        .route("/v1/sessions", get(list_sessions))
        .layer(middleware::from_fn_with_state(
            Arc::clone(&state),
            jwt_auth_middleware,
        ));

    let app = Router::new()
        .route("/health", get(health))
        .merge(SwaggerUi::new("/swagger-ui").url("/api-docs/openapi.json", ApiDoc::openapi()))
        .merge(protected)
        .with_state(state);

    let listener = TcpListener::bind(&addr).await?;
    log::info!("API server listening on http://{addr}");
    log::info!("Swagger UI available at http://{addr}/swagger-ui");
    if jwt_enabled {
        log::info!("JWT authentication enabled for /v1/* routes");
    }

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    agent_loop.close_mcp().await;
    log::info!("MCP connections closed.");

    Ok(())
}
