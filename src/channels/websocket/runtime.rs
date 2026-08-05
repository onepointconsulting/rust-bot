use std::collections::HashMap;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use async_trait::async_trait;
use axum::{
    Router,
    extract::{
        Query, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use chrono::Utc;
use futures::{SinkExt, StreamExt};
use garde::{Path, Report, Validate};
use serde::{Deserialize, Deserializer, Serialize};
use tokio::{
    net::TcpListener,
    sync::{Mutex as AsyncMutex, Notify, mpsc},
};
use uuid::Uuid;

use crate::{
    bus::{
        events::{InboundMessage, OutboundMessage},
        outbound_events::{
            OutboundEvent::RuntimeModelUpdated, RuntimeModelUpdatedEvent,
            outbound_message_for_event,
        },
        queue::MessageBus,
    },
    channels::base::{BaseChannel, BaseChannelCommon},
    config::schema::{ChannelsConfig, JwtConfig},
    security::jwt::{JwtValidationOpts, validate_jwt_token},
};

/// Strip a trailing `/`, keeping root `"/"` unchanged.
fn strip_trailing_slash(path: &str) -> String {
    if path.len() > 1 && path.ends_with('/') {
        path.trim_end_matches('/').to_string()
    } else if path.is_empty() {
        "/".to_string()
    } else {
        path.to_string()
    }
}

/// Normalize a WebSocket config path for consistent routing.
fn normalize_config_path(path: &str) -> String {
    strip_trailing_slash(path)
}

/// Serde equivalent of a Pydantic `@field_validator("path")`:
/// require a leading `/`, then normalize trailing slashes.
fn deserialize_path<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    if !value.starts_with('/') {
        return Err(serde::de::Error::custom(r#"path must start with "/""#));
    }
    Ok(normalize_config_path(&value))
}

/// When JWT is enabled, `jwt.aud` must equal the normalized WebSocket `path`.
/// Empty `aud` is left to [`JwtConfig`]'s own validator.
fn validate_jwt_aud_matches_path(cfg: &WebSocketConfig) -> garde::Result {
    if !cfg.jwt.enabled || cfg.jwt.aud.trim().is_empty() {
        return Ok(());
    }
    // `path` is already normalized by `deserialize_path`.
    if normalize_config_path(&cfg.jwt.aud) != cfg.path {
        return Err(garde::Error::new(format!(
            "jwt.aud ({}) must match path ({})",
            cfg.jwt.aud, cfg.path
        )));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct WebSocketConfig {
    pub enabled: bool,
    pub host: String,
    pub port: u16,
    #[serde(deserialize_with = "deserialize_path")]
    pub path: String,
    pub jwt: JwtConfig,
    pub allow_from: Vec<String>,
    pub streaming: bool,
    pub max_message_bytes: usize,
    pub ping_interval_s: u64,
    pub ping_timeout_s: u64,
    pub ssl_certfile: String,
    pub ssl_keyfile: String,
}

impl Default for WebSocketConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            host: "127.0.0.1".to_string(),
            port: 8765,
            path: "/".to_string(),
            jwt: JwtConfig::default(),
            allow_from: vec![],
            streaming: false,
            max_message_bytes: 1024 * 1024 * 32,
            ping_interval_s: 30,
            ping_timeout_s: 30,
            ssl_certfile: "".to_string(),
            ssl_keyfile: "".to_string(),
        }
    }
}

impl Validate for WebSocketConfig {
    type Context = ();

    fn validate_into(
        &self,
        ctx: &Self::Context,
        parent: &mut dyn FnMut() -> Path,
        report: &mut Report,
    ) {
        self.jwt
            .validate_into(ctx, &mut || parent().join("jwt"), report);

        if let Err(err) = validate_jwt_aud_matches_path(self) {
            report.append(parent().join("jwt").join("aud"), err);
        }
    }
}

/// Enqueue a runtime model snapshot for websocket subscribers (fan-out in-channel).
pub fn publish_runtime_model_update(
    bus: Arc<MessageBus>,
    model: &str,
    model_preset: Option<&str>,
) {
    let res = bus.outbound.put_nowait(outbound_message_for_event(
        "websocket",
        "*",
        RuntimeModelUpdated(RuntimeModelUpdatedEvent {
            model: Some(model.to_string()),
            model_preset: model_preset.map(|p| p.to_string()),
        }),
        None,
        None,
    ));
    if let Err(e) = res {
        log::error!("Error publishing runtime model update: {e}");
    }
}

/// Return a typed envelope dict if the frame is a new-style JSON envelope, else None.
/// 
/// A frame qualifies when it parses as a JSON object with a string ``type`` field.
/// Legacy frames (plain text, or ``{"content": ...}`` without ``type``) return None;
/// callers should fall back to :func:`_parse_inbound_payload` for those.
fn parse_envelope(raw: &str) -> Option<HashMap<String, serde_json::Value>> {
    let text = raw.trim();
    if !text.starts_with('{') {
        return None;
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return None;
    };
    let serde_json::Value::Object(envelope) = value else {
        return None;
    };
    if let Some(t) = envelope.get("type") && t.is_string() {
        return Some(envelope.into_iter().collect());
    }
    None
}

/// Parse a client frame into text; return `None` for empty or unrecognized content.
///
/// Accepts either plain text or a JSON object with a `content`/`text`/`message`
/// string field (in that priority order). A frame that merely *looks* like JSON
/// (starts with `{`) but fails to parse is treated as literal text, matching
/// nanobot's `_parse_inbound_payload`.
fn parse_inbound_payload(raw: &str) -> Option<String> {
    let text = raw.trim();
    if text.is_empty() {
        return None;
    }
    if !text.starts_with('{') {
        return Some(text.to_string());
    }

    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return Some(text.to_string());
    };
    let serde_json::Value::Object(map) = value else {
        return None;
    };
    for key in ["content", "text", "message"] {
        if let Some(serde_json::Value::String(s)) = map.get(key)
            && !s.trim().is_empty()
        {
            return Some(s.clone());
        }
    }
    None
}

/// Registry of open connections, keyed by `chat_id`, so [`WebSocketChannel::send`]
/// can route an outbound message to the right socket. Shared (via `Arc`) between
/// the channel itself and every per-connection task spawned by axum.
type ConnectionRegistry = Arc<AsyncMutex<HashMap<String, mpsc::UnboundedSender<Message>>>>;

/// State handed to axum's per-connection handlers.
///
/// Kept separate from [`WebSocketChannel`] (rather than reaching for `Arc<Self>`)
/// because axum's `State<S>` extractor requires an owned, `'static` `S: Clone`,
/// while [`BaseChannel::start`] only hands us `&self`. Every field here is
/// itself cheap to clone (an `Arc`, or plain config data), so cloning `WsShared`
/// once per connection is fine.
#[derive(Clone)]
struct WsShared {
    bus: Arc<MessageBus>,
    channels_config: ChannelsConfig,
    jwt: JwtConfig,
    jwt_public_key_pem: Option<Arc<Vec<u8>>>,
    connections: ConnectionRegistry,
}

/// Query params accepted on the WebSocket upgrade request, mirroring nanobot's
/// `ws://{host}:{port}{path}?client_id=...&token=...`.
#[derive(Debug, Deserialize)]
struct WsUpgradeQuery {
    #[serde(default)]
    client_id: Option<String>,
    #[serde(default)]
    token: Option<String>,
}

/// Check *sender_id* against the shared allow list.
///
/// Duplicates [`BaseChannel::is_allowed`]'s logic (empty list denies all,
/// `"*"` allows all) rather than calling through the trait, since the axum
/// handler only has a [`WsShared`] clone, not `&dyn BaseChannel`.
fn sender_allowed(channels_config: &ChannelsConfig, sender_id: &str) -> bool {
    if channels_config.allow_from.is_empty() {
        log::warn!("No allow list configured for channel websocket");
        return false;
    }
    channels_config.allow_from.iter().any(|s| s == "*" || s == sender_id)
}

/// Reject the upgrade with 401 when JWT auth is enabled and the token is
/// missing/invalid. No-op (always `Ok`) when JWT is disabled.
fn authorize(shared: &WsShared, token: Option<&str>) -> Result<(), StatusCode> {
    let Some(public_key_pem) = shared.jwt_public_key_pem.as_ref() else {
        return Ok(());
    };
    let token = token.filter(|t| !t.trim().is_empty()).ok_or(StatusCode::UNAUTHORIZED)?;
    let opts = JwtValidationOpts {
        iss: shared.jwt.iss.clone(),
        aud: shared.jwt.aud.clone(),
    };
    validate_jwt_token(token, public_key_pem.as_slice(), &opts)
        .map(|_claims| ())
        .map_err(|e| {
            log::warn!("WebSocket channel: rejected connection with invalid JWT: {e}");
            StatusCode::UNAUTHORIZED
        })
}

/// Axum handler for the WebSocket upgrade route: authorize, then hand off to
/// [`handle_socket`] for the connection's lifetime.
async fn ws_upgrade_handler(
    State(shared): State<WsShared>,
    Query(query): Query<WsUpgradeQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    if let Err(status) = authorize(&shared, query.token.as_deref()) {
        return status.into_response();
    }

    let chat_id = query.client_id.unwrap_or_else(|| {
        let generated = Uuid::new_v4().to_string();
        log::info!("WebSocket channel: no client_id supplied, generated '{generated}'");
        generated
    });

    ws.on_upgrade(move |socket| handle_socket(socket, shared, chat_id))
}

/// Drive one connection for its lifetime: register an outbound sender, forward
/// inbound text frames to the bus, and clean up the registry entry on close.
///
/// Ping/pong keep-alive is handled by axum/tokio-tungstenite automatically
/// (server auto-replies to client pings); `ping_interval_s`/`ping_timeout_s`
/// (server-initiated liveness probing) are not wired yet.
async fn handle_socket(socket: WebSocket, shared: WsShared, chat_id: String) {
    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
    shared.connections.lock().await.insert(chat_id.clone(), tx);

    let writer = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if sink.send(msg).await.is_err() {
                break;
            }
        }
    });

    while let Some(frame) = stream.next().await {
        let msg = match frame {
            Ok(msg) => msg,
            Err(e) => {
                log::warn!("WebSocket channel: connection '{chat_id}' error: {e}");
                break;
            }
        };
        match msg {
            Message::Text(text) => {
                let Some(content) = parse_inbound_payload(text.as_str()) else {
                    continue;
                };
                if !sender_allowed(&shared.channels_config, &chat_id) {
                    log::warn!(
                        "Sender {chat_id} is not allowed to send messages to channel websocket"
                    );
                    continue;
                }
                let inbound = InboundMessage {
                    channel: "websocket".to_string(),
                    sender_id: chat_id.clone(),
                    chat_id: chat_id.clone(),
                    content,
                    timestamp: Utc::now(),
                    media: Vec::new(),
                    metadata: HashMap::new(),
                    session_key_override: None,
                };
                if let Err(e) = shared.bus.publish_inbound(inbound) {
                    log::error!("WebSocket channel: failed to publish inbound message: {e}");
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    shared.connections.lock().await.remove(&chat_id);
    writer.abort();
}

/// WebSocket server channel: rust-bot acts as a WebSocket server, serving
/// connected clients over `axum`'s `ws` feature (a thin wrapper around
/// `tokio-tungstenite`).
///
/// Not yet wired: `unix_socket_path`-style local-socket serving, TLS via
/// `ssl_certfile`/`ssl_keyfile` (would need `axum-server`'s TLS acceptor),
/// streaming deltas (`send_delta`), and the typed JSON envelope protocol
/// nanobot layers on top of [`parse_inbound_payload`] for non-chat control
/// messages.
pub struct WebSocketChannel {
    base: BaseChannelCommon,
    channels_config: ChannelsConfig,
    config: WebSocketConfig,
    connections: ConnectionRegistry,
    jwt_public_key_pem: Option<Arc<Vec<u8>>>,
    /// `Arc`-wrapped so [`Self::start`] can move an owned clone into the
    /// `'static` shutdown future `axum::serve` requires, rather than
    /// borrowing `&self` (which cannot outlive this method).
    shutdown: Arc<Notify>,
}

impl WebSocketChannel {
    pub fn new(config: WebSocketConfig, bus: Arc<MessageBus>, channels_config: ChannelsConfig) -> Self {
        let jwt_public_key_pem = if config.jwt.enabled {
            if config.jwt.public_key_path.trim().is_empty() {
                panic!("WebSocket channel: jwt.enabled is true but jwt.public_key_path is empty");
            }
            let pem = std::fs::read(&config.jwt.public_key_path).unwrap_or_else(|e| {
                panic!(
                    "WebSocket channel: failed to read JWT public key '{}': {e}",
                    config.jwt.public_key_path
                )
            });
            Some(Arc::new(pem))
        } else {
            None
        };

        Self {
            base: BaseChannelCommon {
                bus,
                running: AtomicBool::new(false),
                transcription_api_key: String::new(),
            },
            channels_config,
            config,
            connections: Arc::new(AsyncMutex::new(HashMap::new())),
            jwt_public_key_pem,
            shutdown: Arc::new(Notify::new()),
        }
    }

    fn shared(&self) -> WsShared {
        WsShared {
            bus: Arc::clone(&self.base.bus),
            channels_config: self.channels_config.clone(),
            jwt: self.config.jwt.clone(),
            jwt_public_key_pem: self.jwt_public_key_pem.clone(),
            connections: Arc::clone(&self.connections),
        }
    }

    fn router(&self) -> Router {
        Router::new()
            .route(&self.config.path, get(ws_upgrade_handler))
            .with_state(self.shared())
    }
}

#[async_trait]
impl BaseChannel for WebSocketChannel {
    fn name(&self) -> &'static str {
        "websocket"
    }

    fn display_name(&self) -> &'static str {
        "WebSocket"
    }

    fn running(&self) -> bool {
        self.base.running.load(Ordering::Relaxed)
    }

    fn bus(&self) -> &MessageBus {
        self.base.bus.as_ref()
    }

    fn config(&self) -> &ChannelsConfig {
        &self.channels_config
    }

    fn transcription_api_key(&self) -> &str {
        &self.base.transcription_api_key
    }

    fn set_transcription_api_key(&mut self, key: String) {
        self.base.transcription_api_key = key;
    }

    async fn start(&self) {
        if !self.config.enabled {
            return;
        }

        let addr = format!("{}:{}", self.config.host, self.config.port);
        let listener = match TcpListener::bind(&addr).await {
            Ok(listener) => listener,
            Err(e) => {
                log::error!("WebSocket channel: failed to bind {addr}: {e}");
                return;
            }
        };

        self.base.running.store(true, Ordering::Relaxed);
        log::info!("WebSocket channel listening on ws://{addr}{}", self.config.path);

        let app = self.router();
        let shutdown_signal = Arc::clone(&self.shutdown);
        let shutdown = async move { shutdown_signal.notified().await };
        if let Err(e) = axum::serve(listener, app)
            .with_graceful_shutdown(shutdown)
            .await
        {
            log::error!("WebSocket channel: server error: {e}");
        }

        self.base.running.store(false, Ordering::Relaxed);
    }

    async fn stop(&self) {
        self.shutdown.notify_waiters();
    }

    async fn send(&self, msg: OutboundMessage) -> Result<(), String> {
        // Placeholder wire format: the real WebUI multiplex protocol
        // (`outbound_event_from_message` in nanobot) still needs porting.
        let payload = serde_json::json!({
            "chatId": msg.chat_id,
            "content": msg.content,
            "metadata": msg.metadata,
        });

        let connections = self.connections.lock().await;
        let tx = connections
            .get(&msg.chat_id)
            .ok_or_else(|| format!("No open WebSocket connection for chat_id '{}'", msg.chat_id))?;
        tx.send(Message::text(payload.to_string()))
            .map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_config_path_strips_trailing_slash() {
        assert_eq!(normalize_config_path("/ws/"), "/ws");
        assert_eq!(normalize_config_path("/"), "/");
    }

    #[test]
    fn path_must_start_with_slash() {
        let err = serde_json::from_str::<WebSocketConfig>(r#"{"path":"bad"}"#)
            .expect_err("path without leading slash should fail");
        assert!(
            err.to_string().contains(r#"path must start with "/""#),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn path_is_normalized_on_deserialize() {
        let cfg: WebSocketConfig =
            serde_json::from_str(r#"{"path":"/ws/"}"#).expect("valid path should deserialize");
        assert_eq!(cfg.path, "/ws");
    }

    #[test]
    fn jwt_aud_must_match_path_when_enabled() {
        let mut cfg = WebSocketConfig {
            path: "/ws".to_string(),
            ..WebSocketConfig::default()
        };
        cfg.jwt.enabled = true;
        cfg.jwt.aud = "/other".to_string();

        let report = cfg.validate();
        assert!(report.is_err(), "mismatched aud should fail validation");
        let err = report.unwrap_err().to_string();
        assert!(
            err.contains("must match path"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn jwt_aud_matching_path_ok_when_enabled() {
        let mut cfg = WebSocketConfig {
            path: "/ws".to_string(),
            ..WebSocketConfig::default()
        };
        cfg.jwt.enabled = true;
        cfg.jwt.aud = "/ws/".to_string(); // trailing slash normalized in compare

        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn jwt_enabled_requires_non_empty_aud() {
        let mut cfg = WebSocketConfig {
            path: "/ws".to_string(),
            ..WebSocketConfig::default()
        };
        cfg.jwt.enabled = true;
        cfg.jwt.aud = String::new();

        let report = cfg.validate();
        assert!(report.is_err(), "empty aud with jwt.enabled should fail");
        let err = report.unwrap_err().to_string();
        assert!(
            err.contains("aud must be non-empty when JWT is enabled"),
            "unexpected error: {err}"
        );
    }

    // --- parse_inbound_payload ---

    #[test]
    fn parse_inbound_payload_empty_or_whitespace_is_none() {
        assert_eq!(parse_inbound_payload(""), None);
        assert_eq!(parse_inbound_payload("   "), None);
    }

    #[test]
    fn parse_inbound_payload_plain_text_passes_through() {
        assert_eq!(
            parse_inbound_payload("hello, world"),
            Some("hello, world".to_string())
        );
    }

    #[test]
    fn parse_inbound_payload_extracts_content_field() {
        assert_eq!(
            parse_inbound_payload(r#"{"content": "hi"}"#),
            Some("hi".to_string())
        );
    }

    #[test]
    fn parse_inbound_payload_falls_back_through_text_then_message() {
        assert_eq!(
            parse_inbound_payload(r#"{"text": "hi"}"#),
            Some("hi".to_string())
        );
        assert_eq!(
            parse_inbound_payload(r#"{"message": "hi"}"#),
            Some("hi".to_string())
        );
    }

    #[test]
    fn parse_inbound_payload_prefers_content_over_text_and_message() {
        assert_eq!(
            parse_inbound_payload(r#"{"content": "c", "text": "t", "message": "m"}"#),
            Some("c".to_string())
        );
    }

    #[test]
    fn parse_inbound_payload_object_without_recognized_key_is_none() {
        assert_eq!(parse_inbound_payload(r#"{"other": "hi"}"#), None);
    }

    #[test]
    fn parse_inbound_payload_malformed_json_falls_back_to_literal_text() {
        assert_eq!(
            parse_inbound_payload("{not valid json"),
            Some("{not valid json".to_string())
        );
    }

    #[test]
    fn parse_inbound_payload_blank_recognized_field_is_none() {
        assert_eq!(parse_inbound_payload(r#"{"content": "   "}"#), None);
    }

    // --- sender_allowed ---

    #[test]
    fn sender_allowed_empty_list_denies_all() {
        let cfg = ChannelsConfig {
            allow_from: vec![],
            ..ChannelsConfig::default()
        };
        assert!(!sender_allowed(&cfg, "anyone"));
    }

    #[test]
    fn sender_allowed_wildcard_allows_all() {
        let cfg = ChannelsConfig {
            allow_from: vec!["*".to_string()],
            ..ChannelsConfig::default()
        };
        assert!(sender_allowed(&cfg, "anyone"));
    }

    #[test]
    fn sender_allowed_matches_specific_id_only() {
        let cfg = ChannelsConfig {
            allow_from: vec!["client-1".to_string()],
            ..ChannelsConfig::default()
        };
        assert!(sender_allowed(&cfg, "client-1"));
        assert!(!sender_allowed(&cfg, "client-2"));
    }
}
