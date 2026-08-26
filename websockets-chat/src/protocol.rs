//! Wire protocol for the gateway's WebSocket channel.
//!
//! Mirrors the JSON shapes defined server-side in `src/channels/websocket/types.rs`
//! (and the handlers in `src/channels/websocket/runtime.rs` that emit them):
//! this module owns both directions of that conversation from the browser's
//! side of the connection.
//!
//! * Outbound: [`ClientEnvelope`], currently `message`, `new_chat`,
//!   `attach`, `list_chats`, `rename_chat`, `delete_chat`, `fork_chat`, and
//!   `abort_turn`.
//! * Inbound: [`ServerEvent`], one variant per `event` value the gateway can
//!   send, decoded by [`parse_server_event`].
//!
//! Everything here is plain data + parsing with no Leptos/wasm/`web-sys`
//! dependency, so it is unit-testable with plain `#[test]` on the host
//! target — no `wasm-bindgen-test` machinery required.

use std::collections::HashMap;

use chat_ui::models::{ChatEntry, ImageAttachment, Role, SessionTokenUsage, ToolEvent};
use serde::{Deserialize, Serialize};

/// Outbound envelope sent to the gateway.
///
/// The backend's `EnvelopeType` (`src/channels/websocket/types.rs`) covers
/// more inbound types than this crate has a use for (`set_workspace_scope`,
/// `transcribe_audio`). This struct covers the ones the frontend actually
/// sends (`message`, `new_chat`, `attach`, `list_chats`, `rename_chat`,
/// `delete_chat`, `fork_chat`, `abort_turn`); constructors pin `type_` so
/// callers cannot invent a shape the gateway would reject with "unknown
/// type".
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ClientEnvelope {
    #[serde(rename = "type")]
    pub type_: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chat_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media: Option<Vec<serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Set only by [`Self::set_model_preset`] — a preset name (or `"default"`
    /// to clear the session's override), never sent alongside any other
    /// envelope type.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_preset: Option<String>,
    /// Set only by [`Self::fork_chat_before`]: a 0-based index into the
    /// source chat's *user* messages. The gateway copies history up to (not
    /// including) that user turn, so the assistant reply just before it is
    /// kept. Omitted by [`Self::fork_chat`] to fork the whole chat.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before_user_index: Option<u64>,
    /// Always `true`: this crate *is* the WebUI frontend, and the gateway's
    /// dispatch logic (`webui_authenticated` in
    /// `EnvelopeDispatchContext`) treats this flag as a client's own
    /// self-declaration of that fact.
    pub webui: bool,
}

impl ClientEnvelope {
    /// Build the envelope for an outbound chat message.
    pub fn message(
        chat_id: impl Into<String>,
        turn_id: Option<String>,
        content: impl Into<String>,
        media: Option<Vec<serde_json::Value>>,
    ) -> Self {
        Self {
            type_: "message",
            chat_id: Some(chat_id.into()),
            turn_id,
            content: Some(content.into()),
            media,
            title: None,
            model_preset: None,
            before_user_index: None,
            webui: true,
        }
    }

    /// Ask the gateway to mint a new chat on this connection.
    ///
    /// The reply is an `attached` event carrying the new `chat_id` (and a
    /// `session_updated` with the resolved workspace scope). A rejected
    /// scope comes back as `error` with no `chat_id`.
    pub fn new_chat() -> Self {
        Self {
            type_: "new_chat",
            chat_id: None,
            turn_id: None,
            content: None,
            media: None,
            title: None,
            model_preset: None,
            before_user_index: None,
            webui: true,
        }
    }

    /// Ask the gateway to subscribe this connection to an existing `chat_id`.
    ///
    /// The reply is an `attached` event carrying that `chat_id` and a
    /// display `history` snapshot (empty if the session has no messages).
    pub fn attach(chat_id: impl Into<String>) -> Self {
        Self {
            type_: "attach",
            chat_id: Some(chat_id.into()),
            turn_id: None,
            content: None,
            media: None,
            title: None,
            model_preset: None,
            before_user_index: None,
            webui: true,
        }
    }

    /// Ask the gateway to fork `chat_id`'s entire history into a brand-new
    /// chat. The reply is an `attached` event carrying the new chat's id and
    /// full history — same shape as `attach`'s reply, just for a chat that
    /// didn't exist a moment ago. No `before_user_index` is sent: omitting
    /// it tells the gateway to fork the whole chat (see
    /// `handle_envelope_fork_chat` server-side).
    pub fn fork_chat(chat_id: impl Into<String>) -> Self {
        Self::fork_chat_inner(chat_id, None)
    }

    /// Ask the gateway to fork `chat_id`'s history up to (not including) the
    /// user turn at `before_user_index`. The assistant reply just before that
    /// user message is kept. Same `attached` reply as [`Self::fork_chat`].
    pub fn fork_chat_before(chat_id: impl Into<String>, before_user_index: u64) -> Self {
        Self::fork_chat_inner(chat_id, Some(before_user_index))
    }

    fn fork_chat_inner(chat_id: impl Into<String>, before_user_index: Option<u64>) -> Self {
        Self {
            type_: "fork_chat",
            chat_id: Some(chat_id.into()),
            turn_id: None,
            content: None,
            media: None,
            title: None,
            model_preset: None,
            before_user_index,
            webui: true,
        }
    }

    /// Ask the gateway for this connection's forkable `websocket:*` chats.
    /// The reply is a `chats` event (see [`ServerEvent::ChatsList`]).
    pub fn list_chats() -> Self {
        Self {
            type_: "list_chats",
            chat_id: None,
            turn_id: None,
            content: None,
            media: None,
            title: None,
            model_preset: None,
            before_user_index: None,
            webui: true,
        }
    }

    /// Persist a new display title on an existing `chat_id`.
    ///
    /// The reply is a `chat_renamed` event carrying that `chat_id` and the
    /// stored `title`. Rejections come back as `error` (`missing title`,
    /// `session_not_found`, `access_denied`, `rename_failed`).
    pub fn rename_chat(chat_id: impl Into<String>, title: impl Into<String>) -> Self {
        Self {
            type_: "rename_chat",
            chat_id: Some(chat_id.into()),
            turn_id: None,
            content: None,
            media: None,
            title: Some(title.into()),
            model_preset: None,
            before_user_index: None,
            webui: true,
        }
    }

    /// Permanently delete an existing `chat_id`'s session.
    ///
    /// The reply is a `chat_deleted` event carrying that `chat_id` — sent to
    /// every connection subscribed to it, not just the requester. Rejections
    /// come back as `error` (`session_not_found`, `access_denied`, `delete_failed`).
    pub fn delete_chat(chat_id: impl Into<String>) -> Self {
        Self {
            type_: "delete_chat",
            chat_id: Some(chat_id.into()),
            turn_id: None,
            content: None,
            media: None,
            title: None,
            model_preset: None,
            before_user_index: None,
            webui: true,
        }
    }

    /// Cancel the in-flight agent turn on `chat_id`, leaving the chat and its
    /// history intact.
    ///
    /// `turn_id` is the id this client minted when it sent the message being
    /// cancelled. The gateway treats it as a staleness guard: an abort naming
    /// a turn that is no longer the running one comes back as `error` with
    /// `turn_not_active` instead of cancelling whatever ran next. Omitting it
    /// aborts whatever is currently running on the chat.
    ///
    /// The reply is a `turn_aborted` event — fanned out to every connection
    /// attached to `chat_id`, since they were all rendering the same stream.
    pub fn abort_turn(chat_id: impl Into<String>, turn_id: Option<String>) -> Self {
        Self {
            type_: "abort_turn",
            chat_id: Some(chat_id.into()),
            turn_id,
            content: None,
            media: None,
            title: None,
            model_preset: None,
            before_user_index: None,
            webui: true,
        }
    }

    /// Set (or clear, via `"default"`) `chat_id`'s model-preset override.
    ///
    /// The reply is a `model_preset_set` event carrying the resolved
    /// `model_preset`/`model`. Rejections come back as `error` (e.g.
    /// `invalid_model_preset`, `session_not_found`, `missing_model_preset`).
    pub fn set_model_preset(chat_id: impl Into<String>, model_preset: impl Into<String>) -> Self {
        Self {
            type_: "set_model_preset",
            chat_id: Some(chat_id.into()),
            turn_id: None,
            content: None,
            media: None,
            title: None,
            model_preset: Some(model_preset.into()),
            before_user_index: None,
            webui: true,
        }
    }
}

/// One chat summary entry inside a `chats` event's list — mirrors the
/// backend's `list_websocket_chats` output shape
/// (`src/channels/websocket/runtime.rs`): a `websocket:*` session with its
/// `key`/`path` stripped and replaced by a bare `chat_id`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatSummary {
    pub chat_id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub updated_at: String,
}

