//! Pure, signal-free state-transition functions applied to a
//! `Vec<chat_ui::models::ChatEntry>` as gateway [`crate::protocol::ServerEvent`]s
//! arrive, plus the pure reconnect/URL-building helpers used to drive the
//! WebSocket lifecycle.
//!
//! Everything here operates on plain data (`Vec<ChatEntry>`,
//! `HashMap<String, u64>`) rather than Leptos `RwSignal`s, so the actual
//! reducer logic — delta accumulation, stream finalization, tool/reasoning
//! attachment, backoff timing, URL construction — is unit-testable with plain
//! `#[test]` on the host target. The Leptos UI layer (built on top of this
//! crate in a follow-up step) is expected to own the real
//! `RwSignal<Vec<ChatEntry>>`, call these functions from inside its
//! event-handling closures, and write the mutated `Vec` back into the signal.

use std::collections::HashMap;

use chat_ui::models::{
    ChatEntry, ImageAttachment, McpPresetSummary, Role, SessionListItem, ToolEvent,
};

use crate::protocol::{QuestionChoice, ToolApprovalCall};

/// Prefix identifying a gateway-served media URL (`serve_media` in
/// `src/channels/websocket/webui/media.rs`) rather than a `data:` or
/// arbitrary `http(s)://` reference — see [`authorize_media_attachments`].
const GATEWAY_MEDIA_URL_PREFIX: &str = "/v1/media/";

/// What fills the main area next to the sidebar: the chat, or one of the list pages.
/// One value instead of a flag per page, so two pages can never be open at once.
/// Never persisted: every page load starts on [`MainView::Chat`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MainView {
    /// The conversation (the default).
    #[default]
    Chat,
    /// The MCP server list.
    Mcps,
    /// The child agents and spawn tasks list.
    Subagents,
}

/// What the Settings dialog's configuration file editor is showing.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ConfigFileView {
    /// No editor: the dialog shows its intro line.
    #[default]
    Closed,
    /// The file was requested and has not arrived yet.
    Loading,
    /// The gateway's `config.json`: where it lives and its raw text.
    Loaded { path: String, content: String },
}

impl ConfigFileView {
    /// The view after the gateway delivered the file: only a pending request turns
    /// into the editor, so a reply that lands after the editor was closed is dropped.
    pub fn after_file_arrived(&self, path: String, content: String) -> Self {
        match self {
            Self::Loading => Self::Loaded { path, content },
            other => other.clone(),
        }
    }

    /// The view after the gateway refused or failed a request: a pending load is
    /// abandoned, an open editor stays so the user keeps their edits.
    pub fn after_failure(&self) -> Self {
        match self {
            Self::Loading => Self::Closed,
            other => other.clone(),
        }
    }
}

impl MainView {
    /// Whether the chat itself is showing.
    pub fn is_chat(self) -> bool {
        self == MainView::Chat
    }
}

/// Swap in `updated` for the preset of the same name in `presets`. Does nothing when
/// the list does not hold it (it was closed or refreshed in the meantime).
pub fn replace_mcp_preset(presets: &mut [McpPresetSummary], updated: McpPresetSummary) {
    if let Some(slot) = presets.iter_mut().find(|p| p.name == updated.name) {
        *slot = updated;
    }
}

/// Remember that `name` was changed and needs a restart to take effect. Each name is
/// kept once, in the order first changed.
pub fn note_mcp_restart_needed(changed: &mut Vec<String>, name: &str) {
    if !changed.iter().any(|existing| existing == name) {
        changed.push(name.to_string());
    }
}

/// After deleting `deleted_id` from the sidebar list (most-recent first),
/// the session that should become active: the row immediately after the
/// deleted one, or the previous row if it was last. `None` when the list
/// is empty or `deleted_id` is the only remaining session.
pub fn next_session_id_after(sessions: &[SessionListItem], deleted_id: &str) -> Option<String> {
    let index = sessions
        .iter()
        .position(|session| session.id == deleted_id)?;
    sessions
        .get(index + 1)
        .or_else(|| index.checked_sub(1).and_then(|prev| sessions.get(prev)))
        .map(|session| session.id.clone())
}

/// Mark `chat_id` as pinned or unpinned in the sidebar list. Returns whether
/// a row with that id exists; unknown ids leave the list untouched.
pub fn set_session_pinned(sessions: &mut [SessionListItem], chat_id: &str, pinned: bool) -> bool {
    match sessions.iter_mut().find(|session| session.id == chat_id) {
        Some(session) => {
            session.pinned = pinned;
            true
        }
        None => false,
    }
}

/// Whether the in-transcript Fork control should render next to this entry.
///
/// Shown on every completed assistant reply, not just the last one; hidden
/// while a turn is in flight (`pending`) or the bubble is still streaming.
pub fn show_fork_for_entry(entry: &ChatEntry, pending: bool) -> bool {
    !pending && entry.role == Role::Assistant && !entry.streaming
}

/// 0-based user-message index to send as `before_user_index` when forking
/// after `entry_id`'s assistant reply.
///
/// Counts `Role::User` rows up to and including that assistant bubble, so
/// the gateway keeps this reply and drops later user turns. `None` when
/// `entry_id` is missing or not an assistant row.
pub fn before_user_index_after_entry(entries: &[ChatEntry], entry_id: u64) -> Option<u64> {
    let mut user_count = 0u64;
    for entry in entries {
        if entry.role == Role::User {
            user_count += 1;
        }
        if entry.id == entry_id {
            return (entry.role == Role::Assistant).then_some(user_count);
        }
    }
    None
}

/// Look up the entry tracking `turn_id` and hand back a mutable reference to
/// it, if both the turn is known and its entry still exists.
///
/// Centralizes the two-step "turn_id -> entry id -> entry" lookup so each
/// `apply_*` function below reads as its actual business logic instead of
/// repeating this boilerplate. Returns `None` silently (rather than
/// panicking) when the turn is unknown or its entry has since been evicted —
/// e.g. by the app's own history-trimming — since a late-arriving frame for a
/// turn we've stopped tracking is expected, not a bug.
fn find_entry_for_turn<'a>(
    entries: &'a mut Vec<ChatEntry>,
    turn_index: &HashMap<String, u64>,
    turn_id: &str,
) -> Option<&'a mut ChatEntry> {
    let entry_id = *turn_index.get(turn_id)?;
    entries.iter_mut().find(|entry| entry.id == entry_id)
}

/// Insert the user bubble and a streaming assistant placeholder for
/// `turn_id`, recording the placeholder in `turn_index` — the pure core of
/// `app.rs`'s `start_local_turn`, shared by a turn this tab sends
/// (`do_send`) and one it adopts from another connection's
/// (`ServerEvent::User`).
///
/// Returns `false` without touching `entries`/`next_id` if `turn_id` is
/// already in `turn_index` — the sender's own optimistic insert already ran
/// for a turn this tab sent, and a duplicate `ServerEvent::User` echo (or a
/// slow reconnect replaying one) must not insert the bubble twice. Callers
/// should skip persisting/activating the turn when this returns `false`.
pub fn begin_turn(
    entries: &mut Vec<ChatEntry>,
    turn_index: &mut HashMap<String, u64>,
    next_id: &mut u64,
    turn_id: &str,
    text: String,
    attachments: Vec<ImageAttachment>,
    sender_user_id: Option<String>,
) -> bool {
    if let Some(&placeholder_id) = turn_index.get(turn_id) {
        if let Some(sender_user_id) = sender_user_id {
            if let Some(entry) = entries
                .iter_mut()
                .find(|entry| entry.id + 1 == placeholder_id && entry.role == Role::User)
            {
                if entry.user_id.is_none() {
                    entry.user_id = Some(sender_user_id);
                }
            }
        }
        return false;
    }
    let entry_id = *next_id;
    entries.push(ChatEntry {
        id: entry_id,
        role: Role::User,
        content: text,
        attachments,
        streaming: false,
        tool_events: None,
        reasoning: None,
        user_id: sender_user_id,
        timestamp: None,
    });
    let placeholder_id = entry_id + 1;
    entries.push(ChatEntry {
        id: placeholder_id,
        role: Role::Assistant,
        content: String::new(),
        attachments: Vec::new(),
        streaming: true,
        tool_events: None,
        reasoning: None,
        user_id: None,
        timestamp: None,
    });
    *next_id = placeholder_id + 1;
    turn_index.insert(turn_id.to_string(), placeholder_id);
    true
}

/// Append a `delta` event's text chunk onto the entry tracking `turn_id`.
pub fn apply_delta(
    entries: &mut Vec<ChatEntry>,
    turn_index: &HashMap<String, u64>,
    turn_id: &str,
    delta_text: &str,
) {
    if let Some(entry) = find_entry_for_turn(entries, turn_index, turn_id) {
        entry.content.push_str(delta_text);
    }
}

/// Whether a `stream_end` is only a pause in the same logical answer.
///
/// The gateway sets `resuming` when tool calls follow and a later stream
/// segment will continue this turn (`on_stream_end(true)`). `merge_next`
/// means more deltas will arrive on the same `stream_id`. Either flag must
/// keep the client turn alive; otherwise later `delta` frames are dropped.
pub fn stream_end_continues_turn(resuming: Option<bool>, merge_next: Option<bool>) -> bool {
    resuming.unwrap_or(false) || merge_next.unwrap_or(false)
}

