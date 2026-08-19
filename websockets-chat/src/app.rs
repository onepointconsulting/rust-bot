//! Top-level signal-driven composition: login, the gateway WebSocket
//! connection lifecycle (connect / reconnect-with-backoff / manual retry),
//! and dispatching inbound [`ServerEvent`]s into the `entries` transcript
//! via `state.rs`'s pure reducers.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

// Aliased/anonymous imports: `leptos::prelude::*` (below) also re-exports
// `reactive_graph::owner::{LocalStorage, Storage}` — the arena-storage
// marker type and trait used for `RwSignal<T, LocalStorage>` (see
// `WsContext::ws_sender`) — which would otherwise collide with gloo_storage's
// unrelated same-named browser-storage type/trait.
use gloo_storage::LocalStorage as BrowserLocalStorage;
use gloo_storage::SessionStorage;
use gloo_storage::Storage as _;
use leptos::prelude::*;
use leptos::task::spawn_local;
use uuid::Uuid;

use chat_ui::api::login;
use chat_ui::components::LoginForm;
use chat_ui::models::{ChatEntry, ImageAttachment, OutgoingMessage, Role, SessionListItem};

use crate::api::{self, WsSender};
use crate::components::ChatShell;
use crate::protocol::{self, ServerEvent};
use crate::state::{self, ConnectionStatus};
use crate::storage_keys::CHAT_ID_STORAGE_KEY;
use crate::storage_keys::CHAT_OPEN_STORAGE_KEY;
use crate::storage_keys::EXPANDED_STORAGE_KEY;

const TOKEN_STORAGE_KEY: &str = "rust-bot-websockets-chat-token";
const ENTRIES_STORAGE_KEY: &str = "rust-bot-websockets-chat-entries";

/// Persisted in `LocalStorage` (survives across tabs and reloads, unlike
/// `SessionStorage`) because the gateway's `allow_from` allow-list is keyed
/// by `client_id` — a reconnect should present the same identity it used
/// before, not a fresh random one on every page load.
const CLIENT_ID_STORAGE_KEY: &str = "rust-bot-websockets-chat-client-id";

/// The gateway WebSocket path this app connects to. Must match the backend
/// config's `channels.extra["websocket"].path` (`WebSocketConfig::path`
/// server-side) — there's no discovery mechanism, so a mismatch here means
/// every connection attempt is rejected at the upgrade.
///
/// Deliberately NOT the server-side default of `"/"`: the gateway registers
/// the WebSocket upgrade handler as a literal route at `channels.websocket.path`,
/// which takes priority over the static-file fallback that serves this app's
/// own `index.html` when `gateway.webRoot` points at this app's built `dist/`.
/// With path `"/"`, a plain browser page-load GET to `/` always lands on the
/// upgrade handler instead of the SPA shell, and axum rejects it with
/// "Connection header did not include 'upgrade'" — the SPA never loads. Using
/// a non-root path here (and in the backend config's `channels.websocket.path`
/// + matching `jwt.aud`, which must always equal it) leaves `/` free for the
/// static UI while the WebSocket lives at its own path.
const GATEWAY_WS_PATH: &str = "/ws";

/// Max user/assistant exchanges kept in `SessionStorage` across a refresh.
const MAX_STORED_TURNS: usize = 10;

fn read_stored_token() -> Option<String> {
    SessionStorage::get::<String>(TOKEN_STORAGE_KEY).ok()
}

fn read_stored_entries() -> Vec<ChatEntry> {
    SessionStorage::get::<Vec<ChatEntry>>(ENTRIES_STORAGE_KEY).unwrap_or_default()
}

/// `data:` attachments are re-encoded, in-memory image bytes; they are far
/// too large (and pointless) to round-trip through SessionStorage, so only
/// `http(s)://` attachments survive a page refresh.
fn strip_data_url_attachments(entries: &[ChatEntry]) -> Vec<ChatEntry> {
    entries
        .iter()
        .map(|entry| {
            let mut entry = entry.clone();
            entry
                .attachments
                .retain(|attachment| !attachment.url.starts_with("data:"));
            entry
        })
        .collect()
}

/// Snapshot `entries` to SessionStorage for the next page load.
///
/// Trimmed as well as sanitized: an `attached` history snapshot can be up to
/// the gateway's `MAX_HISTORY_MESSAGES` (500) rows, which is fine to hold in
/// memory but would push a long chat past the ~5 MB storage quota and fail
/// the whole write. The push path has already trimmed by the time it calls
/// here, so this is a no-op there.
fn persist_entries(entries: &[ChatEntry]) {
    let sanitized = strip_data_url_attachments(entries);
    let trimmed = trim_to_max_turns(sanitized, MAX_STORED_TURNS);
    let _ = SessionStorage::set(ENTRIES_STORAGE_KEY, &trimmed);
}

fn clear_stored_entries() {
    SessionStorage::delete(ENTRIES_STORAGE_KEY);
}

fn next_entry_id(entries: &[ChatEntry]) -> u64 {
    entries
        .iter()
        .map(|e| e.id)
        .max()
        .map(|id| id + 1)
        .unwrap_or(0)
}

/// Keep the last `max_turns` user messages and everything after the first kept user message.
fn trim_to_max_turns(entries: Vec<ChatEntry>, max_turns: usize) -> Vec<ChatEntry> {
    let user_indices: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.role == Role::User)
        .map(|(index, _)| index)
        .collect();

    if user_indices.len() <= max_turns {
        return entries;
    }

    let start = user_indices[user_indices.len() - max_turns];
    entries[start..].to_vec()
}

/// Read (once) or mint a stable per-browser `client_id`.
fn read_stored_chat_id() -> Option<String> {
    BrowserLocalStorage::get::<String>(CHAT_ID_STORAGE_KEY)
        .ok()
        .filter(|id| !id.is_empty())
}