/// One event the gateway can push down the WebSocket connection.
///
/// Every variant here corresponds to a documented `event` value on the wire;
/// `Unknown` is the catch-all for anything else (including a missing or
/// non-string `event` field), so a future gateway event this crate doesn't
/// know about yet degrades gracefully instead of breaking the receive loop.
///
/// `tool_events` on [`ServerEvent::Message`] deserializes directly into
/// `chat_ui::models::ToolEvent` — its `name`/`status`/`detail` fields already
/// match the wire shape exactly, so no separate payload type is needed here.
#[derive(Debug, Clone, PartialEq)]
pub enum ServerEvent {
    /// The connection is up and the gateway has assigned (or confirmed) a
    /// `chat_id` and echoed back the connecting `client_id`.
    ///
    /// `streaming` is the channel's `supports_streaming` flag: when `true`
    /// the turn arrives as `delta`/`stream_end` frames; when `false` the
    /// client should expect a single final `message` and show a thinking
    /// indicator instead of a token cursor. Missing on older gateways,
    /// treated as `false`.
    Ready {
        chat_id: String,
        client_id: String,
        streaming: bool,
    },
    /// A server-side error, optionally scoped to one chat / in-flight turn.
    /// `chat_id` is omitted when the error is not attached to any chat yet
    /// (e.g. a rejected `new_chat` workspace scope).
    Error {
        chat_id: Option<String>,
        turn_id: Option<String>,
        detail: String,
    },
    /// The connection is now attached to `chat_id` — the reply to a
    /// `new_chat`, `attach`, or `fork_chat` envelope. `history` is the
    /// display snapshot from the gateway (`[]` when the chat is new or has
    /// no messages, and when an older gateway omits the field).
    ///
    /// `model_presets`/`model_preset` are the process-wide preset catalog
    /// and this chat's resolved selection (`"default"` when there's no
    /// session override) — see `model_preset_attached_fields` server-side.
    /// Empty/`None` on an older gateway that doesn't send them yet.
    ///
    /// `token_usage` is the session's lifetime totals — `None` for a brand
    /// new chat, a session that predates usage tracking, or an older
    /// gateway that doesn't send the field yet (see
    /// `token_usage_attached_fields` server-side).
    Attached {
        chat_id: String,
        history: Vec<ChatEntry>,
        model_presets: Vec<String>,
        model_preset: Option<String>,
        token_usage: Option<SessionTokenUsage>,
    },
    /// Sent when the server-side session state changes. Shape not yet
    /// finalized server-side, so the raw JSON is kept as-is (`scope` and
    /// `workspace_scope`/`token_usage` are read directly off this value by
    /// the app rather than a typed field here).
    SessionUpdated(serde_json::Value),
    /// Acknowledges that a `message` envelope was accepted for processing.
    MessageAccepted { chat_id: String, turn_id: String },
    /// Sustained-goal state snapshot. Shape not yet finalized server-side, so
    /// the raw JSON is kept as-is.
    GoalState(serde_json::Value),
    /// Sustained-goal lifecycle status (e.g. `"running"`).
    GoalStatus {
        chat_id: String,
        status: String,
        started_at: Option<f64>,
        turn_id: Option<String>,
    },
    /// A complete chat message. When `kind` is `Some("progress")` or
    /// `Some("tool_hint")` this is a live-progress update (often carrying
    /// `tool_events`) rather than the turn's final answer; `kind == None`
    /// means a plain final message.
    Message {
        chat_id: String,
        text: String,
        media: Option<serde_json::Value>,
        reply_to: Option<String>,
        latency_ms: Option<u64>,
        kind: Option<String>,
        tool_events: Option<Vec<ToolEvent>>,
    },
    /// One streamed text chunk for an in-progress turn.
    Delta {
        chat_id: String,
        text: String,
        stream_id: Option<String>,
    },
    /// End of a text stream. `text`, when present, is the authoritative full
    /// text and should overwrite anything accumulated from `Delta` chunks.
    StreamEnd {
        chat_id: String,
        text: Option<String>,
        stream_id: Option<String>,
        resuming: Option<bool>,
        merge_next: Option<bool>,
    },
    /// One streamed reasoning/thinking-text chunk for an in-progress turn.
    ReasoningDelta {
        chat_id: String,
        text: String,
        stream_id: Option<String>,
    },
    /// End of a reasoning stream. Carries no replacement text (unlike
    /// `StreamEnd`) — it's purely a "this stream is done" marker.
    ReasoningEnd {
        chat_id: String,
        stream_id: Option<String>,
    },
    /// One or more file edits applied by a tool during this turn.
    FileEdit {
        chat_id: String,
        edits: Vec<HashMap<String, String>>,
    },
    /// Reply to a `list_chats` envelope: every `websocket:*` chat on this
    /// connection, most-recently-updated first. Not scoped to any one
    /// `chat_id` — see [`ServerEvent::chat_id`].
    ChatsList { chats: Vec<ChatSummary> },
    /// Reply to a `rename_chat` envelope: `chat_id` now has display `title`.
    ChatRenamed { chat_id: String, title: String },
    /// Reply to a `delete_chat` envelope, fanned out to every connection
    /// that was subscribed to `chat_id` — not just the requester.
    ChatDeleted { chat_id: String },
    /// Reply to an `abort_turn` envelope: the in-flight turn on `chat_id` was
    /// cancelled, so no `stream_end` or final `message` is coming for it.
    /// Fanned out to every connection attached to `chat_id`. `turn_id` is
    /// absent when the gateway had no turn identity to name (see
    /// [`ClientEnvelope::abort_turn`]).
    TurnAborted {
        chat_id: String,
        turn_id: Option<String>,
    },
    /// Reply to a [`ClientEnvelope::set_model_preset`] envelope: `chat_id`'s
    /// session now resolves to `model_preset`/`model`.
    ModelPresetSet {
        chat_id: String,
        model_preset: String,
        model: String,
    },
    /// An `event` value this crate doesn't recognize (or a missing/non-string
    /// `event` field), carrying the raw decoded JSON so nothing is lost.
    Unknown(serde_json::Value),
}

