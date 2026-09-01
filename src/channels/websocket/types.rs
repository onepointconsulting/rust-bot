//! Plain data types for the WebSocket channel — config schema, per-connection
//! shared state, and the envelope-dispatch context. Split out of `runtime.rs`
//! (which holds the channel's actual behavior: handlers, dispatch, and the
//! `WebSocketChannel`/`BaseChannel` impl) purely to keep that file's size
//! manageable; nothing here has any logic beyond `WebSocketConfig`'s own
//! deserialization/validation.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};

use garde::{Report, Validate};
use serde::{Deserialize, Deserializer, Serialize};
use tokio::sync::Mutex as AsyncMutex;

use crate::agent::model_runtime::ModelRuntimeResolver;
use crate::{
    bus::queue::MessageBus,
    channels::gateway_services::GatewayServices,
    channels::websocket::registry::ConnectionRegistry,
    config::schema::{ChannelsConfig, JwtConfig},
    security::workspace_requests::WorkspaceRequestHandler,
    session::manager::SessionManager,
};

pub type Envelope = HashMap<String, serde_json::Value>;

/// The `type` tag on an inbound WebSocket envelope. Mirrors nanobot's
/// `_dispatch_envelope` type strings (`channels/websocket/runtime.py`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvelopeType {
    NewChat,
    ForkChat,
    Attach,
    SetWorkspaceScope,
    TranscribeAudio,
    Message,
    /// List this connection's forkable chats (`websocket:*` sessions). Rust-side
    /// protocol addition with no nanobot precedent — the Python reference has
    /// no chat-discovery envelope at all, since `fork_chat` always assumes the
    /// caller already knows `source_chat_id` from the chat it's attached to.
    /// Added so a UI can offer "fork one of my other chats", not just "fork
    /// the one I'm currently in".
    ListChats,
    /// List skills installed on this process (workspace + builtin
    /// `SKILL.md` directories). Rust-side addition with no nanobot
    /// precedent — the Python reference has no skills-discovery envelope;
    /// skills only appear in the agent system prompt.
    ListSkills,
    /// Persist a new display title on an existing `websocket:{chat_id}`
    /// session. Rust-side addition with no nanobot precedent — the Python
    /// reference has no rename envelope; titles are LLM-generated only.
    RenameChat,
    /// Delete an existing `websocket:{chat_id}` session.
    /// Rust-side addition with no nanobot precedent — the Python reference has no delete envelope.
    DeleteChat,
    /// Cancel the in-flight agent turn for an existing `websocket:{chat_id}`
    /// session, leaving the session itself intact. Rust-side addition with no
    /// nanobot precedent — the Python reference has no cancel envelope; its
    /// only cancellation path is the `/stop` chat command, which costs a
    /// visible user message and an agent reply just to stop a turn.
    AbortTurn,
    /// Persist a named model-preset override on an existing
    /// `websocket:{chat_id}` session. Rust-side addition with no nanobot
    /// precedent — the Python reference has no preset envelope; its closest
    /// path is the `/model-preset` chat command.
    SetModelPreset,
    /// Persist a Standard/Minimal agent-mode override on an existing
    /// `websocket:{chat_id}` session. Rust-side addition — same role as
    /// `/mode`.
    SetMode,
    /// Wipe an existing `websocket:{chat_id}` session's messages (and the
    /// goal/usage metadata that would otherwise leak into the next turn)
    /// without deleting the session itself. Rust-side addition with no
    /// nanobot precedent — the Python reference has no clear envelope; its
    /// closest path is the `/new` chat command.
    ClearSession,
    /// An envelope whose `type` didn't match any known variant. Carries the
    /// raw type string so the dispatcher can reply with nanobot's
    /// `f"unknown type: {t!r}"` (`runtime.py:850`) — by the time an envelope
    /// reaches dispatch, `_parse_envelope` has already guaranteed `type` is
    /// a string (not missing, not some other JSON value), so `String` here
    /// (not `Option<String>` or `serde_json::Value`) is the right shape.
    Unrecognized(String),
}

