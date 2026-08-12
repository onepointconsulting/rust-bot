use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
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
use tokio::{
    net::TcpListener,
    sync::{Mutex as AsyncMutex, Notify, mpsc},
};
use uuid::Uuid;

use crate::channels::base::handle_message;
use crate::channels::gateway_services::GatewayServices;
use crate::channels::websocket::registry::ConnectionRegistry;
use crate::channels::websocket::types::{
    ConnectionRegistryHandle, Envelope, EnvelopeDispatchContext, EnvelopeType, WebSocketConfig,
    WsOutboundEvent, WsShared, WsUpgradeQuery,
};
use crate::channels::websocket::webui::metadata::WEBSOCKET_TURN_OWNER_METADATA_KEY;
use crate::channels::websocket::webui::transcript::client_turn_metadata;
use crate::command::normalize_command_text;
use crate::command::types::{ChatCommand, CommandLifecycle};
use crate::runtime_context::{RUNTIME_CONTEXT_INPUT_META, webui_quote_runtime_context};
use crate::security::{WORKSPACE_SCOPE_METADATA_KEY, WorkspaceScope, WorkspaceScopeError};
use crate::session::goal_state::goal_state_ws_blob;
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
    config::schema::ChannelsConfig,
    security::attachment_ingress::store_inbound_attachments,
    security::jwt::{JwtValidationOpts, validate_jwt_token},
    security::workspace_requests::WorkspaceRequestHandler,
    session::manager::SessionManager,
};

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

/// Custom JWT claim value marking a token as minted for the WebUI frontend
/// specifically, distinct from `aud` (which, for this channel, is already
/// pinned to the route path — see `validate_jwt_aud_matches_path`). Checked
/// by [`authorize`]; mint one via `generate-jwt generate-jwt-token --purpose
/// webui` — see `security::jwt::Claims::purpose`.
const WEBUI_JWT_PURPOSE: &str = "webui";