fn persist_chat_id(chat_id: &str) {
    let _ = BrowserLocalStorage::set(CHAT_ID_STORAGE_KEY, &chat_id.to_string());
}

fn clear_stored_chat_id() {
    BrowserLocalStorage::delete(CHAT_ID_STORAGE_KEY);
}

/// Remember `chat_id` in memory and localStorage so a refresh or browser
/// restart keeps appending to the same gateway session.
fn adopt_chat_id(ctx: &WsContext, chat_id: String) {
    persist_chat_id(&chat_id);
    ctx.chat_id.set(Some(chat_id));
}

fn read_or_create_client_id() -> String {
    if let Ok(existing) = BrowserLocalStorage::get::<String>(CLIENT_ID_STORAGE_KEY) {
        if !existing.is_empty() {
            return existing;
        }
    }
    let generated = Uuid::new_v4().to_string();
    let _ = BrowserLocalStorage::set(CLIENT_ID_STORAGE_KEY, &generated);
    generated
}

/// Parse an optional `?wsBase=...` query parameter off the current page
/// URL, letting local dev point the WebSocket connection at a gateway
/// running on a different port than the one Trunk itself serves from (e.g.
/// `trunk serve --open` then visiting
/// `http://127.0.0.1:8902/?wsBase=ws://127.0.0.1:18790`). Read once at
/// startup; a single well-known key doesn't need a query-string crate.
fn read_ws_base_override() -> Option<String> {
    let window = web_sys::window()?;
    let search = window.location().search().ok()?;
    let query = search.strip_prefix('?')?;
    for pair in query.split('&') {
        let mut parts = pair.splitn(2, '=');
        let key = parts.next()?;
        if key == "wsBase" {
            let raw_value = parts.next().unwrap_or_default();
            let decoded = js_sys::decode_uri_component(raw_value)
                .ok()
                .and_then(|value| value.as_string())
                .unwrap_or_else(|| raw_value.to_string());
            return (!decoded.is_empty()).then_some(decoded);
        }
    }
    None
}

/// Build the gateway WebSocket URL for the current page's origin (or the
/// `?wsBase=` override), [`GATEWAY_WS_PATH`], and the given `client_id`/`token`.
fn build_gateway_ws_url(client_id: &str, token: &str, ws_base_override: Option<&str>) -> String {
    let (scheme, host) = web_sys::window()
        .map(|window| {
            let location = window.location();
            let scheme = location.protocol().unwrap_or_else(|_| "http:".to_string());
            let host = location.host().unwrap_or_default();
            (scheme.trim_end_matches(':').to_string(), host)
        })
        .unwrap_or_else(|| ("http".to_string(), String::new()));
    state::build_ws_url(
        &scheme,
        &host,
        GATEWAY_WS_PATH,
        client_id,
        token,
        ws_base_override,
    )
}

/// The gateway's websocket ingestion (`store_inbound_attachments` in
/// `src/security/attachment_ingress.rs`) only accepts inline `data:` URL
/// attachments shaped as `{"data_url": "data:..."}` — unlike the REST API's
/// more permissive `image_url` shape, it does not accept `http(s)://`
/// references. `http(s)://` attachments therefore stay visible locally (as
/// attachment chips on the user's own bubble) but are not forwarded to the
/// gateway as part of the turn; only `data:` attachments (the common case
/// from the file picker or a paste) are.
fn build_media_payload(attachments: &[ImageAttachment]) -> Option<Vec<serde_json::Value>> {
    let items: Vec<serde_json::Value> = attachments
        .iter()
        .filter(|attachment| attachment.url.starts_with("data:"))
        .map(|attachment| serde_json::json!({ "data_url": attachment.url }))
        .collect();
    (!items.is_empty()).then_some(items)
}

