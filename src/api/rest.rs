use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State},
    http::{
        HeaderValue, Method, Request, StatusCode,
        header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE},
    },
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use chrono::Utc;
use tokio::net::TcpListener;
use tower_http::cors::{Any, CorsLayer};
use tower_http::services::{ServeDir, ServeFile};
use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};
use utoipa::{Modify, OpenApi};
use utoipa_swagger_ui::SwaggerUi;
use uuid::Uuid;

use crate::api::types::{ChatLoginRequest, ChatLoginResponse};
use crate::api::user_registry::{verify_password, User, UserRegistry};
use crate::config::schema::{CorsConfig, JwtConfig};
use crate::security::jwt::{
    generate_jwt_token, validate_jwt_token, JwtValidationOpts, DEFAULT_EXPIRES_IN_MONTHS,
};
use crate::{
    agent::agent_loop::AgentLoop,
    api::types::{ChatCommandRequest, ChatCommandResponse},
    bus::events::OutboundMessage,
    command::types::ChatCommand,
};

use super::media::{materialize_image_urls, MAX_IMAGE_BYTES};
use super::types::{
    AssistantMessage, ChatCompletionChoice, ChatCompletionRequest, ChatCompletionResponse,
    ChatMessage, SessionSummary, SessionsListResponse, Usage, extract_last_user_turn,
};

/// Axum's default request body limit is 2 MiB, far too small for a chat
/// request carrying base64-encoded images. Size the limit generously above
/// `MAX_IMAGE_BYTES` (per-image cap, checked again after decoding in
/// `media::materialize_image_urls`) to comfortably fit several
/// base64-encoded attachments (~1.34x their raw size) plus JSON overhead.
const MAX_CHAT_REQUEST_BODY_BYTES: usize = MAX_IMAGE_BYTES * 4 + 1024 * 1024;

pub struct ApiServer {
    pub agent_loop: Arc<AgentLoop>,
    pub host: String,
    pub port: u16,
    pub session_id: String,
    pub model_name: String,
    pub timeout: u64,
    pub jwt: JwtConfig,
    pub cors: CorsConfig,
    /// Directory of pre-built web-chat static assets (`index.html`, JS,
    /// WASM) to serve alongside the API. `None` disables web UI serving.
    pub web_root: Option<PathBuf>,
    pub user_registry: Arc<Mutex<dyn UserRegistry + Send>>,
}

#[derive(Clone)]
struct AppState {
    agent_loop: Arc<AgentLoop>,
    session_id: String,
    model_name: String,
    timeout: Duration,
    /// When `Some`, JWT auth is required on protected routes.
    jwt_auth: Option<JwtAuthState>,
    pub user_registry: Arc<Mutex<dyn UserRegistry + Send>>,
}

#[derive(Clone)]
struct JwtAuthState {
    public_key_pem: Arc<Vec<u8>>,
    private_key_path: String,
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
            if server.jwt.private_key_path.trim().is_empty() {
                panic!("JWT enabled but api.jwt.private_key_path is empty");
            }
            Some(JwtAuthState {
                public_key_pem: Arc::new(public_key_pem),
                private_key_path: server.jwt.private_key_path.clone(),
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
            user_registry: server.user_registry,
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
    paths(health, chat_completions, chat_commands, list_sessions, login),
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
        ChatLoginRequest,
        ChatLoginResponse,
    )),
    modifiers(&SecurityAddon),
    tags(
        (name = "chat", description = "OpenAI-compatible chat completions API"),
        (name = "security", description = "Authentication and token issuance"),
    )
)]
struct ApiDoc;

#[derive(Debug)]
pub(crate) struct ApiError {
    status: StatusCode,
    message: String,
    error_type: Option<String>,
}

impl ApiError {
    pub(crate) fn bad_request(message: impl Into<String>) -> Self {
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

    fn payload_too_large(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            message: message.into(),
            error_type: Some("payload_too_large".to_string()),
        }
    }

