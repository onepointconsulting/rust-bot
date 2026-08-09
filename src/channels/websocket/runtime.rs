use std::net::SocketAddr;
use std::sync::{Arc, LazyLock, Mutex as StdMutex, atomic::Ordering};

use async_trait::async_trait;
use axum::{
    Router,
    extract::{
        ConnectInfo, Query, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use futures::{SinkExt, StreamExt};
use garde::{Path, Report, Validate};
use regex::Regex;
use serde::{Deserialize, Deserializer, Serialize};
use tokio::{
    net::TcpListener,
    sync::{Mutex as AsyncMutex, Notify, mpsc},
};
use uuid::Uuid;

use crate::channels::base::handle_message;
use crate::channels::gateway_services::GatewayServices;
use crate::channels::types::{Envelope, EnvelopeType};
use crate::channels::websocket::registry::ConnectionRegistry;
use crate::{
    bus::{
        events::OutboundMessage,
        outbound_events::{
            OutboundEvent::RuntimeModelUpdated, RuntimeModelUpdatedEvent,
            outbound_message_for_event,
        },
        queue::MessageBus,
    },
    channels::base::{BaseChannel, BaseChannelCommon},
    config::paths::get_media_dir,
    config::schema::{ChannelsConfig, JwtConfig},
    security::attachment_ingress::store_inbound_attachments,
    security::jwt::{JwtValidationOpts, validate_jwt_token},
    security::workspace_requests::WorkspaceRequestHandler,
    session::manager::SessionManager,
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
    /// `"native"` (the app shell, never facing an untrusted network) or `"browser"` (served
    /// over the network, so workspace-scope escalation additionally requires a loopback
    /// client). Mirrors nanobot's `webui_runtime_surface` (`cli/gateway_runtime.py:211`).
    pub runtime_surface: String,
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
            runtime_surface: "browser".to_string(),
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
pub fn publish_runtime_model_update(bus: Arc<MessageBus>, model: &str, model_preset: Option<&str>) {
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
fn parse_envelope(raw: &str) -> Option<Envelope> {
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
    if let Some(t) = envelope.get("type")
        && t.is_string()
    {
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

/// Accept UUIDs and short scoped keys like "unified:default". Keeps the
/// capability namespace small enough to rule out path traversal / quote
/// injection tricks. Mirrors nanobot's `_CHAT_ID_RE`.
static CHAT_ID_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_:-]{1,64}$").unwrap());

/// Mirrors nanobot's `_is_valid_chat_id`. The connect path always generates
/// its own chat_id (a UUID, always valid by construction) — this exists for
/// the future envelope-dispatch layer, which will need to validate
/// client-supplied chat_ids (e.g. `attach`/`message` envelopes).
fn is_valid_chat_id(value: &str) -> bool {
    CHAT_ID_RE.is_match(value)
}

/// Shared handle to the many-to-many chat_id/connection registry.
type ConnectionRegistryHandle = Arc<AsyncMutex<ConnectionRegistry>>;

/// State handed to axum's per-connection handlers.
///
/// Kept separate from [`WebSocketChannel`] (rather than reaching for `Arc<Self>`)
/// because axum's `State<S>` extractor requires an owned, `'static` `S: Clone`,
/// while [`BaseChannel::start`] only hands us `&self`. Every field here is
/// itself cheap to clone (an `Arc`, or plain config data), so cloning `WsShared`
/// once per connection is fine.
#[derive(Clone)]
struct WsShared {
    name: &'static str,
    bus: Arc<MessageBus>,
    channels_config: ChannelsConfig,
    jwt: JwtConfig,
    jwt_public_key_pem: Option<Arc<Vec<u8>>>,
    connections: ConnectionRegistryHandle,
    supports_streaming: bool,
    gateway_services: Arc<GatewayServices>,
    _session_manager: Arc<StdMutex<SessionManager>>,
    _workspace_request_handler: WorkspaceRequestHandler,
    runtime_surface: String,
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

#[derive(Clone, Copy)]
struct EnvelopeDispatchContext<'a> {
    envelope: &'a Envelope,
    connection_id: &'a str,
    client_id: &'a str,
    shared: &'a WsShared,
    remote_addr: SocketAddr,
}

impl<'a> EnvelopeDispatchContext<'a> {
    /// See [`workspace_controls_available`].
    ///
    /// Not yet called: workspace-scope escalation over the websocket envelope
    /// dispatcher isn't wired up yet, but the gating logic is implemented and
    /// unit-tested ahead of that integration.
    #[allow(dead_code)]
    fn workspace_controls_available(&self) -> bool {
        workspace_controls_available(self.shared, self.remote_addr)
    }
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
    channels_config
        .allow_from
        .iter()
        .any(|s| s == "*" || s == sender_id)
}

/// Reject the upgrade with 401 when JWT auth is enabled and the token is
/// missing/invalid. No-op (always `Ok`) when JWT is disabled.
fn authorize(shared: &WsShared, token: Option<&str>) -> Result<(), StatusCode> {
    let Some(public_key_pem) = shared.jwt_public_key_pem.as_ref() else {
        return Ok(());
    };
    let token = token
        .filter(|t| !t.trim().is_empty())
        .ok_or(StatusCode::UNAUTHORIZED)?;
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

/// Mirrors nanobot's `_workspace_controls_available` / `ws_http.workspace_controls_available`
/// (`webui/ws_http.py:229`): workspace-scope escalation is allowed for the native app shell
/// (never facing an untrusted network) or for a loopback client on the browser-served surface.
#[allow(dead_code)]
fn workspace_controls_available(shared: &WsShared, remote_addr: SocketAddr) -> bool {
    shared.runtime_surface == "native" || remote_addr.ip().is_loopback()
}

/// Send a control event (`error`, `attached`, ...) to one connection, or
/// silently drop it if the connection is already gone. Mirrors nanobot's
/// `_send_event` (`channels/websocket/runtime.py:377-392`).
///
/// `base_fields` (e.g. `chat_id` / `turn_id` rejection context) and `fields`
/// are both merged into the payload alongside `"event"`, mirroring
/// `_send_event(..., detail=..., **rejection_fields)`.
async fn send_event(
    shared: &WsShared,
    connection_id: &str,
    event: &str,
    base_fields: Option<&serde_json::Map<String, serde_json::Value>>,
    fields: serde_json::Value,
) {
    let sender = shared.connections.lock().await.sender_for(connection_id);
    let Some(sender) = sender else { return };

    let mut payload = serde_json::Map::new();
    payload.insert("event".to_string(), serde_json::Value::String(event.to_string()));
    if let Some(base) = base_fields {
        payload.extend(base.clone());
    }
    if let serde_json::Value::Object(map) = fields {
        payload.extend(map);
    }

    if sender
        .send(Message::text(serde_json::Value::Object(payload).to_string()))
        .is_err()
    {
        log::warn!(
            "WebSocket channel: connection '{connection_id}' closed while sending '{event}' event"
        );
        shared.connections.lock().await.cleanup_connection(connection_id);
    }
}

/// Axum handler for the WebSocket upgrade route: authorize, then hand off to
/// [`handle_socket`] for the connection's lifetime.
///
/// The query-string `client_id` is purely a sender identity (allow-list
/// check, `InboundMessage.sender_id`) — it is never used as a chat_id.
/// Mirrors nanobot's `_connection_loop` (`runtime.py:557-563`).
async fn ws_upgrade_handler(
    State(shared): State<WsShared>,
    Query(query): Query<WsUpgradeQuery>,
    ConnectInfo(remote_addr): ConnectInfo<SocketAddr>,
    ws: WebSocketUpgrade,
) -> Response {
    if let Err(status) = authorize(&shared, query.token.as_deref()) {
        return status.into_response();
    }

    let client_id = match query
        .client_id
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    {
        Some(raw) if raw.chars().count() > 128 => {
            log::warn!(
                "WebSocket channel: client_id too long ({} chars), truncating",
                raw.chars().count()
            );
            raw.chars().take(128).collect()
        }
        Some(raw) => raw,
        None => format!("anon-{}", &Uuid::new_v4().simple().to_string()[..12]),
    };

    ws.on_upgrade(move |socket| handle_socket(socket, shared, client_id, remote_addr))
}

/// Drive one connection for its lifetime: mint a fresh `chat_id` for it,
/// announce it via a `ready` frame, register an outbound sender, forward
/// inbound text frames to the bus, and clean up the registry entry on close.
///
/// The chat_id is always server-generated here — never taken from the
/// client — mirroring nanobot's `default_chat_id` (`runtime.py:565`).
/// Envelope dispatch (which would let a connection attach to additional,
/// possibly client-named, chat_ids) is not implemented yet; every plain-text
/// frame on this connection routes to this one default chat_id.
///
/// Ping/pong keep-alive is handled by axum/tokio-tungstenite automatically
/// (server auto-replies to client pings); `ping_interval_s`/`ping_timeout_s`
/// (server-initiated liveness probing) are not wired yet.
async fn handle_socket(
    socket: WebSocket,
    shared: WsShared,
    client_id: String,
    remote_addr: SocketAddr,
) {
    let connection_id = Uuid::new_v4().to_string();
    let chat_id = Uuid::new_v4().to_string();
    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();

    // Send `ready` before registering, so a reply can never race ahead of
    // the client learning its own chat_id (mirrors nanobot's ordering
    // comment at runtime.py:578).
    let ready = serde_json::json!({
        "event": "ready",
        "chat_id": chat_id,
        "client_id": client_id,
    });
    if sink.send(Message::text(ready.to_string())).await.is_err() {
        return;
    }
    shared
        .connections
        .lock()
        .await
        .register(&connection_id, &chat_id, tx);

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
                log::warn!("WebSocket channel: connection '{connection_id}' error: {e}");
                break;
            }
        };
        match msg {
            Message::Text(text) => {
                let raw = text.as_str();
                if let Some(envelope) = parse_envelope(raw) {
                    let envelope_dispatch_context = EnvelopeDispatchContext {
                        envelope: &envelope,
                        connection_id: &connection_id,
                        client_id: &client_id,
                        shared: &shared,
                        remote_addr,
                    };
                    dispatch_envelope(envelope_dispatch_context).await;
                    continue;
                };
                let Some(content) = parse_inbound_payload(raw) else {
                    continue;
                };
                handle_message(
                    &client_id,
                    &chat_id,
                    &content,
                    None,
                    None,
                    None,
                    sender_allowed(&shared.channels_config, &client_id),
                    shared.supports_streaming,
                    shared.name,
                    &shared.bus,
                )
                .await;
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    shared
        .connections
        .lock()
        .await
        .cleanup_connection(&connection_id);
    writer.abort();
}

async fn dispatch_envelope<'a>(
    envelope_dispatch_context: EnvelopeDispatchContext<'a>,
) {
    let type_str = envelope_dispatch_context.envelope
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    match EnvelopeType::from(type_str) {
        EnvelopeType::NewChat => { /* ... */ }
        EnvelopeType::ForkChat => { /* ... */ }
        EnvelopeType::Attach => { /* ... */ }
        EnvelopeType::SetWorkspaceScope => { /* ... */ }
        EnvelopeType::TranscribeAudio => { /* ... */ }
        EnvelopeType::Message => {
            handle_envelope_message(envelope_dispatch_context).await;
        }
        EnvelopeType::Unrecognized(_t) => {
            // reply with nanobot's `f"unknown type: {t!r}"` equivalent
        }
    }
}

async fn handle_envelope_message<'a>(
    envelope_dispatch_context: EnvelopeDispatchContext<'a>,
) {
    let envelope = envelope_dispatch_context.envelope;
    let connection_id = envelope_dispatch_context.connection_id;
    let client_id = envelope_dispatch_context.client_id;
    let shared = envelope_dispatch_context.shared;

    let cid = envelope.get("chat_id").and_then(|v| v.as_str()).unwrap_or_default();
    if !is_valid_chat_id(cid) {
        send_event(
            shared,
            connection_id,
            "error",
            None,
            serde_json::json!({"detail": "invalid chat_id"}),
        )
        .await;
        return;
    }

    let raw_turn_id = envelope.get("turn_id").and_then(|v| v.as_str());
    let turn_id = raw_turn_id.filter(|t| !t.is_empty());

    let mut rejection_fields = serde_json::Map::new();
    rejection_fields.insert("chat_id".to_string(), serde_json::Value::String(cid.to_string()));
    if let Some(turn_id) = turn_id {
        rejection_fields.insert("turn_id".to_string(), serde_json::Value::String(turn_id.to_string()));
    }

    // The allowlist can change while an authenticated websocket stays open.
    // Reject the exact application turn before hydration, transcript
    // persistence, or an acceptance ACK — mirrors runtime.py:701-712.
    if !sender_allowed(&shared.channels_config, client_id) {
        send_event(
            shared,
            connection_id,
            "error",
            Some(&rejection_fields),
            serde_json::json!({"detail": "access_denied"}),
        )
        .await;
        return;
    }

    let Some(content) = envelope.get("content").and_then(|v| v.as_str()) else {
        send_event(
            shared,
            connection_id,
            "error",
            Some(&rejection_fields),
            serde_json::json!({"detail": "missing content"}),
        )
        .await;
        return;
    };

    if let Some(message_rejection) = shared.gateway_services.ingress.validate_text(content) {
        send_event(
            shared,
            connection_id,
            "error",
            Some(&rejection_fields),
            serde_json::json!({
                "detail": "message_rejected",
                "reason": message_rejection,
            }),
        )
        .await;
        return;
    }

    let mut media_paths: Vec<String> = Vec::new();
    if let Some(raw_media) = envelope.get("media") {
        let Some(media_array) = raw_media.as_array() else {
            send_event(
                shared,
                connection_id,
                "error",
                Some(&rejection_fields),
                serde_json::json!({"detail": "attachment_rejected", "reason": "malformed"}),
            )
            .await;
            return;
        };
        let media_dir = get_media_dir(Some("websocket"));
        match store_inbound_attachments(media_array, &media_dir, shared.gateway_services.ingress.attachments) {
            Ok(paths) => media_paths = paths,
            Err(reason) => {
                send_event(
                    shared,
                    connection_id,
                    "error",
                    Some(&rejection_fields),
                    serde_json::json!({"detail": "attachment_rejected", "reason": reason.as_str()}),
                )
                .await;
                return;
            }
        }
    }

    // Allow media-only turns (content may be empty when attachments are present).
    if content.trim().is_empty() && media_paths.is_empty() {
        send_event(
            shared,
            connection_id,
            "error",
            Some(&rejection_fields),
            serde_json::json!({"detail": "missing content"}),
        )
        .await;
        return;
    }

    // Auto-attach on first use so clients can one-shot without a separate
    // `attach` envelope — mirrors runtime.py:765.
    shared.connections.lock().await.attach(connection_id, cid);

    // Still missing before this can actually admit a turn (mirrors
    // runtime.py:766-849) — hydrate-after-subscribe (replaying recent
    // history to this connection; no transcript/history-replay module
    // exists yet), WorkspaceRequestHandler::scope_for_message (+
    // error-to-send_event wrapping), transcript persistence,
    // cli_apps/mcp_presets/quoted-context normalization, turn registration,
    // the handle_message(...) call itself, and the final `message_accepted`
    // ack.
    let _ = media_paths;
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
    connections: ConnectionRegistryHandle,
    jwt_public_key_pem: Option<Arc<Vec<u8>>>,
    /// Built once at channel construction and shared (via `Arc::clone`) into
    /// every [`WsShared`] snapshot — see [`Self::shared`].
    gateway_services: Arc<GatewayServices>,
    /// `Arc`-wrapped so [`Self::start`] can move an owned clone into the
    /// `'static` shutdown future `axum::serve` requires, rather than
    /// borrowing `&self` (which cannot outlive this method).
    shutdown: Arc<Notify>,
}

impl WebSocketChannel {
    pub fn new(
        config: WebSocketConfig,
        bus: Arc<MessageBus>,
        channels_config: ChannelsConfig,
        session_manager: Arc<StdMutex<SessionManager>>,
        workspace_request_handler: WorkspaceRequestHandler,
    ) -> Self {
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
            base: BaseChannelCommon::new(bus, session_manager, workspace_request_handler),
            channels_config,
            config,
            connections: Arc::new(AsyncMutex::new(ConnectionRegistry::default())),
            jwt_public_key_pem,
            gateway_services: Arc::new(GatewayServices::default()),
            shutdown: Arc::new(Notify::new()),
        }
    }

    fn shared(&self) -> WsShared {
        WsShared {
            name: self.name(),
            bus: Arc::clone(&self.base.bus),
            channels_config: self.channels_config.clone(),
            jwt: self.config.jwt.clone(),
            jwt_public_key_pem: self.jwt_public_key_pem.clone(),
            connections: Arc::clone(&self.connections),
            supports_streaming: BaseChannel::supports_streaming(self),
            _session_manager: Arc::clone(&self.base.session_manager),
            _workspace_request_handler: self.base.workspace_request_handler.clone(),
            runtime_surface: self.config.runtime_surface.clone(),
            gateway_services: Arc::clone(&self.gateway_services),
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
        log::info!(
            "WebSocket channel listening on ws://{addr}{}",
            self.config.path
        );

        let app = self.router();
        let shutdown_signal = Arc::clone(&self.shutdown);
        let shutdown = async move { shutdown_signal.notified().await };
        if let Err(e) = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(shutdown)
        .await
        {
            log::error!("WebSocket channel: server error: {e}");
        }

        // Mirrors nanobot's `stop()` clearing `_subs`/`_conn_chats`/etc. once
        // the server task is confirmed stopped, so a later `start()` (e.g.
        // after a restart) never inherits stale registry entries.
        self.connections.lock().await.clear();
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
        let text = payload.to_string();

        // Fan out to every connection subscribed to this chat_id, mirroring
        // nanobot's `conns = list(self._subs.get(chat_id, ()))` + per-connection
        // `_safe_send_to` loop.
        let recipients = self.connections.lock().await.senders_for_chat(&msg.chat_id);
        if recipients.is_empty() {
            return Err(format!(
                "No open WebSocket connection for chat_id '{}'",
                msg.chat_id
            ));
        }

        let mut delivered = 0usize;
        for (connection_id, tx) in recipients {
            if tx.send(Message::text(text.clone())).is_ok() {
                delivered += 1;
            } else {
                log::warn!("WebSocket channel: connection '{connection_id}' gone, cleaning up");
                self.connections
                    .lock()
                    .await
                    .cleanup_connection(&connection_id);
            }
        }

        // Only fail when nobody received it — a partial failure must not
        // trigger a retry that would re-deliver duplicate content to
        // recipients that already succeeded.
        if delivered == 0 {
            return Err(format!(
                "All WebSocket connections for chat_id '{}' were closed",
                msg.chat_id
            ));
        }
        Ok(())
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
        assert!(err.contains("must match path"), "unexpected error: {err}");
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

    // --- is_valid_chat_id ---

    #[test]
    fn is_valid_chat_id_accepts_uuid_and_scoped_key() {
        assert!(is_valid_chat_id("550e8400-e29b-41d4-a716-446655440000"));
        assert!(is_valid_chat_id("unified:default"));
    }

    #[test]
    fn is_valid_chat_id_rejects_empty() {
        assert!(!is_valid_chat_id(""));
    }

    #[test]
    fn is_valid_chat_id_rejects_too_long() {
        let too_long = "a".repeat(65);
        assert!(!is_valid_chat_id(&too_long));
        let max_len = "a".repeat(64);
        assert!(is_valid_chat_id(&max_len));
    }

    #[test]
    fn is_valid_chat_id_rejects_disallowed_characters() {
        assert!(!is_valid_chat_id("has space"));
        assert!(!is_valid_chat_id("has\"quote"));
        assert!(!is_valid_chat_id("has;semicolon"));
    }

    // --- workspace_controls_available ---

    fn test_shared(runtime_surface: &str) -> WsShared {
        let dir = tempfile::tempdir().unwrap();
        WsShared {
            name: "websocket",
            bus: Arc::new(MessageBus::new()),
            channels_config: ChannelsConfig::default(),
            jwt: JwtConfig::default(),
            jwt_public_key_pem: None,
            connections: Arc::new(AsyncMutex::new(ConnectionRegistry::default())),
            supports_streaming: false,
            _session_manager: Arc::new(StdMutex::new(SessionManager::new(dir.keep()))),
            _workspace_request_handler: WorkspaceRequestHandler::new(
                tempfile::tempdir().unwrap().keep(),
                true,
            ),
            runtime_surface: runtime_surface.to_string(),
            gateway_services: Arc::new(GatewayServices::default()),
        }
    }

    fn addr(ip: &str) -> SocketAddr {
        SocketAddr::new(ip.parse().unwrap(), 12345)
    }

    #[test]
    fn workspace_controls_available_true_for_native_surface_regardless_of_address() {
        let shared = test_shared("native");
        assert!(workspace_controls_available(&shared, addr("203.0.113.5")));
        assert!(workspace_controls_available(&shared, addr("127.0.0.1")));
    }

    #[test]
    fn workspace_controls_available_true_for_loopback_on_browser_surface() {
        let shared = test_shared("browser");
        assert!(workspace_controls_available(&shared, addr("127.0.0.1")));
        assert!(workspace_controls_available(&shared, addr("::1")));
    }

    #[test]
    fn workspace_controls_available_false_for_non_loopback_on_browser_surface() {
        let shared = test_shared("browser");
        assert!(!workspace_controls_available(&shared, addr("203.0.113.5")));
    }
}