impl ServerEvent {
    /// The `chat_id` this event is scoped to, if the payload carries one.
    ///
    /// Used by the app to ignore leftover frames from a previous chat after
    /// `new_chat` switches the connection onto a new id. `Unknown`,
    /// [`ServerEvent::ChatsList`], [`ServerEvent::ChatRenamed`]/
    /// [`ServerEvent::ChatDeleted`] (both can target any sidebar row), and
    /// unscoped errors return `None`.
    pub fn chat_id(&self) -> Option<&str> {
        match self {
            ServerEvent::Ready { chat_id, .. }
            | ServerEvent::Attached { chat_id, .. }
            | ServerEvent::MessageAccepted { chat_id, .. }
            | ServerEvent::GoalStatus { chat_id, .. }
            | ServerEvent::Message { chat_id, .. }
            | ServerEvent::Delta { chat_id, .. }
            | ServerEvent::StreamEnd { chat_id, .. }
            | ServerEvent::ReasoningDelta { chat_id, .. }
            | ServerEvent::ReasoningEnd { chat_id, .. }
            | ServerEvent::FileEdit { chat_id, .. }
            | ServerEvent::TurnAborted { chat_id, .. }
            | ServerEvent::ModelPresetSet { chat_id, .. } => Some(chat_id.as_str()),
            ServerEvent::Error { chat_id, .. } => chat_id.as_deref(),
            ServerEvent::SessionUpdated(value) | ServerEvent::GoalState(value) => {
                value.get("chat_id").and_then(serde_json::Value::as_str)
            }
            // A rename/delete can target any sidebar row, not just the
            // active chat, so these events are unscoped for drop purposes
            // (see `should_drop_event`). The payload still carries `chat_id`.
            ServerEvent::ChatsList { .. }
            | ServerEvent::ChatRenamed { .. }
            | ServerEvent::ChatDeleted { .. }
            | ServerEvent::Unknown(_) => None,
        }
    }
}

/// Error produced by [`parse_server_event`] when a frame names a *known*
/// `event` value but its payload doesn't match the expected shape, or the
/// frame isn't valid JSON at all.
///
/// An `event` value that is missing, non-string, or simply not one of the
/// known names is *not* an error — see [`ServerEvent::Unknown`]. This mirrors
/// the backend's own tolerant envelope parsing: an unrecognized shape from a
/// newer/older gateway build shouldn't crash the frontend's receive loop.
#[derive(Debug, Clone)]
pub struct ProtocolError {
    pub message: String,
}

impl ProtocolError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for ProtocolError {}

/// Decode a `{"event": "ready", ...}` frame's known fields.
#[derive(Deserialize)]
struct ReadyWire {
    chat_id: String,
    client_id: String,
    #[serde(default)]
    streaming: bool,
}

#[derive(Deserialize)]
struct ErrorWire {
    #[serde(default)]
    chat_id: Option<String>,
    #[serde(default)]
    turn_id: Option<String>,
    detail: String,
}

/// One tool-hint/progress line the backend buffered against the answer row
/// it precedes chronologically — see `transcript_chat_history`'s doc comment
/// in `src/channels/websocket/webui/transcript.rs`. `kind` is always
/// `"tool_hint"` or `"progress"`, matching the live `ServerEvent::Message`
/// `kind` values these were originally recorded from.
#[derive(Debug, Clone, PartialEq, Deserialize)]
struct HistoryActivity {
    kind: String,
    text: String,
}

/// One row of the `attached` event's `history` array — mirrors
/// `websocket_chat_history` in `src/channels/websocket/runtime.rs`.
///
/// `media` is a list of already browser-reachable URLs: either
/// `/v1/media/...` (rewritten server-side from a stored file path by
/// `resolve_history_media`, needing a `?token=` before it's fetchable — see
/// `state::authorize_media_attachments`) or a surviving `http(s)://`
/// reference. Never a `data:` URL — restored history has no in-memory bytes
/// to embed; only a just-sent, not-yet-persisted message shows those (see
/// `build_media_payload`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
struct HistoryMessage {
    role: String,
    #[serde(default)]
    content: String,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    activity: Option<Vec<HistoryActivity>>,
    #[serde(default)]
    media: Vec<String>,
}

/// Rebuild the tool-activity chips for one history row from its buffered
/// `activity` lines, reusing the exact synthesizers the live path uses for
/// `kind: "tool_hint"`/`"progress"` messages (`state::synthesize_*`) so a
/// replayed chip renders identically to the live one it replaced.
///
/// Tool-hint chips are forced to `"done"` rather than the `"running"` status
/// [`crate::state::synthesize_tool_hint_event`] normally returns: that
/// status models a tool call in flight, which by definition cannot still be
/// true for a turn already fully recorded in history (mirrors
/// `state::finish_any_running_tool_events`'s treatment of orphaned live
/// chips). Progress notes already synthesize as a static `"note"` chip, so
/// no override is needed there.
fn history_activity_to_tool_events(activity: &[HistoryActivity]) -> Option<Vec<ToolEvent>> {
    if activity.is_empty() {
        return None;
    }
    Some(
        activity
            .iter()
            .map(|item| {
                if item.kind == "progress" {
                    crate::state::synthesize_progress_note_event(&item.text)
                } else {
                    let mut event = crate::state::synthesize_tool_hint_event(&item.text);
                    event.status = "done".to_string();
                    event
                }
            })
            .collect(),
    )
}