/// Everything the WebSocket connection lifecycle (`open_connection`,
/// `schedule_reconnect`, `dispatch_server_event`, ...) needs, bundled so
/// those free functions take one argument instead of a dozen signals.
///
/// Every field is a `Copy` `RwSignal` handle (the one field that wraps
/// non-`Send`/non-`Sync` data, `ws_sender`, uses `RwSignal::new_local` —
/// see its own doc comment), which makes `WsContext` itself `Copy`. That
/// matters because `chat_ui::components::{LoginForm, ChatInput}` require
/// their callback props (`on_submit`, `on_send`) to be `Fn + 'static +
/// Copy`, and those callbacks close over a `WsContext`.
#[derive(Clone, Copy)]
struct WsContext {
    token: RwSignal<Option<String>>,
    client_id: RwSignal<String>,
    chat_id: RwSignal<Option<String>>,
    connection_status: RwSignal<ConnectionStatus>,
    entries: RwSignal<Vec<ChatEntry>>,
    next_id: RwSignal<u64>,
    turn_index: RwSignal<HashMap<String, u64>>,
    /// The turn most recently sent by this client, if any.
    ///
    /// `delta`/`stream_end`/`reasoning_delta`/`reasoning_end` events from
    /// the gateway (see `protocol::ServerEvent`) carry only a `stream_id`,
    /// never a `turn_id` — those streaming events aren't scoped to a turn
    /// on the wire at all. Since this UI only ever has one turn in flight
    /// at a time (`ChatInput`'s `pending` prop, driven by whether any entry
    /// is still `streaming`, disables sending until it resolves), the
    /// active turn is unambiguous, so those events are routed to whichever
    /// turn_id is recorded here rather than to anything parsed from the
    /// event's own payload.
    active_turn_id: RwSignal<Option<String>>,
    chat_error: RwSignal<Option<String>>,
    /// 0-indexed count of reconnect attempts since the last successful
    /// connection, per the convention shared with
    /// [`state::compute_backoff_delay_ms`] and
    /// [`state::reconnect_attempts_exhausted`].
    reconnect_attempt: RwSignal<u32>,
    reconnect_exhausted: RwSignal<bool>,
    ws_base_override: RwSignal<Option<String>>,
    /// The current socket's send half, if connected. `RwSignal::new_local`
    /// (rather than `RwSignal::new`) because `WsSender` wraps a
    /// browser-only `web_sys`/`gloo_net` type that is not `Send`/`Sync`
    /// (unlike every other field here, which is plain, thread-agnostic
    /// data) — `LocalStorage`-backed signals drop the `Send + Sync` bound
    /// in exchange for being pinned to the thread that created them, which
    /// is exactly right for a `wasm32-unknown-unknown` CSR app that only
    /// ever runs on the browser's single JS thread.
    ws_sender: RwSignal<Rc<RefCell<Option<WsSender>>>, LocalStorage>,
    /// Bumped by every [`open_connection`]/[`close_connection`] call. A
    /// receive loop's `on_close` callback captures the generation value at
    /// the time its connection was opened and compares it against the
    /// current one; a mismatch means a newer connection has already
    /// superseded it, so its close is a stale no-op rather than triggering
    /// a redundant reconnect. See [`handle_connection_closed`].
    generation: RwSignal<u64>,
    /// From the gateway `ready` event: whether this channel will stream
    /// token `delta`s. After the first visible token, drives the in-progress
    /// indicator (blinking cursor vs nothing); the thinking spinner is shown
    /// until then in either mode. Defaults to `false` until `ready` arrives,
    /// matching the server's `WebSocketConfig.streaming` default.
    token_streaming: RwSignal<bool>,
    /// This connection's `websocket:*` chats, for the sessions sidebar.
    /// Refreshed by [`request_chat_list`] on `ready`/`attached` and after
    /// each turn finishes, since title generation runs asynchronously with
    /// no push notification when it completes.
    sessions: RwSignal<Vec<SessionListItem>>,
    /// A sidebar session the user picked while the socket was down, to be
    /// attached as soon as `ready` arrives on the reopened connection. Takes
    /// priority over resuming [`Self::chat_id`], since it is the more recent
    /// intent. Cleared once the `attach` envelope is sent.
    pending_attach: RwSignal<Option<String>>,
    /// When true, the next `delta` / `reasoning_delta` for the active turn
    /// opens a new assistant bubble instead of appending to the one just
    /// closed by a `resuming` `stream_end`. Delayed until that first token
    /// so a following `tool_hint` still attaches to the thought that
    /// requested the tools — matching session history, which stores one
    /// assistant message per LLM round.
    split_stream_on_next_delta: RwSignal<bool>,
}

/// Clone the current `entries`/`turn_index` out of their signals, apply a
/// `state.rs` reducer to the plain data, and write the mutated `Vec` back —
/// the bridge the plan calls for between this app's signals and `state.rs`'s
/// signal-free functions.
/// Apply a `state.rs` reducer to `entries` and write the result back into
/// the signal — and, since this is the one bridge every streaming/tool-hint/
/// finalization update goes through, also re-persist to `SessionStorage`.
/// Without this, a delta/stream_end/tool_hint update was visible in the live
/// UI but never survived a refresh: only `push_entry` used to call
/// `persist_entries`, so the stored snapshot stayed frozen at whatever an
/// assistant entry looked like the instant its empty placeholder was
/// created (streaming, no content) — exactly what a refresh would then
/// restore, regardless of how the turn actually finished.
fn update_entries(
    ctx: &WsContext,
    mutator: impl FnOnce(&mut Vec<ChatEntry>, &HashMap<String, u64>),
) {
    let index = ctx.turn_index.get_untracked();
    let mut entries = ctx.entries.get_untracked();
    mutator(&mut entries, &index);
    persist_entries(&entries);
    ctx.entries.set(entries);
}

fn append_finished_assistant_entry(ctx: &WsContext, text: String) {
    let id = ctx.next_id.get_untracked();
    ctx.next_id.set(id + 1);
    ctx.entries.update(|list| {
        list.push(ChatEntry {
            id,
            role: Role::Assistant,
            content: text,
            attachments: Vec::new(),
            streaming: false,
            tool_events: None,
            reasoning: None,
        });
    });
    persist_entries(&ctx.entries.get_untracked());
}