/// Reject the upgrade with 401 when JWT auth is enabled and the token is
/// missing/invalid. No-op (always `Ok`) when JWT is disabled.
///
/// Returns whether the connection's JWT proves it was minted for the WebUI
/// frontend (`purpose == "webui"`) — `false` whenever there's no JWT to make
/// that claim from (JWT disabled), not just when validation fails. Mirrors
/// nanobot's `_webui_connections` gate (`channels/websocket/runtime.py:458-462`),
/// which is only ever populated by a token issued specifically for webui use.
fn authorize(shared: &WsShared, token: Option<&str>) -> Result<bool, StatusCode> {
    let Some(public_key_pem) = shared.jwt_public_key_pem.as_ref() else {
        return Ok(false);
    };
    let token = token
        .filter(|t| !t.trim().is_empty())
        .ok_or(StatusCode::UNAUTHORIZED)?;
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
async fn send_event(
    shared: &WsShared,
    connection_id: &str,
    event: WsOutboundEvent,
    base_fields: Option<&serde_json::Map<String, serde_json::Value>>,
    fields: serde_json::Value,
) {
    let sender = shared.connections.lock().await.sender_for(connection_id);
    let Some(sender) = sender else { return };

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

    if sender
        .send(Message::text(
            serde_json::Value::Object(payload).to_string(),
        ))
        .is_err()
    {
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
    // comment at runtime.py:578).
    let ready = serde_json::json!({
        "event": WsOutboundEvent::Ready.as_str(),
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
        EnvelopeType::NewChat => { /* ... */ }
        EnvelopeType::ForkChat => { /* ... */ }
        EnvelopeType::Attach => { /* ... */ }
        EnvelopeType::SetWorkspaceScope => { /* ... */ }
        EnvelopeType::TranscribeAudio => { /* ... */ }
        EnvelopeType::Message => {
            handle_envelope_message(envelope_dispatch_context).await;
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

async fn handle_envelope_message<'a>(envelope_dispatch_context: EnvelopeDispatchContext<'a>) {
    let envelope = envelope_dispatch_context.envelope;
    let connection_id = envelope_dispatch_context.connection_id;
    let client_id = envelope_dispatch_context.client_id;
    let shared = envelope_dispatch_context.shared;

    let cid = envelope
        .get("chat_id")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    if !is_valid_chat_id(cid) {
        send_event(
            shared,
            connection_id,
            WsOutboundEvent::Error,
            None,
            serde_json::json!({"detail": "invalid chat_id"}),
        )
        .await;
        return;
    }

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
                ws_shared._workspace_request_handler.scope_for_message(
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
    let Some(scope) = workspace_scope_or_error(shared, cid, turn_id, connection_id, resolver).await
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
        metadata.insert(
            "webui".to_string(),
            serde_json::json!(true),
        );
        metadata.extend(client_turn_metadata(envelope.get("turn_id")));
    }
    let cli_apps_raw = envelope.get("cli_apps");
    let cli_apps = normalize_cli_app_mentions(cli_apps_raw);
    if !cli_apps.is_empty() {
        metadata.insert(
            "cli_apps".to_string(),
            serde_json::json!(cli_apps),
        );
    }
    let mcp_presets = crate::agent::tools::mcp::mcp_presets_api::normalize_mcp_preset_mentions(envelope.get("mcp_presets"));
    if !mcp_presets.is_empty() {
        metadata.insert(
            "mcp_presets".to_string(),
            serde_json::json!(mcp_presets),
        );
    }
    metadata.insert(WORKSPACE_SCOPE_METADATA_KEY.to_string(), scope.metadata());
    {
        // Recover from a poisoned mutex rather than panicking the WS handler —
        // same pattern as the scope resolver above.
        let mut session_manager = shared
            .session_manager
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        shared
            ._workspace_request_handler
            .persist_scope(&mut session_manager, cid, &scope);
    }

    let is_webui = metadata.get("webui").and_then(|v| v.as_bool()) == Some(true);
    let webui_quote_allowed = webui_quote_allowed(is_webui, envelope_dispatch_context.webui_authenticated);
    let mut queued_owner_metadata: Option<String> = None;
    if is_webui && builtin_command_starts_agent_turn(content) {
        let mut turn_registry = shared.gateway_services.turn_registry.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(queued_owner) = turn_registry.register_queued_turn_if_idle(cid, turn_id) {
            metadata.insert(WEBSOCKET_TURN_OWNER_METADATA_KEY.to_string(), serde_json::json!(queued_owner));
            queued_owner_metadata = Some(queued_owner);
        }
    }
    if is_webui {
        // Recover from a poisoned mutex rather than panicking the WS handler —
        // same pattern as the turn registry lock above.
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
        if webui_quote_allowed
            && let Some(block) = webui_quote_runtime_context(envelope.get("quoted_context"))
        {
            metadata.insert(
                RUNTIME_CONTEXT_INPUT_META.to_string(),
                serde_json::to_value([block]).unwrap_or(serde_json::Value::Null),
            );
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
async fn workspace_scope_or_error(
    shared: &WsShared,
    cid: &str,
    turn_id: Option<&str>,
    connection_id: &str,
    resolver: ScopeResolver,
) -> Option<WorkspaceScope> {
    let err = match resolver().await {
        Ok(scope) => return Some(scope),
        Err(err) => err,
    };
    let mut base_fields = serde_json::Map::new();
    base_fields.insert(
        "chat_id".to_string(),
        serde_json::Value::String(cid.to_string()),
    );
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
        session_manager.read_session_file(format!("websocket:{chat_id}").as_str())
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
            session_manager: Arc::clone(&self.base.session_manager),
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
    use crate::config::schema::JwtConfig;

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
        let items: Vec<serde_json::Value> =
            (0..12).map(|i| serde_json::json!({"name": format!("app{i}")})).collect();
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
            session_manager: Arc::new(StdMutex::new(SessionManager::new(dir.keep()))),
            _workspace_request_handler: WorkspaceRequestHandler::new(
                tempfile::tempdir().unwrap().keep(),
                true,
            ),
            runtime_surface: runtime_surface.to_string(),
            gateway_services: Arc::new(GatewayServices::new(tempfile::tempdir().unwrap().keep())),
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
        shared.jwt_public_key_pem =
            Some(Arc::new(std::fs::read(&keys.public_key_path).unwrap()));
        (shared, keys.private_key_path)
    }

    fn mint_token_with_purpose(private_key_path: &std::path::Path, purpose: Option<&str>) -> String {
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
            workspace_scope_or_error(&shared, "chat-1", Some("turn-1"), "conn-1", resolver).await;

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
            workspace_scope_or_error(&shared, "chat-1", Some("turn-1"), "conn-1", resolver).await;

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
}
