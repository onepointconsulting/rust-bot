//! Persisted [`crate::session::manager::Session::metadata`] key names.
//!
//! Only keys that live on the session JSONL metadata object belong here.
//! Inbound/outbound *message* metadata (`webui_turn_id`, `_websocket_turn_owner`,
//! stream flags, `token_usage`, …) stays next to the channel or bus code that
//! owns it.

/// Session-scoped model-preset override, written by `/model-preset <name>`
/// and read by [`crate::agent::model_runtime::ModelRuntimeResolver::runtime_for_session`].
pub const SESSION_MODEL_PRESET_METADATA_KEY: &str = "model_preset";

/// Session-scoped agent mode override (`standard` / `minimal`), written by
/// `/mode <name>` and `set_mode`. Absent or invalid falls back to the
/// process-wide `agents.mode` default.
pub const SESSION_AGENT_MODE_METADATA_KEY: &str = "mode";

/// Sustained-goal blob (`objective` / `status` / `recap`). See `goal_state`.
pub const GOAL_STATE_KEY: &str = "goal_state";

/// Persisted [`crate::security::workspace_access::WorkspaceScope`] override
/// for this session.
pub const WORKSPACE_SCOPE_METADATA_KEY: &str = "workspace_scope";

/// In-flight turn checkpoint used to recover after a crash or `/stop`.
pub const RUNTIME_CHECKPOINT_KEY: &str = "runtime_checkpoint";

/// Accumulated LLM token/cost totals for this session's lifetime.
pub const SESSION_TOKEN_USAGE_KEY: &str = "token_usage";

/// Marks the session as originating from the WebUI / WebSocket channel.
pub const SESSION_WEBUI_METADATA_KEY: &str = "webui";

/// The WebSocket `client_id` (query param + `LocalStorage`, see
/// `read_or_create_client_id` in `websockets-chat/src/app.rs`) that first
/// created this session. Stamped once — on `new_chat`, first `message`
/// persist, or as a fork's destination — and never overwritten afterward.
///
/// Only consulted when [`crate::channels::websocket::types::WsShared::require_auth`]
/// is `false` (guest-capable instance): `list_chats`/`attach`/`rename_chat`/
/// `delete_chat`/`fork_chat` then scope to sessions owned by the requesting
/// connection's `client_id`, hiding everything else (including sessions with
/// no stamped owner at all) rather than treating "unowned" as "shared". See
/// the "Optional WebSocket login" plan's guest session isolation section.
pub const SESSION_WEBSOCKET_OWNER_CLIENT_ID_KEY: &str = "websocket_owner_client_id";

/// Session display title (LLM-generated or user-renamed).
pub const SESSION_TITLE_METADATA_KEY: &str = "title";

/// Hidden history marker.
pub const HIDDEN_HISTORY_KEY: &str = "_hidden_history";

/// Automation turn marker.
pub const AUTOMATION_HISTORY_KEY: &str = "_automation_turn";

/// Command marker
pub const COMMAND_KEY: &str = "_command";
