//! Wire protocol for the gateway's WebSocket channel.
//!
//! Mirrors the JSON shapes defined server-side in `src/channels/websocket/types.rs`
//! (and the handlers in `src/channels/websocket/runtime.rs` that emit them):
//! this module owns both directions of that conversation from the browser's
//! side of the connection.
//!
//! * Outbound: [`ClientEnvelope`], currently `message`, `new_chat`,
//!   `attach`, and `list_chats`.
//! * Inbound: [`ServerEvent`], one variant per `event` value the gateway can
//!   send, decoded by [`parse_server_event`].
//!
//! Everything here is plain data + parsing with no Leptos/wasm/`web-sys`
//! dependency, so it is unit-testable with plain `#[test]` on the host
//! target — no `wasm-bindgen-test` machinery required.

use std::collections::HashMap;

use chat_ui::models::{ChatEntry, Role, ToolEvent};
use serde::{Deserialize, Serialize};

/// Outbound envelope sent to the gateway.
///
/// The backend's `EnvelopeType` (`src/channels/websocket/types.rs`) has six
/// variants (`new_chat`, `fork_chat`, `attach`, `set_workspace_scope`,
/// `transcribe_audio`, `message`). This struct covers the ones the frontend
/// actually sends (`message`, `new_chat`, `attach`, `list_chats`); constructors
/// pin `type_` so callers cannot invent a shape the gateway would reject
/// with "unknown type".
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
    /// `new_chat` or `attach` envelope. `history` is the display snapshot
    /// from the gateway (`[]` when the chat is new or has no messages, and
    /// when an older gateway omits the field).
    Attached {
        chat_id: String,
        history: Vec<ChatEntry>,
    },
    /// Sent when the server-side session state changes. Shape not yet
    /// finalized server-side, so the raw JSON is kept as-is.
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
    /// An `event` value this crate doesn't recognize (or a missing/non-string
    /// `event` field), carrying the raw decoded JSON so nothing is lost.
    Unknown(serde_json::Value),
}

impl ServerEvent {
    /// The `chat_id` this event is scoped to, if the payload carries one.
    ///
    /// Used by the app to ignore leftover frames from a previous chat after
    /// `new_chat` switches the connection onto a new id. `Unknown` and
    /// unscoped errors (no `chat_id`) return `None`.
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
            | ServerEvent::FileEdit { chat_id, .. } => Some(chat_id.as_str()),
            ServerEvent::Error { chat_id, .. } => chat_id.as_deref(),
            ServerEvent::SessionUpdated(value) | ServerEvent::GoalState(value) => {
                value.get("chat_id").and_then(serde_json::Value::as_str)
            }
            ServerEvent::ChatsList { .. } | ServerEvent::Unknown(_) => None,
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

/// One row of the `attached` event's `history` array — mirrors
/// `websocket_chat_history` in `src/channels/websocket/runtime.rs`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
struct HistoryMessage {
    role: String,
    #[serde(default)]
    content: String,
    #[serde(default)]
    reasoning_content: Option<String>,
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
                attachments: Vec::new(),
                streaming: false,
                tool_events: None,
                reasoning: message
                    .reasoning_content
                    .clone()
                    .filter(|s| !s.is_empty()),
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
        _ => Ok(ServerEvent::Unknown(value)),
    }
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
            }
        );
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
            ServerEvent::Attached { chat_id, history } => {
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