impl From<&str> for EnvelopeType {
    /// Maps a raw envelope `type` string (e.g. `"new_chat"`) to its variant.
    /// Infallible by design — anything that isn't one of the known values
    /// becomes `Unrecognized`, mirroring nanobot's `_dispatch_envelope`
    /// fallthrough (`runtime.py:850`) rather than failing to parse.
    fn from(value: &str) -> Self {
        match value {
            "new_chat" => Self::NewChat,
            "fork_chat" => Self::ForkChat,
            "rename_chat" => Self::RenameChat,
            "delete_chat" => Self::DeleteChat,
            "abort_turn" => Self::AbortTurn,
            "attach" => Self::Attach,
            "set_workspace_scope" => Self::SetWorkspaceScope,
            "transcribe_audio" => Self::TranscribeAudio,
            "message" => Self::Message,
            "list_chats" => Self::ListChats,
            "list_skills" => Self::ListSkills,
            "set_model_preset" => Self::SetModelPreset,
            "set_mode" => Self::SetMode,
            "clear_session" => Self::ClearSession,
            other => Self::Unrecognized(other.to_string()),
        }
    }
}

/// Outbound WebSocket control-frame event names — the direct, synchronous
/// send layer (`send_event`/`send_goal_state`/`send_goal_status` in
/// `channels::websocket::runtime`), as opposed to the generic bus-published
/// `OutboundEvent` (`bus::outbound_events`) used for turn/content delivery.
/// Mirrors the `event` values nanobot's direct `_send_event`/
/// `send_goal_state`/`send_goal_status` produce (`channels/websocket/runtime.py`).
/// Kept next to [`EnvelopeType`] since both are type-tags for the same
/// envelope conversation, just opposite directions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WsOutboundEvent {
    Ready,
    Error,
    Attached,
    SessionUpdated,
    MessageAccepted,
    GoalState,
    GoalStatus,
    /// Reply to [`EnvelopeType::ListChats`] — Rust-side addition, no nanobot
    /// wire-name precedent to mirror (see that variant's doc comment).
    ChatsList,
    /// Reply to [`EnvelopeType::ListSkills`] — Rust-side addition, no nanobot
    /// wire-name precedent to mirror (see that variant's doc comment).
    SkillsList,
    /// Reply to [`EnvelopeType::RenameChat`] — Rust-side addition, no nanobot
    /// wire-name precedent to mirror (see that variant's doc comment).
    ChatRenamed,
    /// Fan-out for [`EnvelopeType::DeleteChat`], sent to every connection
    /// that was subscribed to the deleted chat (not just the requester).
    /// Rust-side addition, no nanobot wire-name precedent to mirror (see
    /// that variant's doc comment).
    ChatDeleted,
    /// Reply to [`EnvelopeType::AbortTurn`] — Rust-side addition, no nanobot
    /// wire-name precedent to mirror (see that variant's doc comment).
    TurnAborted,
    /// Reply to [`EnvelopeType::SetModelPreset`] — Rust-side addition, no nanobot
    /// wire-name precedent to mirror (see that variant's doc comment).
    ModelPresetSet,
    /// Reply to [`EnvelopeType::SetMode`].
    ModeSet,
    /// Fan-out of the user half of a just-accepted [`EnvelopeType::Message`]
    /// turn, sent to every connection subscribed to the chat (including the
    /// sender) — not a reply to the sender alone, unlike every other variant
    /// here. Lets a client watching the same chat from elsewhere insert the
    /// prompt and adopt `turn_id` so it can follow the reply as `delta`/
    /// `stream_end` frames arrive. Rust-side addition, no nanobot wire-name
    /// precedent to mirror.
    User,
    /// Reply to [`EnvelopeType::ClearSession`] — Rust-side addition, no nanobot
    /// wire-name precedent to mirror (see that variant's doc comment).
    SessionCleared,
}