/// Handle a `message` event, branching on whether `kind` is present (a live
/// progress/tool-hint update) or absent (the turn's final, non-streaming
/// answer).
fn handle_message_event(
    ctx: &WsContext,
    text: String,
    reply_to: Option<String>,
    kind: Option<String>,
    tool_events: Option<Vec<chat_ui::models::ToolEvent>>,
) {
    match kind {
        None => {
            // A plain, non-streaming final reply behaves exactly like a
            // `stream_end` carrying the authoritative full text, so
            // `apply_stream_end` is reused verbatim instead of duplicating
            // "overwrite content, clear streaming" here.
            let turn_id = reply_to.or_else(|| ctx.active_turn_id.get_untracked());
            match turn_id {
                Some(turn_id) => {
                    update_entries(ctx, |entries, index| {
                        state::apply_stream_end(entries, index, &turn_id, Some(&text));
                    });
                    ctx.active_turn_id.set(None);
                    ctx.split_stream_on_next_delta.set(false);
                    request_chat_list(ctx);
                    schedule_delayed_chat_list_refresh(*ctx);
                }
                None => {
                    // No turn to attach to (a server-initiated message
                    // outside any client-sent turn) — surface it as a new,
                    // already-finished assistant entry rather than
                    // dropping it.
                    append_finished_assistant_entry(ctx, text);
                }
            }
        }
        Some(kind) => {
            let turn_id = reply_to.or_else(|| ctx.active_turn_id.get_untracked());
            let Some(turn_id) = turn_id else {
                log::info!("{kind} message with no known turn to attach to: {text}");
                return;
            };
            // The backend's `before_execute_tools` hook only ever sends
            // free-text hints — a tool-call line (e.g. `web_search("...")`,
            // `kind: "tool_hint"`) and, whenever no `on_stream` callback is
            // wired up, a narration line (the model's in-between "thinking
            // out loud" text, `kind: "progress"`) — with `tool_events`
            // always left `None`. No code path in the backend populates the
            // structured shape this crate was originally built to render
            // (see `state::synthesize_tool_hint_event`'s doc comment for the
            // full explanation). Fall back to wrapping the text as a single
            // synthetic chip for both known kinds instead of dropping them;
            // any other/future kind with no structured events is still just
            // logged.
            let events = match tool_events {
                Some(events) if !events.is_empty() => events,
                _ if kind == "tool_hint" && !text.is_empty() => {
                    vec![state::synthesize_tool_hint_event(&text)]
                }
                _ if kind == "progress" && !text.is_empty() => {
                    vec![state::synthesize_progress_note_event(&text)]
                }
                _ => {
                    log::info!("{kind} message for turn {turn_id} with no tool_events: {text}");
                    return;
                }
            };
            update_entries(ctx, |entries, index| {
                state::apply_tool_hint(entries, index, &turn_id, events);
            });
        }
    }
}

/// Handle a `stream_end` event.
///
/// `merge_next` means more deltas will arrive on the *same* stream segment
/// (keep the current bubble streaming). `resuming` means tool calls follow
/// and a later LLM round will continue this turn — that round is stored as
/// its own assistant message in the session file, so live UI opens a new
/// bubble on the next token rather than concatenating ("onGood").
fn handle_stream_end(
    ctx: &WsContext,
    text: Option<String>,
    resuming: Option<bool>,
    merge_next: Option<bool>,
) {
    let Some(turn_id) = ctx.active_turn_id.get_untracked() else {
        return;
    };
    let resuming = resuming.unwrap_or(false);
    let merge_next = merge_next.unwrap_or(false);
    update_entries(ctx, |entries, index| {
        state::apply_stream_end(entries, index, &turn_id, text.as_deref());
        if merge_next {
            state::reopen_streaming(entries, index, &turn_id);
        }
    });
    if merge_next {
        ctx.split_stream_on_next_delta.set(false);
    } else if resuming {
        ctx.split_stream_on_next_delta.set(true);
    } else {
        ctx.split_stream_on_next_delta.set(false);
        ctx.active_turn_id.set(None);
        request_chat_list(ctx);
        schedule_delayed_chat_list_refresh(*ctx);
    }
}

/// If a `resuming` stream_end asked us to start a new bubble, retarget the
/// turn onto a fresh streaming assistant entry before the next tokens land.
fn maybe_begin_next_stream_segment(ctx: &WsContext, turn_id: &str) {
    if !ctx.split_stream_on_next_delta.get_untracked() {
        return;
    }
    ctx.split_stream_on_next_delta.set(false);
    let new_id = ctx.next_id.get_untracked();
    ctx.next_id.set(new_id + 1);
    let mut entries = ctx.entries.get_untracked();
    ctx.turn_index.update(|map| {
        state::begin_next_stream_segment(&mut entries, map, turn_id, new_id);
    });
    persist_entries(&entries);
    ctx.entries.set(entries);
}