/// Re-mark the entry as streaming after a continuing [`apply_stream_end`].
///
/// `apply_stream_end` always clears `streaming`; call this when
/// `merge_next` is set so the cursor stays on for more deltas on the *same*
/// stream segment. A `resuming` stream_end (next LLM round after tools)
/// should call [`begin_next_stream_segment`] instead — otherwise later
/// deltas concatenate onto the previous thought with no separator, which is
/// why live streaming disagreed with attach's one-bubble-per-round history.
pub fn reopen_streaming(
    entries: &mut Vec<ChatEntry>,
    turn_index: &HashMap<String, u64>,
    turn_id: &str,
) {
    if let Some(entry) = find_entry_for_turn(entries, turn_index, turn_id) {
        entry.streaming = true;
    }
}

/// Open a new streaming assistant bubble for the next LLM round of `turn_id`
/// and retarget `turn_index` so later deltas land there.
///
/// Matches how the session file stores one assistant message per round —
/// the view `websocket_chat_history` sends on `attached`.
///
/// Closes any still-running tool chips on the previous segment first: a
/// `resuming` split only happens after that round's tools have finished, and
/// the following `stream_end` will target this new bubble, so leaving those
/// chips alone would strand the last one in the amber "running" state.
pub fn begin_next_stream_segment(
    entries: &mut Vec<ChatEntry>,
    turn_index: &mut HashMap<String, u64>,
    turn_id: &str,
    new_id: u64,
) {
    if let Some(entry) = find_entry_for_turn(entries, turn_index, turn_id) {
        finish_any_running_tool_events(entry);
    }
    entries.push(ChatEntry {
        id: new_id,
        role: Role::Assistant,
        content: String::new(),
        attachments: Vec::new(),
        streaming: true,
        tool_events: None,
        reasoning: None,
        user_id: None,
        timestamp: None,
    });
    turn_index.insert(turn_id.to_string(), new_id);
}

/// Apply a `stream_end` event: mark the entry no longer streaming, and, when
/// the gateway supplied the authoritative full `text`, overwrite the
/// accumulated delta content with it. The backend sends `text` as the
/// source-of-truth final content, which may differ slightly from a naive
/// concatenation of every delta it streamed, so an override always wins over
/// the accumulated content; when no override is supplied, the accumulated
/// content is left as-is.
///
/// Running tool-activity chips are closed on **every** entry, not only the
/// current segment. After a `resuming` split, `turn_index` already points at
/// the new bubble while the last tool hint still lives on the previous one —
/// see [`finish_any_running_tool_events`].
pub fn apply_stream_end(
    entries: &mut Vec<ChatEntry>,
    turn_index: &HashMap<String, u64>,
    turn_id: &str,
    final_text: Option<&str>,
) {
    if let Some(entry) = find_entry_for_turn(entries, turn_index, turn_id) {
        entry.streaming = false;
        if let Some(text) = final_text {
            entry.content = text.to_string();
        }
    }
    for entry in entries.iter_mut() {
        finish_any_running_tool_events(entry);
    }
}

/// Appended to an aborted turn's bubble by [`apply_turn_aborted`]. Markdown
/// italics, since bubble content is rendered through `chat_ui::markdown`.
pub const ABORTED_TURN_MARKER: &str = "_Stopped._";

/// Apply a `turn_aborted` event: close the turn like a final [`apply_stream_end`]
/// would, then mark the bubble as user-cancelled.
///
/// The marker is what distinguishes an abort from a completed answer. Without
/// it a cancelled turn is indistinguishable from a model that simply stopped
/// mid-sentence — and when the abort lands before the first token, from an
/// empty bubble with nothing in it at all.
pub fn apply_turn_aborted(
    entries: &mut Vec<ChatEntry>,
    turn_index: &HashMap<String, u64>,
    turn_id: &str,
) {
    apply_stream_end(entries, turn_index, turn_id, None);
    let Some(entry) = find_entry_for_turn(entries, turn_index, turn_id) else {
        return;
    };
    entry.content = if entry.content.trim().is_empty() {
        ABORTED_TURN_MARKER.to_string()
    } else {
        format!("{}\n\n{ABORTED_TURN_MARKER}", entry.content.trim_end())
    };
}

/// Coarse lifecycle bucket a [`ToolEvent`]'s free-form `status` string maps
/// to. Shared between this module's own [`finish_any_running_tool_events`]
/// normalization and `components::tool_activity`'s chip styling, rather than
/// duplicating the same heuristic in both places.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatusBucket {
    Running,
    Done,
    Failed,
}

/// Classify a tool event's free-form `status` string into a lifecycle/styling
/// bucket.
///
/// The backend's `ToolEvent::status` (`src/bus/outbound_events.rs`) is a
/// free-form string, not an enum, so this applies a permissive,
/// case-insensitive substring heuristic rather than an exact match:
/// - anything containing `"fail"` or `"error"` (e.g. `"failed"`, `"error"`)
///   did not complete successfully — checked first so a hypothetical status
///   like `"error_running"` isn't misclassified as still in flight;
/// - anything containing `"run"` or `"progress"` (e.g. `"running"`,
///   `"in_progress"`) is still in flight;
/// - everything else (`"done"`, `"success"`, `"complete"`, ...) is treated
///   as finished successfully — the fallback bucket, so an unknown future
///   status string degrades to "done" rather than being ignored.
pub fn classify_tool_status(status: &str) -> ToolStatusBucket {
    let lower = status.to_lowercase();
    if lower.contains("fail") || lower.contains("error") {
        ToolStatusBucket::Failed
    } else if lower.contains("run") || lower.contains("progress") {
        ToolStatusBucket::Running
    } else {
        ToolStatusBucket::Done
    }
}

/// Flip any tool-activity chip still marked as running to a finished state
/// once the turn itself has ended.
///
/// The backend hook that emits tool-hint progress (`before_execute_tools` in
/// `agent_loop.rs`) fires *before* a tool executes and has no corresponding
/// "after execute" hook to report a real success/failure outcome — see
/// [`synthesize_tool_hint_event`]'s doc comment. Left alone, a chip could
/// show "running" forever after the turn's final answer has already
/// arrived, which reads as broken rather than merely imprecise. This is a
/// deliberately honest compromise: it does not claim the tool succeeded
/// (that data doesn't exist), only that the turn — and whatever the tool was
/// doing as part of it — has finished.
fn finish_any_running_tool_events(entry: &mut ChatEntry) {
    let Some(events) = entry.tool_events.as_mut() else {
        return;
    };
    for event in events.iter_mut() {
        if classify_tool_status(&event.status) == ToolStatusBucket::Running {
            event.status = "done".to_string();
        }
    }
}

/// Normalize every entry restored from `SessionStorage` at startup, closing
/// out anything left mid-turn.
///
/// `turn_index`/`active_turn_id` are never persisted (only `entries` is), so
/// a page refresh that lands mid-turn restores an entry with `streaming:
/// true` and no `turn_id` mapping left to ever resume or finish it — a
/// cursor that would blink forever with nothing driving it further. This
/// applies the same [`finish_any_running_tool_events`] treatment used at a
/// normal turn's `stream_end` to every restored entry unconditionally, plus
/// clearing `streaming` itself (which `finish_any_running_tool_events` alone
/// does not touch, since at a real `stream_end` the caller already clears it
/// directly).
pub fn finish_orphaned_entries(entries: &mut [ChatEntry]) {
    for entry in entries.iter_mut() {
        entry.streaming = false;
        finish_any_running_tool_events(entry);
    }
}

/// Wrap a free-text tool-hint as a single synthetic [`ToolEvent`] so
/// `ToolActivity` has something concrete to render.
///
/// The backend's `before_execute_tools` hook (`agent_loop.rs`) only ever
/// publishes a human-readable hint string (e.g. `web_search("...")`, built by
/// `format_tool_hints`) with `tool_events` left `None` — no code path in the
/// backend currently populates the structured shape this crate was
/// originally built to render (confirmed: `tool_events: Some(...)` is
/// constructed nowhere outside `runtime.rs`'s own unit tests). Rather than
/// silently dropping every tool-hint message, this treats the hint text
/// itself as the chip's `name` and always reports `"running"`: the hook
/// fires before the tool executes, so that is the only status honestly known
/// at this point (see [`finish_any_running_tool_events`] for how a stale
/// "running" chip gets closed out once the turn ends).
pub fn synthesize_tool_hint_event(text: &str) -> ToolEvent {
    ToolEvent {
        name: format!("⚙ {text}"),
        status: "running".to_string(),
        detail: None,
    }
}