impl WsOutboundEvent {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Error => "error",
            Self::Attached => "attached",
            Self::SessionUpdated => "session_updated",
            Self::MessageAccepted => "message_accepted",
            Self::GoalState => "goal_state",
            Self::GoalStatus => "goal_status",
            Self::ChatsList => "chats",
            Self::SkillsList => "skills",
            Self::ChatRenamed => "chat_renamed",
            Self::ChatDeleted => "chat_deleted",
            Self::TurnAborted => "turn_aborted",
            Self::ModelPresetSet => "model_preset_set",
            Self::ModeSet => "mode_set",
            Self::User => "user",
            Self::SessionCleared => "session_cleared",
        }
    }
}

/// Shared handle to the many-to-many chat_id/connection registry.
pub type ConnectionRegistryHandle = Arc<AsyncMutex<ConnectionRegistry>>;

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

fn default_require_auth() -> bool {
    true
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
    /// Whether a valid `purpose=webui` JWT is required to open a connection
    /// (and to fetch media) when `jwt.enabled` is `true`. Defaults to `true`
    /// so existing deployments keep requiring login. When `false`, a missing
    /// token is allowed (an invalid one is still rejected) and the
    /// connection's `webui_authenticated` flag is `false`, same as when JWT
    /// is disabled entirely — this is what lets a single instance offer
    /// guest access while JWT (and `/v1/login`) stay available for anyone
    /// who wants to sign in. Ignored when `jwt.enabled` is `false` (there is
    /// nothing to require in that case).
    #[serde(default = "default_require_auth")]
    pub require_auth: bool,
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

pub const DEFAULT_AUD: &str = "/ws";

impl Default for WebSocketConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            host: "127.0.0.1".to_string(),
            port: 8765,
            path: DEFAULT_AUD.to_string(),
            jwt: JwtConfig::default(),
            require_auth: default_require_auth(),
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
        parent: &mut dyn FnMut() -> garde::Path,
        report: &mut Report,
    ) {
        self.jwt
            .validate_into(ctx, &mut || parent().join("jwt"), report);

        if let Err(err) = validate_jwt_aud_matches_path(self) {
            report.append(parent().join("jwt").join("aud"), err);
        }
    }
}

/// Query params accepted on the WebSocket upgrade request, mirroring nanobot's
/// `ws://{host}:{port}{path}?client_id=...&token=...`.
#[derive(Debug, Deserialize)]
pub struct WsUpgradeQuery {
    #[serde(default)]
    pub client_id: Option<String>,
    #[serde(default)]
    pub token: Option<String>,
}

/// State handed to axum's per-connection handlers.
///
/// Kept separate from `WebSocketChannel` (rather than reaching for `Arc<Self>`)
/// because axum's `State<S>` extractor requires an owned, `'static` `S: Clone`,
/// while `BaseChannel::start` only hands us `&self`. Every field here is
/// itself cheap to clone (an `Arc`, or plain config data), so cloning `WsShared`
/// once per connection is fine.
#[derive(Clone)]
pub struct WsShared {
    pub name: &'static str,
    pub bus: Arc<MessageBus>,
    pub channels_config: ChannelsConfig,
    pub jwt: JwtConfig,
    pub jwt_public_key_pem: Option<Arc<Vec<u8>>>,
    /// Copied from [`WebSocketConfig::require_auth`] at channel construction —
    /// see that field's doc comment for the exact semantics `authorize` and
    /// `webui::media::authorize_media_request` apply to it.
    pub require_auth: bool,
    pub connections: ConnectionRegistryHandle,
    pub supports_streaming: bool,
    pub gateway_services: Arc<GatewayServices>,
    pub session_manager: Arc<StdMutex<SessionManager>>,
    pub workspace_request_handler: WorkspaceRequestHandler,
    pub runtime_surface: String,
    /// Root directory uploaded WebUI attachments are stored under
    /// (`config::paths::get_media_dir(None)`), resolved once per [`WsShared`]
    /// snapshot rather than re-resolved per request — see
    /// `webui::media::serve_media` (confines every request's `key` to this
    /// root) and `channels::websocket::runtime::resolve_history_media`
    /// (rewrites `attached.history` `media` refs into `/v1/media/...` URLs
    /// under it). Explicit rather than read from the process-wide config
    /// singleton at request time so tests can point it at an isolated
    /// tempdir instead of racing other tests' `set_config_path` calls.
    pub media_root: PathBuf,
    /// Same `Arc` as [`crate::agent::agent_loop::AgentLoop::runtime_resolver`],
    /// cloned into every snapshot so envelope handlers (e.g. `set_model_preset`)
    /// resolve and validate presets against the process catalog without going
    /// through the agent loop. Per-session persistence still writes
    /// `model_preset` on the session via [`Self::session_manager`].
    pub runtime_resolver: Arc<ModelRuntimeResolver>,
    /// Process-wide `agents.mode` default used when a session has no
    /// persisted override (or an invalid one).
    pub default_agent_mode: crate::agent::modes::AgentMode,
}

