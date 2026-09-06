use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::str::FromStr;
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
use regex::Regex;
use tokio::sync::{Mutex as AsyncMutex, Notify, mpsc};
use uuid::Uuid;

use crate::agent::model_runtime::ModelRuntimeResolver;
use crate::agent::modes::{AgentMode, RESERVED_AGENT_MODE_NAME, SESSION_AGENT_MODE_METADATA_KEY};
use crate::agent::skills::SkillsLoader;
use crate::channels::base::handle_message;
use crate::channels::gateway_services::GatewayServices;
use crate::channels::websocket::get_session_id;
use crate::channels::websocket::registry::ConnectionRegistry;
use crate::channels::websocket::types::{
    ConnectionRegistryHandle, Envelope, EnvelopeDispatchContext, EnvelopeType, WebSocketConfig,
    WsOutboundEvent, WsShared, WsUpgradeQuery,
};
use crate::channels::websocket::webui::metadata::{
    WEBSOCKET_TURN_OWNER_METADATA_KEY, WEBUI_TURN_METADATA_KEY,
};
use crate::channels::websocket::webui::transcript::client_turn_metadata;
use crate::command::normalize_command_text;
use crate::command::types::{ChatCommand, CommandLifecycle};
use crate::runtime_context::{RUNTIME_CONTEXT_INPUT_META, webui_quote_runtime_context};
use crate::security::{WORKSPACE_SCOPE_METADATA_KEY, WorkspaceScope, WorkspaceScopeError};
use crate::session::goal_state::goal_state_ws_blob;
use crate::session::history_visibility::is_hidden_history_message;
use crate::session::keys::COMMAND_KEY;
use crate::session::{SESSION_MODEL_PRESET_METADATA_KEY, SESSION_WEBSOCKET_OWNER_CLIENT_ID_KEY};
use crate::{
    bus::{
        events::OutboundMessage,
        outbound_events::{
            FileEditEvent, OutboundEvent, OutboundEvent::RuntimeModelUpdated, ProgressKind,
            RuntimeModelUpdatedEvent, outbound_message_for_event,
        },
        queue::MessageBus,
    },
    channels::base::{BaseChannel, BaseChannelCommon},
    config::paths::get_media_dir,
    config::schema::{ChannelsConfig, RESERVED_MODEL_PRESET_NAME},
    security::attachment_ingress::store_inbound_attachments,
    security::jwt::{JwtValidationOpts, validate_jwt_token},
    security::workspace_requests::WorkspaceRequestHandler,
    session::manager::{DeleteSessionError, RenameSessionError, Session, SessionManager},
};

const MAX_HISTORY_MESSAGES: usize = 200;

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

/// Pull `chat_id` off an inbound envelope, sending an unscoped
/// `invalid chat_id` error and returning `None` when it's missing or
/// malformed. The error is deliberately unscoped (no `chat_id` field):
/// a client mid-session-switch has already cleared its local chat_id,
/// so a chat-scoped error frame would be filtered out as belonging to
/// the previous subscription.
async fn require_valid_chat_id<'a>(
    envelope_dispatch_context: &EnvelopeDispatchContext<'a>,
) -> Option<&'a str> {
    let cid = envelope_dispatch_context
        .envelope
        .get("chat_id")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    if is_valid_chat_id(cid) {
        return Some(cid);
    }
    send_event(
        envelope_dispatch_context.shared,
        envelope_dispatch_context.connection_id,
        WsOutboundEvent::Error,
        None,
        serde_json::json!({"detail": "invalid chat_id"}),
    )
    .await;
    None
}

impl<'a> EnvelopeDispatchContext<'a> {
    /// `(shared, connection_id, client_id)` — the three fields every handler
    /// needs to send events and check the sender allowlist.
    fn connection_fields(&self) -> (&'a WsShared, &'a str, &'a str) {
        (self.shared, self.connection_id, self.client_id)
    }

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

/// Custom JWT claim value marking a token as minted for the WebUI frontend
/// specifically, distinct from `aud` (which, for this channel, is already
/// pinned to the route path — see `validate_jwt_aud_matches_path`). Checked
/// by [`authorize`]; mint one via `rust-bot generate-jwt-token --purpose
/// webui` — see `security::jwt::Claims::purpose`.
pub(crate) const WEBUI_JWT_PURPOSE: &str = "webui";

/// Outbound metadata key carrying an opaque WebUI-rendered "agent UI" blob to
/// echo verbatim onto the wire message. Mirrors nanobot's
/// `OUTBOUND_META_AGENT_UI` (`bus/events.py:13`). Not populated by any
/// current agent-loop call site — forward-compatible plumbing only.
const OUTBOUND_META_AGENT_UI: &str = "_agent_ui";

/// Reject the upgrade with 401 when JWT auth is enabled, a token is
/// required (`WsShared::require_auth`), and the token is missing/invalid.
/// No-op (always `Ok`) when JWT is disabled. When JWT is enabled but
/// `require_auth` is `false` (a guest-capable instance), a missing token is
/// also allowed — but a present, invalid one is still rejected, so a client
/// that attempts auth and fails doesn't silently fall back to guest.
///
/// Returns whether the connection's JWT proves it was minted for the WebUI
/// frontend (`purpose == "webui"`) — `false` whenever there's no JWT to make
/// that claim from (JWT disabled, or a guest connecting with no token), not
/// just when validation fails. Mirrors nanobot's `_webui_connections` gate
/// (`channels/websocket/runtime.py:458-462`), which is only ever populated
/// by a token issued specifically for webui use.
fn authorize(shared: &WsShared, token: Option<&str>) -> Result<bool, StatusCode> {
    let Some(public_key_pem) = shared.jwt_public_key_pem.as_ref() else {
        return Ok(false);
    };
    let token = token.filter(|t| !t.trim().is_empty());
    let Some(token) = token else {
        return if shared.require_auth {
            Err(StatusCode::UNAUTHORIZED)
        } else {
            Ok(false)
        };
    };
    let opts = JwtValidationOpts {
        iss: shared.jwt.iss.clone(),
        aud: shared.jwt.aud.clone(),
    };
    validate_jwt_token(token, public_key_pem.as_slice(), &opts)
        .map(|claims| claims.purpose.as_deref() == Some(WEBUI_JWT_PURPOSE))
        .map_err(|e| {
            log::warn!("WebSocket channel: rejected connection with invalid JWT: {e}");
            StatusCode::UNAUTHORIZED
        })
}

/// Whether the current turn may inject the WebUI "quoted context" into the
/// model prompt: requires both the client's self-declared `is_webui` flag
/// *and* the stronger, connection-level `webui_authenticated` signal from
/// [`authorize`] — neither alone is sufficient. Mirrors nanobot's `is_webui
/// and connection in self._webui_connections` (`channels/websocket/runtime.py:824`),
/// the one place in this function where client-supplied text becomes
/// model-visible context, so the bare client-declared flag isn't trusted.
fn webui_quote_allowed(is_webui: bool, webui_authenticated: bool) -> bool {
    is_webui && webui_authenticated
}

/// Mirrors nanobot's `_workspace_controls_available` / `ws_http.workspace_controls_available`
/// (`webui/ws_http.py:229`): workspace-scope escalation is allowed for the native app shell
/// (never facing an untrusted network) or for a loopback client on the browser-served surface.
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
///
/// [`WsOutboundEvent::Error`] is also logged at error level with the full
/// JSON payload, including when the connection has already gone — those
/// frames are otherwise easy to miss (client-side `access_denied` /
/// `invalid chat_id` / save failures all funnel through here).
async fn send_event(
    shared: &WsShared,
    connection_id: &str,
    event: WsOutboundEvent,
    base_fields: Option<&serde_json::Map<String, serde_json::Value>>,
    fields: serde_json::Value,
) {
    let mut payload = serde_json::Map::new();
    payload.insert(
        "event".to_string(),
        serde_json::Value::String(event.as_str().to_string()),
    );
    if let Some(base) = base_fields {
        payload.extend(base.clone());
    }
    if let serde_json::Value::Object(map) = fields {
        payload.extend(map);
    }
    let payload = serde_json::Value::Object(payload);
    if matches!(event, WsOutboundEvent::Error) {
        log::error!("WebSocket channel: error event to '{connection_id}': {payload}");
    }

    let sender = shared.connections.lock().await.sender_for(connection_id);
    let Some(sender) = sender else { return };

    if sender.send(Message::text(payload.to_string())).is_err() {
        log::warn!(
            "WebSocket channel: connection '{connection_id}' closed while sending '{}' event",
            event.as_str()
        );
        shared
            .connections
            .lock()
            .await
            .cleanup_connection(connection_id);
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
    let webui_authenticated = match authorize(&shared, query.token.as_deref()) {
        Ok(webui_authenticated) => webui_authenticated,
        Err(status) => return status.into_response(),
    };

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

    ws.on_upgrade(move |socket| {
        handle_socket(socket, shared, client_id, remote_addr, webui_authenticated)
    })
}

fn ready_event(chat_id: &str, client_id: &str, streaming: bool) -> serde_json::Value {
    serde_json::json!({
        "event": WsOutboundEvent::Ready.as_str(),
        "chat_id": chat_id,
        "client_id": client_id,
        "streaming": streaming,
    })
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
    webui_authenticated: bool,
) {
    let connection_id = Uuid::new_v4().to_string();
    let chat_id = Uuid::new_v4().to_string();
    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();

    // Send `ready` before registering, so a reply can never race ahead of
    // the client learning its own chat_id (mirrors nanobot's ordering
    // comment at runtime.py:578). `streaming` tells the WebUI whether this
    // channel will emit `delta` frames or a single final `message`.
    let ready = ready_event(&chat_id, &client_id, shared.supports_streaming);
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
                        webui_authenticated,
                    };
                    dispatch_envelope(envelope_dispatch_context).await;
                    continue;
                };
                let Some(content) = parse_inbound_payload(raw) else {
                    continue;
                };
                if let Err(e) = handle_message(
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
                .await
                {
                    log::warn!(
                        "WebSocket channel: failed to publish message for chat '{chat_id}': {e}"
                    );
                }
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

async fn dispatch_envelope<'a>(envelope_dispatch_context: EnvelopeDispatchContext<'a>) {
    let type_str = envelope_dispatch_context
        .envelope
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    match EnvelopeType::from(type_str) {
        EnvelopeType::NewChat => {
            handle_envelope_new_chat(envelope_dispatch_context).await;
        }
        EnvelopeType::ForkChat => {
            handle_envelope_fork_chat(envelope_dispatch_context).await;
        }
        EnvelopeType::RenameChat => {
            handle_envelope_rename_chat(envelope_dispatch_context).await;
        }
        EnvelopeType::DeleteChat => {
            handle_envelope_delete_chat(envelope_dispatch_context).await;
        }
        EnvelopeType::AbortTurn => {
            handle_envelope_abort_turn(envelope_dispatch_context).await;
        }
        EnvelopeType::Attach => {
            handle_envelope_attach(envelope_dispatch_context).await;
        }
        EnvelopeType::SetWorkspaceScope => { /* ... */ }
        EnvelopeType::TranscribeAudio => { /* ... */ }
        EnvelopeType::Message => {
            handle_envelope_message(envelope_dispatch_context).await;
        }
        EnvelopeType::ListChats => {
            handle_envelope_list_chats(envelope_dispatch_context).await;
        }
        EnvelopeType::ListSkills => {
            handle_envelope_list_skills(envelope_dispatch_context).await;
        }
        EnvelopeType::SetModelPreset => {
            handle_envelope_set_model_preset(envelope_dispatch_context).await;
        }
        EnvelopeType::SetMode => {
            handle_envelope_set_mode(envelope_dispatch_context).await;
        }
        EnvelopeType::ClearSession => {
            handle_envelope_clear_session(envelope_dispatch_context).await;
        }
        EnvelopeType::Unrecognized(t) => {
            send_event(
                envelope_dispatch_context.shared,
                envelope_dispatch_context.connection_id,
                WsOutboundEvent::Error,
                None,
                serde_json::json!({"detail": format!("unknown type: {t:?}")}),
            )
            .await;
        }
    }
}

/// Handle a `set_model_preset` envelope: persist a named model-preset
/// override on an existing `websocket:{chat_id}` session and ack with
/// `model_preset_set`. `"default"` clears the override so later turns use
/// the process-wide default — same as `/model-preset default`. Existence of
/// a session file *is* required — unlike `attach`, setting a preset on a
/// chat that was never persisted would create a metadata-only ghost session.
/// Rust-side protocol addition with no nanobot precedent — see
/// [`EnvelopeType::SetModelPreset`].
async fn handle_envelope_set_model_preset<'a>(
    envelope_dispatch_context: EnvelopeDispatchContext<'a>,
) {
    let (shared, connection_id, client_id) = envelope_dispatch_context.connection_fields();

    let Some(cid) = require_valid_chat_id(&envelope_dispatch_context).await else {
        return;
    };

    let rejection_fields = create_rejection_fields(&cid);

    if !sender_allowed(&shared.channels_config, client_id) {
        send_event(
            shared,
            connection_id,
            WsOutboundEvent::Error,
            Some(&rejection_fields),
            serde_json::json!({"detail": "access_denied"}),
        )
        .await;
        return;
    }

    let model_preset = envelope_dispatch_context
        .envelope
        .get("model_preset")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let Some(model_preset) = model_preset else {
        send_event(
            shared,
            connection_id,
            WsOutboundEvent::Error,
            Some(&rejection_fields),
            serde_json::json!({"detail": "missing_model_preset"}),
        )
        .await;
        return;
    };

    // Validate *before* touching the session. `"default"` is the reserved
    // clear-override name and must not be stored — `runtime_for_session`
    // treats a missing key as "use `current_default()`", which tracks
    // process-wide `/model` changes; storing `"default"` would pin the
    // session to `config.agents.*` instead.
    let runtime = if model_preset == RESERVED_MODEL_PRESET_NAME {
        shared.runtime_resolver.current_default()
    } else {
        match shared.runtime_resolver.resolve_preset(model_preset) {
            Ok(runtime) => runtime,
            Err(_) => {
                send_event(
                    shared,
                    connection_id,
                    WsOutboundEvent::Error,
                    Some(&rejection_fields),
                    serde_json::json!({"detail": "invalid_model_preset"}),
                )
                .await;
                return;
            }
        }
    };

    let save_result = {
        // Drop the `MutexGuard` before `send_event`'s `.await` — same
        // discipline as every other `session_manager` use in this file.
        let mut session_manager = shared
            .session_manager
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(mut session) = session_manager.get_session_internal(&get_session_id(cid)) {
            if model_preset == RESERVED_MODEL_PRESET_NAME {
                session.metadata.remove(SESSION_MODEL_PRESET_METADATA_KEY);
            } else {
                session.metadata.insert(
                    SESSION_MODEL_PRESET_METADATA_KEY.to_string(),
                    serde_json::Value::String(model_preset.to_string()),
                );
            }
            session_manager.save(session).map_err(|e| {
                log::error!("Failed to save model preset for session {cid}: {e}");
                "failed_to_save_session"
            })
        } else {
            Err("session_not_found")
        }
    };
    if let Err(detail) = save_result {
        send_event(
            shared,
            connection_id,
            WsOutboundEvent::Error,
            Some(&rejection_fields),
            serde_json::json!({"detail": detail}),
        )
        .await;
        return;
    }

    send_event(
        shared,
        connection_id,
        WsOutboundEvent::ModelPresetSet,
        Some(&rejection_fields),
        serde_json::json!({
            "model_preset": runtime.preset_name,
            "model": runtime.model,
        }),
    )
    .await;
}

/// Handle a `set_mode` envelope: persist Standard/Minimal (or clear with
/// `"default"`) on an existing `websocket:{chat_id}` session and ack with
/// `mode_set`. Same persistence rules as [`handle_envelope_set_model_preset`].
async fn handle_envelope_set_mode<'a>(envelope_dispatch_context: EnvelopeDispatchContext<'a>) {
    let (shared, connection_id, client_id) = envelope_dispatch_context.connection_fields();

    let Some(cid) = require_valid_chat_id(&envelope_dispatch_context).await else {
        return;
    };

    let rejection_fields = create_rejection_fields(&cid);

    if !sender_allowed(&shared.channels_config, client_id) {
        send_event(
            shared,
            connection_id,
            WsOutboundEvent::Error,
            Some(&rejection_fields),
            serde_json::json!({"detail": "access_denied"}),
        )
        .await;
        return;
    }

    let mode_arg = envelope_dispatch_context
        .envelope
        .get("mode")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let Some(mode_arg) = mode_arg else {
        send_event(
            shared,
            connection_id,
            WsOutboundEvent::Error,
            Some(&rejection_fields),
            serde_json::json!({"detail": "missing_mode"}),
        )
        .await;
        return;
    };

    let resolved = if mode_arg.eq_ignore_ascii_case(RESERVED_AGENT_MODE_NAME) {
        shared.default_agent_mode
    } else {
        match AgentMode::parse(mode_arg) {
            Some(mode) => mode,
            None => {
                send_event(
                    shared,
                    connection_id,
                    WsOutboundEvent::Error,
                    Some(&rejection_fields),
                    serde_json::json!({"detail": "invalid_mode"}),
                )
                .await;
                return;
            }
        }
    };

    let save_result = {
        let mut session_manager = shared
            .session_manager
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(mut session) = session_manager.get_session_internal(&get_session_id(cid)) {
            if mode_arg.eq_ignore_ascii_case(RESERVED_AGENT_MODE_NAME) {
                session.metadata.remove(SESSION_AGENT_MODE_METADATA_KEY);
            } else {
                session.metadata.insert(
                    SESSION_AGENT_MODE_METADATA_KEY.to_string(),
                    serde_json::Value::String(resolved.as_str().to_string()),
                );
            }
            session_manager.save(session).map_err(|e| {
                log::error!("Failed to save agent mode for session {cid}: {e}");
                "failed_to_save_session"
            })
        } else {
            Err("session_not_found")
        }
    };
    if let Err(detail) = save_result {
        send_event(
            shared,
            connection_id,
            WsOutboundEvent::Error,
            Some(&rejection_fields),
            serde_json::json!({"detail": detail}),
        )
        .await;
        return;
    }

    send_event(
        shared,
        connection_id,
        WsOutboundEvent::ModeSet,
        Some(&rejection_fields),
        serde_json::json!({ "mode": resolved.as_str() }),
    )
    .await;
}

/// Handle an `attach` envelope: subscribe this connection to an existing
/// `chat_id` (page-reload rehydrate / session switch). Mirrors nanobot's
/// `attach` branch (`channels/websocket/runtime.py`): validate, `_attach`,
/// ack `attached`, then `_hydrate_after_subscribe`. Existence of a session
/// file is not required — subscribe is idempotent, `history` is `[]` when
/// nothing is persisted, and hydrate is a no-op in that case.
async fn handle_envelope_attach<'a>(envelope_dispatch_context: EnvelopeDispatchContext<'a>) {
    let (shared, connection_id, client_id) = envelope_dispatch_context.connection_fields();

    let Some(cid) = require_valid_chat_id(&envelope_dispatch_context).await else {
        return;
    };

    // `attached` carries a transcript snapshot, so this envelope reads chat
    // content and must clear the same allowlist bar as `message` — the
    // upgrade only proves the JWT was valid (and `authorize` returns `false`
    // rather than erroring when JWT is disabled entirely).
    //
    // The rejection stays unscoped (no `chat_id` field), like
    // `require_valid_chat_id`: a client mid-session-switch has already
    // cleared its local chat_id, so a chat-scoped error frame would be
    // filtered out as belonging to the previous subscription.
    if !sender_allowed(&shared.channels_config, client_id) {
        send_event(
            shared,
            connection_id,
            WsOutboundEvent::Error,
            None,
            serde_json::json!({"detail": "access_denied"}),
        )
        .await;
        return;
    }

    if !check_owner_allows_access(shared, connection_id, client_id, &get_session_id(cid), None)
        .await
    {
        return;
    }

    attach_chat(connection_id, cid, shared).await;
}

async fn handle_envelope_new_chat<'a>(envelope_dispatch_context: EnvelopeDispatchContext<'a>) {
    let shared = envelope_dispatch_context.shared;
    let connection_id = envelope_dispatch_context.connection_id;
    let client_id = envelope_dispatch_context.client_id;

    let new_id = Uuid::new_v4().to_string();
    let scope_for_new_chat = {
        let ws_shared = shared.clone();
        let envelope = envelope_dispatch_context.envelope.clone();
        // Capture a bool — not `EnvelopeDispatchContext` — so the `Arc<dyn Fn… + 'static>`
        // closure doesn't inherit the handler's short lifetime.
        let controls_available = envelope_dispatch_context.workspace_controls_available();
        Arc::new(move || {
            let result = {
                let mut session_manager = ws_shared
                    .session_manager
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                ws_shared.workspace_request_handler.scope_for_new_chat(
                    &mut session_manager,
                    &envelope,
                    controls_available,
                )
            };
            Box::pin(async move { result })
                as Pin<Box<dyn Future<Output = Result<WorkspaceScope, WorkspaceScopeError>> + Send>>
        })
    };
    // `None` here (not `Some(&new_id)`): mirrors nanobot's `new_chat` handler,
    // which omits `chat_id` from a rejected new-chat's scope error — the
    // chat was never attached to anything, so there is no id worth reporting.
    let scope =
        workspace_scope_or_error(shared, None, None, connection_id, scope_for_new_chat).await;
    let Some(scope) = scope else {
        return;
    };
    let new_session = {
        // Run the sync work (and drop the `MutexGuard`) *before* the `.await`s
        // below: `std::sync::MutexGuard` is `!Send` and can't live across an
        // await point inside this connection's `Send` future — same pattern
        // as `handle_envelope_message`'s own `persist_scope` call.
        let mut session_manager = shared
            .session_manager
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        shared.workspace_request_handler.persist_scope(
            &mut session_manager,
            &new_id,
            &scope,
            client_id,
        );
        // `persist_scope` just created/saved this session, so this always
        // finds it — reloaded (rather than reusing a value from the closure
        // above) since `persist_scope` only has a `SessionManager`-internal
        // snapshot to hand back.
        session_manager.get_session_internal(&get_session_id(&new_id))
    };
    shared
        .connections
        .lock()
        .await
        .attach(connection_id, &new_id);
    let mut attached_payload = serde_json::json!({"chat_id": new_id});
    merge_json(
        &mut attached_payload,
        model_preset_attached_fields(shared, new_session.as_ref()),
    );
    merge_json(
        &mut attached_payload,
        agent_mode_attached_fields(shared, new_session.as_ref()),
    );
    merge_json(
        &mut attached_payload,
        token_usage_attached_fields(new_session.as_ref()),
    );
    send_event(
        shared,
        connection_id,
        WsOutboundEvent::Attached,
        None,
        attached_payload,
    )
    .await;
    send_event(
        shared,
        connection_id,
        WsOutboundEvent::SessionUpdated,
        None,
        serde_json::json!({
            "chat_id": new_id,
            "scope": "metadata",
            "workspace_scope": scope.payload(),
        }),
    )
    .await;
    hydrate_after_subscribe(&new_id, shared).await;
}

/// Filter [`SessionManager::list_sessions`]'s output down to this channel's
/// own `websocket:`-keyed chats, stripping that prefix down to a bare
/// `chat_id` the client can pass straight back as `fork_chat`'s
/// `source_chat_id` / `attach`'s `chat_id`. Kept as a plain, signal-free
/// function (no lock, no I/O) so the filtering/reshaping logic is
/// unit-testable without a real `SessionManager`.
///
/// `list_sessions()` returns every persisted session regardless of owning
/// channel (`cli:*`, `cron:*`, ...) — those are deliberately excluded here:
/// they aren't chats this WebSocket UI could sensibly attach to or fork.
fn list_websocket_chats(sessions: Vec<serde_json::Value>) -> Vec<serde_json::Value> {
    const PREFIX: &str = "websocket:";
    sessions
        .into_iter()
        .filter_map(|mut entry| {
            let key = entry.get("key")?.as_str()?.to_string();
            let chat_id = key.strip_prefix(PREFIX)?;
            if !is_valid_chat_id(chat_id) {
                return None;
            }
            let chat_id = chat_id.to_string();
            let map = entry.as_object_mut()?;
            map.remove("key");
            map.remove("path"); // an internal filesystem detail, not for the wire
            map.insert("chat_id".to_string(), serde_json::Value::String(chat_id));
            Some(entry)
        })
        .collect()
}

/// Whether `client_id` may act on `session_key` (a `websocket:{chat_id}`
/// key) under this instance's guest session-isolation policy — see the
/// "Optional WebSocket login" plan's guest session isolation section.
///
/// Always `true` when [`WsShared::require_auth`] is on: the existing "any
/// authenticated connection may see any chat" behavior is unchanged there,
/// since [`SESSION_WEBSOCKET_OWNER_CLIENT_ID_KEY`] is only ever *consulted*
/// under guest scoping, even though `persist_scope` stamps it unconditionally.
///
/// When `require_auth` is off:
/// - a session that doesn't exist yet is allowed — there's nothing to own,
///   and the caller's own first persist claims it (`new_chat`/`message`) or
///   `attach` simply stays idempotent against nothing;
/// - an existing session is allowed only when its stamped owner matches
///   `client_id` **exactly** — including a session with *no* stamped owner
///   at all (predates this feature), which fails closed rather than
///   treating "unowned" as "shared".
fn owner_allows_access(
    shared: &WsShared,
    session_manager: &SessionManager,
    session_key: &str,
    client_id: &str,
) -> bool {
    if shared.require_auth {
        return true;
    }
    match session_manager.get_session_internal(session_key) {
        None => true,
        Some(session) => {
            session
                .metadata
                .get(SESSION_WEBSOCKET_OWNER_CLIENT_ID_KEY)
                .and_then(|v| v.as_str())
                == Some(client_id)
        }
    }
}

/// Reject with an `access_denied` `error` event and return `false` when
/// [`owner_allows_access`] denies this connection's `client_id` access to
/// `session_key`. `true` means the caller may proceed. `rejection_fields` is
/// `None` for `attach` (same unscoped-rejection reasoning as
/// `require_valid_chat_id`) and `Some(&chat_id_fields)` everywhere else,
/// matching each handler's existing `sender_allowed` rejection shape.
async fn check_owner_allows_access(
    shared: &WsShared,
    connection_id: &str,
    client_id: &str,
    session_key: &str,
    rejection_fields: Option<&serde_json::Map<String, serde_json::Value>>,
) -> bool {
    let allowed = {
        // Scoped so the (synchronous) `MutexGuard` is dropped before
        // `send_event`'s `.await` below — same discipline as every other
        // `session_manager` use in this file.
        let session_manager = shared
            .session_manager
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        owner_allows_access(shared, &session_manager, session_key, client_id)
    };
    if !allowed {
        send_event(
            shared,
            connection_id,
            WsOutboundEvent::Error,
            rejection_fields,
            serde_json::json!({"detail": "access_denied"}),
        )
        .await;
    }
    allowed
}

/// Handle a `rename_chat` envelope: persist a new display `title` on an
/// existing `websocket:{chat_id}` session and ack with `chat_renamed`.
/// Existence of a session file *is* required — unlike `attach`, renaming
/// a chat that was never persisted would create a title-only ghost session.
/// Rust-side protocol addition with no nanobot precedent — see
/// [`EnvelopeType::RenameChat`].
async fn handle_envelope_rename_chat<'a>(envelope_dispatch_context: EnvelopeDispatchContext<'a>) {
    let (shared, connection_id, client_id) = envelope_dispatch_context.connection_fields();

    let Some(cid) = require_valid_chat_id(&envelope_dispatch_context).await else {
        return;
    };

    let rejection_fields = create_rejection_fields(&cid);

    if !sender_allowed(&shared.channels_config, client_id) {
        send_event(
            shared,
            connection_id,
            WsOutboundEvent::Error,
            Some(&rejection_fields),
            serde_json::json!({"detail": "access_denied"}),
        )
        .await;
        return;
    }

    if !check_owner_allows_access(
        shared,
        connection_id,
        client_id,
        &get_session_id(cid),
        Some(&rejection_fields),
    )
    .await
    {
        return;
    }

    let title = envelope_dispatch_context
        .envelope
        .get("title")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let Some(title) = title else {
        send_event(
            shared,
            connection_id,
            WsOutboundEvent::Error,
            Some(&rejection_fields),
            serde_json::json!({"detail": "missing title"}),
        )
        .await;
        return;
    };

    let rename_result = {
        // Drop the `MutexGuard` before `send_event`'s `.await` — same
        // discipline as every other `session_manager` use in this file.
        let mut session_manager = shared
            .session_manager
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        session_manager.rename_session(&get_session_id(cid), title)
    };
    if let Err(e) = rename_result {
        let detail = match e {
            RenameSessionError::NotFound => "session_not_found",
            RenameSessionError::Save(e) => {
                log::error!("Failed to rename session {cid}: {e}");
                "rename_failed"
            }
        };
        send_event(
            shared,
            connection_id,
            WsOutboundEvent::Error,
            Some(&rejection_fields),
            serde_json::json!({"detail": detail}),
        )
        .await;
        return;
    }

    send_event(
        shared,
        connection_id,
        WsOutboundEvent::ChatRenamed,
        None,
        serde_json::json!({"chat_id": cid, "title": title}),
    )
    .await;
}

/// Handle an `abort_turn` envelope: cancel the in-flight agent turn for
/// `chat_id` — leaving the session and its history intact, unlike
/// `delete_chat` — then announce `turn_aborted` to everyone watching that
/// chat. New protocol surface with no nanobot precedent — see
/// [`EnvelopeType::AbortTurn`]'s doc comment.
///
/// Cancellation is session-scoped rather than turn-scoped: `abort_session`
/// keys off `websocket:{chat_id}` and `register_queued_turn_if_idle` admits
/// only one in-flight turn per chat, so there is never a second turn on the
/// same chat to spare. A supplied `turn_id` is therefore only a staleness
/// guard — it stops a Stop click that lost the race against turn completion
/// from reaching whatever turn ran next.
async fn handle_envelope_abort_turn<'a>(envelope_dispatch_context: EnvelopeDispatchContext<'a>) {
    let (shared, connection_id, client_id) = envelope_dispatch_context.connection_fields();

    let Some(cid) = require_valid_chat_id(&envelope_dispatch_context).await else {
        return;
    };

    let rejection_fields = create_rejection_fields(cid);

    if !sender_allowed(&shared.channels_config, client_id) {
        send_event(
            shared,
            connection_id,
            WsOutboundEvent::Error,
            Some(&rejection_fields),
            serde_json::json!({"detail": "access_denied"}),
        )
        .await;
        return;
    }

    let requested_turn_id = envelope_dispatch_context
        .envelope
        .get("turn_id")
        .and_then(|v| v.as_str())
        .filter(|t| !t.is_empty());

    // Read the live turn identity before the abort below clears it, so the
    // ack can name the turn it ended even when the client sent no `turn_id`.
    // Scoped so the `MutexGuard` is dropped before any `.await` — same
    // discipline as every other `turn_registry` use in this file.
    let active_turn_id = {
        let turn_registry = shared
            .gateway_services
            .turn_registry
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        turn_registry.websocket_turn_id(cid)
    };

    // Reject only a *positive* mismatch: the projection carries a `turn_id`
    // solely for turns that came in over a WebUI envelope that supplied one
    // (see `register_queued_turn_if_idle`'s call site), so a `None` here is
    // just as likely to mean "running, identity unknown" as "idle". Aborting
    // on `None` keeps the button working in that case, and costs nothing when
    // the chat really is idle — `abort_session` is a no-op with no tasks.
    if let Some(requested) = requested_turn_id
        && let Some(active) = active_turn_id.as_deref()
        && requested != active
    {
        send_event(
            shared,
            connection_id,
            WsOutboundEvent::Error,
            Some(&rejection_fields),
            serde_json::json!({"detail": "turn_not_active", "turn_id": requested}),
        )
        .await;
        return;
    }

    // Abort in-flight agent tasks and subagents for this chat, the same body
    // `/stop` and `delete_chat` run (see `SessionWorkCanceller`). `None` in
    // every test fixture and whenever no live `AgentLoop` was wired in — see
    // `GatewayServices::set_work_canceller`.
    if let Some(canceller) = shared.gateway_services.work_canceller() {
        canceller.abort("websocket", cid).await;
    }
    // The abort above publishes a `TurnEnd`, which clears the projection via
    // `WebSocketChannel::send` — but only when there *was* a canceller to run
    // it. Clear directly too so the chat can never be left reading as
    // "running" with nothing behind it.
    shared
        .gateway_services
        .turn_registry
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear_chat(cid);

    // Fanned out rather than sent to the requester alone: every connection
    // attached to `cid` was rendering this turn's stream, so all of them need
    // to stop waiting on it — same reasoning as `chat_deleted`.
    send_turn_aborted(
        cid,
        active_turn_id.as_deref().or(requested_turn_id),
        connection_id,
        shared,
    )
    .await;
}