/// Wrap a free-text `progress` (non-tool-hint) narration line as a single
/// synthetic [`ToolEvent`], for the same reason [`synthesize_tool_hint_event`]
/// exists: `agent_loop.rs`'s `before_execute_tools` hook also fires
/// `on_progress(thought, ProgressKind::Plain)` — the model's in-between
/// "thinking out loud" text, shown as `↳ ...` lines in the CLI — whenever no
/// `on_stream` callback is wired up, and that too arrives with `tool_events`
/// left `None`. When streaming is enabled, that narration arrives as `delta`
/// frames instead and this helper is unused.
///
/// Deliberately given the status `"note"` rather than `"running"`: unlike a
/// tool call, a narration line isn't a stateful in-flight operation with a
/// future outcome to report — there is nothing to transition from "running"
/// to "done" for — so it renders as a static, non-pulsing chip
/// (`classify_tool_status("note")` falls into the `Done` bucket, since
/// `"note"` contains none of `Running`'s `"run"`/`"progress"` substrings).
/// The `↳` prefix follows the CLI's rendering for the same text, and is
/// chosen over a progress glyph so the marker doesn't contradict that.
pub fn synthesize_progress_note_event(text: &str) -> ToolEvent {
    ToolEvent {
        name: format!("↳ {text}"),
        status: "note".to_string(),
        detail: None,
    }
}

/// Attach or merge live tool-activity chips onto the entry tracking `turn_id`.
///
/// Merging is by `name`: an incoming [`ToolEvent`] whose `name` matches an
/// existing chip replaces it in place (so a tool's `status` can move from
/// `"running"` to `"done"`/`"failed"` without duplicating chips); a `name`
/// not already present is appended.
pub fn apply_tool_hint(
    entries: &mut Vec<ChatEntry>,
    turn_index: &HashMap<String, u64>,
    turn_id: &str,
    tool_events: Vec<ToolEvent>,
) {
    let Some(entry) = find_entry_for_turn(entries, turn_index, turn_id) else {
        return;
    };
    let existing = entry.tool_events.get_or_insert_with(Vec::new);
    for incoming in tool_events {
        match existing
            .iter_mut()
            .find(|event| event.name == incoming.name)
        {
            Some(slot) => *slot = incoming,
            None => existing.push(incoming),
        }
    }
}

/// Append a `reasoning_delta` event's text chunk onto the entry tracking
/// `turn_id`, initializing an empty reasoning buffer on first use.
pub fn apply_reasoning_delta(
    entries: &mut Vec<ChatEntry>,
    turn_index: &HashMap<String, u64>,
    turn_id: &str,
    delta_text: &str,
) {
    if let Some(entry) = find_entry_for_turn(entries, turn_index, turn_id) {
        entry
            .reasoning
            .get_or_insert_with(String::new)
            .push_str(delta_text);
    }
}

/// Finalize the entry's reasoning buffer on a `reasoning_end` event.
///
/// The wire `reasoning_end` shape carries no replacement text (unlike
/// `stream_end`'s optional `text`), so there is nothing to overwrite here.
/// This still ensures `reasoning` is `Some(String::new())` rather than `None`
/// if the gateway ends a reasoning stream that never sent a single delta, so
/// a UI checking "did this turn have a reasoning panel at all" via
/// `is_some()` behaves consistently regardless of whether any deltas arrived.
pub fn apply_reasoning_end(
    entries: &mut Vec<ChatEntry>,
    turn_index: &HashMap<String, u64>,
    turn_id: &str,
) {
    if let Some(entry) = find_entry_for_turn(entries, turn_index, turn_id) {
        entry.reasoning.get_or_insert_with(String::new);
    }
}

/// Exponential reconnect backoff: base 500ms, doubling per attempt, capped at
/// 15s so a long outage doesn't push retries out to minutes-long gaps.
///
/// `attempt` is 0-indexed (the first retry after a disconnect is
/// `attempt = 0`, giving a 500ms initial delay). Uses `checked_shl`/
/// `saturating_mul` so a very large `attempt` (from a long-lived reconnect
/// loop) saturates at the cap instead of overflowing or panicking.
pub fn compute_backoff_delay_ms(attempt: u32) -> u32 {
    const BASE_MS: u32 = 500;
    const CAP_MS: u32 = 15_000;
    let multiplier = 1u32.checked_shl(attempt).unwrap_or(u32::MAX);
    BASE_MS.saturating_mul(multiplier).min(CAP_MS)
}

/// Coarse WebSocket connection lifecycle, driving the connection-status dot
/// and label in `components::chat_shell::ChatShell`.
///
/// `app.rs` owns the real `RwSignal<ConnectionStatus>` and the transitions
/// between these states (connect attempt, `ready` event, unexpected close,
/// reconnect backoff, manual retry); this type and its helpers below are
/// kept signal-free so the label/styling mapping is unit-testable like the
/// rest of this module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionStatus {
    /// Initial connection attempt (`reconnect_attempt == 0`) in flight.
    Connecting,
    /// A `ready` event has been received for the current socket.
    Connected,
    /// A prior connection dropped unexpectedly and a backoff-scheduled retry
    /// (`reconnect_attempt > 0`) is in flight.
    Reconnecting,
    /// No socket is open and no retry is scheduled — either the reconnect
    /// cap was hit (see [`reconnect_attempts_exhausted`], surfacing a manual
    /// "Retry" affordance) or the user is logged out.
    Disconnected,
}

impl ConnectionStatus {
    /// Human-readable label shown next to the status dot.
    pub fn label(self) -> &'static str {
        match self {
            ConnectionStatus::Connecting => "Connecting",
            ConnectionStatus::Connected => "Connected",
            ConnectionStatus::Reconnecting => "Reconnecting",
            ConnectionStatus::Disconnected => "Disconnected",
        }
    }

    /// Tailwind component class selecting the status dot's color/animation
    /// (see the `.connection-dot--*` rules in `style/input.css`).
    /// `Connecting` and `Reconnecting` intentionally share the same
    /// (amber, pulsing) treatment — both mean "a connection attempt is in
    /// flight" from the user's point of view.
    pub fn dot_modifier_class(self) -> &'static str {
        match self {
            ConnectionStatus::Connected => "connection-dot--connected",
            ConnectionStatus::Connecting | ConnectionStatus::Reconnecting => {
                "connection-dot--pending"
            }
            ConnectionStatus::Disconnected => "connection-dot--disconnected",
        }
    }
}

/// Which top-level screen the open chat widget shows on startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupView {
    /// No session yet and the gateway's login policy (`GET /v1/auth/config`)
    /// is still unknown — show a loading indicator instead of a blank page
    /// or a login form that might be replaced a moment later.
    Loading,
    /// Login is required and the user has no session.
    Login,
    /// The user has a session, or the gateway allows guest use.
    Chat,
}

/// Decides the [`StartupView`] from whether a session token exists and the
/// gateway's `requireLogin` policy (`None` while the fetch is in flight).
/// A failed fetch is recorded by `app.rs` as `Some(true)` (fail closed), so
/// `Loading` never lasts past the request.
pub fn startup_view(has_token: bool, require_login: Option<bool>) -> StartupView {
    match (has_token, require_login) {
        (true, _) | (false, Some(false)) => StartupView::Chat,
        (false, Some(true)) => StartupView::Login,
        (false, None) => StartupView::Loading,
    }
}

/// Maximum automatic reconnect attempts before giving up and surfacing a
/// manual "Retry" affordance, per the 0-indexed `attempt` convention shared
/// with [`compute_backoff_delay_ms`].
pub const MAX_RECONNECT_ATTEMPTS: u32 = 5;

/// True once `attempt` has reached [`MAX_RECONNECT_ATTEMPTS`] and automatic
/// reconnection should stop in favor of a manual retry.
pub fn reconnect_attempts_exhausted(attempt: u32) -> bool {
    attempt >= MAX_RECONNECT_ATTEMPTS
}

/// The running background-agent count after a `goal_status` frame: the count
/// it reports, or `current` unchanged when it reports none (a plain turn
/// start/end says nothing about background agents).
pub fn background_agents_after(current: usize, reported: Option<usize>) -> usize {
    reported.unwrap_or(current)
}

/// Whether the agent is working on a sub-agent's result after a `goal_status`
/// frame: what it reports, or `current` unchanged when it reports nothing.
pub fn working_on_results_after(current: bool, reported: Option<bool>) -> bool {
    reported.unwrap_or(current)
}

/// The status line for `running` background agents and for the main agent
/// `working` on their results, or `None` when there is neither. Worded like the
/// gateway CLI's own banner.
pub fn background_banner(running: usize, working: bool) -> Option<String> {
    let waiting = match running {
        0 => None,
        1 => Some("Waiting for 1 background agent to finish".to_string()),
        count => Some(format!("Waiting for {count} background agents to finish")),
    };
    match (waiting, working) {
        (Some(waiting), true) => Some(format!("{waiting} · working on the results so far")),
        (Some(waiting), false) => Some(waiting),
        (None, true) => Some("Working on the background agents' results".to_string()),
        (None, false) => None,
    }
}

/// The longest hint shown under the banner, in characters.
const BACKGROUND_HINT_MAX_CHARS: usize = 140;