/// Route one parsed [`ServerEvent`] to the appropriate `state.rs` reducer
/// (or, for events with no reducer, a log line) per the dispatch table in
/// the task's plan.
fn dispatch_server_event(ctx: &WsContext, event: ServerEvent) {
    if should_drop_event(ctx, &event) {
        return;
    }
    match event {
        ServerEvent::Ready {
            chat_id,
            client_id,
            streaming,
        } => {
            log::info!(
                "gateway ready: chat_id={chat_id} client_id={client_id} streaming={streaming}"
            );
            ctx.connection_status.set(ConnectionStatus::Connected);
            ctx.token_streaming.set(streaming);
            // The chat list is connection-scoped, not chat-scoped — ask for
            // it on every `ready`, not only after `attached`. A refresh
            // always has a stored `chat_id`, so the previous "list only on
            // the no-resume path" left the sidebar empty whenever attach
            // was slow, rejected, or its follow-up send never ran.
            request_chat_list(ctx);
            // The gateway mints a fresh chat_id per connection and
            // subscribes the connection to *that* id. So whenever we mean to
            // continue an existing chat (refresh, reconnect, or a session
            // picked while the socket was down), saying nothing would leave
            // this connection fanned out on a chat nobody is looking at
            // until the next `message` envelope happens to re-attach it.
            let resume = ctx
                .pending_attach
                .get_untracked()
                .or_else(|| ctx.chat_id.get_untracked());
            match resume {
                Some(target) => {
                    ctx.pending_attach.set(None);
                    send_client_envelope(
                        *ctx,
                        protocol::ClientEnvelope::attach(target),
                        "Failed to encode the attach request.",
                    );
                }
                None => adopt_chat_id(ctx, chat_id),
            }
        }
        ServerEvent::MessageAccepted { chat_id, turn_id } => {
            log::info!("message accepted: chat_id={chat_id} turn_id={turn_id}");
        }
        ServerEvent::GoalStatus {
            chat_id,
            status,
            started_at,
            turn_id,
        } => {
            log::info!(
                "goal status: chat_id={chat_id} status={status} started_at={started_at:?} turn_id={turn_id:?}"
            );
        }
        ServerEvent::Message {
            text,
            reply_to,
            kind,
            tool_events,
            ..
        } => {
            handle_message_event(ctx, text, reply_to, kind, tool_events);
        }
        ServerEvent::Delta { text, .. } => {
            if let Some(turn_id) = ctx.active_turn_id.get_untracked() {
                maybe_begin_next_stream_segment(ctx, &turn_id);
                update_entries(ctx, |entries, index| {
                    state::apply_delta(entries, index, &turn_id, &text);
                });
            }
        }
        ServerEvent::StreamEnd {
            text,
            resuming,
            merge_next,
            ..
        } => {
            handle_stream_end(ctx, text, resuming, merge_next);
        }
        ServerEvent::ReasoningDelta { text, .. } => {
            if let Some(turn_id) = ctx.active_turn_id.get_untracked() {
                maybe_begin_next_stream_segment(ctx, &turn_id);
                update_entries(ctx, |entries, index| {
                    state::apply_reasoning_delta(entries, index, &turn_id, &text);
                });
            }
        }
        ServerEvent::ReasoningEnd { .. } => {
            if let Some(turn_id) = ctx.active_turn_id.get_untracked() {
                update_entries(ctx, |entries, index| {
                    state::apply_reasoning_end(entries, index, &turn_id);
                });
            }
        }
        ServerEvent::Error {
            turn_id, detail, ..
        } => {
            ctx.chat_error.set(Some(detail));
            let turn_id = turn_id.or_else(|| ctx.active_turn_id.get_untracked());
            if let Some(turn_id) = turn_id {
                // No replacement text; just stop showing the entry as
                // still streaming.
                update_entries(ctx, |entries, index| {
                    state::apply_stream_end(entries, index, &turn_id, None);
                });
            }
            ctx.active_turn_id.set(None);
            ctx.split_stream_on_next_delta.set(false);
        }
        ServerEvent::Attached { chat_id, history } => {
            log::info!(
                "attached to chat_id={chat_id} history_len={}",
                history.len()
            );
            adopt_chat_id(ctx, chat_id);
            ctx.chat_error.set(None);
            ctx.turn_index.set(HashMap::new());
            ctx.active_turn_id.set(None);
            ctx.split_stream_on_next_delta.set(false);
            // An empty snapshot means the gateway has nothing *persisted*
            // for this chat (brand new, or a first turn still in flight) —
            // not that the transcript is empty. Adopting it verbatim would
            // erase a just-sent message when `ready` re-attaches after a
            // reconnect. A genuine session switch still lands on a blank
            // transcript because `on_select_session` clears entries itself.
            if !history.is_empty() {
                ctx.next_id.set(next_entry_id(&history));
                persist_entries(&history);
                ctx.entries.set(history);
            }
            request_chat_list(ctx);
        }
        ServerEvent::SessionUpdated(value) => log::info!("session updated: {value}"),
        ServerEvent::GoalState(value) => {
            log::info!("goal state (shape not finalized server-side): {value}")
        }
        ServerEvent::FileEdit { chat_id, edits } => {
            log::info!("file_edit for chat_id={chat_id}: {edits:?}");
        }
        ServerEvent::ChatsList { chats } => {
            let items: Vec<SessionListItem> = chats
                .into_iter()
                .map(|chat| SessionListItem {
                    id: chat.chat_id,
                    title: chat.title,
                    created_at: chat.created_at,
                    updated_at: chat.updated_at,
                })
                .collect();
            ctx.sessions.set(items);
        }
        ServerEvent::Unknown(value) => log::info!("unrecognized gateway event, ignoring: {value}"),
    }
}

/// Drop leftover frames from a previous chat after "New chat" clears
/// `chat_id` and before the `attached` ack lands — and, afterward, frames
/// still fanned out for the old subscription. `attached` / `ready` always
/// pass so the new id can be installed; unscoped errors (rejected `new_chat`)
/// pass so the user still sees the rejection.
fn should_drop_event(ctx: &WsContext, event: &ServerEvent) -> bool {
    match event {
        ServerEvent::Attached { .. } | ServerEvent::Ready { .. } | ServerEvent::Unknown(_) => false,
        _ => match event.chat_id() {
            None => false,
            Some(cid) => ctx.chat_id.get_untracked().as_deref() != Some(cid),
        },
    }
}

/// Open a new gateway WebSocket connection for `ctx.token`, wiring its
/// receive loop into [`dispatch_server_event`] and [`handle_connection_closed`].
/// No-ops if there is no token (logged out).
fn open_connection(ctx: WsContext) {
    let Some(token) = ctx.token.get_untracked() else {
        return;
    };

    let attempt = ctx.reconnect_attempt.get_untracked();
    ctx.connection_status.set(if attempt == 0 {
        ConnectionStatus::Connecting
    } else {
        ConnectionStatus::Reconnecting
    });

    let generation = ctx.generation.get_untracked() + 1;
    ctx.generation.set(generation);

    let url = build_gateway_ws_url(
        &ctx.client_id.get_untracked(),
        &token,
        ctx.ws_base_override.get_untracked().as_deref(),
    );

    match api::connect(&url) {
        Ok((sender, receiver)) => {
            ctx.ws_sender.set(Rc::new(RefCell::new(Some(sender))));
            ctx.reconnect_attempt.set(0);
            ctx.reconnect_exhausted.set(false);

            let event_ctx = ctx;
            let close_ctx = ctx;
            api::spawn_receive_loop(
                receiver,
                move |event| dispatch_server_event(&event_ctx, event),
                move || handle_connection_closed(close_ctx, generation),
            );
        }
        Err(err) => {
            ctx.chat_error
                .set(Some(format!("Failed to connect to the gateway: {err}")));
            schedule_reconnect(ctx);
        }
    }
}