/// Tell every connection subscribed to `chat_id` that its in-flight turn was
/// cancelled, plus `requester_id` itself — a client that asked to abort a chat
/// it isn't attached to still needs the ack to stop showing a Stop button.
/// Fan-out + cleanup-on-failure follows [`send_goal_status`].
async fn send_turn_aborted(
    chat_id: &str,
    turn_id: Option<&str>,
    requester_id: &str,
    ws_shared: &WsShared,
) {
    let recipients = {
        let connections = ws_shared.connections.lock().await;
        let mut recipients = connections.senders_for_chat(chat_id);
        if !recipients.iter().any(|(id, _)| id == requester_id)
            && let Some(sender) = connections.sender_for(requester_id)
        {
            recipients.push((requester_id.to_string(), sender));
        }
        recipients
    };
    let mut body = serde_json::json!({
        "event": WsOutboundEvent::TurnAborted.as_str(),
        "chat_id": chat_id,
    });
    if let Some(turn_id) = turn_id {
        body["turn_id"] = serde_json::json!(turn_id);
    }
    let raw = body.to_string();
    for (connection_id, tx) in recipients {
        if tx.send(Message::text(raw.clone())).is_err() {
            log::warn!(
                "WebSocket channel: connection '{connection_id}' gone while sending turn_aborted, cleaning up"
            );
            ws_shared
                .connections
                .lock()
                .await
                .cleanup_connection(&connection_id);
        }
    }
}

/// Fan-out `session_cleared` to every connection subscribed to `chat_id`,
/// plus the requester even if it isn't currently attached to that chat
/// (e.g. clearing a different sidebar row). Unlike `chat_deleted`, this
/// does **not** detach anyone — the session is still live.
async fn send_session_cleared(chat_id: &str, requester_id: &str, ws_shared: &WsShared) {
    let recipients = {
        let connections = ws_shared.connections.lock().await;
        let mut recipients = connections.senders_for_chat(chat_id);
        if !recipients.iter().any(|(id, _)| id == requester_id)
            && let Some(sender) = connections.sender_for(requester_id)
        {
            recipients.push((requester_id.to_string(), sender));
        }
        recipients
    };
    let raw = serde_json::json!({
        "event": WsOutboundEvent::SessionCleared.as_str(),
        "chat_id": chat_id,
    })
    .to_string();
    for (connection_id, tx) in recipients {
        if tx.send(Message::text(raw.clone())).is_err() {
            log::warn!(
                "WebSocket channel: connection '{connection_id}' gone while sending session_cleared, cleaning up"
            );
            ws_shared
                .connections
                .lock()
                .await
                .cleanup_connection(&connection_id);
        }
    }
}

/// Handle a `fork_chat` envelope: branch a new `websocket:{chat_id}` session
/// from an existing one at a zero-based user-message index, then attach and
/// hydrate the requesting connection on the new chat. Mirrors nanobot's
/// `create_webui_chat_fork` + `attach_webui_fork`
/// (`webui/forking.py`), except the `attached` ack here also carries a
/// `history` snapshot — nanobot's WebUI instead reloads a thread over HTTP,
/// which rust-bot's `websockets-chat` client doesn't have.
///
/// Title metadata, `session_updated`, and `append_fork_marker` (nanobot
/// marks the fork boundary in the new transcript once it starts accepting
/// turns) aren't ported yet — out of scope for wiring `attached.history`.
///
/// A missing `before_user_index` means "fork the whole chat" (computed as
/// the source session's total user-message count) rather than "fork
/// nothing" — the sidebar "Fork session" kebab omits this field. In-transcript
/// Fork buttons send `before_user_index` so a mid-thread assistant reply
/// can branch without copying later turns.
async fn handle_envelope_fork_chat<'a>(envelope_dispatch_context: EnvelopeDispatchContext<'a>) {
    let (shared, connection_id, client_id) = envelope_dispatch_context.connection_fields();

    let Some(cid) = require_valid_chat_id(&envelope_dispatch_context).await else {
        return;
    };

    let envelope = envelope_dispatch_context.envelope;
    let key = get_session_id(cid);

    if !check_owner_allows_access(shared, connection_id, client_id, &key, None).await {
        return;
    }
    let before_user_index = match envelope.get("before_user_index").and_then(|v| v.as_u64()) {
        Some(v) => v as usize,
        None => {
            // Sidebar "Fork session" omits the field, meaning "fork the
            // whole chat". Both `fork_session_before_user_index` and
            // `fork_transcript_before_user_index` already treat an index
            // equal to the total user-message count as "copy everything".
            let session_manager = shared
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            session_manager
                .get_session_internal(&key)
                .map(|session| {
                    session
                        .messages
                        .iter()
                        .filter(|m| m.get("role").and_then(|v| v.as_str()) == Some("user"))
                        .count()
                })
                .unwrap_or(0)
        }
    };

    let new_id = Uuid::new_v4().to_string();
    let new_session_key = get_session_id(&new_id);
    let fork_result = {
        // Drop the `MutexGuard` before any `.await` below — same discipline
        // as every other `session_manager` use in this file.
        let mut session_manager = shared
            .session_manager
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        session_manager.fork_session_before_user_index(&key, &new_session_key, before_user_index)
    };
    let Ok(forked) = fork_result else {
        send_event(
            shared,
            connection_id,
            WsOutboundEvent::Error,
            None,
            serde_json::json!({"detail": "invalid fork source or index"}),
        )
        .await;
        return;
    };
    {
        // Scoped so the (synchronous) `MutexGuard` is dropped before this
        // function's `.await`s below — same discipline as every other
        // `session_manager` use in this file. Explicit rather than relying
        // on the source's cloned metadata alone: the requesting connection
        // is always the owner of a fork it was allowed to create (see
        // `check_owner_allows_access` above), so this is a no-op whenever
        // that metadata already carried an owner, and only matters for a
        // fork of a legacy, never-owned source made while `require_auth`
        // was still `true`.
        let mut session_manager = shared
            .session_manager
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        session_manager.stamp_websocket_owner_if_absent(&new_session_key, client_id);
    }

    let transcripts = Arc::clone(&shared.gateway_services.transcripts);
    // Scoped so each `MutexGuard` drops before this function's `.await`s
    // below — same discipline as every other `gateway_services.transcripts`
    // use in this file.
    let mut history = {
        let recorder = transcripts.lock().unwrap_or_else(|e| e.into_inner());
        let transcript_ok =
            recorder.fork_transcript_before_user_index(&key, &new_session_key, before_user_index);
        if transcript_ok {
            recorder.chat_history(&new_session_key, MAX_HISTORY_MESSAGES)
        } else {
            websocket_chat_history(Some(&forked), MAX_HISTORY_MESSAGES)
        }
    };
    resolve_history_media(&mut history, &shared.media_root);

    shared
        .connections
        .lock()
        .await
        .attach(connection_id, &new_id);
    let mut attached_payload = serde_json::json!({"chat_id": new_id, "history": history});
    merge_json(
        &mut attached_payload,
        model_preset_attached_fields(shared, Some(&forked)),
    );
    merge_json(
        &mut attached_payload,
        agent_mode_attached_fields(shared, Some(&forked)),
    );
    merge_json(
        &mut attached_payload,
        token_usage_attached_fields(Some(&forked)),
    );
    send_event(
        shared,
        connection_id,
        WsOutboundEvent::Attached,
        None,
        attached_payload,
    )
    .await;
    hydrate_after_subscribe(&new_id, shared).await;
}

/// Handle a `delete_chat` envelope: permanently delete a `websocket:{chat_id}`
/// session — tombstone + unlink its JSONL, abort any in-flight agent work,
/// detach every connection subscribed to it, and fan a `chat_deleted` event
/// out to all of them (not just the requester, unlike rename — other tabs/
/// connections attached to a shared chat need to know it's gone too). New
/// protocol surface with no nanobot precedent — see
/// [`EnvelopeType::DeleteChat`]'s doc comment.
async fn handle_envelope_delete_chat<'a>(envelope_dispatch_context: EnvelopeDispatchContext<'a>) {
    let (shared, connection_id, client_id) = envelope_dispatch_context.connection_fields();

    let Some(cid) = require_valid_chat_id(&envelope_dispatch_context).await else {
        return;
    };

    let rejection_fields = create_rejection_fields(cid);

    if !sender_allowed(&shared.channels_config, client_id) {
        send_event(
            shared,
            connection_id,
            WsOutboundEvent::Error,
            Some(&rejection_fields),
            serde_json::json!({"detail": "access_denied"}),
        )
        .await;
        return;
    }

    if !check_owner_allows_access(
        shared,
        connection_id,
        client_id,
        &get_session_id(cid),
        Some(&rejection_fields),
    )
    .await
    {
        return;
    }

    let delete_result = {
        // Drop the `MutexGuard` before any `.await` below — same discipline
        // as every other `session_manager` use in this file.
        let mut session_manager = shared
            .session_manager
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        session_manager.delete_session(&get_session_id(cid))
    };
    if let Err(e) = delete_result {
        let detail = match e {
            DeleteSessionError::NotFound => "session_not_found",
            DeleteSessionError::Io(e) => {
                log::error!("Failed to delete session {cid}: {e}");
                "delete_failed"
            }
        };
        send_event(
            shared,
            connection_id,
            WsOutboundEvent::Error,
            Some(&rejection_fields),
            serde_json::json!({"detail": detail}),
        )
        .await;
        return;
    }

    // Cancel any in-flight agent turn/subagents for this chat so a stale
    // write doesn't keep landing on the tombstoned session (see
    // `SessionManager::delete_session`'s doc comment). `None` in every test
    // fixture and whenever no live `AgentLoop` was wired in — see
    // `GatewayServices::set_work_canceller`.
    if let Some(canceller) = shared.gateway_services.work_canceller() {
        canceller.abort("websocket", cid).await;
    }
    // The abort above may publish a `TurnEnd`, but that only clears the
    // *agent's* bookkeeping — also clear the WebSocket-side turn projection
    // directly so a client that queries it after `chat_deleted` never sees
    // this chat_id as still "running".
    shared
        .gateway_services
        .turn_registry
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear_chat(cid);

    // Best-effort: unlink the WebUI transcript too. Never blocks the
    // deletion itself — a missing/never-created transcript is not an error.
    shared
        .gateway_services
        .transcripts
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .forget_session(cid);

    // `detach_chat` only returns connections currently *subscribed* to
    // `cid` (i.e. attached to it as their active chat). The requester may
    // be deleting a different, inactive sidebar row — e.g. any chat but
    // the one it's currently viewing — in which case it wouldn't be in
    // that list at all and would never learn the delete succeeded, leaving
    // the row stuck in its sidebar. Always include the requester.
    let recipients = {
        let mut connections = shared.connections.lock().await;
        let mut recipients = connections.detach_chat(cid);
        if !recipients.iter().any(|(id, _)| id == connection_id)
            && let Some(sender) = connections.sender_for(connection_id)
        {
            recipients.push((connection_id.to_string(), sender));
        }
        recipients
    };
    let raw = serde_json::json!({
        "event": WsOutboundEvent::ChatDeleted.as_str(),
        "chat_id": cid,
    })
    .to_string();
    for (recipient_id, tx) in recipients {
        if tx.send(Message::text(raw.clone())).is_err() {
            log::warn!(
                "WebSocket channel: connection '{recipient_id}' gone while sending chat_deleted, cleaning up"
            );
            shared
                .connections
                .lock()
                .await
                .cleanup_connection(&recipient_id);
        }
    }
}

/// Handle a `list_chats` envelope: reply with every `websocket:`-keyed
/// session as a `chats` event, most-recently-updated first (the order
/// `list_sessions()` already sorts in). New protocol surface with no
/// nanobot precedent — see [`EnvelopeType::ListChats`]'s doc comment.
///
/// When [`WsShared::require_auth`] is off, [`scope_chats_to_owner`] narrows
/// this down to chats owned by the requesting connection's `client_id` —
/// see the "Optional WebSocket login" plan's guest session isolation
/// section. `require_auth == true` keeps the existing global listing.
async fn handle_envelope_list_chats<'a>(envelope_dispatch_context: EnvelopeDispatchContext<'a>) {
    let shared = envelope_dispatch_context.shared;
    let connection_id = envelope_dispatch_context.connection_id;
    let client_id = envelope_dispatch_context.client_id;

    let sessions = {
        // Scoped so the (synchronous) `MutexGuard` is dropped before
        // `send_event`'s `.await` below — same discipline as every other
        // `session_manager` use in this file.
        let session_manager = shared
            .session_manager
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        session_manager.list_sessions()
    };
    let mut chats = list_websocket_chats(sessions);
    if !shared.require_auth {
        chats = scope_chats_to_owner(chats, client_id);
    }
    strip_owner_client_id(&mut chats);
    send_event(
        shared,
        connection_id,
        WsOutboundEvent::ChatsList,
        None,
        serde_json::json!({"chats": chats}),
    )
    .await;
}

/// Handle a `list_skills` envelope: reply with every skill installed on
/// this process (workspace + builtin `SKILL.md` directories) as a `skills`
/// event. New protocol surface with no nanobot precedent — see
/// [`EnvelopeType::ListSkills`]'s doc comment.
async fn handle_envelope_list_skills<'a>(envelope_dispatch_context: EnvelopeDispatchContext<'a>) {
    let shared = envelope_dispatch_context.shared;
    let connection_id = envelope_dispatch_context.connection_id;

    let loader = SkillsLoader::new(&shared.workspace_request_handler.default_workspace, None);
    let summaries = loader.list_skill_summaries();
    send_event(
        shared,
        connection_id,
        WsOutboundEvent::SkillsList,
        None,
        serde_json::json!({"skills": summaries}),
    )
    .await;
}

/// Guest session isolation: keep only entries whose `owner_client_id` field
/// (see [`SessionManager::list_sessions`]) matches `client_id` exactly.
/// Fails closed — an entry with no owner at all (a session that predates
/// this feature) is dropped, not treated as shared. Kept as a plain,
/// signal-free function alongside [`list_websocket_chats`] so the filtering
/// is unit-testable without a real `SessionManager`.
fn scope_chats_to_owner(chats: Vec<serde_json::Value>, client_id: &str) -> Vec<serde_json::Value> {
    chats
        .into_iter()
        .filter(|entry| {
            entry
                .get("owner_client_id")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                == Some(client_id)
        })
        .collect()
}

/// Drop the internal `owner_client_id` field before a chat list goes out
/// over the wire — it's bookkeeping for [`scope_chats_to_owner`], not part
/// of the `chats` event's public shape.
fn strip_owner_client_id(chats: &mut [serde_json::Value]) {
    for entry in chats.iter_mut() {
        if let Some(map) = entry.as_object_mut() {
            map.remove("owner_client_id");
        }
    }
}