/// The line to show under the banner for a progress `message` of the agent's
/// background work: the first non-blank line of a `tool_hint` or `progress`
/// message, cut to [`BACKGROUND_HINT_MAX_CHARS`]. `None` for other kinds and
/// for blank text.
pub fn background_hint_from(kind: &str, text: &str) -> Option<String> {
    if kind != "tool_hint" && kind != "progress" {
        return None;
    }
    let line = text.lines().map(str::trim).find(|line| !line.is_empty())?;
    if line.chars().count() <= BACKGROUND_HINT_MAX_CHARS {
        return Some(line.to_string());
    }
    let mut cut: String = line.chars().take(BACKGROUND_HINT_MAX_CHARS).collect();
    cut.push('…');
    Some(cut)
}

/// Percent-encode a string for safe inclusion in a URL query value.
///
/// Reimplements just the RFC 3986 "unreserved characters" allowlist locally
/// instead of adding a URL-encoding crate dependency: this module must stay
/// testable with plain `#[test]` on the host target, so a wasm-only helper
/// like `js_sys::encode_uri_component` is off the table (it has no body
/// outside `wasm32-unknown-unknown`), and nothing already in this crate's
/// dependency tree exposes a pure-Rust equivalent.
fn percent_encode_query_value(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char);
            }
            other => encoded.push_str(&format!("%{other:02X}")),
        }
    }
    encoded
}

/// Append `?token=<token>` to `url` if it's gateway-served
/// (`/v1/media/...`), leaving `data:` and other `http(s)://` URLs
/// untouched. Shared by [`authorize_media_attachments`] (existing history
/// rows) and [`media_urls_to_attachments`] (a live `user` event's
/// freshly-resolved URLs) — both need the same query-param auth on the same
/// URL shape.
fn stamp_media_url(url: &mut String, token: &str) {
    if url.starts_with(GATEWAY_MEDIA_URL_PREFIX) {
        *url = format!("{url}?token={}", percent_encode_query_value(token));
    }
}

/// Append `?token=<token>` to every gateway-served (`/v1/media/...`)
/// attachment URL on `entries`, leaving `data:` and other `http(s)://` URLs
/// untouched.
///
/// Called once, right after `attach`/`fork_chat`/reconnect turns a history
/// snapshot into `entries` (see `app.rs`'s `ServerEvent::Attached` handler) —
/// never on a live, just-uploaded `data:` bubble, which was never rewritten
/// to `/v1/media/...` in the first place (`protocol::history_to_entries`
/// only ever produces gateway or passthrough `http(s)://` URLs from
/// `history[].media`, never `data:`). A native `<img src>` can't set an
/// `Authorization` header, so the token travels as a query param instead —
/// same convention [`build_ws_url`] uses for the WebSocket upgrade itself.
/// No-op (URLs are returned unchanged) when `token` is `None`/empty, e.g.
/// JWT disabled server-side.
pub fn authorize_media_attachments(
    mut entries: Vec<ChatEntry>,
    token: Option<&str>,
) -> Vec<ChatEntry> {
    let Some(token) = token.filter(|t| !t.is_empty()) else {
        return entries;
    };
    for entry in &mut entries {
        for attachment in &mut entry.attachments {
            stamp_media_url(&mut attachment.url, token);
        }
    }
    entries
}

/// Turn a `ServerEvent::User` event's already-resolved `/v1/media/...` URLs
/// into [`ImageAttachment`]s, token-stamping gateway URLs the same way
/// [`authorize_media_attachments`] does for a restored history snapshot — a
/// remotely announced turn's attachments need the same auth query param to
/// actually load in an `<img src>`.
pub fn media_urls_to_attachments(media: Vec<String>, token: Option<&str>) -> Vec<ImageAttachment> {
    let token = token.filter(|t| !t.is_empty());
    media
        .into_iter()
        .map(|mut url| {
            if let Some(token) = token {
                stamp_media_url(&mut url, token);
            }
            let label = chat_ui::attachments::filename_from_media_url(&url);
            ImageAttachment { url, label }
        })
        .collect()
}

/// Attach files from a final `message` event onto the assistant bubble
/// tracking `turn_id`. No-op when `attachments` is empty or the turn is
/// unknown. Extends rather than replacing, so a later file-bearing
/// `message` on the same turn keeps anything already shown.
pub fn apply_assistant_attachments(
    entries: &mut Vec<ChatEntry>,
    turn_index: &HashMap<String, u64>,
    turn_id: &str,
    attachments: Vec<ImageAttachment>,
) {
    if attachments.is_empty() {
        return;
    }
    if let Some(entry) = find_entry_for_turn(entries, turn_index, turn_id) {
        entry.attachments.extend(attachments);
    }
}

/// Build the gateway WebSocket URL.
///
/// Translates the page's `http(s)` origin `scheme` to the matching `ws(s)`
/// scheme and combines it with `host`, unless `override_base` supplies an
/// explicit scheme+host prefix instead (for local dev, e.g. `trunk serve`
/// pointing at a gateway on a different port than the one Trunk itself serves
/// from). `client_id` is always percent-encoded into the query string.
/// `token` is omitted entirely (not sent as an empty value) when `None` —
/// the guest-connection case, where the gateway's `WebSocketConfig` allows
/// an upgrade with no JWT at all (see `authorize` server-side). `path` is
/// used as-is (the gateway config already normalizes it to start with `/`;
/// see `WebSocketConfig::path` server-side).
pub fn build_ws_url(
    scheme: &str,
    host: &str,
    path: &str,
    client_id: &str,
    token: Option<&str>,
    override_base: Option<&str>,
) -> String {
    let base = match override_base {
        Some(explicit) => explicit.trim_end_matches('/').to_string(),
        None => {
            let ws_scheme = if scheme.eq_ignore_ascii_case("https") {
                "wss"
            } else {
                "ws"
            };
            format!("{ws_scheme}://{host}")
        }
    };
    let path = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    let mut url = format!(
        "{base}{path}?client_id={}",
        percent_encode_query_value(client_id)
    );
    if let Some(token) = token {
        url.push_str(&format!("&token={}", percent_encode_query_value(token)));
    }
    url
}

/// An open `tool_approval_request` plus the user's per-call choice so far.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingApproval {
    pub request_id: String,
    pub calls: Vec<PendingApprovalCall>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PendingApprovalCall {
    pub call: ToolApprovalCall,
    pub approved: bool,
}

impl PendingApproval {
    /// Every call starts approved, so "Run selected" with no changes runs all.
    pub fn new(request_id: String, calls: Vec<ToolApprovalCall>) -> Self {
        Self {
            request_id,
            calls: calls
                .into_iter()
                .map(|call| PendingApprovalCall {
                    call,
                    approved: true,
                })
                .collect(),
        }
    }

    pub fn toggle_call(&mut self, call_id: &str) {
        if let Some(entry) = self.calls.iter_mut().find(|entry| entry.call.id == call_id) {
            entry.approved = !entry.approved;
        }
    }

    pub fn approved_ids(&self) -> Vec<String> {
        self.calls
            .iter()
            .filter(|entry| entry.approved)
            .map(|entry| entry.call.id.clone())
            .collect()
    }

    pub fn all_ids(&self) -> Vec<String> {
        self.calls
            .iter()
            .map(|entry| entry.call.id.clone())
            .collect()
    }
}

/// An open `question_request` awaiting an answer. Plain data on the parent's
/// signal, same reasoning as [`PendingApproval`] — a re-sent request (after
/// reconnect) mustn't desync from what the card is showing.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingQuestion {
    pub request_id: String,
    pub question: String,
    pub options: Vec<QuestionChoice>,
}