    #[cfg(test)]
    pub(crate) fn message(&self) -> &str {
        &self.message
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

/// When a request body exceeds [`MAX_CHAT_REQUEST_BODY_BYTES`], axum's
/// `Json` extractor rejects it with a plain-text `413` before our handler
/// ever runs. The web-chat client can't parse that as JSON and falls back to
/// a bare "Request failed with status 413" message. Rewrite it into the same
/// `{"error": {...}}` shape as every other API error, with actionable text.
async fn friendly_body_limit_middleware(request: Request<axum::body::Body>, next: Next) -> Response {
    let response = next.run(request).await;
    if response.status() == StatusCode::PAYLOAD_TOO_LARGE {
        let limit_mb = MAX_CHAT_REQUEST_BODY_BYTES / (1024 * 1024);
        return ApiError::payload_too_large(format!(
            "Request is too large (limit {limit_mb} MB). Try a smaller image, fewer attachments, or a lower-resolution picture."
        ))
        .into_response();
    }
    response
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

    let turn = extract_last_user_turn(&request.messages).ok_or_else(|| {
        ApiError::bad_request("Request must include at least one non-empty user message.")
    })?;

    let media_paths = materialize_image_urls(&turn.image_urls).await?;

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
    let content = turn.text.clone();
    let media_for_agent = media_paths.clone();
    let process = async move {
        agent_loop
            .process_direct(
                content.as_str(),
                Some(session_id),
                Some("api"),
                Some(chat_id),
                Some(media_for_agent),
                None,
                None,
                None,
            )
            .await
    };

    let result = await_agent_outbound(process, timeout).await;

    for media_path in &media_paths {
        if let Err(e) = std::fs::remove_file(media_path) {
            log::debug!("Failed to delete temporary API media file {media_path}: {e}");
        }
    }

    let outbound = result?;
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

/// Authenticate with email/password, mint a fresh JWT, persist it in the
/// user registry, and return it.
#[utoipa::path(
    post,
    path = "/v1/login",
    request_body = ChatLoginRequest,
    responses(
        (status = 200, description = "Freshly minted JWT for the user", body = ChatLoginResponse),
        (status = 401, description = "Unauthorized"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "security"
)]
async fn login(
    State(state): State<Arc<AppState>>,
    Json(request): Json<ChatLoginRequest>,
) -> Result<Json<ChatLoginResponse>, ApiError> {
    let unauthorized = || ApiError::unauthorized("Invalid email or password");
    let jwt = state
        .jwt_auth
        .as_ref()
        .ok_or_else(|| ApiError::internal("JWT is not enabled; cannot mint login tokens"))?;

    // Copy credentials out so Argon2 does not hold the registry lock.
    let password_hash = {
        let registry = state
            .user_registry
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let user = registry
            .get_user_by_email(&request.email)
            .map_err(|_| unauthorized())?;
        user.password_hash.ok_or_else(unauthorized)?
    };

    let password = request.password.clone();
    let password_hash_for_verify = password_hash.clone();
    let valid = tokio::task::spawn_blocking(move || {
        verify_password(&password, &password_hash_for_verify).unwrap_or(false)
    })
    .await
    .map_err(|_| ApiError::internal("Password verification task failed"))?;

    if !valid {
        return Err(unauthorized());
    }

    let private_key_path = jwt.private_key_path.clone();
    let iss = jwt.opts.iss.clone();
    let aud = jwt.opts.aud.clone();
    let minted = tokio::task::spawn_blocking(move || {
        generate_jwt_token(private_key_path, iss, aud, DEFAULT_EXPIRES_IN_MONTHS)
    })
    .await
    .map_err(|_| ApiError::internal("Token minting task failed"))?
    .map_err(|err| ApiError::internal(format!("Failed to mint JWT: {err}")))?;

    {
        let mut registry = state
            .user_registry
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        registry
            .update_user(
                &request.email,
                &User {
                    email: request.email.clone(),
                    password_hash: Some(password_hash),
                    token: minted.token.clone(),
                },
            )
            .map_err(|err| ApiError::internal(format!("Failed to persist login token: {err}")))?;
    }

    Ok(Json(ChatLoginResponse {
        token: minted.token,
    }))
}

async fn shutdown_signal() {
    if tokio::signal::ctrl_c().await.is_ok() {
        log::info!("Shutdown signal received, stopping API server...");
    }
}

/// Build a CORS layer from config.
///
/// - `enabled: false` → no CORS headers (empty layer).
/// - `origins` empty or containing `"*"` → allow any origin.
/// - otherwise → allow only the listed origins.
fn build_cors_layer(cors: &CorsConfig) -> CorsLayer {
    if !cors.enabled {
        return CorsLayer::new();
    }

    let allow_any = cors.origins.is_empty()
        || cors.origins.iter().any(|origin| origin.trim() == "*");

    let layer = CorsLayer::new()
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers([AUTHORIZATION, CONTENT_TYPE, ACCEPT]);

    if allow_any {
        layer.allow_origin(Any)
    } else {
        let origins: Vec<HeaderValue> = cors
            .origins
            .iter()
            .filter_map(|origin| match origin.parse::<HeaderValue>() {
                Ok(value) => Some(value),
                Err(err) => {
                    log::warn!("Ignoring invalid CORS origin '{origin}': {err}");
                    None
                }
            })
            .collect();
        layer.allow_origin(origins)
    }
}

pub async fn create_api_server(server: ApiServer) -> std::io::Result<()> {
    let addr = format!("{}:{}", server.host, server.port);
    let agent_loop = Arc::clone(&server.agent_loop);
    let jwt_enabled = server.jwt.enabled;
    let cors = server.cors.clone();
    let web_root = server.web_root.clone();

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

    let mut app = Router::new()
        .route("/health", get(health))
        .route("/v1/login", post(login))
        .merge(SwaggerUi::new("/swagger-ui").url("/api-docs/openapi.json", ApiDoc::openapi()))
        .merge(protected)
        .layer(build_cors_layer(&cors))
        .layer(middleware::from_fn(friendly_body_limit_middleware))
        .layer(DefaultBodyLimit::max(MAX_CHAT_REQUEST_BODY_BYTES));

    let web_ui_status = match &web_root {
        Some(root) if root.is_dir() => {
            let index_html = root.join("index.html");
            app = app.fallback_service(ServeDir::new(root).not_found_service(ServeFile::new(index_html)));
            Some(format!("serving `{}`", root.display()))
        }
        Some(root) => {
            log::warn!(
                "api.webRoot / --web-root points at '{}', which is not a directory; web UI serving is disabled",
                root.display()
            );
            None
        }
        None => None,
    };

    let app = app.with_state(state);

    let listener = TcpListener::bind(&addr).await?;
    log::info!("API server listening on http://{addr}");
    log::info!("Swagger UI available at http://{addr}/swagger-ui");
    match web_ui_status {
        Some(status) => log::info!("Web UI available at http://{addr}/ ({status})"),
        None => log::info!("Web UI disabled (no valid --web-root / api.webRoot configured)"),
    }
    if cors.enabled {
        let origins = if cors.origins.is_empty()
            || cors.origins.iter().any(|o| o.trim() == "*")
        {
            "*".to_string()
        } else {
            cors.origins.join(", ")
        };
        log::info!("CORS enabled for origins: {origins}");
    } else {
        log::info!("CORS disabled");
    }
    if jwt_enabled {
        log::info!(
            "JWT authentication enabled for protected /v1/* routes (/v1/login remains public)"
        );
    }

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    agent_loop.close_mcp().await;
    log::info!("MCP connections closed.");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn accept_body(_body: String) -> StatusCode {
        StatusCode::OK
    }

    /// Axum's built-in `413` rejection (triggered when a body exceeds
    /// `DefaultBodyLimit`) is plain text and unparseable as JSON by the
    /// web-chat client, which then shows a bare "Request failed with status
    /// 413". `friendly_body_limit_middleware` should rewrite it into the
    /// same `{"error": {...}}` shape used everywhere else in the API, with
    /// actionable, non-technical wording.
    #[tokio::test]
    async fn oversized_body_gets_friendly_json_error() {
        let tiny_limit = 16usize;
        let app = Router::new()
            .route("/echo", post(accept_body))
            .layer(middleware::from_fn(friendly_body_limit_middleware))
            .layer(DefaultBodyLimit::max(tiny_limit));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test listener");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let body = vec![0u8; tiny_limit + 1024];
        let response = reqwest::Client::new()
            .post(format!("http://{addr}/echo"))
            .body(body)
            .send()
            .await
            .expect("send oversized request");

        assert_eq!(response.status().as_u16(), StatusCode::PAYLOAD_TOO_LARGE.as_u16());
        let json: serde_json::Value = response.json().await.expect("parse JSON body");
        let message = json["error"]["message"].as_str().expect("error.message present");
        assert!(message.contains("too large"), "unexpected message: {message}");
        assert!(
            !message.to_lowercase().contains("length limit"),
            "should not leak axum's internal wording: {message}"
        );
    }
}