/// Map a gateway history snapshot into transcript [`ChatEntry`]s, skipping
/// any row whose `role` isn't `user`/`assistant`. Ids are assigned in order
/// from 0 so the app can resume `next_id` at `entries.len()`.
fn history_to_entries(history: &[HistoryMessage]) -> Vec<ChatEntry> {
    history
        .iter()
        .filter_map(|message| {
            let role = match message.role.as_str() {
                "user" => Role::User,
                "assistant" => Role::Assistant,
                _ => return None,
            };
            Some(ChatEntry {
                id: 0,
                role,
                content: message.content.clone(),
                attachments: message
                    .media
                    .iter()
                    .map(|url| ImageAttachment {
                        url: url.clone(),
                        label: None,
                    })
                    .collect(),
                streaming: false,
                tool_events: message
                    .activity
                    .as_deref()
                    .and_then(history_activity_to_tool_events),
                reasoning: message.reasoning_content.clone().filter(|s| !s.is_empty()),
            })
        })
        .enumerate()
        .map(|(index, mut entry)| {
            entry.id = index as u64;
            entry
        })
        .collect()
}

#[derive(Deserialize)]
struct AttachedWire {
    chat_id: String,
    /// Absent on `new_chat`'s `attached` ack and on older gateways.
    #[serde(default)]
    history: Vec<HistoryMessage>,
    /// Absent on an older gateway that doesn't send the preset catalog yet.
    #[serde(default)]
    model_presets: Vec<String>,
    /// Absent on an older gateway; also `None` when `model_presets` is
    /// empty (nothing resolved, since there was nothing to resolve).
    #[serde(default)]
    model_preset: Option<String>,
    /// Absent when the session has no recorded usage yet, or on an older
    /// gateway that doesn't send it.
    #[serde(default)]
    token_usage: Option<SessionTokenUsage>,
}

#[derive(Deserialize)]
struct MessageAcceptedWire {
    chat_id: String,
    turn_id: String,
}

#[derive(Deserialize)]
struct GoalStatusWire {
    chat_id: String,
    status: String,
    #[serde(default)]
    started_at: Option<f64>,
    #[serde(default)]
    turn_id: Option<String>,
}

#[derive(Deserialize)]
struct MessageWire {
    chat_id: String,
    text: String,
    #[serde(default)]
    media: Option<serde_json::Value>,
    #[serde(default)]
    reply_to: Option<String>,
    #[serde(default)]
    latency_ms: Option<u64>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    tool_events: Option<Vec<ToolEvent>>,
}

#[derive(Deserialize)]
struct DeltaWire {
    chat_id: String,
    text: String,
    #[serde(default)]
    stream_id: Option<String>,
}

#[derive(Deserialize)]
struct StreamEndWire {
    chat_id: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    stream_id: Option<String>,
    #[serde(default)]
    resuming: Option<bool>,
    #[serde(default)]
    merge_next: Option<bool>,
}

#[derive(Deserialize)]
struct ReasoningDeltaWire {
    chat_id: String,
    text: String,
    #[serde(default)]
    stream_id: Option<String>,
}

#[derive(Deserialize)]
struct ReasoningEndWire {
    chat_id: String,
    #[serde(default)]
    stream_id: Option<String>,
}

#[derive(Deserialize)]
struct FileEditWire {
    chat_id: String,
    edits: Vec<HashMap<String, String>>,
}

#[derive(Deserialize)]
struct ChatsListWire {
    chats: Vec<ChatSummary>,
}

#[derive(Deserialize)]
struct ChatRenamedWire {
    chat_id: String,
    title: String,
}

#[derive(Deserialize)]
struct ChatDeletedWire {
    chat_id: String,
}

#[derive(Deserialize)]
struct TurnAbortedWire {
    chat_id: String,
    #[serde(default)]
    turn_id: Option<String>,
}

#[derive(Deserialize)]
struct ModelPresetSetWire {
    chat_id: String,
    model_preset: String,
    model: String,
}

/// Deserialize `value` into a specific wire shape, mapping any failure into a
/// [`ProtocolError`] rather than a raw `serde_json::Error`.
fn decode<T: serde::de::DeserializeOwned>(value: &serde_json::Value) -> Result<T, ProtocolError> {
    serde_json::from_value(value.clone())
        .map_err(|err| ProtocolError::new(format!("failed to decode event payload: {err}")))
}

/// Parse one raw WebSocket text frame from the gateway into a [`ServerEvent`].
///
/// Decodes to a generic [`serde_json::Value`] first, branches on the
/// `"event"` field, then deserializes the matched shape from that same
/// `Value`. A missing/non-string `event`, or a string that isn't one of the
/// known event names, produces `Ok(ServerEvent::Unknown(value))` rather than
/// an error — only a *recognized* event name whose payload fails to
/// deserialize into its expected shape produces `Err`.
pub fn parse_server_event(raw: &str) -> Result<ServerEvent, ProtocolError> {
    let value: serde_json::Value = serde_json::from_str(raw)
        .map_err(|err| ProtocolError::new(format!("malformed JSON frame: {err}")))?;

    let Some(event) = value.get("event").and_then(serde_json::Value::as_str) else {
        return Ok(ServerEvent::Unknown(value));
    };

    match event {
        "ready" => decode::<ReadyWire>(&value).map(|w| ServerEvent::Ready {
            chat_id: w.chat_id,
            client_id: w.client_id,
            streaming: w.streaming,
        }),
        "error" => decode::<ErrorWire>(&value).map(|w| ServerEvent::Error {
            chat_id: w.chat_id,
            turn_id: w.turn_id,
            detail: w.detail,
        }),
        "attached" => decode::<AttachedWire>(&value).map(|w| ServerEvent::Attached {
            chat_id: w.chat_id,
            history: history_to_entries(&w.history),
            model_presets: w.model_presets,
            model_preset: w.model_preset,
            token_usage: w.token_usage,
        }),
        "session_updated" => Ok(ServerEvent::SessionUpdated(value)),
        "message_accepted" => {
            decode::<MessageAcceptedWire>(&value).map(|w| ServerEvent::MessageAccepted {
                chat_id: w.chat_id,
                turn_id: w.turn_id,
            })
        }
        "goal_state" => Ok(ServerEvent::GoalState(value)),
        "goal_status" => decode::<GoalStatusWire>(&value).map(|w| ServerEvent::GoalStatus {
            chat_id: w.chat_id,
            status: w.status,
            started_at: w.started_at,
            turn_id: w.turn_id,
        }),
        "message" => decode::<MessageWire>(&value).map(|w| ServerEvent::Message {
            chat_id: w.chat_id,
            text: w.text,
            media: w.media,
            reply_to: w.reply_to,
            latency_ms: w.latency_ms,
            kind: w.kind,
            tool_events: w.tool_events,
        }),
        "delta" => decode::<DeltaWire>(&value).map(|w| ServerEvent::Delta {
            chat_id: w.chat_id,
            text: w.text,
            stream_id: w.stream_id,
        }),
        "stream_end" => decode::<StreamEndWire>(&value).map(|w| ServerEvent::StreamEnd {
            chat_id: w.chat_id,
            text: w.text,
            stream_id: w.stream_id,
            resuming: w.resuming,
            merge_next: w.merge_next,
        }),
        "reasoning_delta" => {
            decode::<ReasoningDeltaWire>(&value).map(|w| ServerEvent::ReasoningDelta {
                chat_id: w.chat_id,
                text: w.text,
                stream_id: w.stream_id,
            })
        }
        "reasoning_end" => decode::<ReasoningEndWire>(&value).map(|w| ServerEvent::ReasoningEnd {
            chat_id: w.chat_id,
            stream_id: w.stream_id,
        }),
        "file_edit" => decode::<FileEditWire>(&value).map(|w| ServerEvent::FileEdit {
            chat_id: w.chat_id,
            edits: w.edits,
        }),
        "chats" => {
            decode::<ChatsListWire>(&value).map(|w| ServerEvent::ChatsList { chats: w.chats })
        }
        "chat_renamed" => decode::<ChatRenamedWire>(&value).map(|w| ServerEvent::ChatRenamed {
            chat_id: w.chat_id,
            title: w.title,
        }),
        "chat_deleted" => decode::<ChatDeletedWire>(&value)
            .map(|w| ServerEvent::ChatDeleted { chat_id: w.chat_id }),
        "turn_aborted" => decode::<TurnAbortedWire>(&value).map(|w| ServerEvent::TurnAborted {
            chat_id: w.chat_id,
            turn_id: w.turn_id,
        }),
        "model_preset_set" => {
            decode::<ModelPresetSetWire>(&value).map(|w| ServerEvent::ModelPresetSet {
                chat_id: w.chat_id,
                model_preset: w.model_preset,
                model: w.model,
            })
        }
        _ => Ok(ServerEvent::Unknown(value)),
    }
}

