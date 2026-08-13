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

use chat_ui::models::{ChatEntry, ToolEvent};

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

/// Apply a `stream_end` event: mark the entry no longer streaming, and, when
/// the gateway supplied the authoritative full `text`, overwrite the
/// accumulated delta content with it. The backend sends `text` as the
/// source-of-truth final content, which may differ slightly from a naive
/// concatenation of every delta it streamed, so an override always wins over
/// the accumulated content; when no override is supplied, the accumulated
/// content is left as-is. Also closes out any still-`"running"` tool-activity
/// chip on the entry — see [`finish_any_running_tool_events`].
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
        finish_any_running_tool_events(entry);
    }
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
        name: text.to_string(),
        status: "running".to_string(),
        detail: None,
    }
}

/// Wrap a free-text `progress` (non-tool-hint) narration line as a single
/// synthetic [`ToolEvent`], for the same reason [`synthesize_tool_hint_event`]
/// exists: `agent_loop.rs`'s `before_execute_tools` hook also fires
/// `on_progress(thought, false)` — the model's in-between "thinking out
/// loud" text, shown as `↳ ...` lines in the CLI — whenever no `on_stream`
/// callback is wired up (true here, since `channels.websocket.streaming` is
/// `false` in this deployment), and that too arrives with `tool_events` left
/// `None`.
///
/// Deliberately given the status `"note"` rather than `"running"`: unlike a
/// tool call, a narration line isn't a stateful in-flight operation with a
/// future outcome to report — there is nothing to transition from "running"
/// to "done" for — so it renders as a static, non-pulsing chip
/// (`classify_tool_status("note")` falls into the `Done` bucket, since
/// `"note"` contains none of `Running`'s `"run"`/`"progress"` substrings).
pub fn synthesize_progress_note_event(text: &str) -> ToolEvent {
    ToolEvent {
        name: text.to_string(),
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
        match existing.iter_mut().find(|event| event.name == incoming.name) {
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

/// Maximum automatic reconnect attempts before giving up and surfacing a
/// manual "Retry" affordance, per the 0-indexed `attempt` convention shared
/// with [`compute_backoff_delay_ms`].
pub const MAX_RECONNECT_ATTEMPTS: u32 = 5;

/// True once `attempt` has reached [`MAX_RECONNECT_ATTEMPTS`] and automatic
/// reconnection should stop in favor of a manual retry.
pub fn reconnect_attempts_exhausted(attempt: u32) -> bool {
    attempt >= MAX_RECONNECT_ATTEMPTS
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

/// Build the gateway WebSocket URL.
///
/// Translates the page's `http(s)` origin `scheme` to the matching `ws(s)`
/// scheme and combines it with `host`, unless `override_base` supplies an
/// explicit scheme+host prefix instead (for local dev, e.g. `trunk serve`
/// pointing at a gateway on a different port than the one Trunk itself serves
/// from). `client_id`/`token` are always percent-encoded into the query
/// string. `path` is used as-is (the gateway config already normalizes it to
/// start with `/`; see `WebSocketConfig::path` server-side).
pub fn build_ws_url(
    scheme: &str,
    host: &str,
    path: &str,
    client_id: &str,
    token: &str,
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
    format!(
        "{base}{path}?client_id={}&token={}",
        percent_encode_query_value(client_id),
        percent_encode_query_value(token)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chat_ui::models::Role;

    fn assistant_entry(id: u64) -> ChatEntry {
        ChatEntry {
            id,
            role: Role::Assistant,
            content: String::new(),
            attachments: Vec::new(),
            streaming: true,
            tool_events: None,
            reasoning: None,
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
        apply_stream_end(&mut entries, &index, "turn-1", Some("authoritative full text"));

        assert_eq!(entries[0].content, "authoritative full text");
        assert!(!entries[0].streaming);
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
        assert_eq!(events.len(), 2, "search should be updated in place, not duplicated");
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
        assert_eq!(events[0].status, "done", "running chip should be closed out");
        assert_eq!(
            events[1].status, "failed",
            "an already-terminal status must not be overwritten"
        );
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
        assert_eq!(
            entries[0].tool_events.clone().unwrap()[0].status,
            "done"
        );
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
        assert_eq!(classify_tool_status("in_progress"), ToolStatusBucket::Running);
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

        assert_eq!(event.name, r#"web_search("London weather")"#);
        assert_eq!(event.status, "running");
        assert_eq!(event.detail, None);
    }

    #[test]
    fn synthesize_progress_note_event_wraps_text_as_a_static_note() {
        let event = synthesize_progress_note_event("I'll check local docs first.");

        assert_eq!(event.name, "I'll check local docs first.");
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
    fn build_ws_url_translates_http_to_ws() {
        let url = build_ws_url("http", "127.0.0.1:18790", "/ws", "client-1", "tok en", None);
        assert_eq!(
            url,
            "ws://127.0.0.1:18790/ws?client_id=client-1&token=tok%20en"
        );
    }

    #[test]
    fn build_ws_url_translates_https_to_wss() {
        let url = build_ws_url("https", "example.com", "/ws", "client-1", "secret", None);
        assert_eq!(
            url,
            "wss://example.com/ws?client_id=client-1&token=secret"
        );
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

    #[test]
    fn build_ws_url_prefers_override_base_when_present() {
        let url = build_ws_url(
            "https",
            "example.com",
            "/ws",
            "client-1",
            "secret",
            Some("ws://127.0.0.1:18790/"),
        );
        assert_eq!(
            url,
            "ws://127.0.0.1:18790/ws?client_id=client-1&token=secret"
        );
    }
}