/// Flatten a persisted `content` field to the text a transcript can render.
///
/// [`AgentLoop::save_turn`] writes a block array (not a string) whenever the
/// turn carried media, so a plain `as_str()` would silently blank out every
/// message that had an image attached. Non-text blocks have no transcript
/// representation here and are dropped. Text blocks that are themselves an
/// `[image: <path>]`/`[image]` placeholder (`sanitize_persisted_blocks`'
/// stand-in for a stripped `data:` image — see `image_placeholder_text`) are
/// dropped too: [`extract_media_refs`] recovers the thumbnail from the same
/// placeholder, so showing it as literal text as well would duplicate it.
fn display_text(content: Option<&serde_json::Value>) -> String {
    match content {
        Some(serde_json::Value::String(text)) => text.clone(),
        Some(serde_json::Value::Array(blocks)) => blocks
            .iter()
            .filter(|block| block.get("type").and_then(|v| v.as_str()) == Some("text"))
            .filter_map(|block| block.get("text").and_then(|v| v.as_str()))
            .filter(|text| !text.is_empty() && !is_image_placeholder_text(text))
            .collect::<Vec<&str>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Whether `text` is exactly the placeholder [`image_placeholder_text`]
/// produces for a sanitized `data:` image block (`"[image: <path>]"` when a
/// path was recorded, bare `"[image]"` otherwise).
fn is_image_placeholder_text(text: &str) -> bool {
    text == "[image]" || (text.starts_with("[image: ") && text.ends_with(']'))
}

/// Recover the placeholder's stored path, or `None` for a pathless
/// `"[image]"` placeholder (nothing on disk to link back to).
fn image_placeholder_path(text: &str) -> Option<&str> {
    text.strip_prefix("[image: ")?.strip_suffix(']')
}

/// Extract raw (unresolved) media references from a persisted message's
/// content blocks: local file paths recovered from `[image: <path>]`
/// placeholders (the `sanitize_persisted_blocks` stand-in for a stripped
/// `data:` image) and any `image_url` blocks that survived sanitization
/// because they were already an `http(s)://` reference rather than a
/// `data:` one. Kept pure/filesystem-free — same reasoning as
/// [`crate::channels::websocket::webui::transcript::transcript_chat_history`]'s
/// own raw `media` field — with actual path -> URL resolution left to
/// [`resolve_history_media`].
fn extract_media_refs(content: Option<&serde_json::Value>) -> Vec<String> {
    let Some(serde_json::Value::Array(blocks)) = content else {
        return Vec::new();
    };
    blocks
        .iter()
        .filter_map(|block| match block.get("type").and_then(|v| v.as_str())? {
            "text" => {
                let text = block.get("text").and_then(|v| v.as_str())?;
                image_placeholder_path(text).map(str::to_string)
            }
            "image_url" => block
                .get("image_url")
                .and_then(|v| v.as_object())
                .and_then(|iu| iu.get("url"))
                .and_then(|v| v.as_str())
                .filter(|url| url.starts_with("http://") || url.starts_with("https://"))
                .map(str::to_string),
            _ => None,
        })
        .collect()
}

/// Resolve raw `media` refs on each display-history row (as attached by
/// [`crate::channels::websocket::webui::transcript::WebUiTranscriptRecorder::chat_history`]
/// / [`websocket_chat_history`] below — still either an absolute on-disk
/// path or a passthrough `http(s)://` URL) into browser-reachable
/// `/v1/media/...` URLs, dropping refs that don't resolve (file missing, or
/// outside `media_root`) and removing the `media` key entirely when nothing
/// resolves. The one place in the attach/fork history pipeline that touches
/// the filesystem, kept separate from the pure projector functions above so
/// their own unit tests stay filesystem-free.
fn resolve_history_media(history: &mut [serde_json::Value], media_root: &std::path::Path) {
    for entry in history.iter_mut() {
        let Some(raw) = entry.get("media").and_then(|v| v.as_array()).cloned() else {
            continue;
        };
        let resolved: Vec<serde_json::Value> = raw
            .iter()
            .filter_map(|v| v.as_str())
            .filter_map(|raw_ref| {
                if raw_ref.starts_with("http://") || raw_ref.starts_with("https://") {
                    Some(raw_ref.to_string())
                } else {
                    crate::channels::websocket::webui::media::media_url_from_stored_path(
                        raw_ref, media_root,
                    )
                }
            })
            .map(serde_json::Value::String)
            .collect();
        let Some(obj) = entry.as_object_mut() else {
            continue;
        };
        if resolved.is_empty() {
            obj.remove("media");
        } else {
            obj.insert("media".to_string(), serde_json::json!(resolved));
        }
    }
}

/// Shape one session's messages for the `attached` envelope's `history`
/// field. Kept as a plain, signal-free function (no lock, no I/O) so the
/// filter/cap/projection is unit-testable without a live WebSocket.
///
/// Unlike [`Session::get_history`] (the LLM prompt window: drops the
/// consolidated prefix and keeps tool rows), this is a *display* view: the
/// full conversation (capped), `user`/`assistant` only, with slash-command
/// and hidden/automation turns stripped.
fn websocket_chat_history(
    session: Option<&Session>,
    max_messages: usize,
) -> Vec<serde_json::Value> {
    let Some(session) = session else {
        return Vec::new();
    };
    if max_messages == 0 {
        return Vec::new();
    }
    let mut visible: Vec<&serde_json::Value> = session
        .messages
        .iter()
        .filter(|message| {
            let role = message.get("role").and_then(|v| v.as_str()).unwrap_or("");
            (role == "user" || role == "assistant")
                && message.get(COMMAND_KEY).is_none()
                && !is_hidden_history_message(message)
        })
        .collect();

    if visible.len() > max_messages {
        visible = visible[visible.len() - max_messages..].to_vec();
    }
    // Don't open the transcript on a dangling assistant reply.
    if let Some(start) = visible
        .iter()
        .position(|message| message.get("role").and_then(|v| v.as_str()) == Some("user"))
    {
        visible = visible[start..].to_vec();
    }

    visible
        .into_iter()
        .map(|message| {
            let mut entry = serde_json::json!({
                "role": message.get("role").and_then(|v| v.as_str()).unwrap_or(""),
                "content": display_text(message.get("content")),
            });
            if let Some(timestamp) = message.get("timestamp").and_then(|v| v.as_str()) {
                entry["timestamp"] = serde_json::json!(timestamp);
            }
            if let Some(reasoning) = message
                .get("reasoning_content")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
            {
                entry["reasoning_content"] = serde_json::json!(reasoning);
            }
            let media = extract_media_refs(message.get("content"));
            if !media.is_empty() {
                entry["media"] = serde_json::json!(media);
            }
            entry
        })
        .collect()
}

/// Merge `extra`'s top-level object fields into `base`. Both `base` and
/// `extra` are expected to be JSON objects — mirrors the same
/// `serde_json::Map::extend` pattern [`send_event`] uses to combine a
/// payload from several field sources.
fn merge_json(base: &mut serde_json::Value, extra: serde_json::Value) {
    if let (Some(base_map), serde_json::Value::Object(extra_map)) = (base.as_object_mut(), extra) {
        base_map.extend(extra_map);
    }
}

/// Model-preset catalog plus this session's resolved selection, merged into
/// every `attached` payload (attach, fork) so the client learns both without
/// a separate round-trip. `session: None` resolves to the process-wide
/// default — same as a chat that was never persisted.
///
/// `model_preset`/`model` are the *resolved* runtime, not the raw stored
/// metadata string: `runtime_for_session` already falls back to the process
/// default for a stale/unknown preset name, and the wire should report that
/// same fallback rather than a name the client can't reconcile with `model`.
fn model_preset_attached_fields(shared: &WsShared, session: Option<&Session>) -> serde_json::Value {
    let runtime = shared.runtime_resolver.runtime_for_session(session);
    serde_json::json!({
        "model_presets": shared.runtime_resolver.available_preset_names(),
        "model_preset": runtime.preset_name,
        "model": runtime.model,
    })
}

fn agent_mode_attached_fields(shared: &WsShared, session: Option<&Session>) -> serde_json::Value {
    let mode = AgentMode::resolve(shared.default_agent_mode, session.map(|s| &s.metadata));
    serde_json::json!({ "mode": mode.as_str() })
}

/// Session lifetime `token_usage`, merged into every `attached` payload
/// (attach, new_chat, fork) alongside [`model_preset_attached_fields`] so the
/// client learns the running totals without a separate round-trip.
///
/// Returns `{}` (nothing to merge) when the session has no usage yet — a
/// brand new chat, or a gateway build that predates usage tracking — rather
/// than sending an explicit `null` that would need special-casing client-side.
fn token_usage_attached_fields(session: Option<&Session>) -> serde_json::Value {
    let Some(usage) = session.and_then(Session::usage) else {
        return serde_json::json!({});
    };
    serde_json::json!({ "token_usage": usage })
}

/// Subscribe `connection_id` to `chat_id`, ack with `attached` (including
/// a display `history` snapshot), then replay any in-flight goal/turn
/// strip. Argument order matches [`ConnectionRegistry::attach`] /
/// nanobot's `_attach(connection, chat_id)`.
///
/// History prefers the durable WebUI transcript — same
/// `chat_history`/`MAX_HISTORY_MESSAGES` source `handle_envelope_fork_chat`
/// reads from — over the `Session`, falling back to the `Session` only when
/// the transcript has no rows (e.g. a chat that predates the transcript
/// write path, or was never driven through the webui-flagged envelope
/// path).
async fn attach_chat(connection_id: &str, chat_id: &str, shared: &WsShared) {
    shared
        .connections
        .lock()
        .await
        .attach(connection_id, chat_id);
    let session_key = get_session_id(chat_id);
    let transcript_history = {
        // Scoped so the (synchronous) `MutexGuard` is dropped before
        // `send_event`'s `.await` below — same discipline as every other
        // `gateway_services.transcripts` use in this file.
        let recorder = shared
            .gateway_services
            .transcripts
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        recorder.chat_history(&session_key, MAX_HISTORY_MESSAGES)
    };
    // Always loaded now (previously only on the transcript-empty fallback
    // path below) so `model_preset_attached_fields` has the session's
    // stored preset override regardless of which history source is used.
    let session = {
        // Scoped so the (synchronous) `MutexGuard` is dropped before
        // `send_event`'s `.await` below — same discipline as every other
        // `session_manager` use in this file.
        let session_manager = shared
            .session_manager
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        session_manager.get_session_internal(&session_key)
    };
    let mut history = if !transcript_history.is_empty() {
        transcript_history
    } else {
        log::info!(
            "websocket: no transcript history found for chat {chat_id}, using session history"
        );
        websocket_chat_history(session.as_ref(), MAX_HISTORY_MESSAGES)
    };
    resolve_history_media(&mut history, &shared.media_root);
    let mut payload = serde_json::json!({"chat_id": chat_id, "history": history});
    merge_json(
        &mut payload,
        model_preset_attached_fields(shared, session.as_ref()),
    );
    merge_json(
        &mut payload,
        agent_mode_attached_fields(shared, session.as_ref()),
    );
    merge_json(&mut payload, token_usage_attached_fields(session.as_ref()));
    send_event(
        shared,
        connection_id,
        WsOutboundEvent::Attached,
        None,
        payload,
    )
    .await;
    hydrate_after_subscribe(chat_id, shared).await;
}

async fn handle_envelope_message<'a>(envelope_dispatch_context: EnvelopeDispatchContext<'a>) {
    let envelope = envelope_dispatch_context.envelope;
    let (shared, connection_id, client_id) = envelope_dispatch_context.connection_fields();

    let Some(cid) = require_valid_chat_id(&envelope_dispatch_context).await else {
        return;
    };

    let raw_turn_id = envelope.get("turn_id").and_then(|v| v.as_str());
    let turn_id = raw_turn_id.filter(|t| !t.is_empty());

    let mut rejection_fields = serde_json::Map::new();
    rejection_fields.insert(
        "chat_id".to_string(),
        serde_json::Value::String(cid.to_string()),
    );
    if let Some(turn_id) = turn_id {
        rejection_fields.insert(
            "turn_id".to_string(),
            serde_json::Value::String(turn_id.to_string()),
        );
    }

    // The allowlist can change while an authenticated websocket stays open.
    // Reject the exact application turn before hydration, transcript
    // persistence, or an acceptance ACK — mirrors runtime.py:701-712.
    if !sender_allowed(&shared.channels_config, client_id) {
        send_event(
            shared,
            connection_id,
            WsOutboundEvent::Error,
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
            WsOutboundEvent::Error,
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
            WsOutboundEvent::Error,
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
    // A JSON `null` is treated the same as an absent key — mirrors Python's
    // `if raw_media is not None:` (`runtime.py:734`), where `envelope.get("media")`
    // already returns `None` for both. `envelope.get("media")` alone doesn't:
    // it returns `Some(&Value::Null)` for an explicit `"media": null`, which
    // would otherwise fall into the `as_array()` mismatch below and get
    // rejected as malformed.
    if let Some(raw_media) = envelope.get("media").filter(|v| !v.is_null()) {
        let Some(media_array) = raw_media.as_array() else {
            send_event(
                shared,
                connection_id,
                WsOutboundEvent::Error,
                Some(&rejection_fields),
                serde_json::json!({"detail": "attachment_rejected", "reason": "malformed"}),
            )
            .await;
            return;
        };
        let media_dir = get_media_dir(Some("websocket"));
        match store_inbound_attachments(
            media_array,
            &media_dir,
            shared.gateway_services.ingress.attachments,
        ) {
            Ok(paths) => media_paths = paths,
            Err(reason) => {
                send_event(
                    shared,
                    connection_id,
                    WsOutboundEvent::Error,
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
            WsOutboundEvent::Error,
            Some(&rejection_fields),
            serde_json::json!({"detail": "missing content"}),
        )
        .await;
        return;
    }

    // Auto-attach on first use so clients can one-shot without a separate
    // `attach` envelope — mirrors runtime.py:765.
    shared.connections.lock().await.attach(connection_id, cid);
    hydrate_after_subscribe(cid, shared).await;

    // Resolve after hydration so a concurrent downgrade cannot be overwritten.
    let resolver = {
        let cid = cid.to_string();
        let ws_shared = shared.clone();
        let envelope = envelope.clone();
        // Capture a bool — not `EnvelopeDispatchContext` — so the `Arc<dyn Fn… + 'static>`
        // closure doesn't inherit the handler's short lifetime.
        let controls_available = envelope_dispatch_context.workspace_controls_available();
        Arc::new(move || {
            // Run the sync work (and drop `MutexGuard`s) *before* boxing the future:
            // `std::sync::MutexGuard` is `!Send` and can't live inside a `Send` future.
            let result = {
                let mut session_manager = ws_shared
                    .session_manager
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                let turn_registry = ws_shared
                    .gateway_services
                    .turn_registry
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                ws_shared.workspace_request_handler.scope_for_message(
                    &mut session_manager,
                    &envelope,
                    &cid,
                    turn_registry.websocket_turn_wall_started_at(&cid).is_some(),
                    controls_available,
                )
            };
            Box::pin(async move { result })
                as Pin<Box<dyn Future<Output = Result<WorkspaceScope, WorkspaceScopeError>> + Send>>
        })
    };
    let Some(scope) =
        workspace_scope_or_error(shared, Some(cid), turn_id, connection_id, resolver).await
    else {
        return;
    };

    // Hydration and scope resolution can yield. Re-check immediately
    // before transcript/bus mutation so a mid-flight revocation cannot
    // fall through BaseChannel's silent deny and still receive an ACK.
    if !sender_allowed(&shared.channels_config, client_id) {
        send_event(
            shared,
            connection_id,
            WsOutboundEvent::Error,
            Some(&rejection_fields),
            serde_json::json!({"detail": "access_denied"}),
        )
        .await;
        return;
    }

    let mut metadata: HashMap<String, serde_json::Value> = HashMap::new();
    metadata.insert(
        "remote".to_string(),
        serde_json::json!(envelope_dispatch_context.remote_addr.to_string()),
    );
    // Mirror Python's `envelope.get("webui") is True` — only a JSON boolean
    // true counts; presence of other values must not set the webui flag.
    if envelope.get("webui").and_then(|v| v.as_bool()) == Some(true) {
        metadata.insert("webui".to_string(), serde_json::json!(true));
        metadata.extend(client_turn_metadata(envelope.get("turn_id")));
    }
    let cli_apps_raw = envelope.get("cli_apps");
    let cli_apps = normalize_cli_app_mentions(cli_apps_raw);
    if !cli_apps.is_empty() {
        metadata.insert("cli_apps".to_string(), serde_json::json!(cli_apps));
    }
    let mcp_presets = crate::agent::tools::mcp::mcp_presets_api::normalize_mcp_preset_mentions(
        envelope.get("mcp_presets"),
    );
    if !mcp_presets.is_empty() {
        metadata.insert("mcp_presets".to_string(), serde_json::json!(mcp_presets));
    }
    metadata.insert(WORKSPACE_SCOPE_METADATA_KEY.to_string(), scope.metadata());
    {
        // Recover from a poisoned mutex rather than panicking the WS handler —
        // same pattern as the scope resolver above.
        let mut session_manager = shared
            .session_manager
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        shared.workspace_request_handler.persist_scope(
            &mut session_manager,
            cid,
            &scope,
            client_id,
        );
    }

    let is_webui = metadata.get("webui").and_then(|v| v.as_bool()) == Some(true);
    let webui_quote_allowed =
        webui_quote_allowed(is_webui, envelope_dispatch_context.webui_authenticated);
    let mut queued_owner_metadata: Option<String> = None;
    if is_webui && builtin_command_starts_agent_turn(content) {
        let mut turn_registry = shared
            .gateway_services
            .turn_registry
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(queued_owner) = turn_registry.register_queued_turn_if_idle(cid, turn_id) {
            metadata.insert(
                WEBSOCKET_TURN_OWNER_METADATA_KEY.to_string(),
                serde_json::json!(queued_owner),
            );
            queued_owner_metadata = Some(queued_owner);
        }
    }
    if is_webui {
        // Recover from a poisoned mutex rather than panicking the WS handler —
        // same pattern as the turn registry lock above. Scoped so the
        // (synchronous) `MutexGuard` is dropped before this block's own
        // `send_user_turn`/other `.await`s below.
        {
            let mut transcripts = shared
                .gateway_services
                .transcripts
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            transcripts.append_user_message(
                cid,
                content,
                &metadata,
                (!media_paths.is_empty()).then_some(media_paths.as_slice()),
                (!cli_apps.is_empty()).then_some(cli_apps.as_slice()),
                (!mcp_presets.is_empty()).then_some(mcp_presets.as_slice()),
            );
        }
        if webui_quote_allowed
            && let Some(block) = webui_quote_runtime_context(envelope.get("quoted_context"))
        {
            metadata.insert(
                RUNTIME_CONTEXT_INPUT_META.to_string(),
                serde_json::to_value([block]).unwrap_or(serde_json::Value::Null),
            );
        }
        // Fan out the user half of this turn to every subscriber *before*
        // publishing below — see `send_user_turn`'s doc comment for why the
        // ordering matters. Keyed off the normalized turn id (always present
        // here: `client_turn_metadata` above set it whenever `is_webui` is
        // true) rather than the client-supplied `turn_id`, so a client that
        // omitted one still gets a stable id other subscribers can adopt.
        if let Some(normalized_turn_id) = metadata
            .get(WEBUI_TURN_METADATA_KEY)
            .and_then(|v| v.as_str())
        {
            let media_urls = resolve_media_urls(&media_paths, &shared.media_root);
            send_user_turn(cid, normalized_turn_id, content, &media_urls, shared).await;
        }
    }
    let send_result = handle_message(
        client_id,
        cid,
        content,
        (!media_paths.is_empty()).then_some(media_paths),
        Some(metadata),
        None,
        sender_allowed(&shared.channels_config, client_id),
        shared.supports_streaming,
        shared.name,
        &shared.bus,
    )
    .await;
    if let Err(e) = &send_result {
        // Mirrors nanobot's `_handle_message` exception propagating out of
        // the `try` block (`runtime.py:830-841`): unlike a raised exception,
        // a `Result::Err` here doesn't log itself anywhere up the call
        // chain, so without this the failure would be entirely silent.
        log::warn!("WebSocket channel: failed to publish message for chat '{cid}': {e}");
        if let Some(queued_owner) = &queued_owner_metadata {
            let mut turn_registry = shared
                .gateway_services
                .turn_registry
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            turn_registry.clear_turn_if_current(cid, Some(queued_owner.as_str()), false);
        }
    }
    // Mirrors nanobot's `if is_webui and turn_id:` (`runtime.py:842`) — sent
    // only when the publish above actually succeeded (a raised exception in
    // Python skips this line entirely on its way out of the function) *and*
    // the client supplied a turn_id to acknowledge.
    if is_webui
        && send_result.is_ok()
        && let Some(turn_id) = turn_id
    {
        send_event(
            shared,
            connection_id,
            WsOutboundEvent::MessageAccepted,
            None,
            serde_json::json!({"chat_id": cid, "turn_id": turn_id}),
        )
        .await;
    }
}

/// Mirrors nanobot's `_CLI_APP_NAME_RE` (`webui/cli_apps_api.py:15`).
static CLI_APP_NAME_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^[a-z0-9][a-z0-9_-]{0,63}$").unwrap());

/// Attribute keys (besides `name`, handled separately) copied from a
/// client-supplied CLI app mention, each with its own clip length. Mirrors
/// nanobot's `_CLI_APP_ATTACHMENT_KEYS[1:]` (`webui/cli_apps_api.py:16-23`)
/// paired with the `512 if field == "logo_url" else 160` clip rule
/// (`webui/cli_apps_api.py:77-80`).
const CLI_APP_ATTACHMENT_FIELDS: &[(&str, usize)] = &[
    ("display_name", 160),
    ("category", 160),
    ("entry_point", 160),
    ("logo_url", 512),
    ("brand_color", 160),
];

/// Trim a string value and clip it to `limit` *characters*; `None` for
/// non-string, missing, empty, or whitespace-only input. Mirrors nanobot's
/// `_clip_ws_string` (`webui/cli_apps_api.py:48-54`).
fn clip_ws_string(value: Option<&serde_json::Value>, limit: usize) -> Option<String> {
    let text = value?.as_str()?.trim();
    if text.is_empty() {
        return None;
    }
    Some(text.chars().take(limit).collect())
}

/// Sanitize structured CLI app mentions sent by the WebUI. Mirrors nanobot's
/// `normalize_cli_app_mentions` (`webui/cli_apps_api.py:57-84`).
fn normalize_cli_app_mentions(raw: Option<&serde_json::Value>) -> Vec<HashMap<String, String>> {
    let Some(serde_json::Value::Array(items)) = raw else {
        return vec![];
    };

    let mut out = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for item in items.iter().take(8) {
        let Some(app_data) = item.as_object() else {
            continue;
        };
        let Some(name) = clip_ws_string(app_data.get("name"), 64) else {
            continue;
        };
        if !CLI_APP_NAME_RE.is_match(&name) {
            continue;
        }
        let key = name.to_lowercase();
        if !seen.insert(key.clone()) {
            continue;
        }
        let mut row = HashMap::new();
        row.insert("name".to_string(), key);
        for (field, limit) in CLI_APP_ATTACHMENT_FIELDS {
            if let Some(value) = clip_ws_string(app_data.get(*field), *limit) {
                row.insert(field.to_string(), value);
            }
        }
        out.push(row);
    }
    out
}

/// A [`WorkspaceScope`] resolver deferred behind a closure so callers can
/// build it from state (a session lock, an envelope) captured at construction
/// time, while `workspace_scope_or_error` stays agnostic to what kind of
/// scope decision it's resolving.
type ScopeResolver = Arc<
    dyn Fn() -> Pin<Box<dyn Future<Output = Result<WorkspaceScope, WorkspaceScopeError>> + Send>>
        + Send
        + Sync,
>;

/// Resolve a workspace scope, or send the client a `workspace_scope_rejected`
/// error and return `None`. Mirrors nanobot's `_workspace_scope_or_error`
/// (`channels/websocket/runtime.py:852-871`).
///
/// `cid` is `Option<&str>`, not `&str`, because the Python reference's
/// `chat_id` parameter defaults to (and, for `new_chat`, is always called
/// with) `None` — a rejected new-chat scope was never attached to any chat
/// id, so there is nothing to report and the field is omitted entirely
/// rather than naming an id the client was never told about.
async fn workspace_scope_or_error(
    shared: &WsShared,
    cid: Option<&str>,
    turn_id: Option<&str>,
    connection_id: &str,
    resolver: ScopeResolver,
) -> Option<WorkspaceScope> {
    let err = match resolver().await {
        Ok(scope) => return Some(scope),
        Err(err) => err,
    };
    let mut base_fields = serde_json::Map::new();
    if let Some(cid) = cid {
        base_fields.insert(
            "chat_id".to_string(),
            serde_json::Value::String(cid.to_string()),
        );
    }
    if let Some(turn_id) = turn_id {
        base_fields.insert(
            "turn_id".to_string(),
            serde_json::Value::String(turn_id.to_string()),
        );
    }
    send_event(
        shared,
        connection_id,
        WsOutboundEvent::Error,
        Some(&base_fields),
        serde_json::json!({
            "detail": "workspace_scope_rejected",
            "reason": err.message,
        }),
    )
    .await;
    None
}

/// Replay persisted or actively running per-chat state after subscribe.
/// Mirrors nanobot's `_hydrate_after_subscribe`
/// (`channels/websocket/runtime.py:372-375`).
async fn hydrate_after_subscribe(chat_id: &str, ws_shared: &WsShared) {
    maybe_push_active_goal_state(chat_id, ws_shared).await;
    maybe_push_turn_run_wall_clock(chat_id, ws_shared).await;
}

async fn maybe_push_active_goal_state(chat_id: &str, ws_shared: &WsShared) {
    // Scoped so the (synchronous) lock is dropped before `send_goal_state`'s
    // `.await` below — holding a `std::sync::MutexGuard` across an await
    // point risks blocking the executor thread for as long as the send takes.
    let row_option = {
        let session_manager = ws_shared
            .session_manager
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        session_manager.read_session_file(&get_session_id(chat_id))
    };
    let row_data = if let Some(row) = row_option {
        row.metadata
    } else {
        HashMap::new()
    };
    let blob = goal_state_ws_blob(&row_data);
    let active = blob
        .get("active")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if !active {
        return;
    }
    send_goal_state(chat_id, blob, ws_shared).await;
}

/// Push a persisted goal-state snapshot to every connection subscribed to
/// `chat_id` (multi-chat isolation). Mirrors nanobot's `send_goal_state`
/// (`channels/websocket/runtime.py:1270-1278`); fan-out + cleanup-on-failure
/// follows the same pattern as [`WebSocketChannel::send`] below.
async fn send_goal_state(chat_id: &str, blob: serde_json::Value, ws_shared: &WsShared) {
    let recipients = ws_shared.connections.lock().await.senders_for_chat(chat_id);
    if recipients.is_empty() {
        return;
    }
    let body = serde_json::json!({
        "event": WsOutboundEvent::GoalState.as_str(),
        "chat_id": chat_id,
        "goal_state": blob,
    });
    let raw = body.to_string();
    for (connection_id, tx) in recipients {
        if tx.send(Message::text(raw.clone())).is_err() {
            log::warn!(
                "WebSocket channel: connection '{connection_id}' gone while sending goal_state, cleaning up"
            );
            ws_shared
                .connections
                .lock()
                .await
                .cleanup_connection(&connection_id);
        }
    }
}

/// Map stored attachment paths (as returned by [`store_inbound_attachments`])
/// to browser-relative `/v1/media/...` URLs for the outbound `user` event,
/// dropping any that no longer resolve. Always disk paths (never an
/// `http(s)://` passthrough — unlike [`resolve_history_media`]'s `media`
/// field, a freshly stored attachment was never anything else), so this is a
/// direct map over [`media_url_from_stored_path`] rather than needing that
/// function's extra branch. Split out from its one call site in
/// [`handle_envelope_message`] so it's unit-testable against a real
/// `media_root` without going through the global-config-backed
/// `store_inbound_attachments`/`get_media_dir` path.
fn resolve_media_urls(media_paths: &[String], media_root: &std::path::Path) -> Vec<String> {
    media_paths
        .iter()
        .filter_map(|p| {
            crate::channels::websocket::webui::media::media_url_from_stored_path(p, media_root)
        })
        .collect()
}

/// Fan out the user half of a just-accepted webui turn to every connection
/// subscribed to `chat_id`, including the sender. A client watching the same
/// chat from elsewhere has no other way to learn the prompt or `turn_id`, so
/// without this it can only ever render the assistant side of someone else's
/// turn (or nothing at all, since `delta`/`stream_end` are keyed off a
/// `turn_id` it never received). Rust-side addition, no nanobot wire-name
/// precedent to mirror.
///
/// Called from [`handle_envelope_message`] *before* the turn is published to
/// the bus — a fast-starting stream must never emit `delta` before a watcher
/// has had a chance to adopt `turn_id` and set its own `active_turn_id`.
/// Fan-out + cleanup-on-failure follows the same pattern as
/// [`send_goal_state`]. The sender also receives this frame; that's fine —
/// clients that already recorded `turn_id` locally (the sender did, via its
/// own optimistic insert) are expected to ignore a duplicate.
async fn send_user_turn(
    chat_id: &str,
    turn_id: &str,
    text: &str,
    media: &[String],
    ws_shared: &WsShared,
) {
    let recipients = ws_shared.connections.lock().await.senders_for_chat(chat_id);
    if recipients.is_empty() {
        return;
    }
    let mut body = serde_json::json!({
        "event": WsOutboundEvent::User.as_str(),
        "chat_id": chat_id,
        "turn_id": turn_id,
        "text": text,
    });
    if !media.is_empty() {
        body["media"] = serde_json::json!(media);
    }
    let raw = body.to_string();
    for (connection_id, tx) in recipients {
        if tx.send(Message::text(raw.clone())).is_err() {
            log::warn!(
                "WebSocket channel: connection '{connection_id}' gone while sending user turn, cleaning up"
            );
            ws_shared
                .connections
                .lock()
                .await
                .cleanup_connection(&connection_id);
        }
    }
}

/// Replay ``goal_status: running`` when a turn is still active (same-process refresh).
/// Replay `goal_status: running` when a turn is still active (same-process
/// refresh). Mirrors nanobot's `_maybe_push_turn_run_wall_clock`
/// (`channels/websocket/runtime.py:360-370`).
async fn maybe_push_turn_run_wall_clock(chat_id: &str, ws_shared: &WsShared) {
    // Scoped so the lock is dropped before `send_goal_status`'s `.await`
    // below — same reasoning as `maybe_push_active_goal_state`.
    let (t0, turn_id) = {
        let turn_registry = ws_shared
            .gateway_services
            .turn_registry
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let Some(t0) = turn_registry.websocket_turn_wall_started_at(chat_id) else {
            return;
        };
        (t0, turn_registry.websocket_turn_id(chat_id))
    };
    send_goal_status(chat_id, "running", Some(t0), turn_id, ws_shared).await;
}

/// Notify subscribed clients that a turn started or finished (wall-clock
/// hint). Mirrors nanobot's `send_goal_status`
/// (`channels/websocket/runtime.py:1280-1303`).
async fn send_goal_status(
    chat_id: &str,
    status: &str,
    started_at: Option<f64>,
    turn_id: Option<String>,
    ws_shared: &WsShared,
) {
    let recipients = ws_shared.connections.lock().await.senders_for_chat(chat_id);
    if recipients.is_empty() {
        return;
    }
    let mut body = serde_json::json!({
        "event": WsOutboundEvent::GoalStatus.as_str(),
        "chat_id": chat_id,
        "status": status,
    });
    if status == "running"
        && let Some(started_at) = started_at
    {
        body["started_at"] = serde_json::json!(started_at);
    }
    if let Some(turn_id) = turn_id.filter(|t| !t.is_empty()) {
        body["turn_id"] = serde_json::json!(turn_id);
    }
    let raw = body.to_string();
    for (connection_id, tx) in recipients {
        if tx.send(Message::text(raw.clone())).is_err() {
            log::warn!(
                "WebSocket channel: connection '{connection_id}' gone while sending goal_status, cleaning up"
            );
            ws_shared
                .connections
                .lock()
                .await
                .cleanup_connection(&connection_id);
        }
    }
}

/// Handle a `clear_session` envelope: wipe the conversation on an existing
/// `websocket:{chat_id}` session (messages, goal state, token usage) while
/// leaving the session itself, its title, owner, and other metadata intact.
/// Also empties the WebUI transcript so a later `attach` cannot resurrect
/// the wiped history. Rust-side protocol addition with no nanobot
/// precedent — see [`EnvelopeType::ClearSession`].
async fn handle_envelope_clear_session<'a>(envelope_dispatch_context: EnvelopeDispatchContext<'a>) {
    let (shared, connection_id, client_id) = envelope_dispatch_context.connection_fields();

    let Some(cid) = require_valid_chat_id(&envelope_dispatch_context).await else {
        return;
    };

    let rejection_fields = create_rejection_fields(cid);

    if !sender_allowed(&shared.channels_config, client_id) {
        send_event(
            shared,
            connection_id,
            WsOutboundEvent::Error,
            Some(&rejection_fields),
            serde_json::json!({"detail": "access_denied"}),
        )
        .await;
        return;
    }

    if !check_owner_allows_access(
        shared,
        connection_id,
        client_id,
        &get_session_id(cid),
        Some(&rejection_fields),
    )
    .await
    {
        return;
    }

    // Cancel any in-flight agent turn/subagents for this chat *before*
    // wiping state, so a stale write cannot land on the just-cleared
    // session. `None` in every test fixture and whenever no live
    // `AgentLoop` was wired in — see `GatewayServices::set_work_canceller`.
    if let Some(canceller) = shared.gateway_services.work_canceller() {
        canceller.abort("websocket", cid).await;
    }
    // The abort above may publish a `TurnEnd`, but that only clears the
    // *agent's* bookkeeping — also clear the WebSocket-side turn projection
    // directly so a client that queries it after `session_cleared` never
    // sees this chat_id as still "running".
    shared
        .gateway_services
        .turn_registry
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear_chat(cid);

    let save_result = {
        // Drop the `MutexGuard` before any `.await` below — same discipline
        // as every other `session_manager` use in this file.
        let mut session_manager = shared
            .session_manager
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        match session_manager.get_session_internal(&get_session_id(cid)) {
            Some(mut session) => {
                session.clear();
                Some(session_manager.save(session))
            }
            None => None,
        }
    };
    match save_result {
        None => {
            send_event(
                shared,
                connection_id,
                WsOutboundEvent::Error,
                Some(&rejection_fields),
                serde_json::json!({"detail": "session_not_found"}),
            )
            .await;
            return;
        }
        Some(Err(e)) => {
            log::error!("Failed to clear session {cid}: {e}");
            send_event(
                shared,
                connection_id,
                WsOutboundEvent::Error,
                Some(&rejection_fields),
                serde_json::json!({"detail": "clear_failed"}),
            )
            .await;
            return;
        }
        Some(Ok(())) => {}
    }

    // Empty the WebUI transcript without tombstoning the key — `attach`
    // prefers transcript history over the `Session`, so leaving the JSONL
    // in place would resurrect the conversation that was just wiped.
    // Unlike `delete_chat`'s `forget_session`, later appends must still
    // write: the session is still live.
    shared
        .gateway_services
        .transcripts
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear_transcript(cid);

    send_session_cleared(cid, connection_id, shared).await;
}

fn create_rejection_fields(cid: &str) -> serde_json::Map<String, serde_json::Value> {
    let mut rejection_fields = serde_json::Map::new();
    rejection_fields.insert(
        "chat_id".to_string(),
        serde_json::Value::String(cid.to_string()),
    );
    rejection_fields
}

/// WebSocket server channel: rust-bot acts as a WebSocket server, serving
/// connected clients over `axum`'s `ws` feature (a thin wrapper around
/// `tokio-tungstenite`).
///
/// This channel's inbound HTTP serving is owned externally by
/// `cli::commands::run_gateway` (see [`Self::start`]'s doc comment) rather
/// than by this struct's own `start()`, so that login and live WS traffic
/// can share one port.
///
/// Not yet wired: `unix_socket_path`-style local-socket serving, TLS via
/// `ssl_certfile`/`ssl_keyfile` (would need `axum-server`'s TLS acceptor).
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
    /// Accumulated delta text per `(chat_id, stream_id)`, so `send_delta`
    /// can reconstruct the full message on `stream_end`. Mirrors nanobot's
    /// `self._stream_text_buffers` (`channels/websocket/runtime.py:1188-1198`).
    stream_buffers: StdMutex<HashMap<(String, String), Vec<String>>>,
    /// Accumulated reasoning-delta text per `(chat_id, stream_id)`, so
    /// `send_reasoning_end` can persist the fully assembled reasoning trace
    /// instead of a wire chunk. Mirrors nanobot's `self._reasoning_text_buffers`
    /// (`channels/websocket/runtime.py:1869-1870, 1900-1901`).
    reasoning_buffers: StdMutex<HashMap<(String, String), Vec<String>>>,
    /// Same `Arc` as `AgentLoop::runtime_resolver`, cloned into every
    /// [`WsShared`] snapshot — see [`Self::shared`].
    runtime_resolver: Arc<ModelRuntimeResolver>,
    pub(crate) default_agent_mode: AgentMode,
}

impl WebSocketChannel {
    pub fn new(
        config: WebSocketConfig,
        bus: Arc<MessageBus>,
        channels_config: ChannelsConfig,
        session_manager: Arc<StdMutex<SessionManager>>,
        workspace_request_handler: WorkspaceRequestHandler,
        runtime_resolver: Arc<ModelRuntimeResolver>,
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
            stream_buffers: StdMutex::new(HashMap::new()),
            reasoning_buffers: StdMutex::new(HashMap::new()),
            runtime_resolver,
            default_agent_mode: AgentMode::Standard,
        }
    }

    /// Fan a raw wire-protocol JSON string out to every connection currently
    /// subscribed to `chat_id`, cleaning up any connection whose sender is
    /// gone. Returns the number of connections it was actually sent to.
    /// Mirrors nanobot's `conns = list(self._subs.get(chat_id, ()))` +
    /// per-connection `_safe_send_to` loop, shared by `send`/`send_delta`/
    /// `send_reasoning_delta`/`send_reasoning_end`/`send_file_edit_events`.
    /// Push a `session_updated` frame carrying the freshest `token_usage`
    /// totals to every connection subscribed to `chat_id`, right after a
    /// turn finishes. A no-op when the session has no usage yet (nothing
    /// changed to report) or nobody is subscribed.
    ///
    /// The session is already saved by the time [`Self::send`] sees the
    /// `TurnEnd` — `persist_finished_turn` (`agent::agent_loop`) writes
    /// `token_usage` before publishing it — so this always reads the
    /// current totals rather than a stale snapshot.
    async fn fan_out_session_token_usage(&self, chat_id: &str) {
        let usage = {
            let session_manager = self
                .base
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            session_manager
                .get_session_internal(&get_session_id(chat_id))
                .and_then(|session| session.usage())
        };
        let Some(usage) = usage else {
            return;
        };
        let body = serde_json::json!({
            "event": WsOutboundEvent::SessionUpdated.as_str(),
            "chat_id": chat_id,
            "scope": "metadata",
            "token_usage": usage,
        });
        self.fan_out_to_chat(chat_id, &body.to_string()).await;
    }

    async fn fan_out_to_chat(&self, chat_id: &str, raw: &str) -> usize {
        let recipients = self.connections.lock().await.senders_for_chat(chat_id);
        let mut delivered = 0usize;
        for (connection_id, tx) in recipients {
            if tx.send(Message::text(raw.to_string())).is_ok() {
                delivered += 1;
            } else {
                log::warn!("WebSocket channel: connection '{connection_id}' gone, cleaning up");
                self.connections
                    .lock()
                    .await
                    .cleanup_connection(&connection_id);
            }
        }
        delivered
    }

    /// Build the common `{"event": "message", "chat_id", "text"}` payload
    /// plus the optional `media`/`reply_to`/`latency_ms`/`agent_ui` fields
    /// nanobot's `send()` includes whenever present, regardless of whether
    /// this is a plain message or a progress/tool-hint event. Mirrors
    /// `runtime.py:997-1029` (excluding the progress-only `kind`/`tool_events`
    /// fields, added by the caller for `ProgressEvent`s).
    fn build_message_payload(msg: &OutboundMessage) -> serde_json::Value {
        let mut payload = serde_json::json!({
            "event": "message",
            "chat_id": msg.chat_id,
            "text": msg.content,
        });
        if !msg.media.is_empty() {
            payload["media"] = serde_json::json!(msg.media);
        }
        if let Some(reply_to) = &msg.reply_to {
            payload["reply_to"] = serde_json::json!(reply_to);
        }
        let latency_ms = msg
            .metadata
            .get("latency_ms")
            .and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)));
        if let Some(latency_ms) = latency_ms {
            payload["latency_ms"] = serde_json::json!(latency_ms);
        }
        if let Some(agent_ui) = msg.metadata.get(OUTBOUND_META_AGENT_UI) {
            payload["agent_ui"] = agent_ui.clone();
        }
        payload
    }

    /// Snapshot of shared state for axum handlers. `pub(crate)` so the
    /// combined login+gateway server (`cli::commands::run_gateway`) can
    /// build its own router around [`Self::router`] without needing this
    /// channel to own the listener — see [`Self::start`]'s doc comment.
    pub(crate) fn shared(&self) -> WsShared {
        WsShared {
            name: self.name(),
            bus: Arc::clone(&self.base.bus),
            channels_config: self.channels_config.clone(),
            jwt: self.config.jwt.clone(),
            jwt_public_key_pem: self.jwt_public_key_pem.clone(),
            require_auth: self.config.require_auth,
            connections: Arc::clone(&self.connections),
            supports_streaming: BaseChannel::supports_streaming(self),
            session_manager: Arc::clone(&self.base.session_manager),
            workspace_request_handler: self.base.workspace_request_handler.clone(),
            runtime_surface: self.config.runtime_surface.clone(),
            gateway_services: Arc::clone(&self.gateway_services),
            media_root: get_media_dir(None),
            runtime_resolver: Arc::clone(&self.runtime_resolver),
            default_agent_mode: self.default_agent_mode,
        }
    }

    /// The `/ws`-style upgrade route, fully `with_state`'d (`Router<()>`) so
    /// it can be `.merge()`d directly into another server's router — see
    /// [`Self::start`]'s doc comment for why this channel doesn't bind its
    /// own listener when run via the combined gateway server.
    pub(crate) fn router(&self) -> Router {
        Router::new()
            .route(&self.config.path, get(ws_upgrade_handler))
            .route(
                "/v1/media/{*key}",
                get(crate::channels::websocket::webui::media::serve_media),
            )
            .with_state(self.shared())
    }

    /// Clone of the shutdown signal [`BaseChannel::stop`] fires, so an
    /// externally-owned `axum::serve(...)` (built around [`Self::router`])
    /// can wait on the same signal this channel's own `start()` does.
    pub(crate) fn shutdown_signal(&self) -> Arc<Notify> {
        Arc::clone(&self.shutdown)
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

    /// Registered with `ChannelManager` purely for *outbound* dispatch
    /// (`send`/`send_delta`/etc., routed by `ChannelManager::dispatch_outbound`
    /// looking this channel up by name) — it does **not** bind a listener or
    /// serve HTTP itself. Unlike every other `BaseChannel` implementor, the
    /// actual inbound HTTP/WS serving for this channel is owned externally:
    /// `cli::commands::run_gateway` builds one combined `axum` server (this
    /// channel's [`Self::router`] merged with the gateway's login route) and
    /// calls `axum::serve` exactly once, so login and live WS traffic share
    /// one port. This method just marks the channel running and waits for
    /// [`BaseChannel::stop`]'s shutdown signal, matching the "long-running
    /// task per channel" shape `ChannelManager::start_all`'s `JoinSet`
    /// expects from every registered channel.
    async fn start(&self) {
        if !self.config.enabled {
            return;
        }
        self.base.running.store(true, Ordering::Relaxed);
        self.shutdown.notified().await;
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
        // Mirrors nanobot's `send()` (`channels/websocket/runtime.py:946-1084`)
        // for the subset of event kinds actually reachable here:
        // `ChannelManager::send_once` already routes `_stream_delta`/
        // `_stream_end`-flagged messages to `send_delta` instead of `send`,
        // and reasoning-kind `Progress` events to `send_reasoning_delta`/
        // `send_reasoning_end` — so `send` only ever sees a plain `Progress`
        // event, a `TurnEnd`, or no typed event at all. Other `OutboundEvent`
        // variants fail safe with a logged skip rather than reusing the wrong
        // shape.
        let payload = match &msg.event {
            Some(OutboundEvent::TurnEnd(turn_end_event)) => {
                let turn_id = msg
                    .metadata
                    .get(WEBUI_TURN_METADATA_KEY)
                    .and_then(|v| v.as_str())
                    .filter(|t| !t.is_empty())
                    .map(str::to_string);
                // Persist the canonical turn boundary — this is also what
                // makes `append_transcript_object`'s rotate-on-`turn_end`
                // check (`webui/transcript.rs`) actually fire in production.
                if is_webui_metadata(&msg.metadata) {
                    let mut body: HashMap<String, serde_json::Value> = HashMap::from([
                        ("event".to_string(), serde_json::json!("turn_end")),
                        ("chat_id".to_string(), serde_json::json!(msg.chat_id)),
                    ]);
                    if let Some(turn_id) = &turn_id {
                        body.insert("turn_id".to_string(), serde_json::json!(turn_id));
                    }
                    if let Some(latency_ms) = turn_end_event.latency_ms {
                        body.insert("latency_ms".to_string(), serde_json::json!(latency_ms));
                    }
                    if let Some(goal_state) = &turn_end_event.goal_state {
                        body.insert("goal_state".to_string(), serde_json::json!(goal_state));
                    }
                    let mut transcripts = self
                        .gateway_services
                        .transcripts
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    transcripts.append_turn_event(&msg.chat_id, body, &msg.metadata, "complete");
                }
                let owner = msg
                    .metadata
                    .get(WEBSOCKET_TURN_OWNER_METADATA_KEY)
                    .and_then(|v| v.as_str());
                {
                    let mut turn_registry = self
                        .gateway_services
                        .turn_registry
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    if let Some(owner) = owner.filter(|o| !o.is_empty()) {
                        turn_registry.clear_turn_if_current(&msg.chat_id, Some(owner), false);
                    } else {
                        turn_registry.clear_chat(&msg.chat_id);
                    }
                }
                let shared = self.shared();
                send_goal_status(&msg.chat_id, "idle", None, turn_id, &shared).await;
                self.fan_out_session_token_usage(&msg.chat_id).await;
                return Ok(());
            }
            Some(OutboundEvent::Progress(progress_event)) => {
                if let Some(edits) = progress_event
                    .file_edit_events
                    .clone()
                    .filter(|edits| !edits.is_empty())
                {
                    return self
                        .send_file_edit_events(&msg.chat_id, edits, Some(msg.metadata.clone()))
                        .await;
                }
                let mut payload = Self::build_message_payload(&msg);
                if let Some(tool_events) = progress_event
                    .tool_events
                    .as_ref()
                    .filter(|events| !events.is_empty())
                {
                    payload["tool_events"] =
                        serde_json::to_value(tool_events).unwrap_or(serde_json::Value::Null);
                }
                payload["kind"] =
                    serde_json::json!(if progress_event.kind == ProgressKind::ToolHint {
                        "tool_hint"
                    } else {
                        "progress"
                    });
                payload
            }
            None => Self::build_message_payload(&msg),
            Some(other) => {
                log::warn!(
                    "WebSocket channel: no wire mapping yet for {other:?} event \
                     (chat_id '{}'); skipping",
                    msg.chat_id
                );
                return Ok(());
            }
        };

        // Persist before fan-out (matches nanobot's ordering) so a durable
        // record exists even if delivery to live connections fails. Mirrors
        // the `phase = "activity" if payload.get("kind") in (...) else
        // "answer"` split in `send()` (`channels/websocket/runtime.py:1830`).
        if is_webui_metadata(&msg.metadata) {
            let phase = match payload.get("kind").and_then(serde_json::Value::as_str) {
                Some("tool_hint") | Some("progress") => "activity",
                _ => "answer",
            };
            let mut transcripts = self
                .gateway_services
                .transcripts
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            transcripts.append_turn_event(
                &msg.chat_id,
                json_object_to_map(&payload),
                &msg.metadata,
                phase,
            );
        }

        let raw = payload.to_string();
        let delivered = self.fan_out_to_chat(&msg.chat_id, &raw).await;
        // Only fail when nobody received it — a partial failure must not
        // trigger a retry that would re-deliver duplicate content to
        // recipients that already succeeded.
        if delivered == 0 {
            return Err(format!(
                "No open WebSocket connection for chat_id '{}'",
                msg.chat_id
            ));
        }
        Ok(())
    }

    async fn send_delta(
        &self,
        chat_id: &str,
        delta: &str,
        metadata: Option<HashMap<String, serde_json::Value>>,
    ) -> Result<(), String> {
        // Mirrors nanobot's `send_delta` (`channels/websocket/runtime.py:1174-1225`).
        // The trait signature carries `stream_id`/`stream_end`/`resuming`/
        // `merge_next` inside `metadata` rather than as separate parameters
        // (matching `ChannelManager::send_once`, which passes the whole
        // metadata map through unchanged) — read them out under the same
        // `_stream_id`/`_stream_end`/`_resuming`/`_merge_next` keys
        // `agent_loop.rs`'s stream callbacks set.
        let meta = metadata.unwrap_or_default();
        let stream_id = meta
            .get("_stream_id")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let stream_end = meta
            .get("_stream_end")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let resuming = meta
            .get("_resuming")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let merge_next = meta
            .get("_merge_next")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let stream_key = (chat_id.to_string(), stream_id.clone().unwrap_or_default());

        let mut completed_text: Option<String> = None;
        let mut payload = if stream_end {
            // `merge_next` peeks (keeps the buffer alive for a following
            // segment); otherwise the buffer is drained for this stream.
            let full_text = {
                let mut buffers = self
                    .stream_buffers
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                if merge_next {
                    let buffer = buffers.entry(stream_key.clone()).or_default();
                    if !delta.is_empty() {
                        buffer.push(delta.to_string());
                    }
                    buffer.join("")
                } else {
                    let mut buffer = buffers.remove(&stream_key).unwrap_or_default();
                    if !delta.is_empty() {
                        buffer.push(delta.to_string());
                    }
                    buffer.join("")
                }
            };
            let mut payload = serde_json::json!({"event": "stream_end", "chat_id": chat_id});
            // Always echo the assembled buffer when we have one, even if this
            // end frame's own `delta` is empty (the usual case: every token
            // already went out as a `delta` event). The client treats `text`
            // as the authoritative replacement for the live bubble, so a
            // doubled in-memory append still snaps back at stream_end.
            // Incremental `delta` frames are unchanged — this is not a
            // substitute for token streaming.
            if !full_text.is_empty() {
                payload["text"] = serde_json::json!(full_text);
            }
            completed_text = Some(full_text);
            payload
        } else {
            self.stream_buffers
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .entry(stream_key)
                .or_default()
                .push(delta.to_string());
            serde_json::json!({"event": "delta", "chat_id": chat_id, "text": delta})
        };
        if let Some(stream_id) = &stream_id {
            payload["stream_id"] = serde_json::json!(stream_id);
        }
        if stream_end && resuming {
            payload["resuming"] = serde_json::json!(true);
        }
        if stream_end && merge_next {
            payload["merge_next"] = serde_json::json!(true);
        }

        // Persist only the completed reply, never a wire chunk — same
        // "canonical end of a live stream" rule as `send_reasoning_end`.
        // Written as a plain `event: "message"` record (not the wire
        // `"stream_end"` shape) so streamed and non-streamed replies land
        // identically for `is_assistant_transcript_row`/`transcript_chat_history`
        // (`webui/transcript.rs`), which only recognize `"message"` rows.
        if let Some(text) = completed_text.filter(|t| !t.is_empty())
            && is_webui_metadata(&meta)
        {
            let mut event: HashMap<String, serde_json::Value> = HashMap::from([
                ("event".to_string(), serde_json::json!("message")),
                ("chat_id".to_string(), serde_json::json!(chat_id)),
                ("text".to_string(), serde_json::json!(text)),
            ]);
            if let Some(turn_id) = meta
                .get(WEBUI_TURN_METADATA_KEY)
                .and_then(|v| v.as_str())
                .filter(|t| !t.is_empty())
            {
                event.insert("turn_id".to_string(), serde_json::json!(turn_id));
            }
            let mut transcripts = self
                .gateway_services
                .transcripts
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            transcripts.append_turn_event(chat_id, event, &meta, "answer");
        }

        self.fan_out_to_chat(chat_id, &payload.to_string()).await;
        Ok(())
    }

    fn implements_send_delta(&self) -> bool {
        true
    }

    async fn send_reasoning_delta(
        &self,
        chat_id: &str,
        delta: &str,
        metadata: Option<HashMap<String, serde_json::Value>>,
    ) -> Result<(), String> {
        // Mirrors `send_reasoning_delta` (`channels/websocket/runtime.py:1086-1120`).
        if delta.is_empty() {
            return Ok(());
        }
        let meta = metadata.unwrap_or_default();
        let stream_id = meta.get("_stream_id").and_then(|v| v.as_str());
        let mut payload = serde_json::json!({
            "event": "reasoning_delta",
            "chat_id": chat_id,
            "text": delta,
        });
        if let Some(stream_id) = stream_id {
            payload["stream_id"] = serde_json::json!(stream_id);
        }
        // Buffer only — never persisted as a chunk. `send_reasoning_end`
        // pops and joins this to durably record the completed trace.
        let stream_key = (
            chat_id.to_string(),
            stream_id.unwrap_or_default().to_string(),
        );
        self.reasoning_buffers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(stream_key)
            .or_default()
            .push(delta.to_string());
        self.fan_out_to_chat(chat_id, &payload.to_string()).await;
        Ok(())
    }

    async fn send_reasoning_end(
        &self,
        chat_id: &str,
        metadata: Option<HashMap<String, serde_json::Value>>,
    ) -> Result<(), String> {
        // Mirrors `send_reasoning_end` (`channels/websocket/runtime.py:1122-1148`).
        let meta = metadata.unwrap_or_default();
        let stream_id = meta.get("_stream_id").and_then(|v| v.as_str());
        let mut payload = serde_json::json!({"event": "reasoning_end", "chat_id": chat_id});
        if let Some(stream_id) = stream_id {
            payload["stream_id"] = serde_json::json!(stream_id);
        }
        let stream_key = (
            chat_id.to_string(),
            stream_id.unwrap_or_default().to_string(),
        );
        let reasoning_text = self
            .reasoning_buffers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&stream_key)
            .unwrap_or_default()
            .join("");
        if !reasoning_text.is_empty() && is_webui_metadata(&meta) {
            let event: HashMap<String, serde_json::Value> = HashMap::from([
                ("event".to_string(), serde_json::json!("reasoning_end")),
                ("chat_id".to_string(), serde_json::json!(chat_id)),
            ]);
            let mut transcripts = self
                .gateway_services
                .transcripts
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            transcripts.append_stream_event(
                chat_id,
                event,
                Some(&reasoning_text),
                &meta,
                "reasoning",
            );
        }
        self.fan_out_to_chat(chat_id, &payload.to_string()).await;
        Ok(())
    }

    async fn send_file_edit_events(
        &self,
        chat_id: &str,
        edits: Vec<FileEditEvent>,
        metadata: Option<HashMap<String, serde_json::Value>>,
    ) -> Result<(), String> {
        // Mirrors `send_file_edit_events` (`channels/websocket/runtime.py:1150-1172`).
        let payload = serde_json::json!({
            "event": "file_edit",
            "chat_id": chat_id,
            "edits": edits,
        });
        let meta = metadata.unwrap_or_default();
        if is_webui_metadata(&meta) {
            let mut transcripts = self
                .gateway_services
                .transcripts
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            transcripts.append_turn_event(chat_id, json_object_to_map(&payload), &meta, "activity");
        }
        self.fan_out_to_chat(chat_id, &payload.to_string()).await;
        Ok(())
    }
}