/// Fires once a receive loop's stream ends. See [`WsContext::generation`]'s
/// doc comment for why `generation_at_open` guards against a superseded
/// (intentionally replaced) connection's close triggering a redundant
/// reconnect.
fn handle_connection_closed(ctx: WsContext, generation_at_open: u64) {
    if ctx.generation.get_untracked() != generation_at_open {
        return;
    }
    ctx.ws_sender.get_untracked().borrow_mut().take();
    if ctx.token.get_untracked().is_some() {
        schedule_reconnect(ctx);
    } else {
        ctx.connection_status.set(ConnectionStatus::Disconnected);
    }
}

/// Schedule a reconnect attempt after an exponential backoff delay (see
/// [`state::compute_backoff_delay_ms`]), or give up and surface a manual
/// "Retry" affordance once [`state::reconnect_attempts_exhausted`].
fn schedule_reconnect(ctx: WsContext) {
    let attempt = ctx.reconnect_attempt.get_untracked();
    if state::reconnect_attempts_exhausted(attempt) {
        ctx.connection_status.set(ConnectionStatus::Disconnected);
        ctx.reconnect_exhausted.set(true);
        return;
    }
    ctx.connection_status.set(ConnectionStatus::Reconnecting);
    let delay_ms = state::compute_backoff_delay_ms(attempt);
    ctx.reconnect_attempt.set(attempt + 1);
    spawn_local(async move {
        gloo_timers::future::sleep(Duration::from_millis(u64::from(delay_ms))).await;
        open_connection(ctx);
    });
}

/// Reset the reconnect counter and try again immediately — the "Retry"
/// button's handler once automatic reconnection has given up.
fn manual_retry(ctx: WsContext) {
    ctx.reconnect_attempt.set(0);
    ctx.reconnect_exhausted.set(false);
    open_connection(ctx);
}

/// How many event-loop turns to wait for another in-flight send to put the
/// sink back before concluding the socket is gone.
const SEND_CHECKOUT_ATTEMPTS: u32 = 32;

/// Take the send half out of the `RefCell` so [`send_client_envelope`] does
/// not hold a `RefMut` across `.await` (which panics if `ready` queues
/// `list_chats` and `attach` in the same turn, or if `attached` tries to
/// send while the attach frame is still flushing).
async fn checkout_sender(sender_rc: &Rc<RefCell<Option<WsSender>>>) -> Option<WsSender> {
    for _ in 0..SEND_CHECKOUT_ATTEMPTS {
        {
            let mut guard = sender_rc.borrow_mut();
            if let Some(sender) = guard.take() {
                return Some(sender);
            }
        }
        gloo_timers::future::sleep(Duration::from_millis(0)).await;
    }
    None
}

fn checkin_sender(sender_rc: &Rc<RefCell<Option<WsSender>>>, sender: WsSender) {
    let mut guard = sender_rc.borrow_mut();
    if guard.is_none() {
        *guard = Some(sender);
    }
}

/// Serialize `envelope` and send it on the open socket. Surfaces encode /
/// disconnected / send failures on `ctx.chat_error`.
fn send_client_envelope(
    ctx: WsContext,
    envelope: protocol::ClientEnvelope,
    encode_error: &'static str,
) {
    let Ok(payload) = serde_json::to_string(&envelope) else {
        ctx.chat_error.set(Some(encode_error.to_string()));
        return;
    };
    spawn_local(async move {
        let sender_rc = ctx.ws_sender.get_untracked();
        let Some(mut sender) = checkout_sender(&sender_rc).await else {
            ctx.chat_error
                .set(Some("Not connected to the gateway.".to_string()));
            return;
        };
        let result = sender.send_text(payload).await;
        checkin_sender(&sender_rc, sender);
        if let Err(err) = result {
            ctx.chat_error
                .set(Some(format!("Failed to send message: {err}")));
        }
    });
}

/// Ask the gateway to refresh this connection's chat list (sessions
/// sidebar). Fire-and-forget: failures just leave the sidebar's last known
/// state on screen rather than surfacing a user-facing error.
fn request_chat_list(ctx: &WsContext) {
    send_client_envelope(
        *ctx,
        protocol::ClientEnvelope::list_chats(),
        "Failed to encode the chats list request.",
    );
}

/// How long after a turn finishes to re-request the chat list, to pick up a
/// title that was still generating when [`request_chat_list`]'s
/// immediately-on-completion call ran. See [`schedule_delayed_chat_list_refresh`].
const TITLE_REFRESH_DELAY_MS: u32 = 4_000;
/// Second follow-up after [`TITLE_REFRESH_DELAY_MS`], for slower reasoning
/// models whose title call is still in flight at the first refresh.
const TITLE_REFRESH_RETRY_DELAY_MS: u32 = 8_000;

/// Follow-up [`request_chat_list`] a few seconds after a turn completes.
///
/// Title generation (`maybe_schedule_title_generation` server-side) is a
/// fire-and-forget background LLM call kicked off only once the turn
/// finishes — it is not remotely done by the time this connection's own
/// `stream_end`/final `message` arrives, so the immediate refresh at
/// completion almost always still shows the "New chat" placeholder. There is
/// no push notification for "title ready", so this is a best-effort pair of
/// timers rather than a guarantee: if the LLM call is unusually slow, the
/// real title only shows up on the *next* refresh (another turn, a new chat,
/// or a reconnect).
fn schedule_delayed_chat_list_refresh(ctx: WsContext) {
    spawn_local(async move {
        gloo_timers::future::sleep(Duration::from_millis(u64::from(TITLE_REFRESH_DELAY_MS))).await;
        request_chat_list(&ctx);
        gloo_timers::future::sleep(Duration::from_millis(u64::from(
            TITLE_REFRESH_RETRY_DELAY_MS,
        )))
        .await;
        request_chat_list(&ctx);
    });
}