/// Everything one envelope-dispatch call needs, bundled so per-type handler
/// functions (`handle_envelope_message`, and future siblings) take one
/// parameter instead of growing a new one for every field a handler needs.
/// `Clone, Copy` since every field is itself a reference or `Copy` value —
/// letting handlers use the context both before and after calling a
/// sub-helper without move-then-reuse friction.
#[derive(Clone, Copy)]
pub struct EnvelopeDispatchContext<'a> {
    pub envelope: &'a Envelope,
    pub connection_id: &'a str,
    pub client_id: &'a str,
    pub shared: &'a WsShared,
    pub remote_addr: SocketAddr,
    /// Whether this connection's JWT proves it was minted for the WebUI
    /// frontend specifically (`purpose == "webui"`), as opposed to the
    /// client-supplied, self-declared `envelope["webui"]` flag. Set once per
    /// connection at upgrade time (`channels::websocket::runtime::authorize`)
    /// and copied into every envelope's dispatch context for that
    /// connection's lifetime. Mirrors nanobot's `connection in
    /// self._webui_connections` (`channels/websocket/runtime.py:824`).
    pub webui_authenticated: bool,
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
        assert_eq!(cfg.path, DEFAULT_AUD);
    }

    #[test]
    fn require_auth_defaults_to_true_when_omitted() {
        let cfg: WebSocketConfig =
            serde_json::from_str("{}").expect("empty object should deserialize");
        assert!(
            cfg.require_auth,
            "existing configs that omit requireAuth must keep requiring login"
        );
        assert!(WebSocketConfig::default().require_auth);
    }

    #[test]
    fn require_auth_can_be_explicitly_disabled() {
        let cfg: WebSocketConfig = serde_json::from_str(r#"{"requireAuth": false}"#)
            .expect("requireAuth: false should deserialize");
        assert!(!cfg.require_auth);
    }

    #[test]
    fn jwt_aud_must_match_path_when_enabled() {
        let mut cfg = WebSocketConfig {
            path: DEFAULT_AUD.to_string(),
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
            path: DEFAULT_AUD.to_string(),
            ..WebSocketConfig::default()
        };
        cfg.jwt.enabled = true;
        cfg.jwt.aud = "/ws/".to_string(); // trailing slash normalized in compare

        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn jwt_enabled_requires_non_empty_aud() {
        let mut cfg = WebSocketConfig {
            path: DEFAULT_AUD.to_string(),
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

    #[test]
    fn from_str_maps_every_known_type() {
        assert_eq!(EnvelopeType::from("new_chat"), EnvelopeType::NewChat);
        assert_eq!(EnvelopeType::from("fork_chat"), EnvelopeType::ForkChat);
        assert_eq!(EnvelopeType::from("attach"), EnvelopeType::Attach);
        assert_eq!(
            EnvelopeType::from("set_workspace_scope"),
            EnvelopeType::SetWorkspaceScope
        );
        assert_eq!(
            EnvelopeType::from("transcribe_audio"),
            EnvelopeType::TranscribeAudio
        );
        assert_eq!(EnvelopeType::from("message"), EnvelopeType::Message);
        assert_eq!(EnvelopeType::from("list_chats"), EnvelopeType::ListChats);
        assert_eq!(EnvelopeType::from("list_skills"), EnvelopeType::ListSkills);
        assert_eq!(EnvelopeType::from("rename_chat"), EnvelopeType::RenameChat);
        assert_eq!(EnvelopeType::from("delete_chat"), EnvelopeType::DeleteChat);
        assert_eq!(EnvelopeType::from("abort_turn"), EnvelopeType::AbortTurn);
        assert_eq!(
            EnvelopeType::from("set_model_preset"),
            EnvelopeType::SetModelPreset
        );
        assert_eq!(EnvelopeType::from("set_mode"), EnvelopeType::SetMode);
        assert_eq!(
            EnvelopeType::from("clear_session"),
            EnvelopeType::ClearSession
        );
    }

    #[test]
    fn from_str_maps_unknown_type_to_unrecognized() {
        assert_eq!(
            EnvelopeType::from("some_future_type"),
            EnvelopeType::Unrecognized("some_future_type".to_string())
        );
    }

    #[test]
    fn ws_outbound_event_as_str_matches_nanobot_wire_names() {
        assert_eq!(WsOutboundEvent::Ready.as_str(), "ready");
        assert_eq!(WsOutboundEvent::Error.as_str(), "error");
        assert_eq!(WsOutboundEvent::Attached.as_str(), "attached");
        assert_eq!(WsOutboundEvent::SessionUpdated.as_str(), "session_updated");
        assert_eq!(
            WsOutboundEvent::MessageAccepted.as_str(),
            "message_accepted"
        );
        assert_eq!(WsOutboundEvent::GoalState.as_str(), "goal_state");
        assert_eq!(WsOutboundEvent::GoalStatus.as_str(), "goal_status");
    }

    #[test]
    fn ws_outbound_event_chats_list_has_no_nanobot_precedent() {
        // Not part of the previous test's "matches nanobot wire names" set —
        // this event is a Rust-side addition (see `EnvelopeType::ListChats`).
        assert_eq!(WsOutboundEvent::ChatsList.as_str(), "chats");
    }

    #[test]
    fn ws_outbound_event_skills_list_has_no_nanobot_precedent() {
        assert_eq!(WsOutboundEvent::SkillsList.as_str(), "skills");
    }

    #[test]
    fn ws_outbound_event_chat_renamed_has_no_nanobot_precedent() {
        assert_eq!(WsOutboundEvent::ChatRenamed.as_str(), "chat_renamed");
    }

    #[test]
    fn ws_outbound_event_chat_deleted_has_no_nanobot_precedent() {
        assert_eq!(WsOutboundEvent::ChatDeleted.as_str(), "chat_deleted");
    }

    #[test]
    fn ws_outbound_event_turn_aborted_has_no_nanobot_precedent() {
        assert_eq!(WsOutboundEvent::TurnAborted.as_str(), "turn_aborted");
    }

    #[test]
    fn ws_outbound_event_model_preset_set_has_no_nanobot_precedent() {
        assert_eq!(WsOutboundEvent::ModelPresetSet.as_str(), "model_preset_set");
        assert_eq!(WsOutboundEvent::ModeSet.as_str(), "mode_set");
    }

    #[test]
    fn ws_outbound_event_user_has_no_nanobot_precedent() {
        assert_eq!(WsOutboundEvent::User.as_str(), "user");
    }

    #[test]
    fn ws_outbound_event_session_cleared_has_no_nanobot_precedent() {
        assert_eq!(WsOutboundEvent::SessionCleared.as_str(), "session_cleared");
    }
}