impl PendingQuestion {
    pub fn new(request_id: String, question: String, options: Vec<QuestionChoice>) -> Self {
        Self {
            request_id,
            question,
            options,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reported_background_agent_count_replaces_the_current_one() {
        assert_eq!(background_agents_after(3, Some(1)), 1);
        assert_eq!(background_agents_after(3, Some(0)), 0);
        assert_eq!(background_agents_after(0, Some(2)), 2);
    }

    #[test]
    fn a_frame_without_a_count_leaves_the_current_one() {
        assert_eq!(background_agents_after(2, None), 2);
        assert_eq!(background_agents_after(0, None), 0);
    }

    #[test]
    fn a_reported_working_state_replaces_the_current_one() {
        assert!(working_on_results_after(false, Some(true)));
        assert!(!working_on_results_after(true, Some(false)));
    }

    #[test]
    fn a_frame_without_a_working_state_leaves_the_current_one() {
        assert!(working_on_results_after(true, None));
        assert!(!working_on_results_after(false, None));
    }

    #[test]
    fn background_banner_is_hidden_when_nothing_happens() {
        assert_eq!(background_banner(0, false), None);
    }

    #[test]
    fn background_banner_uses_singular_and_plural() {
        assert_eq!(
            background_banner(1, false).as_deref(),
            Some("Waiting for 1 background agent to finish")
        );
        assert_eq!(
            background_banner(4, false).as_deref(),
            Some("Waiting for 4 background agents to finish")
        );
    }

    #[test]
    fn background_banner_says_when_the_agent_works_on_results() {
        assert_eq!(
            background_banner(0, true).as_deref(),
            Some("Working on the background agents' results")
        );
        assert_eq!(
            background_banner(2, true).as_deref(),
            Some("Waiting for 2 background agents to finish · working on the results so far")
        );
    }

    #[test]
    fn background_hint_takes_the_first_line_of_a_tool_hint_or_progress_note() {
        assert_eq!(
            background_hint_from("tool_hint", "\n  write_file(report.html)\nmore").as_deref(),
            Some("write_file(report.html)")
        );
        assert_eq!(
            background_hint_from("progress", "Verifying the report").as_deref(),
            Some("Verifying the report")
        );
    }

    #[test]
    fn background_hint_ignores_other_kinds_and_blank_text() {
        assert_eq!(background_hint_from("status", "something"), None);
        assert_eq!(background_hint_from("tool_hint", "  \n "), None);
    }

    #[test]
    fn background_hint_is_cut_to_a_readable_length() {
        let long = "x".repeat(BACKGROUND_HINT_MAX_CHARS + 30);
        let hint = background_hint_from("tool_hint", &long).unwrap();
        assert_eq!(hint.chars().count(), BACKGROUND_HINT_MAX_CHARS + 1);
        assert!(hint.ends_with('…'));
    }

    fn mcp_preset(name: &str, enabled: bool) -> McpPresetSummary {
        McpPresetSummary {
            name: name.to_string(),
            display_name: name.to_string(),
            description: String::new(),
            category: String::new(),
            transport: "stdio".to_string(),
            enabled,
            status: String::new(),
            install_supported: true,
            requires: String::new(),
            note: String::new(),
            fields: Vec::new(),
        }
    }

    #[test]
    fn main_view_starts_on_the_chat() {
        assert_eq!(MainView::default(), MainView::Chat);
        assert!(MainView::default().is_chat());
    }

    #[test]
    fn main_view_list_pages_are_not_the_chat() {
        assert!(!MainView::Mcps.is_chat());
        assert!(!MainView::Subagents.is_chat());
    }

    #[test]
    fn replace_mcp_preset_swaps_only_the_matching_entry() {
        let mut presets = vec![mcp_preset("a", false), mcp_preset("b", false)];
        replace_mcp_preset(&mut presets, mcp_preset("b", true));
        assert!(!presets[0].enabled);
        assert!(presets[1].enabled);
    }

    #[test]
    fn replace_mcp_preset_ignores_a_preset_the_list_does_not_hold() {
        let mut presets = vec![mcp_preset("a", false)];
        replace_mcp_preset(&mut presets, mcp_preset("zzz", true));
        assert_eq!(presets.len(), 1);
        assert!(!presets[0].enabled);
    }

    #[test]
    fn note_mcp_restart_needed_keeps_each_name_once_in_order() {
        let mut changed = Vec::new();
        note_mcp_restart_needed(&mut changed, "github");
        note_mcp_restart_needed(&mut changed, "exa");
        note_mcp_restart_needed(&mut changed, "github");
        assert_eq!(changed, vec!["github".to_string(), "exa".to_string()]);
    }

    fn approval_call(id: &str) -> ToolApprovalCall {
        ToolApprovalCall {
            id: id.to_string(),
            name: "exec".to_string(),
            arguments_preview: "{}".to_string(),
        }
    }

    #[test]
    fn pending_approval_starts_all_approved() {
        let pending = PendingApproval::new(
            "req-1".to_string(),
            vec![approval_call("a"), approval_call("b")],
        );
        assert_eq!(pending.approved_ids(), vec!["a", "b"]);
        assert_eq!(pending.all_ids(), vec!["a", "b"]);
    }

    #[test]
    fn pending_approval_toggle_flips_one_call() {
        let mut pending = PendingApproval::new(
            "req-1".to_string(),
            vec![approval_call("a"), approval_call("b")],
        );
        pending.toggle_call("a");
        assert_eq!(pending.approved_ids(), vec!["b"]);
        pending.toggle_call("a");
        pending.toggle_call("unknown");
        assert_eq!(pending.approved_ids(), vec!["a", "b"]);
    }

    fn question_choice(id: &str) -> QuestionChoice {
        QuestionChoice {
            id: id.to_string(),
            label: format!("Option {id}"),
            description: None,
        }
    }

    #[test]
    fn pending_question_holds_the_question_and_options() {
        let pending = PendingQuestion::new(
            "req-1".to_string(),
            "Which one?".to_string(),
            vec![question_choice("0"), question_choice("1")],
        );
        assert_eq!(pending.request_id, "req-1");
        assert_eq!(pending.question, "Which one?");
        assert_eq!(
            pending.options,
            vec![question_choice("0"), question_choice("1")]
        );
    }

    use chat_ui::models::{ImageAttachment, Role};

    fn assistant_entry(id: u64) -> ChatEntry {
        ChatEntry {
            id,
            role: Role::Assistant,
            content: String::new(),
            attachments: Vec::new(),
            streaming: true,
            tool_events: None,
            reasoning: None,
            user_id: None,
            timestamp: None,
        }
    }

    fn user_entry(id: u64) -> ChatEntry {
        ChatEntry {
            id,
            role: Role::User,
            content: String::new(),
            attachments: Vec::new(),
            streaming: false,
            tool_events: None,
            reasoning: None,
            user_id: None,
            timestamp: None,
        }
    }

    fn completed_assistant_entry(id: u64) -> ChatEntry {
        ChatEntry {
            streaming: false,
            ..assistant_entry(id)
        }
    }

    fn index_with(turn_id: &str, entry_id: u64) -> HashMap<String, u64> {
        let mut map = HashMap::new();
        map.insert(turn_id.to_string(), entry_id);
        map
    }

    #[test]
    fn apply_delta_accumulates_across_multiple_calls() {
        let mut entries = vec![assistant_entry(0)];
        let index = index_with("turn-1", 0);

        apply_delta(&mut entries, &index, "turn-1", "Hello");
        apply_delta(&mut entries, &index, "turn-1", ", world");
        apply_delta(&mut entries, &index, "turn-1", "!");

        assert_eq!(entries[0].content, "Hello, world!");
    }

    #[test]
    fn apply_delta_is_a_no_op_for_unknown_turn() {
        let mut entries = vec![assistant_entry(0)];
        let index = index_with("turn-1", 0);

        apply_delta(&mut entries, &index, "some-other-turn", "ignored");

        assert_eq!(entries[0].content, "");
    }

    #[test]
    fn begin_turn_inserts_a_user_bubble_and_a_streaming_placeholder() {
        let mut entries = Vec::new();
        let mut turn_index = HashMap::new();
        let mut next_id = 0u64;

        let started = begin_turn(
            &mut entries,
            &mut turn_index,
            &mut next_id,
            "turn-1",
            "hello there".to_string(),
            Vec::new(),
            None,
        );

        assert!(started);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].role, Role::User);
        assert_eq!(entries[0].content, "hello there");
        assert!(!entries[0].streaming);
        assert_eq!(entries[1].role, Role::Assistant);
        assert_eq!(entries[1].content, "");
        assert!(entries[1].streaming);
        assert_eq!(turn_index.get("turn-1"), Some(&entries[1].id));
        assert_eq!(next_id, entries[1].id + 1);
        assert_eq!(entries[0].user_id, None);
        assert_eq!(entries[1].user_id, None);
    }

    #[test]
    fn begin_turn_stamps_sender_user_id_on_the_user_bubble() {
        let mut entries = Vec::new();
        let mut turn_index = HashMap::new();
        let mut next_id = 0u64;

        begin_turn(
            &mut entries,
            &mut turn_index,
            &mut next_id,
            "turn-1",
            "hello".to_string(),
            Vec::new(),
            Some("a@b.com".to_string()),
        );

        assert_eq!(entries[0].user_id.as_deref(), Some("a@b.com"));
        assert_eq!(entries[1].user_id, None);
    }