/// Tear down the current socket without letting [`handle_connection_closed`]
/// schedule an automatic reconnect (bumping `generation` makes its
/// eventual, stale `on_close` callback a no-op). Used by logout.
fn close_connection(ctx: &WsContext) {
    ctx.generation.set(ctx.generation.get_untracked() + 1);
    if let Some(mut sender) = ctx.ws_sender.get_untracked().borrow_mut().take() {
        spawn_local(async move {
            let _ = sender.close().await;
        });
    }
}

#[component]
fn ChatLauncher(on_open: impl Fn() + 'static + Copy) -> impl IntoView {
    view! {
        <button
            type="button"
            aria-label="Open chat"
            class="fixed bottom-6 right-6 z-50 flex h-16 w-16 items-center justify-center rounded-full bg-slate-900 text-3xl shadow-lg transition hover:scale-105 hover:bg-slate-800 focus:outline-none focus:ring-2 focus:ring-slate-400 focus:ring-offset-2"
            on:click=move |_| on_open()
        >
            "🦀"
        </button>
    }
}

#[component]
pub fn App() -> impl IntoView {
    let token = RwSignal::new(read_stored_token());
    let login_error = RwSignal::new(None::<String>);
    let login_pending = RwSignal::new(false);
    let chat_open =
        RwSignal::new(BrowserLocalStorage::get::<bool>(CHAT_OPEN_STORAGE_KEY).unwrap_or(false));
    let expanded =
        RwSignal::new(BrowserLocalStorage::get::<bool>(EXPANDED_STORAGE_KEY).unwrap_or(false));

    let client_id = RwSignal::new(read_or_create_client_id());
    let chat_id = RwSignal::new(read_stored_chat_id());
    let connection_status = RwSignal::new(ConnectionStatus::Disconnected);
    let reconnect_attempt = RwSignal::new(0u32);
    let reconnect_exhausted = RwSignal::new(false);
    let ws_base_override = RwSignal::new(read_ws_base_override());
    let ws_sender = RwSignal::new_local(Rc::new(RefCell::new(None::<WsSender>)));
    let generation = RwSignal::new(0u64);

    // `turn_index`/`active_turn_id` below always start empty on load (never
    // persisted), so any entry restored mid-turn has no way left to ever
    // resume or finish — close those out now rather than leave a cursor
    // blinking forever. Also re-persist so a stale mid-turn snapshot isn't
    // re-normalized on every subsequent load.
    let mut initial_entries = read_stored_entries();
    state::finish_orphaned_entries(&mut initial_entries);
    persist_entries(&initial_entries);
    let next_id = RwSignal::new(next_entry_id(&initial_entries));
    let entries = RwSignal::new(initial_entries);
    let turn_index = RwSignal::new(HashMap::<String, u64>::new());
    let active_turn_id = RwSignal::new(None::<String>);
    let chat_error = RwSignal::new(None::<String>);
    // Not fetched from anywhere: the gateway (unlike `rust-bot api`) has no
    // documented example-prompts endpoint this crate depends on, so the
    // empty-state suggestion list simply stays empty for now.
    let example_prompts = RwSignal::new(Vec::<String>::new());
    let composer_draft = RwSignal::new(String::new());
    let token_streaming = RwSignal::new(false);
    let sessions = RwSignal::new(Vec::<SessionListItem>::new());
    let sidebar_open = RwSignal::new(false);
    let pending_attach = RwSignal::new(None::<String>);
    let split_stream_on_next_delta = RwSignal::new(false);

    let ws_context = WsContext {
        token,
        client_id,
        chat_id,
        connection_status,
        entries,
        next_id,
        turn_index,
        active_turn_id,
        chat_error,
        reconnect_attempt,
        reconnect_exhausted,
        ws_base_override,
        ws_sender,
        generation,
        token_streaming,
        sessions,
        pending_attach,
        split_stream_on_next_delta,
    };

    // Session restored from a previous page load: reopen the WebSocket so a
    // refresh doesn't leave the user "logged in" but disconnected.
    if token.get_untracked().is_some() {
        open_connection(ws_context);
    }

    let push_entry = move |role: Role,
                           content: String,
                           attachments: Vec<ImageAttachment>,
                           streaming: bool|
          -> u64 {
        let id = next_id.get_untracked();
        next_id.set(id + 1);
        entries.update(|list| {
            list.push(ChatEntry {
                id,
                role,
                content,
                attachments,
                streaming,
                tool_events: None,
                reasoning: None,
            });
            *list = trim_to_max_turns(std::mem::take(list), MAX_STORED_TURNS);
        });
        persist_entries(&entries.get_untracked());
        id
    };

    let do_login = move |email: String, password: String| {
        login_error.set(None);
        login_pending.set(true);
        spawn_local(async move {
            match login(&email, &password).await {
                Ok(jwt) => {
                    let _ = SessionStorage::set(TOKEN_STORAGE_KEY, &jwt);
                    clear_stored_entries();
                    entries.set(Vec::new());
                    next_id.set(0);
                    turn_index.set(HashMap::new());
                    active_turn_id.set(None);
                    split_stream_on_next_delta.set(false);
                    reconnect_attempt.set(0);
                    reconnect_exhausted.set(false);
                    token.set(Some(jwt));
                    open_connection(ws_context);
                }
                Err(err) => login_error.set(Some(err.to_string())),
            }
            login_pending.set(false);
        });
    };

    let do_logout = move || {
        close_connection(&ws_context);
        SessionStorage::delete(TOKEN_STORAGE_KEY);
        clear_stored_entries();
        clear_stored_chat_id();
        token.set(None);
        chat_id.set(None);
        pending_attach.set(None);
        entries.set(Vec::new());
        next_id.set(0);
        turn_index.set(HashMap::new());
        active_turn_id.set(None);
        split_stream_on_next_delta.set(false);
        composer_draft.set(String::new());
        chat_error.set(None);
        reconnect_attempt.set(0);
        reconnect_exhausted.set(false);
        connection_status.set(ConnectionStatus::Disconnected);
        token_streaming.set(false);
        sessions.set(Vec::new());
        sidebar_open.set(false);
    };

    let do_send = move |outgoing: OutgoingMessage| {
        let Some(current_chat_id) = chat_id.get_untracked() else {
            chat_error.set(Some(
                "Not connected to the gateway yet — please wait a moment and try again."
                    .to_string(),
            ));
            return;
        };
        let OutgoingMessage { text, attachments } = outgoing;
        let turn_id = Uuid::new_v4().to_string();

        push_entry(Role::User, text.clone(), attachments.clone(), false);
        let placeholder_id = push_entry(Role::Assistant, String::new(), Vec::new(), true);
        turn_index.update(|map| {
            map.insert(turn_id.clone(), placeholder_id);
        });
        active_turn_id.set(Some(turn_id.clone()));
        chat_error.set(None);
        split_stream_on_next_delta.set(false);

        let media = build_media_payload(&attachments);
        send_client_envelope(
            ws_context,
            protocol::ClientEnvelope::message(current_chat_id, Some(turn_id), text, media),
            "Failed to encode the outgoing message.",
        );
    };

    let do_new_chat = move || {
        clear_stored_entries();
        clear_stored_chat_id();
        entries.set(Vec::new());
        next_id.set(0);
        turn_index.set(HashMap::new());
        active_turn_id.set(None);
        chat_id.set(None);
        chat_error.set(None);
        pending_attach.set(None);
        split_stream_on_next_delta.set(false);

        let connected = ws_context
            .ws_sender
            .get_untracked()
            .borrow()
            .as_ref()
            .is_some();
        if connected {
            send_client_envelope(
                ws_context,
                protocol::ClientEnvelope::new_chat(),
                "Failed to encode the new-chat request.",
            );
        } else {
            reconnect_attempt.set(0);
            reconnect_exhausted.set(false);
            open_connection(ws_context);
        }
    };

    let do_retry = move || manual_retry(ws_context);

    let open_chat = move || {
        chat_open.set(true);
        let _ = BrowserLocalStorage::set(CHAT_OPEN_STORAGE_KEY, &chat_open.get());
    };
    let close_chat = move || {
        chat_open.set(false);
        let _ = BrowserLocalStorage::set(CHAT_OPEN_STORAGE_KEY, &chat_open.get());
    };
    let toggle_expand = move || {
        expanded.update(|value| *value = !*value);
        let _ = BrowserLocalStorage::set(EXPANDED_STORAGE_KEY, &expanded.get());
    };
    let toggle_sidebar = move || sidebar_open.update(|open| *open = !*open);
    let close_sidebar = move || sidebar_open.set(false);
    let on_select_session = move |selected_id: String| {
        if chat_id.get_untracked().as_deref() == Some(selected_id.as_str()) {
            return;
        }
        // Same local reset as "New chat" so leftover frames from the old
        // subscription are dropped (`should_drop_event`) until `attached`
        // lands with this chat's history.
        clear_stored_entries();
        clear_stored_chat_id();
        entries.set(Vec::new());
        next_id.set(0);
        turn_index.set(HashMap::new());
        active_turn_id.set(None);
        chat_id.set(None);
        chat_error.set(None);
        split_stream_on_next_delta.set(false);

        let connected = ws_context
            .ws_sender
            .get_untracked()
            .borrow()
            .as_ref()
            .is_some();
        if connected {
            send_client_envelope(
                ws_context,
                protocol::ClientEnvelope::attach(selected_id),
                "Failed to encode the attach request.",
            );
        } else {
            // Nothing to send on yet. Reopen and let `ready` attach, rather
            // than stranding the user on a cleared transcript with no chat
            // — the same fallback `do_new_chat` uses.
            pending_attach.set(Some(selected_id));
            reconnect_attempt.set(0);
            reconnect_exhausted.set(false);
            open_connection(ws_context);
        }
    };

    let pending = Signal::derive(move || entries.get().iter().any(|entry| entry.streaming));

    view! {
        <Show
            when=move || chat_open.get()
            fallback=move || view! { <ChatLauncher on_open=open_chat /> }
        >
            <Show
                when=move || token.get().is_some()
                fallback=move || {
                    view! {
                        <LoginForm
                            error=Signal::derive(move || login_error.get())
                            pending=Signal::derive(move || login_pending.get())
                            on_submit=do_login
                            on_minimize=close_chat
                        />
                    }
                }
            >
                <ChatShell
                    entries=Signal::derive(move || entries.get())
                    pending=pending
                    error=Signal::derive(move || chat_error.get())
                    example_prompts=Signal::derive(move || example_prompts.get())
                    connection_status=Signal::derive(move || connection_status.get())
                    reconnect_exhausted=Signal::derive(move || reconnect_exhausted.get())
                    token_streaming=Signal::derive(move || token_streaming.get())
                    sessions=Signal::derive(move || sessions.get())
                    active_session_id=Signal::derive(move || chat_id.get())
                    sidebar_open=Signal::derive(move || sidebar_open.get())
                    draft=composer_draft
                    on_send=do_send
                    on_new_chat=do_new_chat
                    on_logout=do_logout
                    on_minimize=close_chat
                    on_retry=do_retry
                    expanded=Signal::derive(move || expanded.get())
                    on_toggle_expand=toggle_expand
                    on_toggle_sidebar=toggle_sidebar
                    on_close_sidebar=close_sidebar
                    on_select_session=on_select_session
                />
            </Show>
        </Show>
    }
}