/// Pull `token_usage` off a [`ServerEvent::SessionUpdated`] payload, if
/// present. `session_updated`'s shape isn't finalized server-side (see the
/// variant's doc comment), so this stays a targeted field read rather than a
/// typed wire struct — absent on scope-only updates (e.g. `new_chat`'s
/// workspace-scope notification) or an older gateway.
pub fn session_updated_token_usage(value: &serde_json::Value) -> Option<SessionTokenUsage> {
    value
        .get("token_usage")
        .cloned()
        .and_then(|v| serde_json::from_value(v).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ready() {
        let raw = r#"{"event":"ready","chat_id":"11111111-1111-1111-1111-111111111111","client_id":"browser-abc"}"#;
        let event = parse_server_event(raw).expect("should parse");
        assert_eq!(
            event,
            ServerEvent::Ready {
                chat_id: "11111111-1111-1111-1111-111111111111".to_string(),
                client_id: "browser-abc".to_string(),
                streaming: false,
            }
        );
    }

    #[test]
    fn parses_ready_with_streaming() {
        let raw =
            r#"{"event":"ready","chat_id":"chat-1","client_id":"browser-abc","streaming":true}"#;
        let event = parse_server_event(raw).expect("should parse");
        assert_eq!(
            event,
            ServerEvent::Ready {
                chat_id: "chat-1".to_string(),
                client_id: "browser-abc".to_string(),
                streaming: true,
            }
        );
    }

    #[test]
    fn parses_error_with_turn_id() {
        let raw = r#"{"event":"error","chat_id":"chat-1","turn_id":"turn-1","detail":"boom"}"#;
        let event = parse_server_event(raw).expect("should parse");
        assert_eq!(
            event,
            ServerEvent::Error {
                chat_id: Some("chat-1".to_string()),
                turn_id: Some("turn-1".to_string()),
                detail: "boom".to_string(),
            }
        );
    }

    #[test]
    fn parses_error_without_turn_id() {
        let raw = r#"{"event":"error","chat_id":"chat-1","detail":"boom"}"#;
        let event = parse_server_event(raw).expect("should parse");
        assert_eq!(
            event,
            ServerEvent::Error {
                chat_id: Some("chat-1".to_string()),
                turn_id: None,
                detail: "boom".to_string(),
            }
        );
    }

    #[test]
    fn parses_error_without_chat_id() {
        let raw = r#"{"event":"error","detail":"workspace_scope_rejected"}"#;
        let event = parse_server_event(raw).expect("should parse");
        assert_eq!(
            event,
            ServerEvent::Error {
                chat_id: None,
                turn_id: None,
                detail: "workspace_scope_rejected".to_string(),
            }
        );
    }

    #[test]
    fn parses_attached() {
        let raw = r#"{"event":"attached","chat_id":"chat-1"}"#;
        let event = parse_server_event(raw).expect("should parse");
        assert_eq!(
            event,
            ServerEvent::Attached {
                chat_id: "chat-1".to_string(),
                history: vec![],
                model_presets: vec![],
                model_preset: None,
                token_usage: None,
            }
        );
    }

    #[test]
    fn parses_attached_with_model_preset_catalog_and_selection() {
        let raw = r#"{"event":"attached","chat_id":"chat-1","model_presets":["default","fast"],"model_preset":"fast"}"#;
        let event = parse_server_event(raw).expect("should parse");
        assert_eq!(
            event,
            ServerEvent::Attached {
                chat_id: "chat-1".to_string(),
                history: vec![],
                model_presets: vec!["default".to_string(), "fast".to_string()],
                model_preset: Some("fast".to_string()),
                token_usage: None,
            }
        );
    }

    #[test]
    fn parses_attached_with_token_usage() {
        let raw = r#"{"event":"attached","chat_id":"chat-1","token_usage":{"input_tokens":120,"output_tokens":45}}"#;
        let event = parse_server_event(raw).expect("should parse");
        match event {
            ServerEvent::Attached { token_usage, .. } => {
                let usage = token_usage.expect("expected token_usage");
                assert_eq!(usage.input_tokens, Some(120));
                assert_eq!(usage.output_tokens, Some(45));
            }
            other => panic!("expected Attached, got {other:?}"),
        }
    }

    #[test]
    fn parses_attached_without_token_usage_is_none() {
        let raw = r#"{"event":"attached","chat_id":"chat-1"}"#;
        let event = parse_server_event(raw).expect("should parse");
        match event {
            ServerEvent::Attached { token_usage, .. } => {
                assert!(token_usage.is_none());
            }
            other => panic!("expected Attached, got {other:?}"),
        }
    }

    #[test]
    fn session_updated_token_usage_extracts_field_when_present() {
        let raw = r#"{"event":"session_updated","chat_id":"chat-1","scope":"metadata","token_usage":{"input_tokens":30,"output_tokens":10}}"#;
        let value: serde_json::Value = serde_json::from_str(raw).unwrap();
        let usage = session_updated_token_usage(&value).expect("expected token_usage");
        assert_eq!(usage.input_tokens, Some(30));
        assert_eq!(usage.output_tokens, Some(10));
    }

    #[test]
    fn session_updated_token_usage_none_when_absent() {
        let raw = r#"{"event":"session_updated","chat_id":"chat-1","scope":"metadata","workspace_scope":{}}"#;
        let value: serde_json::Value = serde_json::from_str(raw).unwrap();
        assert!(session_updated_token_usage(&value).is_none());
    }

    #[test]
    fn parses_attached_with_history() {
        let raw = r#"{"event":"attached","chat_id":"chat-1","history":[
            {"role":"user","content":"hello"},
            {"role":"assistant","content":"hi","reasoning_content":"think"},
            {"role":"tool","content":"skipped"}
        ]}"#;
        let event = parse_server_event(raw).expect("should parse");
        match event {
            ServerEvent::Attached {
                chat_id, history, ..
            } => {
                assert_eq!(chat_id, "chat-1");
                assert_eq!(history.len(), 2);
                assert_eq!(history[0].id, 0);
                assert_eq!(history[0].role, Role::User);
                assert_eq!(history[0].content, "hello");
                assert_eq!(history[1].id, 1);
                assert_eq!(history[1].role, Role::Assistant);
                assert_eq!(history[1].content, "hi");
                assert_eq!(history[1].reasoning.as_deref(), Some("think"));
            }
            other => panic!("expected Attached, got {other:?}"),
        }
    }

    #[test]
    fn parses_attached_history_activity_into_tool_events() {
        let raw = r#"{"event":"attached","chat_id":"chat-1","history":[
            {"role":"user","content":"hello"},
            {"role":"assistant","content":"hi","activity":[
                {"kind":"tool_hint","text":"read foo.rs"},
                {"kind":"progress","text":"thinking..."}
            ]}
        ]}"#;
        let event = parse_server_event(raw).expect("should parse");
        match event {
            ServerEvent::Attached { history, .. } => {
                assert_eq!(history.len(), 2);
                assert!(history[0].tool_events.is_none());
                let tool_events = history[1]
                    .tool_events
                    .as_ref()
                    .expect("expected tool_events");
                assert_eq!(tool_events.len(), 2);
                assert_eq!(tool_events[0].name, "⚙ read foo.rs");
                assert_eq!(tool_events[0].status, "done");
                assert_eq!(tool_events[1].name, "↳ thinking...");
                assert_eq!(tool_events[1].status, "note");
            }
            other => panic!("expected Attached, got {other:?}"),
        }
    }

    #[test]
    fn parses_attached_history_media_into_attachments() {
        let raw = r#"{"event":"attached","chat_id":"chat-1","history":[
            {"role":"user","content":"look at this","media":["/v1/media/websocket/abc.png"]},
            {"role":"assistant","content":"a cat"}
        ]}"#;
        let event = parse_server_event(raw).expect("should parse");
        match event {
            ServerEvent::Attached { history, .. } => {
                assert_eq!(history.len(), 2);
                assert_eq!(history[0].attachments.len(), 1);
                assert_eq!(history[0].attachments[0].url, "/v1/media/websocket/abc.png");
                assert!(history[0].attachments[0].label.is_none());
                assert!(history[1].attachments.is_empty());
            }
            other => panic!("expected Attached, got {other:?}"),
        }
    }

    #[test]
    fn parses_attached_history_row_without_media_has_no_attachments() {
        let raw = r#"{"event":"attached","chat_id":"chat-1","history":[
            {"role":"user","content":"hello"}
        ]}"#;
        let event = parse_server_event(raw).expect("should parse");
        match event {
            ServerEvent::Attached { history, .. } => {
                assert!(history[0].attachments.is_empty());
            }
            other => panic!("expected Attached, got {other:?}"),
        }
    }

    #[test]
    fn client_envelope_attach_serializes_expected_shape() {
        let envelope = ClientEnvelope::attach("chat-1");
        let value = serde_json::to_value(&envelope).expect("should serialize");
        assert_eq!(
            value,
            serde_json::json!({
                "type": "attach",
                "chat_id": "chat-1",
                "webui": true,
            })
        );
    }

    #[test]
    fn client_envelope_fork_chat_serializes_expected_shape() {
        let envelope = ClientEnvelope::fork_chat("chat-1");
        let value = serde_json::to_value(&envelope).expect("should serialize");
        assert_eq!(
            value,
            serde_json::json!({
                "type": "fork_chat",
                "chat_id": "chat-1",
                "webui": true,
            })
        );
    }

    #[test]
    fn client_envelope_fork_chat_before_serializes_before_user_index() {
        let envelope = ClientEnvelope::fork_chat_before("chat-1", 2);
        let value = serde_json::to_value(&envelope).expect("should serialize");
        assert_eq!(
            value,
            serde_json::json!({
                "type": "fork_chat",
                "chat_id": "chat-1",
                "before_user_index": 2,
                "webui": true,
            })
        );
    }

    #[test]
    fn parses_session_updated_as_raw_value() {
        let raw = r#"{"event":"session_updated","chat_id":"chat-1","session":{"key":"value"}}"#;
        let event = parse_server_event(raw).expect("should parse");
        assert_eq!(
            event,
            ServerEvent::SessionUpdated(serde_json::from_str(raw).unwrap())
        );
    }

    #[test]
    fn parses_message_accepted() {
        let raw = r#"{"event":"message_accepted","chat_id":"chat-1","turn_id":"turn-1"}"#;
        let event = parse_server_event(raw).expect("should parse");
        assert_eq!(
            event,
            ServerEvent::MessageAccepted {
                chat_id: "chat-1".to_string(),
                turn_id: "turn-1".to_string(),
            }
        );
    }

    #[test]
    fn parses_goal_state_as_raw_value() {
        let raw = r#"{"event":"goal_state","chat_id":"chat-1","goal":"do the thing"}"#;
        let event = parse_server_event(raw).expect("should parse");
        assert_eq!(
            event,
            ServerEvent::GoalState(serde_json::from_str(raw).unwrap())
        );
    }

    #[test]
    fn parses_goal_status_with_all_fields() {
        let raw = r#"{"event":"goal_status","chat_id":"chat-1","status":"running","started_at":123.4,"turn_id":"turn-1"}"#;
        let event = parse_server_event(raw).expect("should parse");
        assert_eq!(
            event,
            ServerEvent::GoalStatus {
                chat_id: "chat-1".to_string(),
                status: "running".to_string(),
                started_at: Some(123.4),
                turn_id: Some("turn-1".to_string()),
            }
        );
    }

    #[test]
    fn parses_goal_status_with_only_required_fields() {
        let raw = r#"{"event":"goal_status","chat_id":"chat-1","status":"running"}"#;
        let event = parse_server_event(raw).expect("should parse");
        assert_eq!(
            event,
            ServerEvent::GoalStatus {
                chat_id: "chat-1".to_string(),
                status: "running".to_string(),
                started_at: None,
                turn_id: None,
            }
        );
    }

    #[test]
    fn parses_plain_final_message() {
        let raw = r#"{"event":"message","chat_id":"chat-1","text":"hello there"}"#;
        let event = parse_server_event(raw).expect("should parse");
        assert_eq!(
            event,
            ServerEvent::Message {
                chat_id: "chat-1".to_string(),
                text: "hello there".to_string(),
                media: None,
                reply_to: None,
                latency_ms: None,
                kind: None,
                tool_events: None,
            }
        );
    }

    #[test]
    fn parses_tool_hint_message_with_tool_events() {
        let raw = r#"{
            "event":"message",
            "chat_id":"chat-1",
            "text":"running a tool",
            "kind":"tool_hint",
            "reply_to":"turn-1",
            "latency_ms":42,
            "tool_events":[{"name":"search","status":"running","detail":"querying docs"}]
        }"#;
        let event = parse_server_event(raw).expect("should parse");
        assert_eq!(
            event,
            ServerEvent::Message {
                chat_id: "chat-1".to_string(),
                text: "running a tool".to_string(),
                media: None,
                reply_to: Some("turn-1".to_string()),
                latency_ms: Some(42),
                kind: Some("tool_hint".to_string()),
                tool_events: Some(vec![ToolEvent {
                    name: "search".to_string(),
                    status: "running".to_string(),
                    detail: Some("querying docs".to_string()),
                }]),
            }
        );
    }

    #[test]
    fn parses_delta() {
        let raw =
            r#"{"event":"delta","chat_id":"chat-1","text":"partial ","stream_id":"stream-1"}"#;
        let event = parse_server_event(raw).expect("should parse");
        assert_eq!(
            event,
            ServerEvent::Delta {
                chat_id: "chat-1".to_string(),
                text: "partial ".to_string(),
                stream_id: Some("stream-1".to_string()),
            }
        );
    }

    #[test]
    fn parses_stream_end_with_full_text() {
        let raw = r#"{"event":"stream_end","chat_id":"chat-1","text":"full text","stream_id":"stream-1","resuming":true,"merge_next":false}"#;
        let event = parse_server_event(raw).expect("should parse");
        assert_eq!(
            event,
            ServerEvent::StreamEnd {
                chat_id: "chat-1".to_string(),
                text: Some("full text".to_string()),
                stream_id: Some("stream-1".to_string()),
                resuming: Some(true),
                merge_next: Some(false),
            }
        );
    }

    #[test]
    fn parses_stream_end_without_text() {
        let raw = r#"{"event":"stream_end","chat_id":"chat-1"}"#;
        let event = parse_server_event(raw).expect("should parse");
        assert_eq!(
            event,
            ServerEvent::StreamEnd {
                chat_id: "chat-1".to_string(),
                text: None,
                stream_id: None,
                resuming: None,
                merge_next: None,
            }
        );
    }

    #[test]
    fn parses_reasoning_delta() {
        let raw = r#"{"event":"reasoning_delta","chat_id":"chat-1","text":"thinking...","stream_id":"stream-1"}"#;
        let event = parse_server_event(raw).expect("should parse");
        assert_eq!(
            event,
            ServerEvent::ReasoningDelta {
                chat_id: "chat-1".to_string(),
                text: "thinking...".to_string(),
                stream_id: Some("stream-1".to_string()),
            }
        );
    }

    #[test]
    fn parses_reasoning_end() {
        let raw = r#"{"event":"reasoning_end","chat_id":"chat-1","stream_id":"stream-1"}"#;
        let event = parse_server_event(raw).expect("should parse");
        assert_eq!(
            event,
            ServerEvent::ReasoningEnd {
                chat_id: "chat-1".to_string(),
                stream_id: Some("stream-1".to_string()),
            }
        );
    }

    #[test]
    fn parses_file_edit() {
        let raw = r#"{"event":"file_edit","chat_id":"chat-1","edits":[{"path":"src/main.rs","diff":"..."}]}"#;
        let event = parse_server_event(raw).expect("should parse");
        let mut expected_edit = HashMap::new();
        expected_edit.insert("path".to_string(), "src/main.rs".to_string());
        expected_edit.insert("diff".to_string(), "...".to_string());
        assert_eq!(
            event,
            ServerEvent::FileEdit {
                chat_id: "chat-1".to_string(),
                edits: vec![expected_edit],
            }
        );
    }

    #[test]
    fn parses_chats_list() {
        let raw = r#"{"event":"chats","chats":[
            {"chat_id":"chat-1","title":"Fix the login bug","created_at":"2024-01-01T00:00:00Z","updated_at":"2024-01-02T00:00:00Z"},
            {"chat_id":"chat-2","title":"","created_at":"2024-01-03T00:00:00Z","updated_at":"2024-01-03T00:00:00Z"}
        ]}"#;
        let event = parse_server_event(raw).expect("should parse");
        assert_eq!(
            event,
            ServerEvent::ChatsList {
                chats: vec![
                    ChatSummary {
                        chat_id: "chat-1".to_string(),
                        title: "Fix the login bug".to_string(),
                        created_at: "2024-01-01T00:00:00Z".to_string(),
                        updated_at: "2024-01-02T00:00:00Z".to_string(),
                    },
                    ChatSummary {
                        chat_id: "chat-2".to_string(),
                        title: String::new(),
                        created_at: "2024-01-03T00:00:00Z".to_string(),
                        updated_at: "2024-01-03T00:00:00Z".to_string(),
                    },
                ],
            }
        );
    }

    #[test]
    fn parses_empty_chats_list() {
        let raw = r#"{"event":"chats","chats":[]}"#;
        let event = parse_server_event(raw).expect("should parse");
        assert_eq!(event, ServerEvent::ChatsList { chats: Vec::new() });
    }

    #[test]
    fn chats_list_has_no_scoping_chat_id() {
        let event = ServerEvent::ChatsList { chats: Vec::new() };
        assert_eq!(event.chat_id(), None);
    }

    #[test]
    fn client_envelope_list_chats_serializes_expected_shape() {
        let envelope = ClientEnvelope::list_chats();
        let value = serde_json::to_value(&envelope).expect("should serialize");
        assert_eq!(
            value,
            serde_json::json!({
                "type": "list_chats",
                "webui": true,
            })
        );
    }

    #[test]
    fn client_envelope_rename_chat_serializes_expected_shape() {
        let envelope = ClientEnvelope::rename_chat("chat-1", "Fix the login bug");
        let value = serde_json::to_value(&envelope).expect("should serialize");
        assert_eq!(
            value,
            serde_json::json!({
                "type": "rename_chat",
                "chat_id": "chat-1",
                "title": "Fix the login bug",
                "webui": true,
            })
        );
    }

    #[test]
    fn parses_chat_renamed() {
        let raw = r#"{"event":"chat_renamed","chat_id":"chat-1","title":"Fix the login bug"}"#;
        let event = parse_server_event(raw).expect("should parse");
        assert_eq!(
            event,
            ServerEvent::ChatRenamed {
                chat_id: "chat-1".to_string(),
                title: "Fix the login bug".to_string(),
            }
        );
        assert_eq!(event.chat_id(), None);
    }

    #[test]
    fn client_envelope_delete_chat_serializes_expected_shape() {
        let envelope = ClientEnvelope::delete_chat("chat-1");
        let value = serde_json::to_value(&envelope).expect("should serialize");
        assert_eq!(
            value,
            serde_json::json!({
                "type": "delete_chat",
                "chat_id": "chat-1",
                "webui": true,
            })
        );
    }

    #[test]
    fn parses_chat_deleted() {
        let raw = r#"{"event":"chat_deleted","chat_id":"chat-1"}"#;
        let event = parse_server_event(raw).expect("should parse");
        assert_eq!(
            event,
            ServerEvent::ChatDeleted {
                chat_id: "chat-1".to_string(),
            }
        );
        assert_eq!(event.chat_id(), None);
    }

    #[test]
    fn client_envelope_abort_turn_serializes_expected_shape() {
        let envelope = ClientEnvelope::abort_turn("chat-1", Some("turn-1".to_string()));
        let value = serde_json::to_value(&envelope).expect("should serialize");
        assert_eq!(
            value,
            serde_json::json!({
                "type": "abort_turn",
                "chat_id": "chat-1",
                "turn_id": "turn-1",
                "webui": true,
            })
        );
    }

    #[test]
    fn client_envelope_abort_turn_omits_an_absent_turn_id() {
        let envelope = ClientEnvelope::abort_turn("chat-1", None);
        let value = serde_json::to_value(&envelope).expect("should serialize");
        assert_eq!(
            value,
            serde_json::json!({
                "type": "abort_turn",
                "chat_id": "chat-1",
                "webui": true,
            })
        );
    }

    #[test]
    fn parses_turn_aborted() {
        let raw = r#"{"event":"turn_aborted","chat_id":"chat-1","turn_id":"turn-1"}"#;
        let event = parse_server_event(raw).expect("should parse");
        assert_eq!(
            event,
            ServerEvent::TurnAborted {
                chat_id: "chat-1".to_string(),
                turn_id: Some("turn-1".to_string()),
            }
        );
        // Scoped, unlike rename/delete: an abort only ever concerns the chat
        // whose stream this connection is rendering.
        assert_eq!(event.chat_id(), Some("chat-1"));
    }

    #[test]
    fn parses_turn_aborted_without_turn_id() {
        let raw = r#"{"event":"turn_aborted","chat_id":"chat-1"}"#;
        let event = parse_server_event(raw).expect("should parse");
        assert_eq!(
            event,
            ServerEvent::TurnAborted {
                chat_id: "chat-1".to_string(),
                turn_id: None,
            }
        );
    }

    #[test]
    fn client_envelope_set_model_preset_serializes_expected_shape() {
        let envelope = ClientEnvelope::set_model_preset("chat-1", "fast");
        let value = serde_json::to_value(&envelope).expect("should serialize");
        assert_eq!(
            value,
            serde_json::json!({
                "type": "set_model_preset",
                "chat_id": "chat-1",
                "model_preset": "fast",
                "webui": true,
            })
        );
    }

    #[test]
    fn parses_model_preset_set() {
        let raw = r#"{"event":"model_preset_set","chat_id":"chat-1","model_preset":"fast","model":"claude-haiku"}"#;
        let event = parse_server_event(raw).expect("should parse");
        assert_eq!(
            event,
            ServerEvent::ModelPresetSet {
                chat_id: "chat-1".to_string(),
                model_preset: "fast".to_string(),
                model: "claude-haiku".to_string(),
            }
        );
        // Scoped, like `turn_aborted`: an ack only ever concerns the chat it
        // was requested for.
        assert_eq!(event.chat_id(), Some("chat-1"));
    }

    #[test]
    fn unknown_event_name_produces_unknown_variant() {
        let raw = r#"{"event":"some_future_event","chat_id":"chat-1","whatever":true}"#;
        let event = parse_server_event(raw).expect("unknown event should not error");
        assert_eq!(
            event,
            ServerEvent::Unknown(serde_json::from_str(raw).unwrap())
        );
    }

    #[test]
    fn missing_event_field_produces_unknown_variant() {
        let raw = r#"{"chat_id":"chat-1","whatever":true}"#;
        let event = parse_server_event(raw).expect("missing event field should not error");
        assert_eq!(
            event,
            ServerEvent::Unknown(serde_json::from_str(raw).unwrap())
        );
    }

    #[test]
    fn non_string_event_field_produces_unknown_variant() {
        let raw = r#"{"event":123,"chat_id":"chat-1"}"#;
        let event = parse_server_event(raw).expect("non-string event field should not error");
        assert_eq!(
            event,
            ServerEvent::Unknown(serde_json::from_str(raw).unwrap())
        );
    }

    #[test]
    fn known_event_with_bad_shape_is_an_error() {
        // "ready" is known, but this payload is missing the required `client_id`.
        let raw = r#"{"event":"ready","chat_id":"chat-1"}"#;
        let err = parse_server_event(raw).expect_err("missing required field should error");
        assert!(!err.message.is_empty());
    }

    #[test]
    fn client_envelope_new_chat_serializes_expected_shape() {
        let envelope = ClientEnvelope::new_chat();
        let value = serde_json::to_value(&envelope).expect("should serialize");
        assert_eq!(
            value,
            serde_json::json!({
                "type": "new_chat",
                "webui": true,
            })
        );
    }

    #[test]
    fn client_envelope_serializes_expected_shape() {
        let envelope =
            ClientEnvelope::message("chat-123", Some("turn-456".to_string()), "hello", None);
        let value = serde_json::to_value(&envelope).expect("should serialize");
        assert_eq!(
            value,
            serde_json::json!({
                "type": "message",
                "chat_id": "chat-123",
                "turn_id": "turn-456",
                "content": "hello",
                "webui": true,
            })
        );
    }

    #[test]
    fn client_envelope_omits_absent_optional_fields() {
        let envelope = ClientEnvelope::message("chat-123", None, "hi", None);
        let value = serde_json::to_value(&envelope).expect("should serialize");
        assert_eq!(
            value,
            serde_json::json!({
                "type": "message",
                "chat_id": "chat-123",
                "content": "hi",
                "webui": true,
            })
        );
    }

    #[test]
    fn client_envelope_includes_media_when_present() {
        let envelope = ClientEnvelope::message(
            "chat-123",
            None,
            "hi",
            Some(vec![
                serde_json::json!({"url": "data:image/png;base64,AAAA"}),
            ]),
        );
        let value = serde_json::to_value(&envelope).expect("should serialize");
        assert_eq!(
            value,
            serde_json::json!({
                "type": "message",
                "chat_id": "chat-123",
                "content": "hi",
                "media": [{"url": "data:image/png;base64,AAAA"}],
                "webui": true,
            })
        );
    }
}