/// Mirror Python's `metadata.get("webui") is True` — only a JSON boolean
/// `true` counts. Used to gate every outbound WebUI-transcript write the
/// same way `handle_envelope_message` gates the inbound one (`is_webui`),
/// so a chat's transcript never ends up with assistant rows and no matching
/// user rows, or vice versa.
fn is_webui_metadata(metadata: &HashMap<String, serde_json::Value>) -> bool {
    metadata.get("webui").and_then(serde_json::Value::as_bool) == Some(true)
}

/// Convert a `serde_json::Value` object into the `HashMap` shape
/// [`WebUiTranscriptRecorder`](crate::channels::websocket::webui::transcript::WebUiTranscriptRecorder)'s
/// append methods take. Non-object input (shouldn't happen for any wire
/// payload built in this module) becomes an empty map.
fn json_object_to_map(value: &serde_json::Value) -> HashMap<String, serde_json::Value> {
    value
        .as_object()
        .map(|obj| obj.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default()
}

/// Return whether WebUI ingress should expect a normal agent lifecycle.
fn builtin_command_starts_agent_turn(text: &str) -> bool {
    let normalized = normalize_command_text(text);
    let (command, separator, args) = match normalized.split_once(' ') {
        Some((before, after)) => (before, " ", after),
        None => (normalized.as_str(), "", ""),
    };
    let command_name = command.strip_prefix('/').unwrap_or(command).to_lowercase();
    let spec = ChatCommand::from_str(&command_name).ok();
    if spec.is_none() || (!separator.is_empty() && !spec.unwrap().accepts_args()) {
        return true;
    }
    match spec.unwrap().lifecycle() {
        Some(CommandLifecycle::AgentTurn) => true,
        Some(CommandLifecycle::AgentTurnWithArgs) => !args.trim().is_empty(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::outbound_events::ProgressEvent;
    use crate::config::schema::JwtConfig;
    use crate::providers::base::LLMUsage;

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

    // --- builtin_command_starts_agent_turn ---

    #[test]
    fn builtin_command_starts_agent_turn_unknown_command_is_true() {
        assert!(builtin_command_starts_agent_turn("/nope"));
    }

    #[test]
    fn builtin_command_starts_agent_turn_plain_text_is_true() {
        assert!(builtin_command_starts_agent_turn("hello there"));
    }

    #[test]
    fn builtin_command_starts_agent_turn_matches_regardless_of_case() {
        // Mirrors nanobot's `command.lower()` comparison
        // (`nanobot/command/builtin.py:192`).
        assert!(!builtin_command_starts_agent_turn("/STOP"));
    }

    #[test]
    fn builtin_command_starts_agent_turn_args_on_no_args_command_is_true() {
        // `/stop` doesn't accept args, so trailing args make it fall back to
        // the default agent-turn lifecycle.
        assert!(builtin_command_starts_agent_turn("/stop now"));
    }

    #[test]
    fn builtin_command_starts_agent_turn_side_channel_command_is_false() {
        assert!(!builtin_command_starts_agent_turn("/stop"));
    }

    #[test]
    fn builtin_command_starts_agent_turn_with_args_and_no_args_is_false() {
        // `/goal` alone (no goal text) is side-channel usage.
        assert!(!builtin_command_starts_agent_turn("/goal"));
    }

    #[test]
    fn builtin_command_starts_agent_turn_with_args_and_blank_args_is_false() {
        assert!(!builtin_command_starts_agent_turn("/goal    "));
    }

    #[test]
    fn builtin_command_starts_agent_turn_with_args_and_args_is_true() {
        assert!(builtin_command_starts_agent_turn("/goal ship the feature"));
    }

    // --- normalize_cli_app_mentions ---

    #[test]
    fn normalize_cli_app_mentions_returns_empty_for_non_list_input() {
        assert_eq!(normalize_cli_app_mentions(None), vec![]);
        let not_a_list = serde_json::json!({"name": "codex"});
        assert_eq!(normalize_cli_app_mentions(Some(&not_a_list)), vec![]);
    }

    #[test]
    fn normalize_cli_app_mentions_skips_non_object_items() {
        let raw = serde_json::json!(["not-an-object", 42, ["nested"]]);
        assert_eq!(normalize_cli_app_mentions(Some(&raw)), vec![]);
    }

    #[test]
    fn normalize_cli_app_mentions_rejects_missing_or_malformed_names() {
        let raw = serde_json::json!([
            {"display_name": "No Name"},
            {"name": ""},
            {"name": "   "},
            {"name": "-leading-hyphen"},
            {"name": "has space"},
        ]);
        assert_eq!(normalize_cli_app_mentions(Some(&raw)), vec![]);
    }

    #[test]
    fn normalize_cli_app_mentions_keeps_a_valid_entry_with_lowercased_name() {
        let raw = serde_json::json!([{"name": "Codex", "category": "  agent  "}]);
        let out = normalize_cli_app_mentions(Some(&raw));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].get("name"), Some(&"codex".to_string()));
        assert_eq!(out[0].get("category"), Some(&"agent".to_string()));
    }

    #[test]
    fn normalize_cli_app_mentions_dedupes_case_insensitively() {
        let raw = serde_json::json!([{"name": "Codex"}, {"name": "codex"}, {"name": "CODEX"}]);
        assert_eq!(normalize_cli_app_mentions(Some(&raw)).len(), 1);
    }

    #[test]
    fn normalize_cli_app_mentions_caps_at_eight_entries() {
        let items: Vec<serde_json::Value> = (0..12)
            .map(|i| serde_json::json!({"name": format!("app{i}")}))
            .collect();
        let raw = serde_json::Value::Array(items);
        assert_eq!(normalize_cli_app_mentions(Some(&raw)).len(), 8);
    }

    #[test]
    fn normalize_cli_app_mentions_ignores_non_string_attribute_values() {
        let raw = serde_json::json!([{"name": "codex", "brand_color": 12345}]);
        let out = normalize_cli_app_mentions(Some(&raw));
        assert_eq!(out.len(), 1);
        assert!(!out[0].contains_key("brand_color"));
    }

    #[test]
    fn normalize_cli_app_mentions_clips_attribute_lengths_per_field() {
        let raw = serde_json::json!([{
            "name": "codex",
            "category": "y".repeat(200),
            "logo_url": "z".repeat(600),
        }]);
        let out = normalize_cli_app_mentions(Some(&raw));
        assert_eq!(out[0].get("category").unwrap().len(), 160);
        assert_eq!(out[0].get("logo_url").unwrap().len(), 512);
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

    #[test]
    fn ready_event_advertises_streaming() {
        let body = ready_event("chat-1", "client-1", true);
        assert_eq!(body["event"], "ready");
        assert_eq!(body["chat_id"], "chat-1");
        assert_eq!(body["client_id"], "client-1");
        assert_eq!(body["streaming"], true);

        let body = ready_event("chat-1", "client-1", false);
        assert_eq!(body["streaming"], false);
    }

    // --- workspace_controls_available ---

    fn test_shared(runtime_surface: &str) -> WsShared {
        let dir = tempfile::tempdir().unwrap();
        let bus = MessageBus::new();
        WsShared {
            name: "websocket",
            bus: Arc::new(bus),
            channels_config: ChannelsConfig::default(),
            jwt: JwtConfig::default(),
            jwt_public_key_pem: None,
            require_auth: true,
            connections: Arc::new(AsyncMutex::new(ConnectionRegistry::default())),
            supports_streaming: false,
            session_manager: Arc::new(StdMutex::new(SessionManager::new(dir.keep()))),
            workspace_request_handler: WorkspaceRequestHandler::new(
                tempfile::tempdir().unwrap().keep(),
                true,
            ),
            runtime_surface: runtime_surface.to_string(),
            gateway_services: Arc::new(GatewayServices::new(tempfile::tempdir().unwrap().keep())),
            media_root: tempfile::tempdir().unwrap().keep(),
            runtime_resolver: ModelRuntimeResolver::for_tests(),
            default_agent_mode: AgentMode::Standard,
        }
    }

    fn addr(ip: &str) -> SocketAddr {
        SocketAddr::new(ip.parse().unwrap(), 12345)
    }

    // --- authorize ---

    /// Build a `WsShared` with JWT enabled against a fresh keypair, returning
    /// the shared config plus the private key path so each test can mint
    /// whatever token shape it needs.
    fn shared_with_jwt_enabled() -> (WsShared, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let keys = crate::security::jwt::generate_jwt_keypair(dir.keep(), false).unwrap();
        let mut shared = test_shared("browser");
        shared.jwt = JwtConfig {
            enabled: true,
            iss: "rust-bot".to_string(),
            aud: String::new(),
            ..JwtConfig::default()
        };
        shared.jwt_public_key_pem = Some(Arc::new(std::fs::read(&keys.public_key_path).unwrap()));
        (shared, keys.private_key_path)
    }

    fn mint_token_with_purpose(
        private_key_path: &std::path::Path,
        purpose: Option<&str>,
    ) -> String {
        let private_pem = std::fs::read(private_key_path).unwrap();
        let now = chrono::Utc::now().timestamp();
        let claims = crate::security::jwt::Claims {
            iss: "rust-bot".to_string(),
            sub: Uuid::new_v4().to_string(),
            aud: None,
            exp: now + 3600,
            iat: now,
            purpose: purpose.map(str::to_string),
        };
        let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::EdDSA);
        let encoding_key = jsonwebtoken::EncodingKey::from_ed_pem(&private_pem).unwrap();
        jsonwebtoken::encode(&header, &claims, &encoding_key).unwrap()
    }

    #[test]
    fn authorize_false_when_jwt_disabled() {
        let shared = test_shared("browser");
        assert_eq!(authorize(&shared, None), Ok(false));
    }

    #[test]
    fn authorize_true_for_webui_purpose_token() {
        let (shared, private_key_path) = shared_with_jwt_enabled();
        let token = mint_token_with_purpose(&private_key_path, Some(WEBUI_JWT_PURPOSE));
        assert_eq!(authorize(&shared, Some(&token)), Ok(true));
    }

    #[test]
    fn authorize_false_for_token_without_purpose() {
        let (shared, private_key_path) = shared_with_jwt_enabled();
        let token = mint_token_with_purpose(&private_key_path, None);
        assert_eq!(authorize(&shared, Some(&token)), Ok(false));
    }

    #[test]
    fn authorize_false_for_token_with_different_purpose() {
        let (shared, private_key_path) = shared_with_jwt_enabled();
        let token = mint_token_with_purpose(&private_key_path, Some("client"));
        assert_eq!(authorize(&shared, Some(&token)), Ok(false));
    }

    #[test]
    fn authorize_rejects_missing_token_when_jwt_enabled() {
        let (shared, _private_key_path) = shared_with_jwt_enabled();
        assert_eq!(authorize(&shared, None), Err(StatusCode::UNAUTHORIZED));
    }

    #[test]
    fn authorize_rejects_invalid_token() {
        let (shared, _private_key_path) = shared_with_jwt_enabled();
        assert_eq!(
            authorize(&shared, Some("not-a-real-token")),
            Err(StatusCode::UNAUTHORIZED)
        );
    }

    #[test]
    fn authorize_allows_missing_token_when_auth_not_required() {
        let (mut shared, _private_key_path) = shared_with_jwt_enabled();
        shared.require_auth = false;
        assert_eq!(authorize(&shared, None), Ok(false));
    }

    #[test]
    fn authorize_still_rejects_invalid_token_when_auth_not_required() {
        let (mut shared, _private_key_path) = shared_with_jwt_enabled();
        shared.require_auth = false;
        assert_eq!(
            authorize(&shared, Some("not-a-real-token")),
            Err(StatusCode::UNAUTHORIZED)
        );
    }

    #[test]
    fn authorize_still_returns_true_for_valid_webui_token_when_auth_not_required() {
        let (mut shared, private_key_path) = shared_with_jwt_enabled();
        shared.require_auth = false;
        let token = mint_token_with_purpose(&private_key_path, Some(WEBUI_JWT_PURPOSE));
        assert_eq!(authorize(&shared, Some(&token)), Ok(true));
    }

    // --- webui_quote_allowed ---

    #[test]
    fn webui_quote_allowed_requires_both_flags() {
        assert!(!webui_quote_allowed(false, true));
        assert!(!webui_quote_allowed(true, false));
        assert!(!webui_quote_allowed(false, false));
        assert!(webui_quote_allowed(true, true));
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

    // --- send_goal_state / maybe_push_active_goal_state ---

    #[tokio::test]
    async fn send_goal_state_delivers_to_subscribed_connections() {
        let shared = test_shared("browser");
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);

        send_goal_state(
            "chat-1",
            serde_json::json!({"active": true, "objective": "ship it"}),
            &shared,
        )
        .await;

        let msg = rx.try_recv().expect("expected a delivered frame");
        let text = msg.into_text().unwrap();
        let body: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(body["event"], "goal_state");
        assert_eq!(body["chat_id"], "chat-1");
        assert_eq!(body["goal_state"]["objective"], "ship it");
    }

    #[tokio::test]
    async fn send_goal_state_noop_when_no_subscribers() {
        let shared = test_shared("browser");
        // Must not panic even though nothing is subscribed to "chat-1".
        send_goal_state("chat-1", serde_json::json!({"active": true}), &shared).await;
    }

    #[tokio::test]
    async fn send_goal_state_cleans_up_a_connection_that_is_gone() {
        let shared = test_shared("browser");
        let (tx, rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);
        drop(rx); // simulate the connection's writer task having already exited

        send_goal_state("chat-1", serde_json::json!({"active": true}), &shared).await;

        assert!(
            shared
                .connections
                .lock()
                .await
                .sender_for("conn-1")
                .is_none()
        );
    }

    // --- send_user_turn / resolve_media_urls ---

    #[tokio::test]
    async fn send_user_turn_delivers_to_every_subscribed_connection() {
        let shared = test_shared("browser");
        let (tx1, mut rx1) = mpsc::unbounded_channel::<Message>();
        let (tx2, mut rx2) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx1);
        shared.connections.lock().await.attach("conn-2", "chat-1");
        shared
            .connections
            .lock()
            .await
            .register("conn-2", "chat-1", tx2);

        send_user_turn("chat-1", "turn-1", "hello there", &[], &shared).await;

        for rx in [&mut rx1, &mut rx2] {
            let body = recv_json(rx);
            assert_eq!(body["event"], "user");
            assert_eq!(body["chat_id"], "chat-1");
            assert_eq!(body["turn_id"], "turn-1");
            assert_eq!(body["text"], "hello there");
            assert!(
                body.get("media").is_none(),
                "media must be omitted, not an empty array, when there's no attachment: {body}"
            );
        }
    }

    #[tokio::test]
    async fn send_user_turn_includes_media_when_present() {
        let shared = test_shared("browser");
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);

        send_user_turn(
            "chat-1",
            "turn-1",
            "look at this",
            &["/v1/media/websocket/abc.png".to_string()],
            &shared,
        )
        .await;

        let body = recv_json(&mut rx);
        assert_eq!(
            body["media"],
            serde_json::json!(["/v1/media/websocket/abc.png"])
        );
    }

    #[tokio::test]
    async fn send_user_turn_noop_when_no_subscribers() {
        let shared = test_shared("browser");
        // Must not panic even though nothing is subscribed to "chat-1".
        send_user_turn("chat-1", "turn-1", "hello", &[], &shared).await;
    }

    #[tokio::test]
    async fn send_user_turn_cleans_up_a_connection_that_is_gone() {
        let shared = test_shared("browser");
        let (tx, rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);
        drop(rx); // simulate the connection's writer task having already exited

        send_user_turn("chat-1", "turn-1", "hello", &[], &shared).await;

        assert!(
            shared
                .connections
                .lock()
                .await
                .sender_for("conn-1")
                .is_none()
        );
    }

    #[test]
    fn resolve_media_urls_converts_a_stored_disk_path_to_a_browser_url() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("websocket");
        std::fs::create_dir_all(&sub).unwrap();
        let image_path = sub.join("abc.png");
        std::fs::write(&image_path, b"fake-png").unwrap();

        let resolved = resolve_media_urls(&[image_path.display().to_string()], dir.path());

        assert_eq!(resolved, vec!["/v1/media/websocket/abc.png".to_string()]);
    }

    #[test]
    fn resolve_media_urls_drops_a_path_that_no_longer_exists() {
        let dir = tempfile::tempdir().unwrap();
        let resolved = resolve_media_urls(
            &[dir.path().join("gone.png").display().to_string()],
            dir.path(),
        );
        assert!(resolved.is_empty());
    }

    #[test]
    fn resolve_media_urls_empty_input_is_empty_output() {
        let dir = tempfile::tempdir().unwrap();
        assert!(resolve_media_urls(&[], dir.path()).is_empty());
    }

    #[tokio::test]
    async fn workspace_scope_or_error_returns_scope_on_success() {
        let shared = test_shared("browser");
        let dir = tempfile::tempdir().unwrap();
        let scope = crate::security::build_workspace_scope(
            dir.path(),
            crate::security::WorkspaceAccessMode::Full,
            None,
        );
        let resolver: ScopeResolver = {
            let scope = scope.clone();
            Arc::new(move || {
                let scope = scope.clone();
                Box::pin(async move { Ok(scope) })
            })
        };

        let resolved =
            workspace_scope_or_error(&shared, Some("chat-1"), Some("turn-1"), "conn-1", resolver)
                .await;

        assert_eq!(resolved, Some(scope));
    }

    #[tokio::test]
    async fn workspace_scope_or_error_sends_rejection_detail_and_reason_on_failure() {
        let shared = test_shared("browser");
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);
        let resolver: ScopeResolver = Arc::new(|| {
            Box::pin(async { Err(WorkspaceScopeError::new(403, "workspace escalation denied")) })
        });

        let resolved =
            workspace_scope_or_error(&shared, Some("chat-1"), Some("turn-1"), "conn-1", resolver)
                .await;

        assert!(resolved.is_none());
        let msg = rx.try_recv().expect("expected a rejection frame");
        let body: serde_json::Value = serde_json::from_str(&msg.into_text().unwrap()).unwrap();
        assert_eq!(body["event"], "error");
        assert_eq!(body["chat_id"], "chat-1");
        assert_eq!(body["turn_id"], "turn-1");
        assert_eq!(body["detail"], "workspace_scope_rejected");
        assert_eq!(body["reason"], "workspace escalation denied");
    }

    #[tokio::test]
    async fn workspace_scope_or_error_omits_chat_id_when_cid_is_none() {
        // Mirrors nanobot's `new_chat` handler, which calls
        // `_workspace_scope_or_error` without a `chat_id` at all: a rejected
        // new-chat scope was never attached to any chat id, so the field
        // must be absent from the error payload, not merely empty.
        let shared = test_shared("browser");
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);
        let resolver: ScopeResolver = Arc::new(|| {
            Box::pin(async { Err(WorkspaceScopeError::new(403, "workspace escalation denied")) })
        });

        let resolved = workspace_scope_or_error(&shared, None, None, "conn-1", resolver).await;

        assert!(resolved.is_none());
        let msg = rx.try_recv().expect("expected a rejection frame");
        let body: serde_json::Value = serde_json::from_str(&msg.into_text().unwrap()).unwrap();
        assert_eq!(body["event"], "error");
        assert!(
            body.get("chat_id").is_none(),
            "chat_id must be omitted, not just null: {body}"
        );
        assert!(body.get("turn_id").is_none());
        assert_eq!(body["detail"], "workspace_scope_rejected");
    }

    #[tokio::test]
    async fn handle_envelope_new_chat_attaches_and_sends_session_updated_with_workspace_scope() {
        // Regression guard for the un-scoped `session_manager` `MutexGuard`
        // bug: held across `.await`s, it made the connection future `!Send`
        // (a compile error) and would have self-deadlocked at runtime, since
        // `hydrate_after_subscribe` re-locks the same mutex on the same call
        // chain. Wrapping in `tokio::time::timeout` turns a reintroduced
        // deadlock into a clean test failure instead of a hung test run.
        let shared = test_shared("browser");
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "initial-chat", tx);

        let mut envelope: Envelope = HashMap::new();
        envelope.insert("type".to_string(), serde_json::json!("new_chat"));
        let ctx = EnvelopeDispatchContext {
            envelope: &envelope,
            connection_id: "conn-1",
            client_id: "client-1",
            shared: &shared,
            remote_addr: addr("127.0.0.1"),
            webui_authenticated: false,
        };

        tokio::time::timeout(std::time::Duration::from_secs(5), dispatch_envelope(ctx))
            .await
            .expect("handle_envelope_new_chat must not hang");

        let attached = rx.try_recv().expect("expected an attached frame");
        let attached_body: serde_json::Value =
            serde_json::from_str(&attached.into_text().unwrap()).unwrap();
        assert_eq!(attached_body["event"], "attached");
        let new_chat_id = attached_body["chat_id"]
            .as_str()
            .expect("attached frame should carry the new chat_id")
            .to_string();
        assert!(!new_chat_id.is_empty());
        assert_eq!(
            attached_body["model_preset"], "default",
            "new_chat's attached frame must report the process-wide default \
             preset for a session with no override: {attached_body}"
        );
        assert_eq!(
            attached_body["model_presets"],
            serde_json::json!(shared.runtime_resolver.available_preset_names())
        );

        let session_updated = rx.try_recv().expect("expected a session_updated frame");
        let session_updated_body: serde_json::Value =
            serde_json::from_str(&session_updated.into_text().unwrap()).unwrap();
        assert_eq!(session_updated_body["event"], "session_updated");
        assert_eq!(session_updated_body["chat_id"], new_chat_id);
        assert_eq!(session_updated_body["scope"], "metadata");
        assert!(
            session_updated_body.get("workspace_scope").is_some(),
            "session_updated must carry the new chat's workspace scope: {session_updated_body}"
        );

        let session_manager = shared
            .session_manager
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let session = session_manager
            .get_session_internal(&get_session_id(&new_chat_id))
            .expect("new_chat must persist a session");
        assert_eq!(
            session.metadata.get(SESSION_WEBSOCKET_OWNER_CLIENT_ID_KEY),
            Some(&serde_json::json!("client-1")),
            "new_chat must stamp the requesting connection's client_id as owner"
        );
    }

    // --- handle_envelope_attach ---

    fn recv_json(rx: &mut mpsc::UnboundedReceiver<Message>) -> serde_json::Value {
        let msg = rx.try_recv().expect("expected a delivered frame");
        serde_json::from_str(&msg.into_text().unwrap()).unwrap()
    }

    async fn dispatch_attach(
        shared: &WsShared,
        connection_id: &str,
        chat_id: Option<serde_json::Value>,
    ) {
        let mut envelope: Envelope = HashMap::new();
        envelope.insert("type".to_string(), serde_json::json!("attach"));
        if let Some(chat_id) = chat_id {
            envelope.insert("chat_id".to_string(), chat_id);
        }
        let ctx = EnvelopeDispatchContext {
            envelope: &envelope,
            connection_id,
            client_id: "client-1",
            shared,
            remote_addr: addr("127.0.0.1"),
            webui_authenticated: false,
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), dispatch_envelope(ctx))
            .await
            .expect("handle_envelope_attach must not hang");
    }

    #[tokio::test]
    async fn handle_envelope_attach_subscribes_and_sends_attached() {
        let shared = test_shared("browser");
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "initial-chat", tx);

        dispatch_attach(&shared, "conn-1", Some(serde_json::json!("existing-chat"))).await;

        let body = recv_json(&mut rx);
        assert_eq!(body["event"], "attached");
        assert_eq!(body["chat_id"], "existing-chat");
        assert_eq!(
            body["history"],
            serde_json::json!([]),
            "a chat with no session file must still carry an empty history array"
        );
        if let Ok(extra) = rx.try_recv() {
            panic!(
                "attach must not send session_updated (that's new_chat-only), got {}",
                extra.into_text().unwrap()
            );
        }

        let recipients = shared
            .connections
            .lock()
            .await
            .senders_for_chat("existing-chat");
        assert_eq!(recipients.len(), 1);
        assert_eq!(recipients[0].0, "conn-1");
        // Multiplex: attach is additive, so the connection's original
        // subscription stays in place (nanobot's `_attach` is a set-add).
        assert_eq!(
            shared
                .connections
                .lock()
                .await
                .senders_for_chat("initial-chat")
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn handle_envelope_attach_reports_default_preset_for_unpersisted_chat() {
        let shared = test_shared("browser");
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "initial-chat", tx);

        dispatch_attach(
            &shared,
            "conn-1",
            Some(serde_json::json!("never-persisted")),
        )
        .await;

        let body = recv_json(&mut rx);
        assert_eq!(body["event"], "attached");
        assert_eq!(body["model_preset"], "default");
        assert_eq!(
            body["model_presets"],
            serde_json::json!(shared.runtime_resolver.available_preset_names())
        );
        assert!(
            body["model_presets"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v == "default"),
            "catalog must include the reserved default preset: {body}"
        );
    }

    #[tokio::test]
    async fn handle_envelope_attach_reports_session_preset_override() {
        let mut shared = test_shared("browser");
        shared.runtime_resolver = test_runtime_resolver_with_preset("fast", "claude-haiku");
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "initial-chat", tx);
        {
            let mut session_manager = shared
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let mut session = Session::new("websocket:existing-chat".to_string());
            session.metadata.insert(
                SESSION_MODEL_PRESET_METADATA_KEY.to_string(),
                serde_json::json!("fast"),
            );
            session_manager.save(session).unwrap();
        }

        dispatch_attach(&shared, "conn-1", Some(serde_json::json!("existing-chat"))).await;

        let body = recv_json(&mut rx);
        assert_eq!(body["event"], "attached");
        assert_eq!(body["model_preset"], "fast");
        assert_eq!(body["model"], "claude-haiku");
    }

    #[tokio::test]
    async fn handle_envelope_attach_includes_token_usage_when_session_has_it() {
        let shared = test_shared("browser");
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "initial-chat", tx);
        {
            let mut session_manager = shared
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let mut session = Session::new("websocket:existing-chat".to_string());
            session.update_usage(LLMUsage {
                input_tokens: Some(120),
                output_tokens: Some(45),
                ..LLMUsage::new()
            });
            session_manager.save(session).unwrap();
        }

        dispatch_attach(&shared, "conn-1", Some(serde_json::json!("existing-chat"))).await;

        let body = recv_json(&mut rx);
        assert_eq!(body["event"], "attached");
        assert_eq!(body["token_usage"]["input_tokens"], 120);
        assert_eq!(body["token_usage"]["output_tokens"], 45);
    }

    #[tokio::test]
    async fn handle_envelope_attach_omits_token_usage_when_session_has_none() {
        let shared = test_shared("browser");
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "initial-chat", tx);

        dispatch_attach(
            &shared,
            "conn-1",
            Some(serde_json::json!("never-persisted")),
        )
        .await;

        let body = recv_json(&mut rx);
        assert_eq!(body["event"], "attached");
        assert!(
            body.get("token_usage").is_none(),
            "a chat with no recorded usage must not carry a token_usage key: {body}"
        );
    }

    #[tokio::test]
    async fn handle_envelope_attach_rejects_invalid_or_missing_chat_id() {
        let shared = test_shared("browser");
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "initial-chat", tx);

        for chat_id in [
            None,
            Some(serde_json::json!("")),
            Some(serde_json::json!("has space")),
            Some(serde_json::json!(123)),
        ] {
            dispatch_attach(&shared, "conn-1", chat_id.clone()).await;
            let body = recv_json(&mut rx);
            assert_eq!(body["event"], "error", "rejected payload: {chat_id:?}");
            assert_eq!(body["detail"], "invalid chat_id");
            assert!(
                body.get("chat_id").is_none(),
                "invalid attach must omit chat_id, matching nanobot: {body}"
            );
        }

        assert!(
            shared
                .connections
                .lock()
                .await
                .senders_for_chat("has space")
                .is_empty()
        );
        assert_eq!(
            shared
                .connections
                .lock()
                .await
                .senders_for_chat("initial-chat")
                .len(),
            1,
            "a rejected attach must not drop the connection's existing subscription"
        );
    }

    #[tokio::test]
    async fn handle_envelope_attach_hydrates_active_goal_state() {
        let shared = test_shared("browser");
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "initial-chat", tx);
        {
            let mut session_manager = shared.session_manager.lock().unwrap();
            crate::session::goal_state::create_session_goal(
                &mut session_manager,
                "websocket:existing-chat",
                "ship the feature",
                None,
            )
            .unwrap();
        }

        dispatch_attach(&shared, "conn-1", Some(serde_json::json!("existing-chat"))).await;

        let attached = recv_json(&mut rx);
        assert_eq!(attached["event"], "attached");
        assert_eq!(attached["chat_id"], "existing-chat");

        let goal = recv_json(&mut rx);
        assert_eq!(goal["event"], "goal_state");
        assert_eq!(goal["chat_id"], "existing-chat");
        assert_eq!(goal["goal_state"]["objective"], "ship the feature");
    }

    #[tokio::test]
    async fn handle_envelope_attach_is_idempotent() {
        let shared = test_shared("browser");
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "initial-chat", tx);

        dispatch_attach(&shared, "conn-1", Some(serde_json::json!("existing-chat"))).await;
        let _ = recv_json(&mut rx);
        dispatch_attach(&shared, "conn-1", Some(serde_json::json!("existing-chat"))).await;
        let body = recv_json(&mut rx);
        assert_eq!(body["event"], "attached");
        assert_eq!(body["chat_id"], "existing-chat");

        assert_eq!(
            shared
                .connections
                .lock()
                .await
                .senders_for_chat("existing-chat")
                .len(),
            1,
            "re-attaching the same chat must not duplicate the subscription"
        );
    }

    #[tokio::test]
    async fn handle_envelope_attach_includes_persisted_session_history() {
        let shared = test_shared("browser");
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "initial-chat", tx);
        {
            let mut session_manager = shared
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let mut session = Session::new("websocket:existing-chat".to_string());
            session.add_message("user", "hello", serde_json::Map::new());
            session.add_message("assistant", "hi there", serde_json::Map::new());
            // Consolidated prefix must still reach the UI (get_history would drop it).
            session.last_consolidated = 2;
            session.add_message("user", "follow-up", serde_json::Map::new());
            session.add_message("assistant", "sure", serde_json::Map::new());
            session_manager.save(session).unwrap();
        }

        dispatch_attach(&shared, "conn-1", Some(serde_json::json!("existing-chat"))).await;

        let body = recv_json(&mut rx);
        assert_eq!(body["event"], "attached");
        assert_eq!(body["chat_id"], "existing-chat");
        let history = body["history"]
            .as_array()
            .expect("attached frame should carry history");
        let contents: Vec<&str> = history
            .iter()
            .filter_map(|m| m["content"].as_str())
            .collect();
        assert_eq!(contents, vec!["hello", "hi there", "follow-up", "sure"]);
        assert!(
            history.iter().all(|m| m.get(COMMAND_KEY).is_none()),
            "wire history must not leak internal markers: {history:?}"
        );
    }

    #[tokio::test]
    async fn handle_envelope_attach_resolves_session_image_placeholder_into_media_url() {
        let shared = test_shared("browser");
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "initial-chat", tx);

        let sub = shared.media_root.join("websocket");
        std::fs::create_dir_all(&sub).unwrap();
        let image_path = sub.join("abc.png");
        std::fs::write(&image_path, b"fake-png").unwrap();

        {
            let mut session_manager = shared
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let mut session = Session::new("websocket:existing-chat".to_string());
            session.messages.push(serde_json::json!({
                "role": "user",
                "content": [
                    {"type": "text", "text": "look at this"},
                    {"type": "text", "text": format!("[image: {}]", image_path.display())},
                ],
            }));
            session.add_message("assistant", "a cat", serde_json::Map::new());
            session_manager.save(session).unwrap();
        }

        dispatch_attach(&shared, "conn-1", Some(serde_json::json!("existing-chat"))).await;

        let body = recv_json(&mut rx);
        let history = body["history"]
            .as_array()
            .expect("attached frame should carry history");
        assert_eq!(history[0]["content"], "look at this");
        assert_eq!(
            history[0]["media"],
            serde_json::json!(["/v1/media/websocket/abc.png"])
        );
        assert!(history[1].get("media").is_none());
    }

    #[tokio::test]
    async fn handle_envelope_attach_prefers_transcript_history_over_a_divergent_session() {
        let shared = test_shared("browser");
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "initial-chat", tx);
        {
            let mut session_manager = shared
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let mut session = Session::new("websocket:existing-chat".to_string());
            session.add_message("user", "session-hello", serde_json::Map::new());
            session.add_message("assistant", "session-reply", serde_json::Map::new());
            session_manager.save(session).unwrap();
        }
        {
            let mut transcripts = shared
                .gateway_services
                .transcripts
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let meta = webui_meta("turn-1");
            transcripts.append_user_message(
                "existing-chat",
                "transcript-hello",
                &meta,
                None,
                None,
                None,
            );
            transcripts.append_turn_event(
                "existing-chat",
                HashMap::from([
                    ("event".to_string(), serde_json::json!("message")),
                    ("text".to_string(), serde_json::json!("transcript-reply")),
                ]),
                &meta,
                "complete",
            );
        }

        dispatch_attach(&shared, "conn-1", Some(serde_json::json!("existing-chat"))).await;

        let body = recv_json(&mut rx);
        assert_eq!(body["event"], "attached");
        assert_eq!(body["chat_id"], "existing-chat");
        let history = body["history"]
            .as_array()
            .expect("attached frame should carry history");
        let contents: Vec<&str> = history
            .iter()
            .filter_map(|m| m["content"].as_str())
            .collect();
        assert_eq!(
            contents,
            vec!["transcript-hello", "transcript-reply"],
            "the transcript must win over a diverging Session when both exist"
        );
    }

    #[tokio::test]
    async fn handle_envelope_attach_denies_a_sender_outside_the_allow_list() {
        let mut shared = test_shared("browser");
        shared.channels_config = ChannelsConfig {
            allow_from: vec!["someone-else".to_string()],
            ..ChannelsConfig::default()
        };
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "initial-chat", tx);
        {
            let mut session_manager = shared
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let mut session = Session::new("websocket:secret-chat".to_string());
            session.add_message("user", "confidential", serde_json::Map::new());
            session_manager.save(session).unwrap();
        }

        dispatch_attach(&shared, "conn-1", Some(serde_json::json!("secret-chat"))).await;

        let body = recv_json(&mut rx);
        assert_eq!(body["event"], "error");
        assert_eq!(
            body["detail"], "access_denied",
            "attach returns transcript content, so it must clear the same bar as `message`"
        );
        assert!(
            body.get("chat_id").is_none(),
            "the rejection must stay unscoped or a switching client filters it out: {body}"
        );
        assert!(
            shared
                .connections
                .lock()
                .await
                .senders_for_chat("secret-chat")
                .is_empty(),
            "a denied attach must not subscribe the connection"
        );
    }

    #[tokio::test]
    async fn handle_envelope_attach_denies_a_guest_attaching_someone_elses_chat() {
        let mut shared = test_shared("browser");
        shared.require_auth = false;
        save_session_with_owner(&shared, "secret-chat", Some("someone-else"));
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "initial-chat", tx);

        // `dispatch_attach` always uses `client_id: "client-1"`.
        dispatch_attach(&shared, "conn-1", Some(serde_json::json!("secret-chat"))).await;

        let body = recv_json(&mut rx);
        assert_eq!(body["event"], "error");
        assert_eq!(body["detail"], "access_denied");
        assert!(
            shared
                .connections
                .lock()
                .await
                .senders_for_chat("secret-chat")
                .is_empty(),
            "a denied attach must not subscribe the connection"
        );
    }

    #[tokio::test]
    async fn handle_envelope_attach_allows_a_guest_attaching_their_own_chat() {
        let mut shared = test_shared("browser");
        shared.require_auth = false;
        save_session_with_owner(&shared, "my-chat", Some("client-1"));
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "initial-chat", tx);

        dispatch_attach(&shared, "conn-1", Some(serde_json::json!("my-chat"))).await;

        let body = recv_json(&mut rx);
        assert_eq!(body["event"], "attached");
    }

    #[tokio::test]
    async fn handle_envelope_attach_allows_a_guest_attaching_a_brand_new_chat_id() {
        let mut shared = test_shared("browser");
        shared.require_auth = false;
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "initial-chat", tx);

        // No session exists yet for this chat_id — attach is idempotent
        // against nothing, so it must not be treated as "unowned == denied".
        dispatch_attach(&shared, "conn-1", Some(serde_json::json!("brand-new"))).await;

        let body = recv_json(&mut rx);
        assert_eq!(body["event"], "attached");
    }

    // --- handle_envelope_fork_chat ---

    async fn dispatch_fork_chat(
        shared: &WsShared,
        connection_id: &str,
        source_chat_id: &str,
        before_user_index: u64,
    ) {
        let mut envelope: Envelope = HashMap::new();
        envelope.insert("type".to_string(), serde_json::json!("fork_chat"));
        envelope.insert("chat_id".to_string(), serde_json::json!(source_chat_id));
        envelope.insert(
            "before_user_index".to_string(),
            serde_json::json!(before_user_index),
        );
        let ctx = EnvelopeDispatchContext {
            envelope: &envelope,
            connection_id,
            client_id: "client-1",
            shared,
            remote_addr: addr("127.0.0.1"),
            webui_authenticated: false,
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), dispatch_envelope(ctx))
            .await
            .expect("handle_envelope_fork_chat must not hang");
    }

    #[tokio::test]
    async fn handle_envelope_fork_chat_sends_attached_with_transcript_history_for_new_chat_id() {
        let shared = test_shared("browser");
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "src", tx);
        {
            let mut session_manager = shared
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let mut session = Session::new("websocket:src".to_string());
            session.add_message("user", "hello", serde_json::Map::new());
            session.add_message("assistant", "hi there", serde_json::Map::new());
            session_manager.save(session).unwrap();
        }
        {
            let mut transcripts = shared
                .gateway_services
                .transcripts
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            transcripts.append(
                "src",
                HashMap::from([
                    ("event".to_string(), serde_json::json!("user")),
                    ("text".to_string(), serde_json::json!("hello")),
                ]),
            );
            transcripts.append(
                "src",
                HashMap::from([
                    ("event".to_string(), serde_json::json!("message")),
                    ("text".to_string(), serde_json::json!("hi there")),
                ]),
            );
        }

        // `before_user_index: 1` == the source's total user-message count,
        // i.e. forking from the end of a completed turn.
        dispatch_fork_chat(&shared, "conn-1", "src", 1).await;

        let attached = recv_json(&mut rx);
        assert_eq!(attached["event"], "attached");
        let new_chat_id = attached["chat_id"]
            .as_str()
            .expect("attached frame should carry the new chat_id")
            .to_string();
        assert_ne!(
            new_chat_id, "src",
            "fork must attach the connection to the new chat, not rewrite the source"
        );

        let history = attached["history"]
            .as_array()
            .expect("attached frame should carry history");
        assert_eq!(history.len(), 2);
        assert_eq!(history[0]["role"], "user");
        assert_eq!(history[0]["content"], "hello");
        assert_eq!(history[1]["role"], "assistant");
        assert_eq!(history[1]["content"], "hi there");

        assert_eq!(
            shared
                .connections
                .lock()
                .await
                .senders_for_chat(&new_chat_id)
                .len(),
            1,
            "fork must subscribe the requesting connection to the new chat_id"
        );
    }

    #[tokio::test]
    async fn handle_envelope_fork_chat_without_before_user_index_forks_the_whole_chat() {
        // `websockets-chat`'s "Fork session" menu item has no partial-fork
        // picker, so its `fork_chat` envelope never sends `before_user_index`
        // at all — this must fork the entire conversation, not nothing.
        let shared = test_shared("browser");
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "src", tx);
        {
            let mut session_manager = shared
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let mut session = Session::new("websocket:src".to_string());
            session.add_message("user", "hello", serde_json::Map::new());
            session.add_message("assistant", "hi there", serde_json::Map::new());
            session.add_message("user", "how are you", serde_json::Map::new());
            session.add_message("assistant", "great, thanks", serde_json::Map::new());
            session_manager.save(session).unwrap();
        }
        {
            let mut transcripts = shared
                .gateway_services
                .transcripts
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            transcripts.append(
                "src",
                HashMap::from([
                    ("event".to_string(), serde_json::json!("user")),
                    ("text".to_string(), serde_json::json!("hello")),
                ]),
            );
            transcripts.append(
                "src",
                HashMap::from([
                    ("event".to_string(), serde_json::json!("message")),
                    ("text".to_string(), serde_json::json!("hi there")),
                ]),
            );
            transcripts.append(
                "src",
                HashMap::from([
                    ("event".to_string(), serde_json::json!("user")),
                    ("text".to_string(), serde_json::json!("how are you")),
                ]),
            );
            transcripts.append(
                "src",
                HashMap::from([
                    ("event".to_string(), serde_json::json!("message")),
                    ("text".to_string(), serde_json::json!("great, thanks")),
                ]),
            );
        }

        let mut envelope: Envelope = HashMap::new();
        envelope.insert("type".to_string(), serde_json::json!("fork_chat"));
        envelope.insert("chat_id".to_string(), serde_json::json!("src"));
        let ctx = EnvelopeDispatchContext {
            envelope: &envelope,
            connection_id: "conn-1",
            client_id: "client-1",
            shared: &shared,
            remote_addr: addr("127.0.0.1"),
            webui_authenticated: false,
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), dispatch_envelope(ctx))
            .await
            .expect("handle_envelope_fork_chat must not hang");

        let attached = recv_json(&mut rx);
        assert_eq!(attached["event"], "attached");
        let history = attached["history"]
            .as_array()
            .expect("attached frame should carry history");
        let contents: Vec<&str> = history
            .iter()
            .filter_map(|m| m["content"].as_str())
            .collect();
        assert_eq!(
            contents,
            vec!["hello", "hi there", "how are you", "great, thanks"],
            "omitting before_user_index must fork the entire conversation"
        );
    }

    #[tokio::test]
    async fn handle_envelope_fork_chat_falls_back_to_session_history_when_transcript_fork_fails() {
        let shared = test_shared("browser");
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "src", tx);
        {
            let mut session_manager = shared
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let mut session = Session::new("websocket:src".to_string());
            session.add_message("user", "hello", serde_json::Map::new());
            session.add_message("assistant", "hi there", serde_json::Map::new());
            session_manager.save(session).unwrap();
        }
        // No transcript rows are written for "src", so
        // `fork_transcript_before_user_index` returns `false` and the
        // handler must fall back to the forked `Session`'s messages.

        dispatch_fork_chat(&shared, "conn-1", "src", 1).await;

        let attached = recv_json(&mut rx);
        assert_eq!(attached["event"], "attached");
        let history = attached["history"]
            .as_array()
            .expect("attached frame should carry history");
        let contents: Vec<&str> = history
            .iter()
            .filter_map(|m| m["content"].as_str())
            .collect();
        assert_eq!(contents, vec!["hello", "hi there"]);
    }

    #[tokio::test]
    async fn handle_envelope_fork_chat_reports_source_sessions_preset_override() {
        let mut shared = test_shared("browser");
        shared.runtime_resolver = test_runtime_resolver_with_preset("fast", "claude-haiku");
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "src", tx);
        {
            let mut session_manager = shared
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let mut session = Session::new("websocket:src".to_string());
            session.add_message("user", "hello", serde_json::Map::new());
            session.metadata.insert(
                SESSION_MODEL_PRESET_METADATA_KEY.to_string(),
                serde_json::json!("fast"),
            );
            session_manager.save(session).unwrap();
        }

        dispatch_fork_chat(&shared, "conn-1", "src", 1).await;

        let attached = recv_json(&mut rx);
        assert_eq!(attached["event"], "attached");
        assert_eq!(
            attached["model_preset"], "fast",
            "fork_session_before_user_index copies model_preset metadata, so the forked \
             chat's attached frame must report the same resolved selection: {attached}"
        );
        assert_eq!(attached["model"], "claude-haiku");
    }

    #[tokio::test]
    async fn handle_envelope_fork_chat_resolves_session_image_placeholder_into_media_url() {
        let shared = test_shared("browser");
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "src", tx);

        let sub = shared.media_root.join("websocket");
        std::fs::create_dir_all(&sub).unwrap();
        let image_path = sub.join("abc.png");
        std::fs::write(&image_path, b"fake-png").unwrap();

        {
            let mut session_manager = shared
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let mut session = Session::new("websocket:src".to_string());
            session.messages.push(serde_json::json!({
                "role": "user",
                "content": [
                    {"type": "text", "text": "look at this"},
                    {"type": "text", "text": format!("[image: {}]", image_path.display())},
                ],
            }));
            session.add_message("assistant", "a cat", serde_json::Map::new());
            session_manager.save(session).unwrap();
        }
        // No transcript rows written for "src", so the fork falls back to
        // the session-file path (`websocket_chat_history`), same as above.

        dispatch_fork_chat(&shared, "conn-1", "src", 1).await;

        let attached = recv_json(&mut rx);
        let history = attached["history"]
            .as_array()
            .expect("attached frame should carry history");
        assert_eq!(history[0]["content"], "look at this");
        assert_eq!(
            history[0]["media"],
            serde_json::json!(["/v1/media/websocket/abc.png"])
        );
    }

    #[tokio::test]
    async fn handle_envelope_fork_chat_rejects_an_invalid_index() {
        let shared = test_shared("browser");
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "src", tx);
        {
            let mut session_manager = shared
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let mut session = Session::new("websocket:src".to_string());
            session.add_message("user", "hello", serde_json::Map::new());
            session_manager.save(session).unwrap();
        }

        // Only one user message exists (index 0), so `before_user_index: 5`
        // is past the end and must be rejected rather than silently
        // producing an empty fork.
        dispatch_fork_chat(&shared, "conn-1", "src", 5).await;

        let body = recv_json(&mut rx);
        assert_eq!(body["event"], "error");
        assert_eq!(body["detail"], "invalid fork source or index");
    }

    #[tokio::test]
    async fn handle_envelope_fork_chat_denies_forking_someone_elses_chat_when_require_auth_is_false()
     {
        let mut shared = test_shared("browser");
        shared.require_auth = false;
        {
            let mut session_manager = shared
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let mut session = Session::new("websocket:src".to_string());
            session.add_message("user", "hello", serde_json::Map::new());
            session.metadata.insert(
                SESSION_WEBSOCKET_OWNER_CLIENT_ID_KEY.to_string(),
                serde_json::json!("someone-else"),
            );
            session_manager.save(session).unwrap();
        }
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "src", tx);

        // `dispatch_fork_chat` always uses `client_id: "client-1"`.
        dispatch_fork_chat(&shared, "conn-1", "src", 1).await;

        let body = recv_json(&mut rx);
        assert_eq!(body["event"], "error");
        assert_eq!(body["detail"], "access_denied");
        assert!(
            rx.try_recv().is_err(),
            "a denied fork must not also send attached"
        );
    }

    #[tokio::test]
    async fn handle_envelope_fork_chat_stamps_the_requesters_client_id_as_owner() {
        let mut shared = test_shared("browser");
        shared.require_auth = false;
        {
            let mut session_manager = shared
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let mut session = Session::new("websocket:src".to_string());
            session.add_message("user", "hello", serde_json::Map::new());
            session.metadata.insert(
                SESSION_WEBSOCKET_OWNER_CLIENT_ID_KEY.to_string(),
                serde_json::json!("client-1"),
            );
            session_manager.save(session).unwrap();
        }
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "src", tx);

        dispatch_fork_chat(&shared, "conn-1", "src", 1).await;

        let attached = recv_json(&mut rx);
        assert_eq!(attached["event"], "attached");
        let new_chat_id = attached["chat_id"].as_str().unwrap().to_string();

        let session_manager = shared
            .session_manager
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let forked = session_manager
            .get_session_internal(&get_session_id(&new_chat_id))
            .expect("fork destination must exist");
        assert_eq!(
            forked.metadata.get(SESSION_WEBSOCKET_OWNER_CLIENT_ID_KEY),
            Some(&serde_json::json!("client-1"))
        );
    }

    #[tokio::test]
    async fn handle_envelope_fork_chat_after_a_real_turn_includes_the_assistant_reply() {
        // Unlike the fork tests above (which seed the transcript directly via
        // `transcripts.append`), this drives both sides of a turn through
        // their real production entry points — `handle_envelope_message` for
        // the user's half, `BaseChannel::send` for the assistant's half — to
        // guard the write path this plan added, not just the fork/read side.
        let channel = test_channel();
        let shared = channel.shared();
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "src", tx);

        let mut envelope: Envelope = HashMap::new();
        envelope.insert("type".to_string(), serde_json::json!("message"));
        envelope.insert("chat_id".to_string(), serde_json::json!("src"));
        envelope.insert("content".to_string(), serde_json::json!("hello"));
        envelope.insert("webui".to_string(), serde_json::json!(true));
        let ctx = EnvelopeDispatchContext {
            envelope: &envelope,
            connection_id: "conn-1",
            client_id: "client-1",
            shared: &shared,
            remote_addr: addr("127.0.0.1"),
            webui_authenticated: false,
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), dispatch_envelope(ctx))
            .await
            .expect("handle_envelope_message must not hang");
        // Drain whatever attach/hydration frames this produced — this test
        // only cares about what lands in the transcript, not the live wire
        // frames from the inbound half of the turn.
        while rx.try_recv().is_ok() {}

        let mut assistant_msg = outbound("src", "hi there", None);
        assistant_msg.metadata = webui_meta("turn-1");
        BaseChannel::send(&channel, assistant_msg).await.unwrap();
        rx.try_recv()
            .expect("expected the assistant's message frame");

        // `fork_session_before_user_index`'s user-turn count comes from the
        // `Session` file, which only the (not-running-here) `AgentLoop`
        // would normally populate as it drains the bus. Seed it directly so
        // the fork's index check succeeds — this test is about the
        // transcript write path, not session persistence.
        {
            let mut session_manager = shared
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let mut session = Session::new("websocket:src".to_string());
            session.add_message("user", "hello", serde_json::Map::new());
            session_manager.save(session).unwrap();
        }

        dispatch_fork_chat(&shared, "conn-1", "src", 1).await;

        let attached = recv_json(&mut rx);
        assert_eq!(attached["event"], "attached");
        let history = attached["history"]
            .as_array()
            .expect("attached frame should carry history");
        assert_eq!(history.len(), 2);
        assert_eq!(history[0]["role"], "user");
        assert_eq!(history[0]["content"], "hello");
        assert_eq!(history[1]["role"], "assistant");
        assert_eq!(history[1]["content"], "hi there");
    }

    // --- handle_envelope_message: `user` fan-out ---

    fn dispatch_message<'a>(
        envelope: &'a Envelope,
        connection_id: &'a str,
        client_id: &'a str,
        shared: &'a WsShared,
    ) -> impl Future<Output = ()> + 'a {
        dispatch_envelope(EnvelopeDispatchContext {
            envelope,
            connection_id,
            client_id,
            shared,
            remote_addr: addr("127.0.0.1"),
            webui_authenticated: false,
        })
    }

    fn message_envelope(chat_id: &str, content: &str, turn_id: Option<&str>) -> Envelope {
        let mut envelope: Envelope = HashMap::new();
        envelope.insert("type".to_string(), serde_json::json!("message"));
        envelope.insert("chat_id".to_string(), serde_json::json!(chat_id));
        envelope.insert("content".to_string(), serde_json::json!(content));
        envelope.insert("webui".to_string(), serde_json::json!(true));
        if let Some(turn_id) = turn_id {
            envelope.insert("turn_id".to_string(), serde_json::json!(turn_id));
        }
        envelope
    }

    #[tokio::test]
    async fn handle_envelope_message_fans_out_user_event_to_every_subscribed_connection() {
        let shared = test_shared("browser");
        let (tx1, mut rx1) = mpsc::unbounded_channel::<Message>();
        let (tx2, mut rx2) = mpsc::unbounded_channel::<Message>();
        let (tx3, mut rx3) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx1);
        shared.connections.lock().await.attach("conn-2", "chat-1");
        shared
            .connections
            .lock()
            .await
            .register("conn-2", "chat-1", tx2);
        // Subscribed to a different chat entirely — must receive nothing.
        shared
            .connections
            .lock()
            .await
            .register("conn-3", "chat-2", tx3);

        let envelope = message_envelope("chat-1", "hello there", Some("turn-1"));
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            dispatch_message(&envelope, "conn-1", "client-1", &shared),
        )
        .await
        .expect("handle_envelope_message must not hang");

        let user_event_1 = recv_json(&mut rx1);
        assert_eq!(user_event_1["event"], "user");
        assert_eq!(user_event_1["chat_id"], "chat-1");
        assert_eq!(user_event_1["turn_id"], "turn-1");
        assert_eq!(user_event_1["text"], "hello there");

        let user_event_2 = recv_json(&mut rx2);
        assert_eq!(
            user_event_2, user_event_1,
            "every subscriber, including the sender, gets the same frame"
        );

        assert!(
            rx3.try_recv().is_err(),
            "a connection subscribed to a different chat must receive nothing"
        );
    }

    #[tokio::test]
    async fn handle_envelope_message_sends_user_event_before_message_accepted() {
        let shared = test_shared("browser");
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);

        let envelope = message_envelope("chat-1", "hello", Some("turn-1"));
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            dispatch_message(&envelope, "conn-1", "client-1", &shared),
        )
        .await
        .expect("handle_envelope_message must not hang");

        let first = recv_json(&mut rx);
        assert_eq!(
            first["event"], "user",
            "a fast-starting stream must never race ahead of this frame"
        );
        let second = recv_json(&mut rx);
        assert_eq!(second["event"], "message_accepted");
    }

    #[tokio::test]
    async fn handle_envelope_message_user_event_uses_a_normalized_turn_id_when_client_omits_one() {
        let shared = test_shared("browser");
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);

        let envelope = message_envelope("chat-1", "hello", None);
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            dispatch_message(&envelope, "conn-1", "client-1", &shared),
        )
        .await
        .expect("handle_envelope_message must not hang");

        let user_event = recv_json(&mut rx);
        assert_eq!(user_event["event"], "user");
        let turn_id = user_event["turn_id"]
            .as_str()
            .expect("turn_id must still be present so a watcher can adopt the turn");
        assert!(!turn_id.is_empty());
        // No `message_accepted` follows: that ack is only sent for a
        // client-supplied `turn_id` (mirrors nanobot's `if is_webui and
        // turn_id:`), unlike `user`, which always carries the normalized id.
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn handle_envelope_message_access_denied_does_not_emit_a_user_event() {
        let mut shared = test_shared("browser");
        shared.channels_config = ChannelsConfig {
            allow_from: vec!["someone-else".to_string()],
            ..ChannelsConfig::default()
        };
        let (tx1, mut rx1) = mpsc::unbounded_channel::<Message>();
        let (tx2, mut rx2) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx1);
        shared.connections.lock().await.attach("conn-2", "chat-1");
        shared
            .connections
            .lock()
            .await
            .register("conn-2", "chat-1", tx2);

        let envelope = message_envelope("chat-1", "hello", Some("turn-1"));
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            dispatch_message(&envelope, "conn-1", "client-1", &shared),
        )
        .await
        .expect("handle_envelope_message must not hang");

        let body = recv_json(&mut rx1);
        assert_eq!(body["event"], "error");
        assert_eq!(body["detail"], "access_denied");
        assert!(
            rx2.try_recv().is_err(),
            "a rejected turn must not fan out a user event to other subscribers"
        );
    }

    #[tokio::test]
    async fn handle_envelope_message_missing_content_does_not_emit_a_user_event() {
        let shared = test_shared("browser");
        let (tx1, mut rx1) = mpsc::unbounded_channel::<Message>();
        let (tx2, mut rx2) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx1);
        shared.connections.lock().await.attach("conn-2", "chat-1");
        shared
            .connections
            .lock()
            .await
            .register("conn-2", "chat-1", tx2);

        let envelope = message_envelope("chat-1", "", None);
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            dispatch_message(&envelope, "conn-1", "client-1", &shared),
        )
        .await
        .expect("handle_envelope_message must not hang");

        let body = recv_json(&mut rx1);
        assert_eq!(body["event"], "error");
        assert_eq!(body["detail"], "missing content");
        assert!(
            rx2.try_recv().is_err(),
            "a rejected turn must not fan out a user event to other subscribers"
        );
    }

    #[tokio::test]
    async fn handle_envelope_attach_after_a_real_turn_reads_history_from_the_transcript() {
        // Mirrors `handle_envelope_fork_chat_after_a_real_turn_includes_the_assistant_reply`
        // above, but for `attach` instead of `fork_chat`: drives both sides of
        // a turn through their real production entry points, then attaches a
        // fresh connection to the same chat and checks the resulting history
        // came from the transcript this plan makes `attach_chat` prefer.
        let channel = test_channel();
        let shared = channel.shared();
        let (tx1, mut rx1) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "src", tx1);

        let mut envelope: Envelope = HashMap::new();
        envelope.insert("type".to_string(), serde_json::json!("message"));
        envelope.insert("chat_id".to_string(), serde_json::json!("src"));
        envelope.insert("content".to_string(), serde_json::json!("hello"));
        envelope.insert("webui".to_string(), serde_json::json!(true));
        let ctx = EnvelopeDispatchContext {
            envelope: &envelope,
            connection_id: "conn-1",
            client_id: "client-1",
            shared: &shared,
            remote_addr: addr("127.0.0.1"),
            webui_authenticated: false,
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), dispatch_envelope(ctx))
            .await
            .expect("handle_envelope_message must not hang");
        while rx1.try_recv().is_ok() {}

        let mut assistant_msg = outbound("src", "hi there", None);
        assistant_msg.metadata = webui_meta("turn-1");
        BaseChannel::send(&channel, assistant_msg).await.unwrap();
        rx1.try_recv()
            .expect("expected the assistant's message frame");

        let (tx2, mut rx2) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-2", "initial-chat", tx2);

        dispatch_attach(&shared, "conn-2", Some(serde_json::json!("src"))).await;

        let attached = recv_json(&mut rx2);
        assert_eq!(attached["event"], "attached");
        assert_eq!(attached["chat_id"], "src");
        let history = attached["history"]
            .as_array()
            .expect("attached frame should carry history");
        assert_eq!(history.len(), 2);
        assert_eq!(history[0]["role"], "user");
        assert_eq!(history[0]["content"], "hello");
        assert_eq!(history[1]["role"], "assistant");
        assert_eq!(history[1]["content"], "hi there");
    }

    // --- websocket_chat_history ---

    fn history_message(role: &str, content: &str) -> serde_json::Value {
        serde_json::json!({
            "role": role,
            "content": content,
            "timestamp": "2026-01-01T00:00:00Z",
        })
    }

    #[test]
    fn websocket_chat_history_empty_when_session_is_missing() {
        assert!(websocket_chat_history(None, 500).is_empty());
    }

    #[test]
    fn websocket_chat_history_maps_user_and_assistant_and_drops_internal_keys() {
        let mut session = Session::new("websocket:chat-1".to_string());
        session.messages.push(history_message("user", "hello"));
        session.messages.push(serde_json::json!({
            "role": "assistant",
            "content": "hi",
            "timestamp": "2026-01-01T00:00:01Z",
            "reasoning_content": "think",
            "tool_calls": [{"id": "c1"}],
        }));

        let history = websocket_chat_history(Some(&session), 500);

        assert_eq!(history.len(), 2);
        assert_eq!(history[0]["role"], "user");
        assert_eq!(history[0]["content"], "hello");
        assert_eq!(history[0]["timestamp"], "2026-01-01T00:00:00Z");
        assert_eq!(history[1]["role"], "assistant");
        assert_eq!(history[1]["content"], "hi");
        assert_eq!(history[1]["reasoning_content"], "think");
        assert!(
            history[1].get("tool_calls").is_none(),
            "LLM tool_calls are not a ChatEntry field: {}",
            history[1]
        );
    }

    #[test]
    fn websocket_chat_history_omits_commands_hidden_tool_and_system_rows() {
        let mut session = Session::new("websocket:chat-1".to_string());
        session.messages.push(history_message("user", "keep me"));
        session.messages.push(serde_json::json!({
            "role": "user",
            "content": "/status",
            "_command": true,
        }));
        session.messages.push(serde_json::json!({
            "role": "user",
            "content": "hidden prompt",
            "_hidden_history": true,
        }));
        session
            .messages
            .push(history_message("tool", "tool output"));
        session.messages.push(history_message("system", "sys"));
        session
            .messages
            .push(history_message("assistant", "visible reply"));

        let history = websocket_chat_history(Some(&session), 500);

        let contents: Vec<&str> = history
            .iter()
            .filter_map(|m| m["content"].as_str())
            .collect();
        assert_eq!(contents, vec!["keep me", "visible reply"]);
    }

    #[test]
    fn websocket_chat_history_keeps_consolidated_prefix() {
        let mut session = Session::new("websocket:chat-1".to_string());
        session.messages.push(history_message("user", "before"));
        session
            .messages
            .push(history_message("assistant", "summarized"));
        session.messages.push(history_message("user", "after"));
        session.last_consolidated = 2;

        let history = websocket_chat_history(Some(&session), 500);

        let contents: Vec<&str> = history
            .iter()
            .filter_map(|m| m["content"].as_str())
            .collect();
        assert_eq!(contents, vec!["before", "summarized", "after"]);
    }

    #[test]
    fn websocket_chat_history_caps_from_the_end_and_aligns_to_a_user_turn() {
        let mut session = Session::new("websocket:chat-1".to_string());
        session.messages.push(history_message("user", "old"));
        session.messages.push(history_message("assistant", "old-a"));
        session
            .messages
            .push(history_message("assistant", "dangling"));
        session.messages.push(history_message("user", "keep"));
        session
            .messages
            .push(history_message("assistant", "keep-a"));

        let history = websocket_chat_history(Some(&session), 3);

        let contents: Vec<&str> = history
            .iter()
            .filter_map(|m| m["content"].as_str())
            .collect();
        // Cap of 3 lands on [dangling, keep, keep-a]; align drops dangling.
        assert_eq!(contents, vec!["keep", "keep-a"]);
    }

    #[test]
    fn websocket_chat_history_flattens_multimodal_block_content() {
        let mut session = Session::new("websocket:chat-1".to_string());
        // The shape `AgentLoop::save_turn` persists for a turn with media.
        session.messages.push(serde_json::json!({
            "role": "user",
            "content": [
                {"type": "text", "text": "what is in this picture?"},
                {"type": "image_url", "image_url": {"url": "https://example.com/a.png"}},
            ],
        }));
        session.messages.push(history_message("assistant", "a cat"));

        let history = websocket_chat_history(Some(&session), 500);

        let contents: Vec<&str> = history
            .iter()
            .filter_map(|m| m["content"].as_str())
            .collect();
        assert_eq!(contents, vec!["what is in this picture?", "a cat"]);
    }

    #[test]
    fn display_text_joins_text_blocks_and_ignores_everything_else() {
        assert_eq!(display_text(Some(&serde_json::json!("plain"))), "plain");
        assert_eq!(
            display_text(Some(&serde_json::json!([
                {"type": "text", "text": "first"},
                {"type": "text", "text": ""},
                {"type": "image_url", "image_url": {"url": "https://example.com/a.png"}},
                {"type": "text", "text": "second"},
            ]))),
            "first\nsecond"
        );
        assert_eq!(display_text(None), "");
        assert_eq!(display_text(Some(&serde_json::Value::Null)), "");
    }

    #[test]
    fn websocket_chat_history_zero_cap_returns_empty() {
        let mut session = Session::new("websocket:chat-1".to_string());
        session.messages.push(history_message("user", "hello"));
        assert!(websocket_chat_history(Some(&session), 0).is_empty());
    }

    // --- websocket_chat_history: media (session-file fallback) ---

    #[test]
    fn websocket_chat_history_turns_image_placeholder_into_media_and_strips_it_from_content() {
        let mut session = Session::new("websocket:chat-1".to_string());
        session.messages.push(serde_json::json!({
            "role": "user",
            "content": [
                {"type": "text", "text": "look at this"},
                {"type": "text", "text": "[image: C:\\data\\media\\websocket\\abc.png]"},
            ],
        }));
        session.messages.push(history_message("assistant", "a cat"));

        let history = websocket_chat_history(Some(&session), 500);

        assert_eq!(history[0]["content"], "look at this");
        assert_eq!(
            history[0]["media"],
            serde_json::json!(["C:\\data\\media\\websocket\\abc.png"])
        );
        assert!(history[1].get("media").is_none());
    }

    #[test]
    fn websocket_chat_history_keeps_surviving_http_image_url_as_media() {
        let mut session = Session::new("websocket:chat-1".to_string());
        session.messages.push(serde_json::json!({
            "role": "user",
            "content": [
                {"type": "text", "text": "what is in this picture?"},
                {"type": "image_url", "image_url": {"url": "https://example.com/a.png"}},
            ],
        }));

        let history = websocket_chat_history(Some(&session), 500);

        assert_eq!(
            history[0]["media"],
            serde_json::json!(["https://example.com/a.png"])
        );
    }

    #[test]
    fn websocket_chat_history_pathless_image_placeholder_yields_no_media() {
        let mut session = Session::new("websocket:chat-1".to_string());
        session.messages.push(serde_json::json!({
            "role": "user",
            "content": [{"type": "text", "text": "[image]"}],
        }));

        let history = websocket_chat_history(Some(&session), 500);

        assert!(history[0].get("media").is_none());
        assert_eq!(history[0]["content"], "");
    }

    // --- is_image_placeholder_text / image_placeholder_path ---

    #[test]
    fn is_image_placeholder_text_matches_both_shapes() {
        assert!(is_image_placeholder_text("[image]"));
        assert!(is_image_placeholder_text("[image: /tmp/a.png]"));
        assert!(!is_image_placeholder_text("[image tag"));
        assert!(!is_image_placeholder_text("just some text"));
    }

    #[test]
    fn image_placeholder_path_extracts_inner_path() {
        assert_eq!(
            image_placeholder_path("[image: /tmp/a.png]"),
            Some("/tmp/a.png")
        );
        assert_eq!(image_placeholder_path("[image]"), None);
        assert_eq!(image_placeholder_path("plain text"), None);
    }

    // --- extract_media_refs ---

    #[test]
    fn extract_media_refs_empty_for_string_or_missing_content() {
        assert!(extract_media_refs(Some(&serde_json::json!("plain"))).is_empty());
        assert!(extract_media_refs(None).is_empty());
    }

    #[test]
    fn extract_media_refs_ignores_data_url_image_blocks() {
        // sanitize_persisted_blocks never actually persists a `data:` image_url
        // block (it rewrites it to a text placeholder first), but guard the
        // extractor against one anyway rather than leaking a huge data URI.
        let content = serde_json::json!([
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}},
        ]);
        assert!(extract_media_refs(Some(&content)).is_empty());
    }

    // --- resolve_history_media ---

    #[test]
    fn resolve_history_media_converts_local_path_and_passes_through_http_url() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("websocket");
        std::fs::create_dir_all(&sub).unwrap();
        let file = sub.join("abc.png");
        std::fs::write(&file, b"fake-png").unwrap();

        let mut history = vec![serde_json::json!({
            "role": "user",
            "content": "hi",
            "media": [file.to_str().unwrap(), "https://example.com/a.png"],
        })];
        resolve_history_media(&mut history, dir.path());

        let media = history[0]["media"].as_array().unwrap();
        assert_eq!(media.len(), 2);
        assert_eq!(media[0], "/v1/media/websocket/abc.png");
        assert_eq!(media[1], "https://example.com/a.png");
    }

    #[test]
    fn resolve_history_media_drops_missing_file_and_removes_empty_media_key() {
        let dir = tempfile::tempdir().unwrap();
        let mut history = vec![serde_json::json!({
            "role": "user",
            "content": "hi",
            "media": ["C:\\nope\\gone.png"],
        })];
        resolve_history_media(&mut history, dir.path());

        assert!(history[0].get("media").is_none());
    }

    #[test]
    fn resolve_history_media_no_op_when_no_media_field() {
        let dir = tempfile::tempdir().unwrap();
        let mut history = vec![serde_json::json!({"role": "assistant", "content": "hi"})];
        resolve_history_media(&mut history, dir.path());

        assert!(history[0].get("media").is_none());
    }

    // --- list_websocket_chats / handle_envelope_list_chats ---

    #[test]
    fn list_websocket_chats_filters_to_websocket_prefixed_keys_and_strips_prefix() {
        let sessions = vec![
            serde_json::json!({
                "key": "websocket:chat-1", "created_at": "t1", "updated_at": "t2", "path": "/some/path", "title": "Fix the login bug",
            }),
            serde_json::json!({
                "key": "cli:direct", "created_at": "t1", "updated_at": "t2", "path": "/other/path",
            }),
            serde_json::json!({
                "key": "cron:job-1", "created_at": "t1", "updated_at": "t2", "path": "/cron/path",
            }),
        ];

        let chats = list_websocket_chats(sessions);

        assert_eq!(
            chats.len(),
            1,
            "only the websocket:-prefixed session should survive"
        );
        assert_eq!(chats[0]["chat_id"], "chat-1");
        assert!(
            chats[0].get("key").is_none(),
            "internal session key must not leak onto the wire"
        );
        assert!(
            chats[0].get("path").is_none(),
            "internal filesystem path must not leak onto the wire"
        );
        assert_eq!(chats[0]["created_at"], "t1");
        assert_eq!(chats[0]["updated_at"], "t2");
        assert_eq!(chats[0]["title"], "Fix the login bug");
    }

    #[test]
    fn list_websocket_chats_skips_invalid_chat_ids_after_stripping_prefix() {
        let sessions = vec![
            serde_json::json!({"key": "websocket:", "created_at": "", "updated_at": "", "path": ""}),
            serde_json::json!({"key": "websocket:has space", "created_at": "", "updated_at": "", "path": ""}),
        ];

        let chats = list_websocket_chats(sessions);

        assert!(
            chats.is_empty(),
            "empty/invalid chat ids must not reach the wire: {chats:?}"
        );
    }

    #[tokio::test]
    async fn handle_envelope_list_chats_returns_only_websocket_sessions() {
        let shared = test_shared("browser");
        {
            let mut session_manager = shared
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            session_manager
                .save(crate::session::manager::Session::new(
                    "websocket:chat-a".to_string(),
                ))
                .unwrap();
            session_manager
                .save(crate::session::manager::Session::new(
                    "websocket:chat-b".to_string(),
                ))
                .unwrap();
            session_manager
                .save(crate::session::manager::Session::new(
                    "cli:direct".to_string(),
                ))
                .unwrap();
        }
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "initial-chat", tx);

        let mut envelope: Envelope = HashMap::new();
        envelope.insert("type".to_string(), serde_json::json!("list_chats"));
        let ctx = EnvelopeDispatchContext {
            envelope: &envelope,
            connection_id: "conn-1",
            client_id: "client-1",
            shared: &shared,
            remote_addr: addr("127.0.0.1"),
            webui_authenticated: false,
        };

        dispatch_envelope(ctx).await;

        let frame = rx.try_recv().expect("expected a chats frame");
        let body: serde_json::Value = serde_json::from_str(&frame.into_text().unwrap()).unwrap();
        assert_eq!(body["event"], "chats");
        let chats = body["chats"].as_array().expect("chats should be an array");
        assert_eq!(chats.len(), 2, "cli:direct must be excluded: {chats:?}");
        let chat_ids: std::collections::HashSet<&str> =
            chats.iter().filter_map(|c| c["chat_id"].as_str()).collect();
        assert!(chat_ids.contains("chat-a"));
        assert!(chat_ids.contains("chat-b"));
    }

    #[tokio::test]
    async fn handle_envelope_list_skills_includes_workspace_skill_name_and_description() {
        let shared = test_shared("browser");
        let skill_dir = shared
            .workspace_request_handler
            .default_workspace
            .join("skills")
            .join("popup-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\ndescription: Helps with popup tests\n---\n# Popup\n",
        )
        .unwrap();

        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "initial-chat", tx);

        let mut envelope: Envelope = HashMap::new();
        envelope.insert("type".to_string(), serde_json::json!("list_skills"));
        let ctx = EnvelopeDispatchContext {
            envelope: &envelope,
            connection_id: "conn-1",
            client_id: "client-1",
            shared: &shared,
            remote_addr: addr("127.0.0.1"),
            webui_authenticated: false,
        };

        dispatch_envelope(ctx).await;

        let frame = rx.try_recv().expect("expected a skills frame");
        let body: serde_json::Value = serde_json::from_str(&frame.into_text().unwrap()).unwrap();
        assert_eq!(body["event"], "skills");
        let skills = body["skills"]
            .as_array()
            .expect("skills should be an array");
        let popup = skills
            .iter()
            .find(|s| s["name"] == "popup-skill")
            .expect("workspace skill must be in the list (cwd builtins may also appear)");
        assert_eq!(popup["description"], "Helps with popup tests");
    }

    // --- guest session isolation (owner_allows_access / scope_chats_to_owner) ---

    #[test]
    fn scope_chats_to_owner_keeps_only_the_matching_owner() {
        let chats = vec![
            serde_json::json!({"chat_id": "mine", "owner_client_id": "client-1"}),
            serde_json::json!({"chat_id": "theirs", "owner_client_id": "client-2"}),
            serde_json::json!({"chat_id": "unowned"}),
        ];

        let scoped = scope_chats_to_owner(chats, "client-1");

        assert_eq!(scoped.len(), 1);
        assert_eq!(scoped[0]["chat_id"], "mine");
    }

    #[test]
    fn scope_chats_to_owner_hides_an_unowned_entry_fail_closed() {
        let chats = vec![serde_json::json!({"chat_id": "legacy", "owner_client_id": ""})];
        assert!(scope_chats_to_owner(chats, "client-1").is_empty());
    }

    #[test]
    fn strip_owner_client_id_removes_the_field_but_keeps_others() {
        let mut chats =
            vec![serde_json::json!({"chat_id": "a", "owner_client_id": "client-1", "title": "t"})];
        strip_owner_client_id(&mut chats);
        assert!(chats[0].get("owner_client_id").is_none());
        assert_eq!(chats[0]["chat_id"], "a");
        assert_eq!(chats[0]["title"], "t");
    }

    fn save_session_with_owner(shared: &WsShared, chat_id: &str, owner_client_id: Option<&str>) {
        let mut session_manager = shared
            .session_manager
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut session = Session::new(get_session_id(chat_id));
        if let Some(owner) = owner_client_id {
            session.metadata.insert(
                SESSION_WEBSOCKET_OWNER_CLIENT_ID_KEY.to_string(),
                serde_json::json!(owner),
            );
        }
        session_manager.save(session).unwrap();
    }

    #[test]
    fn owner_allows_access_ignores_ownership_when_require_auth_is_true() {
        let shared = test_shared("browser");
        save_session_with_owner(&shared, "chat-1", Some("someone-else"));
        let session_manager = shared
            .session_manager
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        assert!(owner_allows_access(
            &shared,
            &session_manager,
            &get_session_id("chat-1"),
            "client-1"
        ));
    }

    #[test]
    fn owner_allows_access_allows_a_session_that_does_not_exist_yet() {
        let mut shared = test_shared("browser");
        shared.require_auth = false;
        let session_manager = shared
            .session_manager
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        assert!(owner_allows_access(
            &shared,
            &session_manager,
            &get_session_id("brand-new"),
            "client-1"
        ));
    }

    #[test]
    fn owner_allows_access_allows_the_matching_owner() {
        let mut shared = test_shared("browser");
        shared.require_auth = false;
        save_session_with_owner(&shared, "chat-1", Some("client-1"));
        let session_manager = shared
            .session_manager
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        assert!(owner_allows_access(
            &shared,
            &session_manager,
            &get_session_id("chat-1"),
            "client-1"
        ));
    }

    #[test]
    fn owner_allows_access_denies_a_mismatched_owner() {
        let mut shared = test_shared("browser");
        shared.require_auth = false;
        save_session_with_owner(&shared, "chat-1", Some("someone-else"));
        let session_manager = shared
            .session_manager
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        assert!(!owner_allows_access(
            &shared,
            &session_manager,
            &get_session_id("chat-1"),
            "client-1"
        ));
    }

    #[test]
    fn owner_allows_access_denies_an_unowned_session_fail_closed() {
        let mut shared = test_shared("browser");
        shared.require_auth = false;
        save_session_with_owner(&shared, "chat-1", None);
        let session_manager = shared
            .session_manager
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        assert!(!owner_allows_access(
            &shared,
            &session_manager,
            &get_session_id("chat-1"),
            "client-1"
        ));
    }

    #[tokio::test]
    async fn handle_envelope_list_chats_scopes_to_owner_when_require_auth_is_false() {
        let mut shared = test_shared("browser");
        shared.require_auth = false;
        save_session_with_owner(&shared, "mine", Some("client-1"));
        save_session_with_owner(&shared, "theirs", Some("client-2"));
        save_session_with_owner(&shared, "legacy", None);
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "initial-chat", tx);

        let mut envelope: Envelope = HashMap::new();
        envelope.insert("type".to_string(), serde_json::json!("list_chats"));
        let ctx = EnvelopeDispatchContext {
            envelope: &envelope,
            connection_id: "conn-1",
            client_id: "client-1",
            shared: &shared,
            remote_addr: addr("127.0.0.1"),
            webui_authenticated: false,
        };

        dispatch_envelope(ctx).await;

        let body = recv_json(&mut rx);
        assert_eq!(body["event"], "chats");
        let chats = body["chats"].as_array().expect("chats should be an array");
        assert_eq!(
            chats.len(),
            1,
            "only client-1's own chat should be listed: {chats:?}"
        );
        assert_eq!(chats[0]["chat_id"], "mine");
        assert!(
            chats[0].get("owner_client_id").is_none(),
            "owner_client_id is internal bookkeeping, not part of the wire shape: {chats:?}"
        );
    }

    #[tokio::test]
    async fn handle_envelope_list_chats_keeps_global_listing_when_require_auth_is_true() {
        let shared = test_shared("browser");
        save_session_with_owner(&shared, "mine", Some("client-1"));
        save_session_with_owner(&shared, "theirs", Some("client-2"));
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "initial-chat", tx);

        let mut envelope: Envelope = HashMap::new();
        envelope.insert("type".to_string(), serde_json::json!("list_chats"));
        let ctx = EnvelopeDispatchContext {
            envelope: &envelope,
            connection_id: "conn-1",
            client_id: "client-1",
            shared: &shared,
            remote_addr: addr("127.0.0.1"),
            webui_authenticated: false,
        };

        dispatch_envelope(ctx).await;

        let body = recv_json(&mut rx);
        let chats = body["chats"].as_array().expect("chats should be an array");
        assert_eq!(chats.len(), 2, "requireAuth == true keeps the global list");
    }

    // --- handle_envelope_rename_chat ---

    async fn dispatch_rename(
        shared: &WsShared,
        connection_id: &str,
        chat_id: Option<serde_json::Value>,
        title: Option<serde_json::Value>,
    ) {
        let mut envelope: Envelope = HashMap::new();
        envelope.insert("type".to_string(), serde_json::json!("rename_chat"));
        if let Some(chat_id) = chat_id {
            envelope.insert("chat_id".to_string(), chat_id);
        }
        if let Some(title) = title {
            envelope.insert("title".to_string(), title);
        }
        let ctx = EnvelopeDispatchContext {
            envelope: &envelope,
            connection_id,
            client_id: "client-1",
            shared,
            remote_addr: addr("127.0.0.1"),
            webui_authenticated: false,
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), dispatch_envelope(ctx))
            .await
            .expect("handle_envelope_rename_chat must not hang");
    }

    #[tokio::test]
    async fn handle_envelope_rename_chat_persists_title_and_sends_chat_renamed() {
        let shared = test_shared("browser");
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);
        {
            let mut session_manager = shared
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            session_manager
                .save(Session::new("websocket:chat-1".to_string()))
                .unwrap();
        }

        dispatch_rename(
            &shared,
            "conn-1",
            Some(serde_json::json!("chat-1")),
            Some(serde_json::json!("  Fix the login bug  ")),
        )
        .await;

        let body = recv_json(&mut rx);
        assert_eq!(body["event"], "chat_renamed");
        assert_eq!(body["chat_id"], "chat-1");
        assert_eq!(body["title"], "Fix the login bug");
        assert!(rx.try_recv().is_err(), "rename must send exactly one frame");

        let session = {
            let session_manager = shared
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            session_manager
                .get_session_internal("websocket:chat-1")
                .expect("session must still exist")
        };
        assert_eq!(
            session
                .metadata
                .get(crate::session::SESSION_TITLE_METADATA_KEY),
            Some(&serde_json::json!("Fix the login bug"))
        );
    }

    #[tokio::test]
    async fn handle_envelope_rename_chat_rejects_invalid_or_missing_chat_id() {
        let shared = test_shared("browser");
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "initial-chat", tx);

        for chat_id in [
            None,
            Some(serde_json::json!("")),
            Some(serde_json::json!("has space")),
            Some(serde_json::json!(123)),
        ] {
            dispatch_rename(
                &shared,
                "conn-1",
                chat_id.clone(),
                Some(serde_json::json!("A title")),
            )
            .await;
            let body = recv_json(&mut rx);
            assert_eq!(body["event"], "error", "rejected payload: {chat_id:?}");
            assert_eq!(body["detail"], "invalid chat_id");
            assert!(
                body.get("chat_id").is_none(),
                "invalid rename must omit chat_id, matching attach: {body}"
            );
        }
    }

    #[tokio::test]
    async fn handle_envelope_rename_chat_rejects_missing_or_empty_title() {
        let shared = test_shared("browser");
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);
        {
            let mut session_manager = shared
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            session_manager
                .save(Session::new("websocket:chat-1".to_string()))
                .unwrap();
        }

        for title in [
            None,
            Some(serde_json::json!("")),
            Some(serde_json::json!("   ")),
            Some(serde_json::json!(123)),
        ] {
            dispatch_rename(
                &shared,
                "conn-1",
                Some(serde_json::json!("chat-1")),
                title.clone(),
            )
            .await;
            let body = recv_json(&mut rx);
            assert_eq!(body["event"], "error", "rejected payload: {title:?}");
            assert_eq!(body["detail"], "missing title");
            assert_eq!(body["chat_id"], "chat-1");
        }

        let session = {
            let session_manager = shared
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            session_manager
                .get_session_internal("websocket:chat-1")
                .expect("session must still exist")
        };
        assert!(
            session
                .metadata
                .get(crate::session::SESSION_TITLE_METADATA_KEY)
                .is_none(),
            "a rejected rename must not persist a title: {:?}",
            session.metadata
        );
    }

    #[tokio::test]
    async fn handle_envelope_rename_chat_rejects_unknown_session() {
        let shared = test_shared("browser");
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);

        dispatch_rename(
            &shared,
            "conn-1",
            Some(serde_json::json!("missing-chat")),
            Some(serde_json::json!("A title")),
        )
        .await;

        let body = recv_json(&mut rx);
        assert_eq!(body["event"], "error");
        assert_eq!(body["detail"], "session_not_found");
        assert_eq!(body["chat_id"], "missing-chat");
        assert!(
            !body["detail"]
                .as_str()
                .unwrap_or_default()
                .contains("websocket:"),
            "wire error must not leak the internal session key: {body}"
        );
    }

    async fn dispatch_delete(
        shared: &WsShared,
        connection_id: &str,
        chat_id: Option<serde_json::Value>,
    ) {
        let mut envelope: Envelope = HashMap::new();
        envelope.insert("type".to_string(), serde_json::json!("delete_chat"));
        if let Some(chat_id) = chat_id {
            envelope.insert("chat_id".to_string(), chat_id);
        }
        let ctx = EnvelopeDispatchContext {
            envelope: &envelope,
            connection_id,
            client_id: "client-1",
            shared,
            remote_addr: addr("127.0.0.1"),
            webui_authenticated: false,
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), dispatch_envelope(ctx))
            .await
            .expect("handle_envelope_delete_chat must not hang");
    }

    #[tokio::test]
    async fn handle_envelope_delete_chat_unlinks_session_and_sends_chat_deleted() {
        let shared = test_shared("browser");
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);
        {
            let mut session_manager = shared
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            session_manager
                .save(Session::new("websocket:chat-1".to_string()))
                .unwrap();
        }

        dispatch_delete(&shared, "conn-1", Some(serde_json::json!("chat-1"))).await;

        let body = recv_json(&mut rx);
        assert_eq!(body["event"], "chat_deleted");
        assert_eq!(body["chat_id"], "chat-1");
        assert!(
            rx.try_recv().is_err(),
            "delete must send exactly one frame to the requester"
        );

        let session_manager = shared
            .session_manager
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        assert!(
            session_manager
                .get_session_internal("websocket:chat-1")
                .is_none(),
            "deleted session must be gone from cache"
        );
    }

    #[tokio::test]
    async fn handle_envelope_delete_chat_notifies_requester_even_when_not_subscribed() {
        // The requester's connection is attached to "chat-1" (its currently
        // active chat) but asks to delete "chat-2" — some other row in its
        // sidebar list. `detach_chat` alone would not return this
        // connection since it isn't subscribed to "chat-2", so the fix
        // must fall back to notifying the requester directly.
        let shared = test_shared("browser");
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);
        {
            let mut session_manager = shared
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            session_manager
                .save(Session::new("websocket:chat-2".to_string()))
                .unwrap();
        }

        dispatch_delete(&shared, "conn-1", Some(serde_json::json!("chat-2"))).await;

        let body = recv_json(&mut rx);
        assert_eq!(body["event"], "chat_deleted");
        assert_eq!(body["chat_id"], "chat-2");
        assert!(
            rx.try_recv().is_err(),
            "delete must send exactly one frame to the requester"
        );

        // The requester's own subscription to "chat-1" must be untouched.
        assert_eq!(
            shared
                .connections
                .lock()
                .await
                .senders_for_chat("chat-1")
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn handle_envelope_delete_chat_notifies_every_subscribed_connection() {
        let shared = test_shared("browser");
        let (tx1, mut rx1) = mpsc::unbounded_channel::<Message>();
        let (tx2, mut rx2) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx1);
        shared.connections.lock().await.attach("conn-2", "chat-1");
        shared
            .connections
            .lock()
            .await
            .register("conn-2", "chat-1", tx2);
        {
            let mut session_manager = shared
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            session_manager
                .save(Session::new("websocket:chat-1".to_string()))
                .unwrap();
        }

        dispatch_delete(&shared, "conn-1", Some(serde_json::json!("chat-1"))).await;

        let body1 = recv_json(&mut rx1);
        assert_eq!(body1["event"], "chat_deleted");
        assert_eq!(body1["chat_id"], "chat-1");
        let body2 = recv_json(&mut rx2);
        assert_eq!(body2["event"], "chat_deleted");
        assert_eq!(body2["chat_id"], "chat-1");

        assert!(
            shared
                .connections
                .lock()
                .await
                .senders_for_chat("chat-1")
                .is_empty(),
            "every connection must be detached from the deleted chat"
        );
    }

    #[tokio::test]
    async fn handle_envelope_delete_chat_rejects_invalid_or_missing_chat_id() {
        let shared = test_shared("browser");
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "initial-chat", tx);

        for chat_id in [
            None,
            Some(serde_json::json!("")),
            Some(serde_json::json!("has space")),
            Some(serde_json::json!(123)),
        ] {
            dispatch_delete(&shared, "conn-1", chat_id.clone()).await;
            let body = recv_json(&mut rx);
            assert_eq!(body["event"], "error", "rejected payload: {chat_id:?}");
            assert_eq!(body["detail"], "invalid chat_id");
            assert!(
                body.get("chat_id").is_none(),
                "invalid delete must omit chat_id, matching attach/rename: {body}"
            );
        }
    }

    #[tokio::test]
    async fn handle_envelope_delete_chat_rejects_unknown_session() {
        let shared = test_shared("browser");
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);

        dispatch_delete(&shared, "conn-1", Some(serde_json::json!("missing-chat"))).await;

        let body = recv_json(&mut rx);
        assert_eq!(body["event"], "error");
        assert_eq!(body["detail"], "session_not_found");
        assert_eq!(body["chat_id"], "missing-chat");
        assert!(
            !body["detail"]
                .as_str()
                .unwrap_or_default()
                .contains("websocket:"),
            "wire error must not leak the internal session key: {body}"
        );
    }

    #[tokio::test]
    async fn handle_envelope_delete_chat_denies_a_sender_outside_the_allow_list() {
        let mut shared = test_shared("browser");
        shared.channels_config = ChannelsConfig {
            allow_from: vec!["someone-else".to_string()],
            ..ChannelsConfig::default()
        };
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);
        {
            let mut session_manager = shared
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            session_manager
                .save(Session::new("websocket:chat-1".to_string()))
                .unwrap();
        }

        dispatch_delete(&shared, "conn-1", Some(serde_json::json!("chat-1"))).await;

        let body = recv_json(&mut rx);
        assert_eq!(body["event"], "error");
        assert_eq!(body["detail"], "access_denied");
        assert_eq!(body["chat_id"], "chat-1");

        let session_manager = shared
            .session_manager
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        assert!(
            session_manager
                .get_session_internal("websocket:chat-1")
                .is_some(),
            "a denied delete must not remove the session"
        );
    }

    #[tokio::test]
    async fn handle_envelope_delete_chat_denies_a_guest_deleting_someone_elses_chat() {
        let mut shared = test_shared("browser");
        shared.require_auth = false;
        save_session_with_owner(&shared, "chat-1", Some("someone-else"));
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);

        // `dispatch_delete` always uses `client_id: "client-1"`.
        dispatch_delete(&shared, "conn-1", Some(serde_json::json!("chat-1"))).await;

        let body = recv_json(&mut rx);
        assert_eq!(body["event"], "error");
        assert_eq!(body["detail"], "access_denied");

        let session_manager = shared
            .session_manager
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        assert!(
            session_manager
                .get_session_internal("websocket:chat-1")
                .is_some(),
            "a denied delete must not remove the session"
        );
    }

    #[tokio::test]
    async fn handle_envelope_delete_chat_then_save_does_not_recreate_the_jsonl() {
        let shared = test_shared("browser");
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);
        let snapshot = {
            let mut session_manager = shared
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let mut session = Session::new("websocket:chat-1".to_string());
            session.add_message("user", "hello", serde_json::Map::new());
            session_manager.save(session.clone()).unwrap();
            session
        };

        dispatch_delete(&shared, "conn-1", Some(serde_json::json!("chat-1"))).await;
        let _ = recv_json(&mut rx);

        // A write that raced past the delete (e.g. a turn that cloned the
        // session before `delete_chat` ran) must not resurrect the file.
        let mut session_manager = shared
            .session_manager
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        session_manager
            .save(snapshot)
            .expect("save after delete must not error");
        assert!(
            session_manager
                .get_session_internal("websocket:chat-1")
                .is_none(),
            "save on a tombstoned key must not resurrect the cache entry"
        );
        let path = session_manager.sessions_dir.join(format!(
            "{}.jsonl",
            crate::utils::helpers::safe_filename("websocket:chat-1")
        ));
        assert!(
            !path.exists(),
            "save on a tombstoned key must not recreate the jsonl file"
        );
    }

    async fn dispatch_clear_session(
        shared: &WsShared,
        connection_id: &str,
        chat_id: Option<serde_json::Value>,
    ) {
        let mut envelope: Envelope = HashMap::new();
        envelope.insert("type".to_string(), serde_json::json!("clear_session"));
        if let Some(chat_id) = chat_id {
            envelope.insert("chat_id".to_string(), chat_id);
        }
        let ctx = EnvelopeDispatchContext {
            envelope: &envelope,
            connection_id,
            client_id: "client-1",
            shared,
            remote_addr: addr("127.0.0.1"),
            webui_authenticated: false,
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), dispatch_envelope(ctx))
            .await
            .expect("handle_envelope_clear_session must not hang");
    }

    #[tokio::test]
    async fn handle_envelope_clear_session_wipes_conversation_and_keeps_the_session() {
        let shared = test_shared("browser");
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);
        start_registry_turn(&shared, "chat-1", Some("turn-1"));
        {
            let mut session_manager = shared
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let mut session = Session::new("websocket:chat-1".to_string());
            session.add_message("user", "hello", serde_json::Map::new());
            session.metadata.insert(
                crate::session::GOAL_STATE_KEY.to_string(),
                serde_json::json!({"objective": "ship it"}),
            );
            session.update_usage(LLMUsage {
                input_tokens: Some(10),
                output_tokens: Some(2),
                ..LLMUsage::new()
            });
            session.metadata.insert(
                crate::session::SESSION_TITLE_METADATA_KEY.to_string(),
                serde_json::json!("Keep me"),
            );
            session.metadata.insert(
                SESSION_MODEL_PRESET_METADATA_KEY.to_string(),
                serde_json::json!("fast"),
            );
            session_manager.save(session).unwrap();
        }
        {
            let mut transcripts = shared
                .gateway_services
                .transcripts
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            transcripts.append_user_message("chat-1", "hello", &HashMap::new(), None, None, None);
        }

        dispatch_clear_session(&shared, "conn-1", Some(serde_json::json!("chat-1"))).await;

        let body = recv_json(&mut rx);
        assert_eq!(body["event"], "session_cleared");
        assert_eq!(body["chat_id"], "chat-1");
        assert!(
            body.get("detail").is_none(),
            "ack must carry chat_id only, matching chat_deleted: {body}"
        );
        assert!(
            rx.try_recv().is_err(),
            "clear must send exactly one frame to the requester"
        );
        assert!(
            !chat_is_running(&shared, "chat-1"),
            "an in-flight turn must be cleared so the chat is not left running"
        );

        {
            let session_manager = shared
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let session = session_manager
                .get_session_internal("websocket:chat-1")
                .expect("cleared session must still exist");
            assert!(session.messages.is_empty(), "messages must be wiped");
            assert!(
                session
                    .metadata
                    .get(crate::session::GOAL_STATE_KEY)
                    .is_none()
            );
            assert!(session.usage().is_none());
            assert_eq!(
                session
                    .metadata
                    .get(crate::session::SESSION_TITLE_METADATA_KEY),
                Some(&serde_json::json!("Keep me"))
            );
            assert_eq!(
                session.metadata.get(SESSION_MODEL_PRESET_METADATA_KEY),
                Some(&serde_json::json!("fast"))
            );
        }

        {
            let mut transcripts = shared
                .gateway_services
                .transcripts
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            assert!(
                transcripts.chat_history("websocket:chat-1", 500).is_empty(),
                "WebUI transcript must be emptied so attach cannot resurrect history"
            );
            assert!(
                transcripts.append_user_message(
                    "chat-1",
                    "after clear",
                    &HashMap::new(),
                    None,
                    None,
                    None,
                ),
                "later appends must still write — clear must not tombstone the key"
            );
            let history = transcripts.chat_history("websocket:chat-1", 500);
            assert_eq!(history.len(), 1);
            assert_eq!(history[0]["content"], "after clear");
        }
    }

    #[tokio::test]
    async fn handle_envelope_clear_session_notifies_requester_even_when_not_subscribed() {
        let shared = test_shared("browser");
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);
        {
            let mut session_manager = shared
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            session_manager
                .save(Session::new("websocket:chat-2".to_string()))
                .unwrap();
        }

        dispatch_clear_session(&shared, "conn-1", Some(serde_json::json!("chat-2"))).await;

        let body = recv_json(&mut rx);
        assert_eq!(body["event"], "session_cleared");
        assert_eq!(body["chat_id"], "chat-2");
        assert!(
            rx.try_recv().is_err(),
            "clear must send exactly one frame to the requester"
        );

        assert_eq!(
            shared
                .connections
                .lock()
                .await
                .senders_for_chat("chat-1")
                .len(),
            1,
            "the requester's subscription to a different chat must be untouched"
        );
    }

    #[tokio::test]
    async fn handle_envelope_clear_session_notifies_every_subscribed_connection() {
        let shared = test_shared("browser");
        let (tx1, mut rx1) = mpsc::unbounded_channel::<Message>();
        let (tx2, mut rx2) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx1);
        shared.connections.lock().await.attach("conn-2", "chat-1");
        shared
            .connections
            .lock()
            .await
            .register("conn-2", "chat-1", tx2);
        {
            let mut session_manager = shared
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            session_manager
                .save(Session::new("websocket:chat-1".to_string()))
                .unwrap();
        }

        dispatch_clear_session(&shared, "conn-1", Some(serde_json::json!("chat-1"))).await;

        let body1 = recv_json(&mut rx1);
        assert_eq!(body1["event"], "session_cleared");
        assert_eq!(body1["chat_id"], "chat-1");
        let body2 = recv_json(&mut rx2);
        assert_eq!(body2["event"], "session_cleared");
        assert_eq!(body2["chat_id"], "chat-1");

        assert_eq!(
            shared
                .connections
                .lock()
                .await
                .senders_for_chat("chat-1")
                .len(),
            2,
            "clear must not detach connections — the session is still live"
        );
    }

    #[tokio::test]
    async fn handle_envelope_clear_session_rejects_invalid_or_missing_chat_id() {
        let shared = test_shared("browser");
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "initial-chat", tx);

        for chat_id in [
            None,
            Some(serde_json::json!("")),
            Some(serde_json::json!("has space")),
            Some(serde_json::json!(123)),
        ] {
            dispatch_clear_session(&shared, "conn-1", chat_id.clone()).await;
            let body = recv_json(&mut rx);
            assert_eq!(body["event"], "error", "rejected payload: {chat_id:?}");
            assert_eq!(body["detail"], "invalid chat_id");
            assert!(
                body.get("chat_id").is_none(),
                "invalid clear must omit chat_id, matching attach/rename: {body}"
            );
        }
    }

    #[tokio::test]
    async fn handle_envelope_clear_session_rejects_unknown_session() {
        let shared = test_shared("browser");
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);

        dispatch_clear_session(&shared, "conn-1", Some(serde_json::json!("missing-chat"))).await;

        let body = recv_json(&mut rx);
        assert_eq!(body["event"], "error");
        assert_eq!(body["detail"], "session_not_found");
        assert_eq!(body["chat_id"], "missing-chat");
        assert!(
            !body["detail"]
                .as_str()
                .unwrap_or_default()
                .contains("websocket:"),
            "wire error must not leak the internal session key: {body}"
        );
    }

    #[tokio::test]
    async fn handle_envelope_clear_session_denies_a_sender_outside_the_allow_list() {
        let mut shared = test_shared("browser");
        shared.channels_config = ChannelsConfig {
            allow_from: vec!["someone-else".to_string()],
            ..ChannelsConfig::default()
        };
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);
        {
            let mut session_manager = shared
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let mut session = Session::new("websocket:chat-1".to_string());
            session.add_message("user", "hello", serde_json::Map::new());
            session_manager.save(session).unwrap();
        }

        dispatch_clear_session(&shared, "conn-1", Some(serde_json::json!("chat-1"))).await;

        let body = recv_json(&mut rx);
        assert_eq!(body["event"], "error");
        assert_eq!(body["detail"], "access_denied");
        assert_eq!(body["chat_id"], "chat-1");

        let session_manager = shared
            .session_manager
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let session = session_manager
            .get_session_internal("websocket:chat-1")
            .expect("a denied clear must not remove the session");
        assert_eq!(
            session.messages.len(),
            1,
            "a denied clear must not wipe messages"
        );
    }

    #[tokio::test]
    async fn handle_envelope_clear_session_denies_a_guest_clearing_someone_elses_chat() {
        let mut shared = test_shared("browser");
        shared.require_auth = false;
        {
            let mut session_manager = shared
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let mut session = Session::new("websocket:chat-1".to_string());
            session.add_message("user", "hello", serde_json::Map::new());
            session.metadata.insert(
                SESSION_WEBSOCKET_OWNER_CLIENT_ID_KEY.to_string(),
                serde_json::json!("someone-else"),
            );
            session_manager.save(session).unwrap();
        }
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);

        // `dispatch_clear_session` always uses `client_id: "client-1"`.
        dispatch_clear_session(&shared, "conn-1", Some(serde_json::json!("chat-1"))).await;

        let body = recv_json(&mut rx);
        assert_eq!(body["event"], "error");
        assert_eq!(body["detail"], "access_denied");

        let session_manager = shared
            .session_manager
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let session = session_manager
            .get_session_internal("websocket:chat-1")
            .expect("a denied clear must not remove the session");
        assert_eq!(
            session.messages.len(),
            1,
            "a denied clear must not wipe messages"
        );
    }

    async fn dispatch_abort_turn(
        shared: &WsShared,
        connection_id: &str,
        chat_id: Option<serde_json::Value>,
        turn_id: Option<serde_json::Value>,
    ) {
        let mut envelope: Envelope = HashMap::new();
        envelope.insert("type".to_string(), serde_json::json!("abort_turn"));
        if let Some(chat_id) = chat_id {
            envelope.insert("chat_id".to_string(), chat_id);
        }
        if let Some(turn_id) = turn_id {
            envelope.insert("turn_id".to_string(), turn_id);
        }
        let ctx = EnvelopeDispatchContext {
            envelope: &envelope,
            connection_id,
            client_id: "client-1",
            shared,
            remote_addr: addr("127.0.0.1"),
            webui_authenticated: false,
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), dispatch_envelope(ctx))
            .await
            .expect("handle_envelope_abort_turn must not hang");
    }

    fn start_registry_turn(shared: &WsShared, chat_id: &str, turn_id: Option<&str>) {
        shared
            .gateway_services
            .turn_registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .start_turn(chat_id, "owner-a", turn_id);
    }

    fn chat_is_running(shared: &WsShared, chat_id: &str) -> bool {
        shared
            .gateway_services
            .turn_registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .websocket_turn_wall_started_at(chat_id)
            .is_some()
    }

    #[tokio::test]
    async fn handle_envelope_abort_turn_clears_the_turn_and_acks() {
        let shared = test_shared("browser");
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);
        start_registry_turn(&shared, "chat-1", Some("turn-1"));

        dispatch_abort_turn(&shared, "conn-1", Some(serde_json::json!("chat-1")), None).await;

        let body = recv_json(&mut rx);
        assert_eq!(body["event"], "turn_aborted");
        assert_eq!(body["chat_id"], "chat-1");
        assert_eq!(
            body["turn_id"], "turn-1",
            "the ack must name the turn it ended even when the client sent no turn_id"
        );
        assert!(
            rx.try_recv().is_err(),
            "abort must send exactly one frame per connection"
        );
        assert!(!chat_is_running(&shared, "chat-1"));
    }

    #[tokio::test]
    async fn handle_envelope_abort_turn_notifies_every_subscribed_connection() {
        // A second tab attached to the same chat was rendering the same
        // stream, so it has to stop waiting on it too.
        let shared = test_shared("browser");
        let (tx1, mut rx1) = mpsc::unbounded_channel::<Message>();
        let (tx2, mut rx2) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx1);
        shared.connections.lock().await.attach("conn-2", "chat-1");
        shared
            .connections
            .lock()
            .await
            .register("conn-2", "chat-1", tx2);
        start_registry_turn(&shared, "chat-1", Some("turn-1"));

        dispatch_abort_turn(&shared, "conn-1", Some(serde_json::json!("chat-1")), None).await;

        assert_eq!(recv_json(&mut rx1)["event"], "turn_aborted");
        assert_eq!(recv_json(&mut rx2)["event"], "turn_aborted");
    }

    #[tokio::test]
    async fn handle_envelope_abort_turn_keeps_the_session_intact() {
        // The whole point of a separate envelope: unlike `delete_chat`, an
        // abort must leave the chat and its history behind.
        let shared = test_shared("browser");
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);
        {
            let mut session_manager = shared
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            session_manager
                .save(Session::new("websocket:chat-1".to_string()))
                .unwrap();
        }

        dispatch_abort_turn(&shared, "conn-1", Some(serde_json::json!("chat-1")), None).await;
        assert_eq!(recv_json(&mut rx)["event"], "turn_aborted");

        assert_eq!(
            shared
                .connections
                .lock()
                .await
                .senders_for_chat("chat-1")
                .len(),
            1,
            "abort must not detach the connection from the chat"
        );
        let session_manager = shared
            .session_manager
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        assert!(
            session_manager
                .get_session_internal("websocket:chat-1")
                .is_some(),
            "abort must not delete the session"
        );
    }

    #[tokio::test]
    async fn handle_envelope_abort_turn_rejects_a_stale_turn_id() {
        let shared = test_shared("browser");
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);
        start_registry_turn(&shared, "chat-1", Some("turn-2"));

        dispatch_abort_turn(
            &shared,
            "conn-1",
            Some(serde_json::json!("chat-1")),
            Some(serde_json::json!("turn-1")),
        )
        .await;

        let body = recv_json(&mut rx);
        assert_eq!(body["event"], "error");
        assert_eq!(body["detail"], "turn_not_active");
        assert_eq!(body["turn_id"], "turn-1");
        assert!(
            chat_is_running(&shared, "chat-1"),
            "a stale abort must leave the turn that actually is running alone"
        );
    }

    #[tokio::test]
    async fn handle_envelope_abort_turn_accepts_a_turn_id_the_projection_does_not_know() {
        // `turn_id` only reaches the projection for turns that arrived over a
        // WebUI envelope carrying one, so `None` there means "identity
        // unknown", not "idle" — the abort must still go through.
        let shared = test_shared("browser");
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);
        start_registry_turn(&shared, "chat-1", None);

        dispatch_abort_turn(
            &shared,
            "conn-1",
            Some(serde_json::json!("chat-1")),
            Some(serde_json::json!("turn-1")),
        )
        .await;

        let body = recv_json(&mut rx);
        assert_eq!(body["event"], "turn_aborted");
        assert_eq!(
            body["turn_id"], "turn-1",
            "with nothing to echo, the ack falls back to the requested turn_id"
        );
        assert!(!chat_is_running(&shared, "chat-1"));
    }

    #[tokio::test]
    async fn handle_envelope_abort_turn_on_an_idle_chat_still_acks() {
        // Losing the race against turn completion must not leave the client
        // without a reply — otherwise its Stop button spins forever.
        let shared = test_shared("browser");
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);

        dispatch_abort_turn(&shared, "conn-1", Some(serde_json::json!("chat-1")), None).await;

        let body = recv_json(&mut rx);
        assert_eq!(body["event"], "turn_aborted");
        assert_eq!(body["chat_id"], "chat-1");
        assert!(
            body.get("turn_id").is_none(),
            "no turn to name, so no turn_id on the wire: {body}"
        );
    }

    #[tokio::test]
    async fn handle_envelope_abort_turn_rejects_invalid_or_missing_chat_id() {
        let shared = test_shared("browser");
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "initial-chat", tx);

        for chat_id in [
            None,
            Some(serde_json::json!("")),
            Some(serde_json::json!("has space")),
            Some(serde_json::json!(123)),
        ] {
            dispatch_abort_turn(&shared, "conn-1", chat_id.clone(), None).await;
            let body = recv_json(&mut rx);
            assert_eq!(body["event"], "error", "rejected payload: {chat_id:?}");
            assert_eq!(body["detail"], "invalid chat_id");
            assert!(
                body.get("chat_id").is_none(),
                "invalid abort must omit chat_id, matching attach/rename/delete: {body}"
            );
        }
    }

    #[tokio::test]
    async fn handle_envelope_abort_turn_denies_a_sender_outside_the_allow_list() {
        let mut shared = test_shared("browser");
        shared.channels_config = ChannelsConfig {
            allow_from: vec!["someone-else".to_string()],
            ..ChannelsConfig::default()
        };
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);
        start_registry_turn(&shared, "chat-1", Some("turn-1"));

        dispatch_abort_turn(&shared, "conn-1", Some(serde_json::json!("chat-1")), None).await;

        let body = recv_json(&mut rx);
        assert_eq!(body["event"], "error");
        assert_eq!(body["detail"], "access_denied");
        assert_eq!(body["chat_id"], "chat-1");
        assert!(
            chat_is_running(&shared, "chat-1"),
            "a denied abort must not cancel the turn"
        );
    }

    #[tokio::test]
    async fn handle_envelope_rename_chat_denies_a_sender_outside_the_allow_list() {
        let mut shared = test_shared("browser");
        shared.channels_config = ChannelsConfig {
            allow_from: vec!["someone-else".to_string()],
            ..ChannelsConfig::default()
        };
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);
        {
            let mut session_manager = shared
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let mut session = Session::new("websocket:chat-1".to_string());
            session.metadata.insert(
                crate::session::SESSION_TITLE_METADATA_KEY.to_string(),
                serde_json::json!("Keep me"),
            );
            session_manager.save(session).unwrap();
        }

        dispatch_rename(
            &shared,
            "conn-1",
            Some(serde_json::json!("chat-1")),
            Some(serde_json::json!("Hijacked")),
        )
        .await;

        let body = recv_json(&mut rx);
        assert_eq!(body["event"], "error");
        assert_eq!(body["detail"], "access_denied");
        assert_eq!(body["chat_id"], "chat-1");

        let session = {
            let session_manager = shared
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            session_manager
                .get_session_internal("websocket:chat-1")
                .expect("session must still exist")
        };
        assert_eq!(
            session
                .metadata
                .get(crate::session::SESSION_TITLE_METADATA_KEY),
            Some(&serde_json::json!("Keep me"))
        );
    }

    #[tokio::test]
    async fn handle_envelope_rename_chat_denies_a_guest_renaming_someone_elses_chat() {
        let mut shared = test_shared("browser");
        shared.require_auth = false;
        {
            let mut session_manager = shared
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let mut session = Session::new("websocket:chat-1".to_string());
            session.metadata.insert(
                crate::session::SESSION_TITLE_METADATA_KEY.to_string(),
                serde_json::json!("Keep me"),
            );
            session.metadata.insert(
                SESSION_WEBSOCKET_OWNER_CLIENT_ID_KEY.to_string(),
                serde_json::json!("someone-else"),
            );
            session_manager.save(session).unwrap();
        }
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);

        // `dispatch_rename` always uses `client_id: "client-1"`.
        dispatch_rename(
            &shared,
            "conn-1",
            Some(serde_json::json!("chat-1")),
            Some(serde_json::json!("Hijacked")),
        )
        .await;

        let body = recv_json(&mut rx);
        assert_eq!(body["event"], "error");
        assert_eq!(body["detail"], "access_denied");

        let session_manager = shared
            .session_manager
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let session = session_manager
            .get_session_internal("websocket:chat-1")
            .expect("session must still exist");
        assert_eq!(
            session
                .metadata
                .get(crate::session::SESSION_TITLE_METADATA_KEY),
            Some(&serde_json::json!("Keep me")),
            "a denied rename must not change the title"
        );
    }

    // --- handle_envelope_set_model_preset ---

    async fn dispatch_set_model_preset(
        shared: &WsShared,
        connection_id: &str,
        chat_id: Option<serde_json::Value>,
        model_preset: Option<serde_json::Value>,
    ) {
        let mut envelope: Envelope = HashMap::new();
        envelope.insert("type".to_string(), serde_json::json!("set_model_preset"));
        if let Some(chat_id) = chat_id {
            envelope.insert("chat_id".to_string(), chat_id);
        }
        if let Some(model_preset) = model_preset {
            envelope.insert("model_preset".to_string(), model_preset);
        }
        let ctx = EnvelopeDispatchContext {
            envelope: &envelope,
            connection_id,
            client_id: "client-1",
            shared,
            remote_addr: addr("127.0.0.1"),
            webui_authenticated: false,
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), dispatch_envelope(ctx))
            .await
            .expect("handle_envelope_set_model_preset must not hang");
    }

    fn test_runtime_resolver_with_preset(name: &str, model: &str) -> Arc<ModelRuntimeResolver> {
        use crate::config::schema::{Config, ModelPresetConfig};
        let mut config = Config::default();
        config.agents.provider = "anthropic".to_string();
        config.providers.anthropic.api_key = "test-key".to_string();
        config.model_presets.insert(
            name.to_string(),
            ModelPresetConfig {
                model: model.to_string(),
                provider: "anthropic".to_string(),
                ..Default::default()
            },
        );
        let provider = crate::providers::factory::create_provider_for(
            &config,
            &config.agents.model,
            &config.agents.provider,
        )
        .expect("test provider");
        Arc::new(ModelRuntimeResolver::new(config, provider))
    }

    #[tokio::test]
    async fn handle_envelope_set_model_preset_persists_named_preset_and_acks() {
        let mut shared = test_shared("browser");
        shared.runtime_resolver = test_runtime_resolver_with_preset("fast", "claude-haiku");
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);
        {
            let mut session_manager = shared
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            session_manager
                .save(Session::new("websocket:chat-1".to_string()))
                .unwrap();
        }

        dispatch_set_model_preset(
            &shared,
            "conn-1",
            Some(serde_json::json!("chat-1")),
            Some(serde_json::json!("  fast  ")),
        )
        .await;

        let body = recv_json(&mut rx);
        assert_eq!(body["event"], "model_preset_set");
        assert_eq!(body["chat_id"], "chat-1");
        assert_eq!(body["model_preset"], "fast");
        assert_eq!(body["model"], "claude-haiku");
        assert!(
            rx.try_recv().is_err(),
            "set_model_preset must send exactly one frame"
        );

        let session = {
            let session_manager = shared
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            session_manager
                .get_session_internal("websocket:chat-1")
                .expect("session must still exist")
        };
        assert_eq!(
            session.metadata.get(SESSION_MODEL_PRESET_METADATA_KEY),
            Some(&serde_json::json!("fast"))
        );
    }

    #[tokio::test]
    async fn handle_envelope_set_model_preset_default_clears_the_override() {
        let shared = test_shared("browser");
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);
        {
            let mut session_manager = shared
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let mut session = Session::new("websocket:chat-1".to_string());
            session.metadata.insert(
                SESSION_MODEL_PRESET_METADATA_KEY.to_string(),
                serde_json::json!("fast"),
            );
            session_manager.save(session).unwrap();
        }

        dispatch_set_model_preset(
            &shared,
            "conn-1",
            Some(serde_json::json!("chat-1")),
            Some(serde_json::json!("default")),
        )
        .await;

        let body = recv_json(&mut rx);
        assert_eq!(body["event"], "model_preset_set");
        assert_eq!(body["model_preset"], "default");
        assert!(
            rx.try_recv().is_err(),
            "set_model_preset must send exactly one frame"
        );

        let session = {
            let session_manager = shared
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            session_manager
                .get_session_internal("websocket:chat-1")
                .expect("session must still exist")
        };
        assert!(
            session
                .metadata
                .get(SESSION_MODEL_PRESET_METADATA_KEY)
                .is_none(),
            "default must remove the session override, not store the string"
        );
    }

    #[tokio::test]
    async fn handle_envelope_set_model_preset_rejects_unknown_preset_without_mutating() {
        let shared = test_shared("browser");
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);
        {
            let mut session_manager = shared
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            session_manager
                .save(Session::new("websocket:chat-1".to_string()))
                .unwrap();
        }

        dispatch_set_model_preset(
            &shared,
            "conn-1",
            Some(serde_json::json!("chat-1")),
            Some(serde_json::json!("nope")),
        )
        .await;

        let body = recv_json(&mut rx);
        assert_eq!(body["event"], "error");
        assert_eq!(body["detail"], "invalid_model_preset");
        assert_eq!(body["chat_id"], "chat-1");

        let session = {
            let session_manager = shared
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            session_manager
                .get_session_internal("websocket:chat-1")
                .expect("session must still exist")
        };
        assert!(
            session
                .metadata
                .get(SESSION_MODEL_PRESET_METADATA_KEY)
                .is_none()
        );
    }

    #[tokio::test]
    async fn handle_envelope_set_model_preset_rejects_missing_or_empty_preset() {
        let shared = test_shared("browser");
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);
        {
            let mut session_manager = shared
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            session_manager
                .save(Session::new("websocket:chat-1".to_string()))
                .unwrap();
        }

        dispatch_set_model_preset(&shared, "conn-1", Some(serde_json::json!("chat-1")), None).await;
        let body = recv_json(&mut rx);
        assert_eq!(body["event"], "error");
        assert_eq!(body["detail"], "missing_model_preset");
        assert_eq!(body["chat_id"], "chat-1");

        dispatch_set_model_preset(
            &shared,
            "conn-1",
            Some(serde_json::json!("chat-1")),
            Some(serde_json::json!("   ")),
        )
        .await;
        let body = recv_json(&mut rx);
        assert_eq!(body["event"], "error");
        assert_eq!(body["detail"], "missing_model_preset");
    }

    #[tokio::test]
    async fn handle_envelope_set_model_preset_rejects_unknown_session() {
        let mut shared = test_shared("browser");
        shared.runtime_resolver = test_runtime_resolver_with_preset("fast", "claude-haiku");
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);

        dispatch_set_model_preset(
            &shared,
            "conn-1",
            Some(serde_json::json!("chat-1")),
            Some(serde_json::json!("fast")),
        )
        .await;

        let body = recv_json(&mut rx);
        assert_eq!(body["event"], "error");
        assert_eq!(body["detail"], "session_not_found");
        assert_eq!(body["chat_id"], "chat-1");
    }

    #[tokio::test]
    async fn handle_envelope_set_model_preset_denies_a_sender_outside_the_allow_list() {
        let mut shared = test_shared("browser");
        shared.runtime_resolver = test_runtime_resolver_with_preset("fast", "claude-haiku");
        shared.channels_config = ChannelsConfig {
            allow_from: vec!["someone-else".to_string()],
            ..ChannelsConfig::default()
        };
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);
        {
            let mut session_manager = shared
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            session_manager
                .save(Session::new("websocket:chat-1".to_string()))
                .unwrap();
        }

        dispatch_set_model_preset(
            &shared,
            "conn-1",
            Some(serde_json::json!("chat-1")),
            Some(serde_json::json!("fast")),
        )
        .await;

        let body = recv_json(&mut rx);
        assert_eq!(body["event"], "error");
        assert_eq!(body["detail"], "access_denied");
        assert_eq!(body["chat_id"], "chat-1");

        let session = {
            let session_manager = shared
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            session_manager
                .get_session_internal("websocket:chat-1")
                .expect("session must still exist")
        };
        assert!(
            session
                .metadata
                .get(SESSION_MODEL_PRESET_METADATA_KEY)
                .is_none()
        );
    }

    // --- handle_envelope_set_mode ---

    async fn dispatch_set_mode(
        shared: &WsShared,
        connection_id: &str,
        chat_id: Option<serde_json::Value>,
        mode: Option<serde_json::Value>,
    ) {
        let mut envelope: Envelope = HashMap::new();
        envelope.insert("type".to_string(), serde_json::json!("set_mode"));
        if let Some(chat_id) = chat_id {
            envelope.insert("chat_id".to_string(), chat_id);
        }
        if let Some(mode) = mode {
            envelope.insert("mode".to_string(), mode);
        }
        let ctx = EnvelopeDispatchContext {
            envelope: &envelope,
            connection_id,
            client_id: "client-1",
            shared,
            remote_addr: addr("127.0.0.1"),
            webui_authenticated: false,
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), dispatch_envelope(ctx))
            .await
            .expect("handle_envelope_set_mode must not hang");
    }

    #[tokio::test]
    async fn handle_envelope_set_mode_persists_minimal_and_acks() {
        let shared = test_shared("browser");
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);
        {
            let mut session_manager = shared
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            session_manager
                .save(Session::new("websocket:chat-1".to_string()))
                .unwrap();
        }

        dispatch_set_mode(
            &shared,
            "conn-1",
            Some(serde_json::json!("chat-1")),
            Some(serde_json::json!("  MINIMAL  ")),
        )
        .await;

        let body = recv_json(&mut rx);
        assert_eq!(body["event"], "mode_set");
        assert_eq!(body["chat_id"], "chat-1");
        assert_eq!(body["mode"], "minimal");
        assert!(
            rx.try_recv().is_err(),
            "set_mode must send exactly one frame"
        );

        let session = {
            let session_manager = shared
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            session_manager
                .get_session_internal("websocket:chat-1")
                .expect("session must still exist")
        };
        assert_eq!(
            session.metadata.get(SESSION_AGENT_MODE_METADATA_KEY),
            Some(&serde_json::json!("minimal"))
        );
    }

    #[tokio::test]
    async fn handle_envelope_set_mode_default_clears_the_override() {
        let shared = test_shared("browser");
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);
        {
            let mut session_manager = shared
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let mut session = Session::new("websocket:chat-1".to_string());
            session.metadata.insert(
                SESSION_AGENT_MODE_METADATA_KEY.to_string(),
                serde_json::json!("minimal"),
            );
            session_manager.save(session).unwrap();
        }

        dispatch_set_mode(
            &shared,
            "conn-1",
            Some(serde_json::json!("chat-1")),
            Some(serde_json::json!("default")),
        )
        .await;

        let body = recv_json(&mut rx);
        assert_eq!(body["event"], "mode_set");
        assert_eq!(body["mode"], "standard");
        assert!(
            rx.try_recv().is_err(),
            "set_mode must send exactly one frame"
        );

        let session = {
            let session_manager = shared
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            session_manager
                .get_session_internal("websocket:chat-1")
                .expect("session must still exist")
        };
        assert!(
            session
                .metadata
                .get(SESSION_AGENT_MODE_METADATA_KEY)
                .is_none()
        );
    }

    #[tokio::test]
    async fn handle_envelope_set_mode_rejects_unknown_without_mutating() {
        let shared = test_shared("browser");
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);
        {
            let mut session_manager = shared
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            session_manager
                .save(Session::new("websocket:chat-1".to_string()))
                .unwrap();
        }

        dispatch_set_mode(
            &shared,
            "conn-1",
            Some(serde_json::json!("chat-1")),
            Some(serde_json::json!("ptc")),
        )
        .await;

        let body = recv_json(&mut rx);
        assert_eq!(body["event"], "error");
        assert_eq!(body["detail"], "invalid_mode");
        let session = {
            let session_manager = shared
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            session_manager
                .get_session_internal("websocket:chat-1")
                .expect("session must still exist")
        };
        assert!(
            session
                .metadata
                .get(SESSION_AGENT_MODE_METADATA_KEY)
                .is_none()
        );
    }

    #[tokio::test]
    async fn handle_envelope_set_mode_rejects_missing_or_empty() {
        let shared = test_shared("browser");
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);
        {
            let mut session_manager = shared
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            session_manager
                .save(Session::new("websocket:chat-1".to_string()))
                .unwrap();
        }

        dispatch_set_mode(&shared, "conn-1", Some(serde_json::json!("chat-1")), None).await;
        let body = recv_json(&mut rx);
        assert_eq!(body["event"], "error");
        assert_eq!(body["detail"], "missing_mode");

        dispatch_set_mode(
            &shared,
            "conn-1",
            Some(serde_json::json!("chat-1")),
            Some(serde_json::json!("   ")),
        )
        .await;
        let body = recv_json(&mut rx);
        assert_eq!(body["event"], "error");
        assert_eq!(body["detail"], "missing_mode");
    }

    #[tokio::test]
    async fn handle_envelope_set_mode_rejects_unknown_session() {
        let shared = test_shared("browser");
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);

        dispatch_set_mode(
            &shared,
            "conn-1",
            Some(serde_json::json!("chat-1")),
            Some(serde_json::json!("minimal")),
        )
        .await;

        let body = recv_json(&mut rx);
        assert_eq!(body["event"], "error");
        assert_eq!(body["detail"], "session_not_found");
    }

    #[tokio::test]
    async fn maybe_push_active_goal_state_noop_when_no_session_file_exists() {
        let shared = test_shared("browser");
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);

        maybe_push_active_goal_state("chat-1", &shared).await;

        assert!(rx.try_recv().is_err(), "expected no frame to be pushed");
    }

    #[tokio::test]
    async fn maybe_push_active_goal_state_pushes_when_a_goal_is_active() {
        let shared = test_shared("browser");
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);
        {
            let mut session_manager = shared.session_manager.lock().unwrap();
            crate::session::goal_state::create_session_goal(
                &mut session_manager,
                "websocket:chat-1",
                "ship the feature",
                None,
            )
            .unwrap();
        }

        maybe_push_active_goal_state("chat-1", &shared).await;

        let msg = rx.try_recv().expect("expected a delivered frame");
        let body: serde_json::Value = serde_json::from_str(&msg.into_text().unwrap()).unwrap();
        assert_eq!(body["event"], "goal_state");
        assert_eq!(body["goal_state"]["objective"], "ship the feature");
    }

    // --- send_goal_status / maybe_push_turn_run_wall_clock ---

    #[tokio::test]
    async fn send_goal_status_includes_started_at_only_when_running() {
        let shared = test_shared("browser");
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);

        send_goal_status("chat-1", "running", Some(123.5), None, &shared).await;

        let msg = rx.try_recv().expect("expected a delivered frame");
        let body: serde_json::Value = serde_json::from_str(&msg.into_text().unwrap()).unwrap();
        assert_eq!(body["event"], "goal_status");
        assert_eq!(body["status"], "running");
        assert_eq!(body["started_at"], 123.5);
        assert!(body.get("turn_id").is_none());
    }

    #[tokio::test]
    async fn send_goal_status_omits_started_at_when_not_running() {
        let shared = test_shared("browser");
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);

        send_goal_status("chat-1", "idle", Some(123.5), None, &shared).await;

        let msg = rx.try_recv().expect("expected a delivered frame");
        let body: serde_json::Value = serde_json::from_str(&msg.into_text().unwrap()).unwrap();
        assert_eq!(body["status"], "idle");
        assert!(body.get("started_at").is_none());
    }

    #[tokio::test]
    async fn send_goal_status_includes_turn_id_when_present_and_non_empty() {
        let shared = test_shared("browser");
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);

        send_goal_status(
            "chat-1",
            "running",
            Some(1.0),
            Some("turn-1".to_string()),
            &shared,
        )
        .await;
        let msg = rx.try_recv().expect("expected a delivered frame");
        let body: serde_json::Value = serde_json::from_str(&msg.into_text().unwrap()).unwrap();
        assert_eq!(body["turn_id"], "turn-1");

        send_goal_status("chat-1", "running", Some(1.0), Some(String::new()), &shared).await;
        let msg = rx.try_recv().expect("expected a second delivered frame");
        let body: serde_json::Value = serde_json::from_str(&msg.into_text().unwrap()).unwrap();
        assert!(
            body.get("turn_id").is_none(),
            "empty turn_id should be omitted"
        );
    }

    #[tokio::test]
    async fn send_goal_status_noop_when_no_subscribers() {
        let shared = test_shared("browser");
        send_goal_status("chat-1", "running", Some(1.0), None, &shared).await;
    }

    #[tokio::test]
    async fn maybe_push_turn_run_wall_clock_noop_when_chat_not_running() {
        let shared = test_shared("browser");
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);

        maybe_push_turn_run_wall_clock("chat-1", &shared).await;

        assert!(rx.try_recv().is_err(), "expected no frame to be pushed");
    }

    #[tokio::test]
    async fn maybe_push_turn_run_wall_clock_pushes_running_status_when_active() {
        let shared = test_shared("browser");
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        shared
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);
        shared
            .gateway_services
            .turn_registry
            .lock()
            .unwrap()
            .start_turn("chat-1", "owner-1", Some("turn-1"));

        maybe_push_turn_run_wall_clock("chat-1", &shared).await;

        let msg = rx.try_recv().expect("expected a delivered frame");
        let body: serde_json::Value = serde_json::from_str(&msg.into_text().unwrap()).unwrap();
        assert_eq!(body["event"], "goal_status");
        assert_eq!(body["status"], "running");
        assert_eq!(body["turn_id"], "turn-1");
        assert!(body["started_at"].is_number());
    }

    // --- send / send_delta / send_reasoning_delta / send_reasoning_end / send_file_edit_events ---

    fn test_channel() -> WebSocketChannel {
        let dir = tempfile::tempdir().unwrap();
        let mut channel = WebSocketChannel::new(
            WebSocketConfig::default(),
            Arc::new(MessageBus::new()),
            ChannelsConfig::default(),
            Arc::new(StdMutex::new(SessionManager::new(dir.keep()))),
            WorkspaceRequestHandler::new(tempfile::tempdir().unwrap().keep(), true),
            ModelRuntimeResolver::for_tests(),
        );
        // `WebSocketChannel::new` defaults `gateway_services` to the real
        // `get_webui_dir()` (production behavior). Tests that exercise
        // transcript persistence (`send`/`send_delta`/etc.) must not touch
        // that real directory, so point it at a tempdir instead — same
        // isolation `test_shared()` already gives `WsShared`.
        channel.gateway_services =
            Arc::new(GatewayServices::new(tempfile::tempdir().unwrap().keep()));
        channel
    }

    fn webui_meta(turn_id: &str) -> HashMap<String, serde_json::Value> {
        HashMap::from([
            ("webui".to_string(), serde_json::json!(true)),
            (
                WEBUI_TURN_METADATA_KEY.to_string(),
                serde_json::json!(turn_id),
            ),
        ])
    }

    fn outbound(chat_id: &str, content: &str, event: Option<OutboundEvent>) -> OutboundMessage {
        OutboundMessage {
            channel: "websocket".to_string(),
            chat_id: chat_id.to_string(),
            content: content.to_string(),
            reply_to: None,
            media: Vec::new(),
            metadata: HashMap::new(),
            event,
        }
    }

    #[tokio::test]
    async fn send_plain_message_has_no_kind_field() {
        let channel = test_channel();
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        channel
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);

        BaseChannel::send(&channel, outbound("chat-1", "hi", None))
            .await
            .unwrap();

        let msg = rx.try_recv().expect("expected a delivered frame");
        let body: serde_json::Value = serde_json::from_str(&msg.into_text().unwrap()).unwrap();
        assert_eq!(body["event"], "message");
        assert_eq!(body["chat_id"], "chat-1");
        assert_eq!(body["text"], "hi");
        assert!(body.get("kind").is_none());
    }

    #[tokio::test]
    async fn send_tool_hint_progress_sets_kind_tool_hint() {
        let channel = test_channel();
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        channel
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);
        let event = Some(OutboundEvent::Progress(ProgressEvent {
            kind: ProgressKind::ToolHint,
            tool_events: Some(vec![]),
            ..Default::default()
        }));

        BaseChannel::send(&channel, outbound("chat-1", "read_file(...)", event))
            .await
            .unwrap();

        let msg = rx.try_recv().expect("expected a delivered frame");
        let body: serde_json::Value = serde_json::from_str(&msg.into_text().unwrap()).unwrap();
        assert_eq!(body["kind"], "tool_hint");
    }

    #[tokio::test]
    async fn send_plain_progress_sets_kind_progress_and_includes_tool_events() {
        let channel = test_channel();
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        channel
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);
        let event = Some(OutboundEvent::Progress(ProgressEvent {
            kind: ProgressKind::Plain,
            tool_events: Some(vec![crate::bus::outbound_events::ToolEvent {
                name: "read_file".to_string(),
                status: "ok".to_string(),
                detail: None,
            }]),
            ..Default::default()
        }));

        BaseChannel::send(&channel, outbound("chat-1", "working...", event))
            .await
            .unwrap();

        let msg = rx.try_recv().expect("expected a delivered frame");
        let body: serde_json::Value = serde_json::from_str(&msg.into_text().unwrap()).unwrap();
        assert_eq!(body["kind"], "progress");
        assert_eq!(body["tool_events"][0]["name"], "read_file");
    }

    #[tokio::test]
    async fn send_progress_with_file_edit_events_delegates_instead_of_sending_message() {
        let channel = test_channel();
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        channel
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);
        let mut edit = FileEditEvent::new();
        edit.insert("path".to_string(), "foo.rs".to_string());
        let event = Some(OutboundEvent::Progress(ProgressEvent {
            file_edit_events: Some(vec![edit]),
            ..Default::default()
        }));

        BaseChannel::send(&channel, outbound("chat-1", "", event))
            .await
            .unwrap();

        let msg = rx.try_recv().expect("expected a delivered frame");
        let body: serde_json::Value = serde_json::from_str(&msg.into_text().unwrap()).unwrap();
        assert_eq!(body["event"], "file_edit");
        assert_eq!(body["edits"][0]["path"], "foo.rs");
    }

    #[tokio::test]
    async fn send_unmapped_event_kind_is_skipped_without_erroring() {
        let channel = test_channel();
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        channel
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);
        let event = Some(OutboundEvent::RetryWait(Default::default()));

        let result = BaseChannel::send(&channel, outbound("chat-1", "", event)).await;

        assert_eq!(result, Ok(()));
        assert!(rx.try_recv().is_err(), "no frame should have been sent");
    }

    #[tokio::test]
    async fn send_turn_end_clears_registry_and_announces_idle() {
        let channel = test_channel();
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        channel
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);
        channel
            .gateway_services
            .turn_registry
            .lock()
            .unwrap()
            .start_turn("chat-1", "owner-1", Some("turn-1"));

        let mut msg = outbound(
            "chat-1",
            "",
            Some(OutboundEvent::TurnEnd(Default::default())),
        );
        msg.metadata.insert(
            WEBSOCKET_TURN_OWNER_METADATA_KEY.to_string(),
            serde_json::json!("owner-1"),
        );
        msg.metadata.insert(
            WEBUI_TURN_METADATA_KEY.to_string(),
            serde_json::json!("turn-1"),
        );

        let result = BaseChannel::send(&channel, msg).await;
        assert_eq!(result, Ok(()));

        let frame = rx.try_recv().expect("expected a goal_status idle frame");
        let body: serde_json::Value = serde_json::from_str(&frame.into_text().unwrap()).unwrap();
        assert_eq!(body["event"], "goal_status");
        assert_eq!(body["status"], "idle");
        assert_eq!(body["turn_id"], "turn-1");
        assert!(body.get("started_at").is_none());
        assert!(
            channel
                .gateway_services
                .turn_registry
                .lock()
                .unwrap()
                .websocket_turn_wall_started_at("chat-1")
                .is_none()
        );
    }

    #[tokio::test]
    async fn send_turn_end_without_owner_clears_the_chat() {
        let channel = test_channel();
        channel
            .gateway_services
            .turn_registry
            .lock()
            .unwrap()
            .start_turn("chat-1", "owner-1", None);

        let result = BaseChannel::send(
            &channel,
            outbound(
                "chat-1",
                "",
                Some(OutboundEvent::TurnEnd(Default::default())),
            ),
        )
        .await;
        assert_eq!(result, Ok(()));
        assert!(
            channel
                .gateway_services
                .turn_registry
                .lock()
                .unwrap()
                .websocket_turn_wall_started_at("chat-1")
                .is_none()
        );
    }

    #[tokio::test]
    async fn send_turn_end_fans_out_session_updated_with_token_usage_when_session_has_it() {
        let channel = test_channel();
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        channel
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);
        {
            let mut session_manager = channel
                .base
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let mut session = Session::new("websocket:chat-1".to_string());
            session.update_usage(LLMUsage {
                input_tokens: Some(30),
                output_tokens: Some(10),
                ..LLMUsage::new()
            });
            session_manager.save(session).unwrap();
        }

        let result = BaseChannel::send(
            &channel,
            outbound(
                "chat-1",
                "",
                Some(OutboundEvent::TurnEnd(Default::default())),
            ),
        )
        .await;
        assert_eq!(result, Ok(()));

        let goal_status = rx.try_recv().expect("expected a goal_status idle frame");
        let goal_status_body: serde_json::Value =
            serde_json::from_str(&goal_status.into_text().unwrap()).unwrap();
        assert_eq!(goal_status_body["event"], "goal_status");

        let session_updated = rx.try_recv().expect("expected a session_updated frame");
        let body: serde_json::Value =
            serde_json::from_str(&session_updated.into_text().unwrap()).unwrap();
        assert_eq!(body["event"], "session_updated");
        assert_eq!(body["chat_id"], "chat-1");
        assert_eq!(body["scope"], "metadata");
        assert_eq!(body["token_usage"]["input_tokens"], 30);
        assert_eq!(body["token_usage"]["output_tokens"], 10);
    }

    #[tokio::test]
    async fn send_turn_end_does_not_fan_out_session_updated_when_session_has_no_usage() {
        let channel = test_channel();
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        channel
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);

        let result = BaseChannel::send(
            &channel,
            outbound(
                "chat-1",
                "",
                Some(OutboundEvent::TurnEnd(Default::default())),
            ),
        )
        .await;
        assert_eq!(result, Ok(()));

        let goal_status = rx.try_recv().expect("expected a goal_status idle frame");
        let goal_status_body: serde_json::Value =
            serde_json::from_str(&goal_status.into_text().unwrap()).unwrap();
        assert_eq!(goal_status_body["event"], "goal_status");
        assert!(
            rx.try_recv().is_err(),
            "no session usage yet must not produce a session_updated frame"
        );
    }

    #[tokio::test]
    async fn send_no_recipients_is_an_error() {
        let channel = test_channel();
        let result = BaseChannel::send(&channel, outbound("chat-1", "hi", None)).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn send_delta_non_end_sends_delta_event_and_buffers() {
        let channel = test_channel();
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        channel
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);
        let mut meta = HashMap::new();
        meta.insert("_stream_id".to_string(), serde_json::json!("s1"));

        BaseChannel::send_delta(&channel, "chat-1", "Hello", Some(meta))
            .await
            .unwrap();

        let msg = rx.try_recv().expect("expected a delivered frame");
        let body: serde_json::Value = serde_json::from_str(&msg.into_text().unwrap()).unwrap();
        assert_eq!(body["event"], "delta");
        assert_eq!(body["text"], "Hello");
        assert_eq!(body["stream_id"], "s1");
    }

    #[tokio::test]
    async fn send_delta_stream_end_flushes_buffered_text() {
        let channel = test_channel();
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        channel
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);
        let mut meta = HashMap::new();
        meta.insert("_stream_id".to_string(), serde_json::json!("s1"));
        BaseChannel::send_delta(&channel, "chat-1", "Hello ", Some(meta.clone()))
            .await
            .unwrap();
        rx.try_recv().unwrap();

        meta.insert("_stream_end".to_string(), serde_json::json!(true));
        BaseChannel::send_delta(&channel, "chat-1", "world", Some(meta))
            .await
            .unwrap();

        let msg = rx.try_recv().expect("expected a delivered frame");
        let body: serde_json::Value = serde_json::from_str(&msg.into_text().unwrap()).unwrap();
        assert_eq!(body["event"], "stream_end");
        assert_eq!(body["text"], "Hello world");
    }

    #[tokio::test]
    async fn send_delta_stream_end_with_empty_delta_still_echoes_buffered_text() {
        let channel = test_channel();
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        channel
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);
        let mut meta = HashMap::new();
        meta.insert("_stream_id".to_string(), serde_json::json!("s1"));
        BaseChannel::send_delta(&channel, "chat-1", "Hello", Some(meta.clone()))
            .await
            .unwrap();
        rx.try_recv().unwrap();

        meta.insert("_stream_end".to_string(), serde_json::json!(true));
        BaseChannel::send_delta(&channel, "chat-1", "", Some(meta))
            .await
            .unwrap();

        let msg = rx.try_recv().expect("expected a delivered frame");
        let body: serde_json::Value = serde_json::from_str(&msg.into_text().unwrap()).unwrap();
        assert_eq!(body["event"], "stream_end");
        assert_eq!(body["text"], "Hello");
    }

    #[tokio::test]
    async fn send_delta_stream_end_omits_text_when_nothing_was_buffered() {
        let channel = test_channel();
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        channel
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);
        let mut meta = HashMap::new();
        meta.insert("_stream_id".to_string(), serde_json::json!("s1"));
        meta.insert("_stream_end".to_string(), serde_json::json!(true));
        BaseChannel::send_delta(&channel, "chat-1", "", Some(meta))
            .await
            .unwrap();

        let msg = rx.try_recv().expect("expected a delivered frame");
        let body: serde_json::Value = serde_json::from_str(&msg.into_text().unwrap()).unwrap();
        assert_eq!(body["event"], "stream_end");
        assert!(body.get("text").is_none());
    }

    #[tokio::test]
    async fn send_delta_merge_next_keeps_buffer_for_next_segment() {
        let channel = test_channel();
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        channel
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);
        let mut meta = HashMap::new();
        meta.insert("_stream_id".to_string(), serde_json::json!("s1"));
        meta.insert("_stream_end".to_string(), serde_json::json!(true));
        meta.insert("_merge_next".to_string(), serde_json::json!(true));
        BaseChannel::send_delta(&channel, "chat-1", "Hello", Some(meta.clone()))
            .await
            .unwrap();
        rx.try_recv().unwrap();

        // A following segment under the same stream_id should still see the
        // earlier buffered text, since merge_next peeked rather than popped.
        meta.remove("_stream_end");
        meta.remove("_merge_next");
        BaseChannel::send_delta(&channel, "chat-1", " world", Some(meta.clone()))
            .await
            .unwrap();
        rx.try_recv().unwrap();
        meta.insert("_stream_end".to_string(), serde_json::json!(true));
        BaseChannel::send_delta(&channel, "chat-1", "!", Some(meta))
            .await
            .unwrap();

        let msg = rx.try_recv().expect("expected a delivered frame");
        let body: serde_json::Value = serde_json::from_str(&msg.into_text().unwrap()).unwrap();
        assert_eq!(body["text"], "Hello world!");
    }

    #[tokio::test]
    async fn send_reasoning_delta_skips_empty_delta() {
        let channel = test_channel();
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        channel
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);

        BaseChannel::send_reasoning_delta(&channel, "chat-1", "", None)
            .await
            .unwrap();

        assert!(rx.try_recv().is_err(), "empty delta must not be sent");
    }

    #[tokio::test]
    async fn send_reasoning_delta_and_end_wire_shapes() {
        let channel = test_channel();
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        channel
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);

        BaseChannel::send_reasoning_delta(&channel, "chat-1", "thinking", None)
            .await
            .unwrap();
        let msg = rx.try_recv().expect("expected a delivered frame");
        let body: serde_json::Value = serde_json::from_str(&msg.into_text().unwrap()).unwrap();
        assert_eq!(body["event"], "reasoning_delta");
        assert_eq!(body["text"], "thinking");

        BaseChannel::send_reasoning_end(&channel, "chat-1", None)
            .await
            .unwrap();
        let msg = rx.try_recv().expect("expected a second delivered frame");
        let body: serde_json::Value = serde_json::from_str(&msg.into_text().unwrap()).unwrap();
        assert_eq!(body["event"], "reasoning_end");
    }

    // --- outbound transcript persistence ---

    fn transcript_rows(channel: &WebSocketChannel, chat_id: &str) -> Vec<serde_json::Value> {
        channel
            .gateway_services
            .transcripts
            .lock()
            .unwrap()
            .read_transcript_lines(&get_session_id(chat_id))
    }

    #[tokio::test]
    async fn send_plain_message_persists_answer_row_when_webui() {
        let channel = test_channel();
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        channel
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);
        let mut msg = outbound("chat-1", "hi there", None);
        msg.metadata = webui_meta("turn-1");

        BaseChannel::send(&channel, msg).await.unwrap();
        rx.try_recv().unwrap();

        let rows = transcript_rows(&channel, "chat-1");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["event"], "message");
        assert_eq!(rows[0]["text"], "hi there");
        assert_eq!(rows[0]["turn_phase"], "answer");
    }

    #[tokio::test]
    async fn send_tool_hint_progress_persists_activity_row() {
        let channel = test_channel();
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        channel
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);
        let mut msg = outbound(
            "chat-1",
            "reading foo.rs",
            Some(OutboundEvent::Progress(ProgressEvent {
                kind: ProgressKind::ToolHint,
                ..Default::default()
            })),
        );
        msg.metadata = webui_meta("turn-1");

        BaseChannel::send(&channel, msg).await.unwrap();
        rx.try_recv().unwrap();

        let rows = transcript_rows(&channel, "chat-1");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["kind"], "tool_hint");
        assert_eq!(rows[0]["turn_phase"], "activity");
    }

    #[tokio::test]
    async fn send_skips_transcript_write_without_webui_metadata() {
        let channel = test_channel();
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        channel
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);

        BaseChannel::send(&channel, outbound("chat-1", "hi there", None))
            .await
            .unwrap();
        rx.try_recv().unwrap();

        assert!(transcript_rows(&channel, "chat-1").is_empty());
    }

    #[tokio::test]
    async fn send_file_edit_events_persists_activity_row() {
        let channel = test_channel();
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        channel
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);
        let mut edit = FileEditEvent::new();
        edit.insert("path".to_string(), "foo.rs".to_string());
        let mut msg = outbound(
            "chat-1",
            "",
            Some(OutboundEvent::Progress(ProgressEvent {
                file_edit_events: Some(vec![edit]),
                ..Default::default()
            })),
        );
        msg.metadata = webui_meta("turn-1");

        BaseChannel::send(&channel, msg).await.unwrap();
        rx.try_recv().unwrap();

        let rows = transcript_rows(&channel, "chat-1");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["event"], "file_edit");
        assert_eq!(rows[0]["turn_phase"], "activity");
    }

    #[tokio::test]
    async fn send_turn_end_persists_turn_end_row_when_webui() {
        let channel = test_channel();
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        channel
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);
        channel
            .gateway_services
            .turn_registry
            .lock()
            .unwrap()
            .start_turn("chat-1", "owner-1", Some("turn-1"));

        let mut msg = outbound(
            "chat-1",
            "",
            Some(OutboundEvent::TurnEnd(
                crate::bus::outbound_events::TurnEndEvent {
                    latency_ms: Some(42),
                    goal_state: None,
                },
            )),
        );
        msg.metadata.insert(
            WEBSOCKET_TURN_OWNER_METADATA_KEY.to_string(),
            serde_json::json!("owner-1"),
        );
        msg.metadata.extend(webui_meta("turn-1"));

        BaseChannel::send(&channel, msg).await.unwrap();
        rx.try_recv().unwrap();

        let rows = transcript_rows(&channel, "chat-1");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["event"], "turn_end");
        assert_eq!(rows[0]["turn_id"], "turn-1");
        assert_eq!(rows[0]["latency_ms"], 42);
    }

    #[tokio::test]
    async fn send_delta_non_end_chunk_does_not_persist() {
        let channel = test_channel();
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        channel
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);
        let mut meta = webui_meta("turn-1");
        meta.insert("_stream_id".to_string(), serde_json::json!("s1"));

        BaseChannel::send_delta(&channel, "chat-1", "Hello", Some(meta))
            .await
            .unwrap();
        rx.try_recv().unwrap();

        assert!(transcript_rows(&channel, "chat-1").is_empty());
    }

    #[tokio::test]
    async fn send_delta_stream_end_persists_completed_text_as_message_row() {
        let channel = test_channel();
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        channel
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);
        let mut meta = webui_meta("turn-1");
        meta.insert("_stream_id".to_string(), serde_json::json!("s1"));
        BaseChannel::send_delta(&channel, "chat-1", "Hello ", Some(meta.clone()))
            .await
            .unwrap();
        rx.try_recv().unwrap();

        meta.insert("_stream_end".to_string(), serde_json::json!(true));
        BaseChannel::send_delta(&channel, "chat-1", "world", Some(meta))
            .await
            .unwrap();
        rx.try_recv().unwrap();

        let rows = transcript_rows(&channel, "chat-1");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["event"], "message");
        assert_eq!(rows[0]["text"], "Hello world");
        assert_eq!(rows[0]["turn_id"], "turn-1");
    }

    #[tokio::test]
    async fn send_delta_stream_end_with_no_text_does_not_persist() {
        let channel = test_channel();
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        channel
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);
        let mut meta = webui_meta("turn-1");
        meta.insert("_stream_id".to_string(), serde_json::json!("s1"));
        meta.insert("_stream_end".to_string(), serde_json::json!(true));

        BaseChannel::send_delta(&channel, "chat-1", "", Some(meta))
            .await
            .unwrap();
        rx.try_recv().unwrap();

        assert!(transcript_rows(&channel, "chat-1").is_empty());
    }

    #[tokio::test]
    async fn send_reasoning_delta_does_not_persist() {
        let channel = test_channel();
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        channel
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);

        BaseChannel::send_reasoning_delta(
            &channel,
            "chat-1",
            "thinking",
            Some(webui_meta("turn-1")),
        )
        .await
        .unwrap();
        rx.try_recv().unwrap();

        assert!(transcript_rows(&channel, "chat-1").is_empty());
    }

    #[tokio::test]
    async fn send_reasoning_end_persists_assembled_reasoning_text() {
        let channel = test_channel();
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        channel
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);
        let meta = webui_meta("turn-1");

        BaseChannel::send_reasoning_delta(&channel, "chat-1", "thinking ", Some(meta.clone()))
            .await
            .unwrap();
        rx.try_recv().unwrap();
        BaseChannel::send_reasoning_delta(&channel, "chat-1", "hard", Some(meta.clone()))
            .await
            .unwrap();
        rx.try_recv().unwrap();
        BaseChannel::send_reasoning_end(&channel, "chat-1", Some(meta))
            .await
            .unwrap();
        rx.try_recv().unwrap();

        let rows = transcript_rows(&channel, "chat-1");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["event"], "reasoning_end");
        assert_eq!(rows[0]["text"], "thinking hard");
        assert_eq!(rows[0]["turn_phase"], "reasoning");
    }

    #[tokio::test]
    async fn implements_send_delta_is_true() {
        assert!(BaseChannel::implements_send_delta(&test_channel()));
    }

    // --- start() / stop() / shutdown_signal() / router() ---
    // `start()` no longer binds a `TcpListener` (that's owned externally by
    // `cli::commands::run_gateway`), so it's testable here without a real
    // socket: it should mark the channel running, block until `stop()`'s
    // shutdown signal fires, then mark it not-running again.

    #[tokio::test]
    async fn start_waits_for_shutdown_signal_then_stops_and_clears_connections() {
        let channel = Arc::new(test_channel());
        let (tx, _rx) = mpsc::unbounded_channel::<Message>();
        channel
            .connections
            .lock()
            .await
            .register("conn-1", "chat-1", tx);

        assert!(!BaseChannel::running(channel.as_ref()));

        let channel_for_start = Arc::clone(&channel);
        let start_handle = tokio::spawn(async move {
            BaseChannel::start(channel_for_start.as_ref()).await;
        });

        // Poll briefly until `start()` has flipped `running` to true (it does
        // so before awaiting the shutdown signal).
        for _ in 0..100 {
            if BaseChannel::running(channel.as_ref()) {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(BaseChannel::running(channel.as_ref()));

        BaseChannel::stop(channel.as_ref()).await;
        start_handle.await.unwrap();

        assert!(!BaseChannel::running(channel.as_ref()));
        assert!(
            channel
                .connections
                .lock()
                .await
                .senders_for_chat("chat-1")
                .is_empty(),
            "connections must be cleared once start() observes shutdown"
        );
    }

    #[test]
    fn shutdown_signal_shares_the_same_notify_stop_uses() {
        let channel = test_channel();
        // Same `Arc<Notify>` allocation as `self.shutdown` — pointer equality
        // confirms an externally-owned `axum::serve` shutdown future waiting
        // on this accessor observes the exact signal `BaseChannel::stop` fires.
        assert!(Arc::ptr_eq(&channel.shutdown_signal(), &channel.shutdown));
    }

    #[test]
    fn router_mounts_the_configured_path() {
        let channel = test_channel();
        let router = channel.router();
        // `Router` doesn't expose its route table for direct inspection, but
        // building it at all (with `.with_state` already applied, i.e.
        // `Router<()>`) is what `run_gateway` needs to `.merge()` it into the
        // combined server — this is a compile-and-construct smoke test.
        let _: Router = router;
    }
}