    #[test]
    fn begin_turn_echo_fills_user_id_when_the_optimistic_insert_had_none() {
        let mut entries = Vec::new();
        let mut turn_index = HashMap::new();
        let mut next_id = 0u64;
        begin_turn(
            &mut entries,
            &mut turn_index,
            &mut next_id,
            "turn-1",
            "hello".to_string(),
            Vec::new(),
            None,
        );

        let started = begin_turn(
            &mut entries,
            &mut turn_index,
            &mut next_id,
            "turn-1",
            "hello".to_string(),
            Vec::new(),
            Some("a@b.com".to_string()),
        );

        assert!(!started);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].user_id.as_deref(), Some("a@b.com"));
    }

    #[test]
    fn begin_turn_carries_attachments_on_the_user_bubble_only() {
        let mut entries = Vec::new();
        let mut turn_index = HashMap::new();
        let mut next_id = 0u64;
        let attachments = vec![ImageAttachment {
            url: "/v1/media/websocket/abc.png".to_string(),
            label: None,
        }];

        begin_turn(
            &mut entries,
            &mut turn_index,
            &mut next_id,
            "turn-1",
            "look at this".to_string(),
            attachments.clone(),
            None,
        );

        assert_eq!(entries[0].attachments, attachments);
        assert!(entries[1].attachments.is_empty());
    }

    #[test]
    fn begin_turn_is_a_no_op_when_turn_id_is_already_known() {
        let mut entries = vec![assistant_entry(7)];
        let mut turn_index = index_with("turn-1", 7);
        let mut next_id = 8u64;

        let started = begin_turn(
            &mut entries,
            &mut turn_index,
            &mut next_id,
            "turn-1",
            "duplicate echo".to_string(),
            Vec::new(),
            None,
        );

        assert!(
            !started,
            "a turn_id already in turn_index must not be inserted twice"
        );
        assert_eq!(entries.len(), 1, "no new bubbles should have been added");
        assert_eq!(next_id, 8, "next_id must be untouched on a no-op");
    }

    #[test]
    fn apply_stream_end_without_override_keeps_accumulated_content() {
        let mut entries = vec![assistant_entry(0)];
        let index = index_with("turn-1", 0);

        apply_delta(&mut entries, &index, "turn-1", "partial");
        apply_stream_end(&mut entries, &index, "turn-1", None);

        assert_eq!(entries[0].content, "partial");
        assert!(!entries[0].streaming);
    }

    #[test]
    fn apply_stream_end_with_override_replaces_accumulated_content() {
        let mut entries = vec![assistant_entry(0)];
        let index = index_with("turn-1", 0);

        apply_delta(&mut entries, &index, "turn-1", "partial");
        apply_stream_end(
            &mut entries,
            &index,
            "turn-1",
            Some("authoritative full text"),
        );

        assert_eq!(entries[0].content, "authoritative full text");
        assert!(!entries[0].streaming);
    }

    #[test]
    fn apply_stream_end_override_snaps_back_doubled_deltas() {
        let mut entries = vec![assistant_entry(0)];
        let index = index_with("turn-1", 0);

        apply_delta(&mut entries, &index, "turn-1", "I'll");
        apply_delta(&mut entries, &index, "turn-1", "I'll");
        apply_delta(&mut entries, &index, "turn-1", " find that");
        apply_delta(&mut entries, &index, "turn-1", " find that");
        apply_stream_end(&mut entries, &index, "turn-1", Some("I'll find that"));

        assert_eq!(entries[0].content, "I'll find that");
    }

    #[test]
    fn apply_turn_aborted_closes_the_turn_and_marks_partial_content() {
        let mut entries = vec![assistant_entry(0)];
        let index = index_with("turn-1", 0);

        apply_delta(&mut entries, &index, "turn-1", "I was halfway through");
        apply_turn_aborted(&mut entries, &index, "turn-1");

        assert_eq!(
            entries[0].content,
            format!("I was halfway through\n\n{ABORTED_TURN_MARKER}")
        );
        assert!(!entries[0].streaming);
    }

    #[test]
    fn apply_turn_aborted_before_the_first_token_leaves_only_the_marker() {
        let mut entries = vec![assistant_entry(0)];
        let index = index_with("turn-1", 0);

        apply_turn_aborted(&mut entries, &index, "turn-1");

        assert_eq!(
            entries[0].content, ABORTED_TURN_MARKER,
            "an empty bubble must not be left blank"
        );
        assert!(!entries[0].streaming);
    }

    #[test]
    fn apply_turn_aborted_closes_running_tool_chips() {
        let mut entries = vec![assistant_entry(0)];
        let index = index_with("turn-1", 0);
        apply_tool_hint(
            &mut entries,
            &index,
            "turn-1",
            vec![ToolEvent {
                name: "web_search(...)".to_string(),
                status: "running".to_string(),
                detail: None,
            }],
        );

        apply_turn_aborted(&mut entries, &index, "turn-1");

        assert_eq!(entries[0].tool_events.clone().unwrap()[0].status, "done");
    }

    #[test]
    fn apply_turn_aborted_is_a_no_op_for_unknown_turn() {
        let mut entries = vec![assistant_entry(0)];
        let index = index_with("turn-1", 0);

        apply_delta(&mut entries, &index, "turn-1", "keep me");
        apply_turn_aborted(&mut entries, &index, "some-other-turn");

        assert_eq!(entries[0].content, "keep me");
        assert!(entries[0].streaming);
    }

    #[test]
    fn stream_end_continues_turn_on_resuming_or_merge_next() {
        assert!(stream_end_continues_turn(Some(true), Some(false)));
        assert!(stream_end_continues_turn(Some(true), None));
        assert!(stream_end_continues_turn(None, Some(true)));
        assert!(stream_end_continues_turn(Some(false), Some(true)));
        assert!(!stream_end_continues_turn(Some(false), Some(false)));
        assert!(!stream_end_continues_turn(None, None));
    }

    #[test]
    fn resuming_stream_end_opens_a_new_bubble_for_the_next_round() {
        let mut entries = vec![assistant_entry(0)];
        let mut index = index_with("turn-1", 0);

        apply_delta(&mut entries, &index, "turn-1", "I'll pull the paper.");
        apply_stream_end(&mut entries, &index, "turn-1", None);
        begin_next_stream_segment(&mut entries, &mut index, "turn-1", 1);

        apply_delta(&mut entries, &index, "turn-1", "Here is the summary.");
        apply_stream_end(&mut entries, &index, "turn-1", None);

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].content, "I'll pull the paper.");
        assert!(!entries[0].streaming);
        assert_eq!(entries[1].content, "Here is the summary.");
        assert!(!entries[1].streaming);
        assert_eq!(index.get("turn-1"), Some(&1));
    }

    #[test]
    fn merge_next_keeps_deltas_on_the_same_bubble() {
        let mut entries = vec![assistant_entry(0)];
        let index = index_with("turn-1", 0);

        apply_delta(&mut entries, &index, "turn-1", "partial");
        apply_stream_end(&mut entries, &index, "turn-1", None);
        reopen_streaming(&mut entries, &index, "turn-1");
        apply_delta(&mut entries, &index, "turn-1", " rest");

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].content, "partial rest");
        assert!(entries[0].streaming);
    }

    #[test]
    fn apply_tool_hint_attaches_new_events() {
        let mut entries = vec![assistant_entry(0)];
        let index = index_with("turn-1", 0);

        apply_tool_hint(
            &mut entries,
            &index,
            "turn-1",
            vec![ToolEvent {
                name: "search".to_string(),
                status: "running".to_string(),
                detail: None,
            }],
        );

        assert_eq!(
            entries[0].tool_events,
            Some(vec![ToolEvent {
                name: "search".to_string(),
                status: "running".to_string(),
                detail: None,
            }])
        );
    }

    #[test]
    fn apply_tool_hint_merges_by_name_instead_of_duplicating() {
        let mut entries = vec![assistant_entry(0)];
        let index = index_with("turn-1", 0);

        apply_tool_hint(
            &mut entries,
            &index,
            "turn-1",
            vec![ToolEvent {
                name: "search".to_string(),
                status: "running".to_string(),
                detail: None,
            }],
        );
        apply_tool_hint(
            &mut entries,
            &index,
            "turn-1",
            vec![
                ToolEvent {
                    name: "search".to_string(),
                    status: "done".to_string(),
                    detail: Some("3 results".to_string()),
                },
                ToolEvent {
                    name: "fetch".to_string(),
                    status: "running".to_string(),
                    detail: None,
                },
            ],
        );

        let events = entries[0].tool_events.clone().expect("tool_events set");
        assert_eq!(
            events.len(),
            2,
            "search should be updated in place, not duplicated"
        );
        assert_eq!(events[0].name, "search");
        assert_eq!(events[0].status, "done");
        assert_eq!(events[0].detail, Some("3 results".to_string()));
        assert_eq!(events[1].name, "fetch");
        assert_eq!(events[1].status, "running");
    }

    #[test]
    fn apply_stream_end_flips_running_tool_events_to_done() {
        let mut entries = vec![assistant_entry(0)];
        let index = index_with("turn-1", 0);

        apply_tool_hint(
            &mut entries,
            &index,
            "turn-1",
            vec![
                ToolEvent {
                    name: "web_search(...)".to_string(),
                    status: "running".to_string(),
                    detail: None,
                },
                ToolEvent {
                    name: "already_failed".to_string(),
                    status: "failed".to_string(),
                    detail: Some("timed out".to_string()),
                },
            ],
        );
        apply_stream_end(&mut entries, &index, "turn-1", Some("final answer"));

        let events = entries[0].tool_events.clone().expect("tool_events set");
        assert_eq!(
            events[0].status, "done",
            "running chip should be closed out"
        );
        assert_eq!(
            events[1].status, "failed",
            "an already-terminal status must not be overwritten"
        );
    }

    #[test]
    fn begin_next_stream_segment_closes_running_chips_on_the_previous_bubble() {
        let mut entries = vec![assistant_entry(0)];
        let mut index = index_with("turn-1", 0);

        apply_tool_hint(
            &mut entries,
            &index,
            "turn-1",
            vec![ToolEvent {
                name: "read README.md".to_string(),
                status: "running".to_string(),
                detail: None,
            }],
        );
        begin_next_stream_segment(&mut entries, &mut index, "turn-1", 1);

        assert_eq!(
            entries[0].tool_events.clone().unwrap()[0].status,
            "done",
            "last tool hint on the previous segment must not stay running after the split"
        );
        assert_eq!(entries[1].tool_events, None);
        assert_eq!(index.get("turn-1"), Some(&1));
    }

    #[test]
    fn apply_stream_end_closes_running_chips_left_on_a_previous_segment() {
        let mut entries = vec![assistant_entry(0), assistant_entry(1)];
        entries[0].streaming = false;
        entries[0].tool_events = Some(vec![
            ToolEvent {
                name: "ls src × 3".to_string(),
                status: "done".to_string(),
                detail: None,
            },
            ToolEvent {
                name: "grep pattern".to_string(),
                status: "done".to_string(),
                detail: None,
            },
            ToolEvent {
                name: "read README.md".to_string(),
                status: "running".to_string(),
                detail: None,
            },
        ]);
        let index = index_with("turn-1", 1);

        apply_stream_end(&mut entries, &index, "turn-1", Some("Here is the summary."));

        let events = entries[0]
            .tool_events
            .clone()
            .expect("tool_events on first bubble");
        assert_eq!(events[0].status, "done");
        assert_eq!(events[1].status, "done");
        assert_eq!(
            events[2].status, "done",
            "the last chip must turn green even though stream_end targeted the new bubble"
        );
        assert!(!entries[1].streaming);
        assert_eq!(entries[1].content, "Here is the summary.");
    }

    #[test]
    fn apply_stream_end_is_a_no_op_when_there_are_no_tool_events() {
        let mut entries = vec![assistant_entry(0)];
        let index = index_with("turn-1", 0);

        // Must not panic when `tool_events` is `None`.
        apply_stream_end(&mut entries, &index, "turn-1", Some("final answer"));

        assert_eq!(entries[0].tool_events, None);
    }

    #[test]
    fn finish_orphaned_entries_clears_streaming_and_closes_running_chips() {
        let mut entries = vec![assistant_entry(0)];
        apply_tool_hint(
            &mut entries,
            &index_with("turn-1", 0),
            "turn-1",
            vec![ToolEvent {
                name: "web_search(...)".to_string(),
                status: "running".to_string(),
                detail: None,
            }],
        );
        assert!(entries[0].streaming, "fixture should start mid-turn");

        finish_orphaned_entries(&mut entries);

        assert!(!entries[0].streaming);
        assert_eq!(entries[0].tool_events.clone().unwrap()[0].status, "done");
    }

    #[test]
    fn finish_orphaned_entries_is_a_no_op_on_already_finished_entries() {
        let mut entries = vec![assistant_entry(0)];
        entries[0].streaming = false;
        entries[0].content = "already finished".to_string();

        finish_orphaned_entries(&mut entries);

        assert!(!entries[0].streaming);
        assert_eq!(entries[0].content, "already finished");
    }

    #[test]
    fn classify_tool_status_buckets_by_substring_with_fail_checked_first() {
        assert_eq!(classify_tool_status("running"), ToolStatusBucket::Running);
        assert_eq!(
            classify_tool_status("in_progress"),
            ToolStatusBucket::Running
        );
        assert_eq!(classify_tool_status("failed"), ToolStatusBucket::Failed);
        assert_eq!(classify_tool_status("error"), ToolStatusBucket::Failed);
        assert_eq!(
            classify_tool_status("error_running"),
            ToolStatusBucket::Failed,
            "fail/error must win even if a running-like substring is also present"
        );
        assert_eq!(classify_tool_status("done"), ToolStatusBucket::Done);
        assert_eq!(
            classify_tool_status("some_future_status"),
            ToolStatusBucket::Done,
            "unknown statuses should fall back to done rather than be ignored"
        );
    }

    #[test]
    fn synthesize_tool_hint_event_wraps_text_as_a_running_chip() {
        let event = synthesize_tool_hint_event(r#"web_search("London weather")"#);

        assert_eq!(event.name, r#"⚙ web_search("London weather")"#);
        assert_eq!(event.status, "running");
        assert_eq!(event.detail, None);
    }

    #[test]
    fn synthesize_progress_note_event_wraps_text_as_a_static_note() {
        let event = synthesize_progress_note_event("I'll check local docs first.");

        assert_eq!(event.name, "↳ I'll check local docs first.");
        assert_eq!(event.status, "note");
        assert_eq!(event.detail, None);
        assert_eq!(
            classify_tool_status(&event.status),
            ToolStatusBucket::Done,
            "a narration note should render as static, not pulsing"
        );
    }

    #[test]
    fn apply_reasoning_delta_accumulates_and_end_finalizes() {
        let mut entries = vec![assistant_entry(0)];
        let index = index_with("turn-1", 0);

        assert_eq!(entries[0].reasoning, None);

        apply_reasoning_delta(&mut entries, &index, "turn-1", "Let me think");
        apply_reasoning_delta(&mut entries, &index, "turn-1", "...");

        assert_eq!(entries[0].reasoning, Some("Let me think...".to_string()));

        apply_reasoning_end(&mut entries, &index, "turn-1");

        // reasoning_end carries no replacement text; accumulated text survives.
        assert_eq!(entries[0].reasoning, Some("Let me think...".to_string()));
    }

    #[test]
    fn apply_reasoning_end_initializes_empty_buffer_if_no_deltas_arrived() {
        let mut entries = vec![assistant_entry(0)];
        let index = index_with("turn-1", 0);

        assert_eq!(entries[0].reasoning, None);

        apply_reasoning_end(&mut entries, &index, "turn-1");

        assert_eq!(entries[0].reasoning, Some(String::new()));
    }

    #[test]
    fn backoff_grows_monotonically_then_plateaus_at_cap() {
        let delays: Vec<u32> = (0..10).map(compute_backoff_delay_ms).collect();

        for window in delays.windows(2) {
            assert!(
                window[0] <= window[1],
                "backoff should never decrease: {delays:?}"
            );
        }

        assert_eq!(delays[0], 500);
        assert_eq!(delays[1], 1000);
        assert_eq!(delays[2], 2000);

        let cap = *delays.last().unwrap();
        assert_eq!(cap, 15_000);
        // Once the cap is hit it should stay there for larger attempts too.
        assert_eq!(compute_backoff_delay_ms(20), cap);
        assert_eq!(compute_backoff_delay_ms(1_000), cap);
    }

    #[test]
    fn authorize_media_attachments_appends_token_to_gateway_media_urls_only() {
        let entries = vec![
            ChatEntry {
                id: 0,
                role: Role::User,
                content: "restored".to_string(),
                attachments: vec![
                    ImageAttachment {
                        url: "/v1/media/websocket/abc.png".to_string(),
                        label: None,
                    },
                    ImageAttachment {
                        url: "https://example.com/a.png".to_string(),
                        label: None,
                    },
                ],
                streaming: false,
                tool_events: None,
                reasoning: None,
                user_id: None,
                timestamp: None,
            },
            ChatEntry {
                id: 1,
                role: Role::User,
                content: "just uploaded".to_string(),
                attachments: vec![ImageAttachment {
                    url: "data:image/png;base64,AAAA".to_string(),
                    label: None,
                }],
                streaming: false,
                tool_events: None,
                reasoning: None,
                user_id: None,
                timestamp: None,
            },
        ];

        let result = authorize_media_attachments(entries, Some("tok en"));

        assert_eq!(
            result[0].attachments[0].url,
            "/v1/media/websocket/abc.png?token=tok%20en"
        );
        assert_eq!(result[0].attachments[1].url, "https://example.com/a.png");
        assert_eq!(result[1].attachments[0].url, "data:image/png;base64,AAAA");
    }

    #[test]
    fn authorize_media_attachments_no_op_without_a_token() {
        let entries = vec![ChatEntry {
            id: 0,
            role: Role::User,
            content: "restored".to_string(),
            attachments: vec![ImageAttachment {
                url: "/v1/media/websocket/abc.png".to_string(),
                label: None,
            }],
            streaming: false,
            tool_events: None,
            reasoning: None,
            user_id: None,
            timestamp: None,
        }];

        let result = authorize_media_attachments(entries.clone(), None);
        assert_eq!(result, entries);
        let result = authorize_media_attachments(entries.clone(), Some(""));
        assert_eq!(result, entries);
    }

    #[test]
    fn media_urls_to_attachments_stamps_gateway_urls_only() {
        let media = vec![
            "/v1/media/websocket/abc.png".to_string(),
            "https://example.com/a.png".to_string(),
        ];

        let attachments = media_urls_to_attachments(media, Some("tok en"));

        assert_eq!(
            attachments,
            vec![
                ImageAttachment {
                    url: "/v1/media/websocket/abc.png?token=tok%20en".to_string(),
                    label: Some("abc.png".to_string()),
                },
                ImageAttachment {
                    url: "https://example.com/a.png".to_string(),
                    label: Some("a.png".to_string()),
                },
            ]
        );
    }

    #[test]
    fn media_urls_to_attachments_no_op_without_a_token() {
        let media = vec!["/v1/media/websocket/abc.png".to_string()];
        let attachments = media_urls_to_attachments(media, None);
        assert_eq!(
            attachments,
            vec![ImageAttachment {
                url: "/v1/media/websocket/abc.png".to_string(),
                label: Some("abc.png".to_string()),
            }]
        );
    }

    #[test]
    fn media_urls_to_attachments_empty_input_is_empty_output() {
        assert_eq!(media_urls_to_attachments(Vec::new(), Some("tok")), vec![]);
    }

    #[test]
    fn apply_assistant_attachments_extends_the_turn_bubble() {
        let mut entries = Vec::new();
        let mut turn_index = HashMap::new();
        let mut next_id = 0;
        begin_turn(
            &mut entries,
            &mut turn_index,
            &mut next_id,
            "turn-1",
            "make a pdf".to_string(),
            Vec::new(),
            None,
        );
        apply_stream_end(&mut entries, &turn_index, "turn-1", Some("here you go"));
        apply_assistant_attachments(
            &mut entries,
            &turn_index,
            "turn-1",
            vec![ImageAttachment {
                url: "/v1/media/websocket/report.pdf".to_string(),
                label: Some("report.pdf".to_string()),
            }],
        );

        assert_eq!(entries[1].attachments.len(), 1);
        assert_eq!(
            entries[1].attachments[0].url,
            "/v1/media/websocket/report.pdf"
        );
        assert_eq!(entries[1].content, "here you go");
    }

    #[test]
    fn apply_assistant_attachments_no_op_when_empty_or_unknown_turn() {
        let mut entries = Vec::new();
        let mut turn_index = HashMap::new();
        let mut next_id = 0;
        begin_turn(
            &mut entries,
            &mut turn_index,
            &mut next_id,
            "turn-1",
            "hi".to_string(),
            Vec::new(),
            None,
        );
        apply_assistant_attachments(&mut entries, &turn_index, "turn-1", Vec::new());
        apply_assistant_attachments(
            &mut entries,
            &turn_index,
            "missing",
            vec![ImageAttachment {
                url: "/v1/media/websocket/a.pdf".to_string(),
                label: Some("a.pdf".to_string()),
            }],
        );
        assert!(entries[1].attachments.is_empty());
    }

    #[test]
    fn build_ws_url_translates_http_to_ws() {
        let url = build_ws_url(
            "http",
            "127.0.0.1:18790",
            "/ws",
            "client-1",
            Some("tok en"),
            None,
        );
        assert_eq!(
            url,
            "ws://127.0.0.1:18790/ws?client_id=client-1&token=tok%20en"
        );
    }

    #[test]
    fn build_ws_url_translates_https_to_wss() {
        let url = build_ws_url(
            "https",
            "example.com",
            "/ws",
            "client-1",
            Some("secret"),
            None,
        );
        assert_eq!(url, "wss://example.com/ws?client_id=client-1&token=secret");
    }

    #[test]
    fn build_ws_url_omits_token_when_none() {
        let url = build_ws_url("https", "example.com", "/ws", "client-1", None, None);
        assert_eq!(url, "wss://example.com/ws?client_id=client-1");
    }

    #[test]
    fn connection_status_label_and_dot_modifier_are_consistent() {
        assert_eq!(ConnectionStatus::Connecting.label(), "Connecting");
        assert_eq!(ConnectionStatus::Connected.label(), "Connected");
        assert_eq!(ConnectionStatus::Reconnecting.label(), "Reconnecting");
        assert_eq!(ConnectionStatus::Disconnected.label(), "Disconnected");

        assert_eq!(
            ConnectionStatus::Connected.dot_modifier_class(),
            "connection-dot--connected"
        );
        assert_eq!(
            ConnectionStatus::Connecting.dot_modifier_class(),
            "connection-dot--pending"
        );
        assert_eq!(
            ConnectionStatus::Reconnecting.dot_modifier_class(),
            "connection-dot--pending"
        );
        assert_eq!(
            ConnectionStatus::Disconnected.dot_modifier_class(),
            "connection-dot--disconnected"
        );
    }

    #[test]
    fn startup_view_covers_token_and_policy_combinations() {
        assert_eq!(startup_view(true, None), StartupView::Chat);
        assert_eq!(startup_view(true, Some(true)), StartupView::Chat);
        assert_eq!(startup_view(true, Some(false)), StartupView::Chat);
        assert_eq!(startup_view(false, Some(false)), StartupView::Chat);
        assert_eq!(startup_view(false, Some(true)), StartupView::Login);
        assert_eq!(startup_view(false, None), StartupView::Loading);
    }

    #[test]
    fn reconnect_attempts_exhausted_respects_cap() {
        for attempt in 0..MAX_RECONNECT_ATTEMPTS {
            assert!(
                !reconnect_attempts_exhausted(attempt),
                "attempt {attempt} should not be exhausted yet"
            );
        }
        assert!(reconnect_attempts_exhausted(MAX_RECONNECT_ATTEMPTS));
        assert!(reconnect_attempts_exhausted(MAX_RECONNECT_ATTEMPTS + 10));
    }

    fn session_item(id: &str) -> SessionListItem {
        SessionListItem {
            id: id.to_string(),
            title: format!("chat {id}"),
            created_at: String::new(),
            updated_at: String::new(),
            has_summary: false,
            pinned: false,
        }
    }

    #[test]
    fn set_session_pinned_updates_only_the_matching_row() {
        let mut sessions = vec![session_item("a"), session_item("b")];
        assert!(set_session_pinned(&mut sessions, "b", true));
        assert!(!sessions[0].pinned);
        assert!(sessions[1].pinned);
        assert!(set_session_pinned(&mut sessions, "b", false));
        assert!(!sessions[1].pinned);
        assert!(!set_session_pinned(&mut sessions, "missing", true));
    }

    #[test]
    fn next_session_id_after_picks_the_row_below_the_deleted_one() {
        let sessions = vec![
            session_item("top"),
            session_item("next"),
            session_item("older"),
        ];

        assert_eq!(
            next_session_id_after(&sessions, "top").as_deref(),
            Some("next"),
            "deleting the top row should select the session after it"
        );
        assert_eq!(
            next_session_id_after(&sessions, "next").as_deref(),
            Some("older")
        );
    }

    #[test]
    fn next_session_id_after_falls_back_to_the_previous_row_when_deleting_the_last() {
        let sessions = vec![session_item("top"), session_item("last")];

        assert_eq!(
            next_session_id_after(&sessions, "last").as_deref(),
            Some("top")
        );
    }

    #[test]
    fn next_session_id_after_is_none_for_the_only_session() {
        let sessions = vec![session_item("only")];
        assert_eq!(next_session_id_after(&sessions, "only"), None);
        assert_eq!(next_session_id_after(&sessions, "missing"), None);
        assert_eq!(next_session_id_after(&[], "only"), None);
    }

    #[test]
    fn before_user_index_after_entry_counts_user_turns_through_the_assistant() {
        let entries = vec![
            user_entry(0),
            assistant_entry(1),
            user_entry(2),
            assistant_entry(3),
        ];

        assert_eq!(before_user_index_after_entry(&entries, 1), Some(1));
        assert_eq!(before_user_index_after_entry(&entries, 3), Some(2));
        assert_eq!(before_user_index_after_entry(&entries, 0), None);
        assert_eq!(before_user_index_after_entry(&entries, 99), None);
    }

    #[test]
    fn show_fork_for_entry_is_completed_assistant_when_not_pending() {
        let user = user_entry(0);
        let streaming = assistant_entry(1);
        let completed = completed_assistant_entry(2);

        assert!(!show_fork_for_entry(&user, false));
        assert!(!show_fork_for_entry(&streaming, false));
        assert!(show_fork_for_entry(&completed, false));
        assert!(!show_fork_for_entry(&completed, true));
        assert!(!show_fork_for_entry(&user, true));
        assert!(!show_fork_for_entry(&streaming, true));
    }

    #[test]
    fn build_ws_url_prefers_override_base_when_present() {
        let url = build_ws_url(
            "https",
            "example.com",
            "/ws",
            "client-1",
            Some("secret"),
            Some("ws://127.0.0.1:18790/"),
        );
        assert_eq!(
            url,
            "ws://127.0.0.1:18790/ws?client_id=client-1&token=secret"
        );
    }
}

#[cfg(test)]
mod config_file_view_tests {
    use super::ConfigFileView;

    fn loaded(content: &str) -> ConfigFileView {
        ConfigFileView::Loaded {
            path: "p".to_string(),
            content: content.to_string(),
        }
    }

    #[test]
    fn a_pending_load_becomes_the_editor_when_the_file_arrives() {
        assert_eq!(
            ConfigFileView::Loading.after_file_arrived("p".into(), "{}".into()),
            loaded("{}")
        );
    }

    #[test]
    fn a_file_arriving_after_the_editor_was_closed_is_dropped() {
        assert_eq!(
            ConfigFileView::Closed.after_file_arrived("p".into(), "{}".into()),
            ConfigFileView::Closed
        );
    }

    #[test]
    fn a_failure_abandons_a_pending_load() {
        assert_eq!(
            ConfigFileView::Loading.after_failure(),
            ConfigFileView::Closed
        );
    }

    #[test]
    fn a_failure_keeps_an_open_editor_and_its_text() {
        assert_eq!(loaded("{}").after_failure(), loaded("{}"));
    }
}
