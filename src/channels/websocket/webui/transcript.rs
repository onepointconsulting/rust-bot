//! Partial port of nanobot's `webui/transcript.py`. Covers the write path
//! `handle_envelope_message` needs: stamping a client-supplied `turn_id`
//! onto inbound message metadata (`client_turn_metadata`), and persisting
//! user messages to the append-only JSONL transcript (`WebUiTranscriptRecorder`,
//! `append_user_message`, `append`).
//!
//! Deliberately NOT ported yet:
//! - **Segment rotation** (`_rotate_active_transcript_if_needed`, segment
//!   files, `manifest.json`): nanobot's `append_transcript_object` triggers
//!   this when the appended record's `event` is `"turn_end"`. `append_user_message`
//!   never produces that event, so the branch is unreachable via this file's
//!   current callers; it's stubbed with a log warning (see
//!   `append_transcript_object`) instead of a full implementation.
//! - **Automation-source tagging** (`webui_message_source`, which needs
//!   `is_automation_kind` from `session/automation_turns.py` and, transitively,
//!   cron/local-trigger specs — none of which exist in rust-bot yet).
//!   `append_user_message` always calls `prepare_and_append` without
//!   `include_source`, so this is never invoked on that path; see
//!   `prepare_event`.
//! - The rest of the read/replay side of `webui/transcript.py` (fork, replay,
//!   pagination, incomplete-turn recovery) — out of scope for the write path.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;
use uuid::Uuid;

/// Maximum size of the active transcript file, past which nanobot rolls
/// older turns into a segment file. Mirrors `_MAX_TRANSCRIPT_FILE_BYTES`
/// (`webui/transcript.py:30`).
const MAX_TRANSCRIPT_FILE_BYTES: usize = 8 * 1024 * 1024;

/// Metadata key carrying the WebUI-tracked turn id. Mirrors nanobot's
/// `WEBUI_TURN_METADATA_KEY` (`webui/metadata.py:3`).
pub const WEBUI_TURN_METADATA_KEY: &str = "webui_turn_id";

/// Mirrors nanobot's `_WEBUI_TURN_ID_RE` (`webui/transcript.py:37`).
static WEBUI_TURN_ID_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9._:-]{1,128}$").unwrap());

/// Accept a client-supplied turn id only if it's a validly-shaped string;
/// otherwise mint a fresh one. Mirrors nanobot's `normalize_webui_turn_id`
/// (`webui/transcript.py:651-656`).
///
/// Takes the raw envelope value (as opposed to an already-extracted `&str`)
/// so it can apply the same `isinstance(value, str)` gate nanobot's
/// dynamically-typed `Any` parameter does: a non-string JSON value (a
/// number, an object, `null`, ...) is treated the same as a missing one.
pub fn normalize_webui_turn_id(value: Option<&serde_json::Value>) -> String {
    if let Some(candidate) = value.and_then(|v| v.as_str()) {
        let candidate = candidate.trim();
        if WEBUI_TURN_ID_RE.is_match(candidate) {
            return candidate.to_string();
        }
    }
    Uuid::new_v4().to_string()
}

/// Build the metadata patch nanobot merges via
/// `metadata.update(self._transcripts.client_turn_metadata(...))`.
/// Mirrors `WebUiTranscriptRecorder.client_turn_metadata`
/// (`webui/transcript.py:681-682`).
pub fn client_turn_metadata(
    turn_id: Option<&serde_json::Value>,
) -> HashMap<String, serde_json::Value> {
    HashMap::from([(
        WEBUI_TURN_METADATA_KEY.to_string(),
        serde_json::Value::String(normalize_webui_turn_id(turn_id)),
    )])
}

/// Prepare and persist WebUI wire events without leaking UI rules into
/// channels. Mirrors nanobot's `WebUiTranscriptRecorder` (`webui/transcript.py:674-773`).
///
/// Owns the in-memory `turn_sequences` bookkeeping (never persisted) plus the
/// directory new transcript files are written under. Constructed with an
/// explicit `webui_dir` (rather than calling `get_webui_dir()` internally)
/// so it can be unit-tested against a tempdir — `get_webui_dir()` resolves
/// through a process-wide `OnceLock` that's only settable once per test
/// binary, so calling it from a unit test risks picking up whatever path
/// some unrelated test module already set.
pub struct WebUiTranscriptRecorder {
    webui_dir: PathBuf,
    /// `(chat_id, turn_id) -> next turn_seq`. Mirrors `_turn_sequences`.
    turn_sequences: HashMap<(String, String), u64>,
    /// Session keys tombstoned by [`Self::forget_session`]. Same reasoning
    /// as `SessionManager`'s `deleted` set (`session::manager`): a `delete_chat`
    /// unlinks the active transcript file, but a write in flight when the
    /// delete ran (built its event before, calls `append` after) would
    /// otherwise resurrect it — `append_to_active_transcript` opens with
    /// `create(true)`.
    forgotten: HashSet<String>,
}

impl WebUiTranscriptRecorder {
    pub fn new(webui_dir: PathBuf) -> Self {
        Self {
            webui_dir,
            turn_sequences: HashMap::new(),
            forgotten: HashSet::new(),
        }
    }

    /// Mirrors `_next_turn_seq` (`webui/transcript.py:751-755`).
    fn next_turn_seq(&mut self, chat_id: &str, turn_id: &str) -> u64 {
        let key = (chat_id.to_string(), turn_id.to_string());
        let seq = self.turn_sequences.get(&key).copied().unwrap_or(0) + 1;
        self.turn_sequences.insert(key, seq);
        seq
    }

    /// Stamp `turn_id`/`turn_phase`/`turn_seq` onto `event` when `phase` and
    /// a valid turn id are both present; evict the sequence counter once the
    /// turn completes. Mirrors `_annotate_turn` (`webui/transcript.py:757-773`).
    fn annotate_turn(
        &mut self,
        chat_id: &str,
        event: &mut HashMap<String, Value>,
        metadata: Option<&HashMap<String, Value>>,
        phase: Option<&str>,
    ) {
        let Some(phase) = phase else {
            return;
        };
        let turn_id = metadata
            .and_then(|m| m.get(WEBUI_TURN_METADATA_KEY))
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty());
        let Some(turn_id) = turn_id else {
            return;
        };
        let turn_id = turn_id.to_string();
        event.insert("turn_id".to_string(), Value::String(turn_id.clone()));
        event.insert("turn_phase".to_string(), Value::String(phase.to_string()));
        event.insert(
            "turn_seq".to_string(),
            Value::from(self.next_turn_seq(chat_id, &turn_id)),
        );
        if phase == "complete" {
            self.turn_sequences.remove(&(chat_id.to_string(), turn_id));
        }
    }

    /// Mirrors `prepare_event` (`webui/transcript.py:684-695`).
    ///
    /// `include_source` isn't supported yet — nanobot's `webui_message_source`
    /// (automation-kind tagging) depends on `is_automation_kind`, which isn't
    /// ported (see module docs). `append_user_message` never sets this flag,
    /// so today this only logs a warning for any future caller that does.
    fn prepare_event(
        &mut self,
        chat_id: &str,
        event: &mut HashMap<String, Value>,
        metadata: Option<&HashMap<String, Value>>,
        phase: Option<&str>,
        include_source: bool,
    ) {
        if include_source {
            log::warn!(
                "webui transcript: include_source requested but webui_message_source \
                 (automation-kind tagging) isn't ported yet; skipping source tag"
            );
        }
        self.annotate_turn(chat_id, event, metadata, phase);
    }

    /// Mirrors `prepare_and_append` (`webui/transcript.py:697-717`).
    fn prepare_and_append(
        &mut self,
        chat_id: &str,
        mut event: HashMap<String, Value>,
        metadata: Option<&HashMap<String, Value>>,
        phase: Option<&str>,
        include_source: bool,
        transcript_overrides: Option<HashMap<String, Value>>,
    ) -> bool {
        self.prepare_event(chat_id, &mut event, metadata, phase, include_source);
        let mut record = event;
        if let Some(overrides) = transcript_overrides {
            record.extend(overrides);
        }
        self.append(chat_id, record)
    }

    /// Persist an inbound user message. Mirrors `append_user_message`
    /// (`webui/transcript.py:719-740`).
    pub fn append_user_message(
        &mut self,
        chat_id: &str,
        text: &str,
        metadata: &HashMap<String, Value>,
        media_paths: Option<&[String]>,
        cli_apps: Option<&[HashMap<String, String>]>,
        mcp_presets: Option<&[HashMap<String, String>]>,
    ) -> bool {
        if text.trim() == "/stop" && media_paths.is_none_or(<[String]>::is_empty) {
            return false;
        }
        let Some(payload) =
            build_user_transcript_event(chat_id, text, media_paths, cli_apps, mcp_presets)
        else {
            return false;
        };
        self.prepare_and_append(chat_id, payload, Some(metadata), Some("user"), false, None)
    }

    /// Deep-copy and durably persist one prepared event. Mirrors `append`
    /// (`webui/transcript.py:742-749`).
    ///
    /// Python defensively round-trips the event through JSON to detach it
    /// from any shared/mutable reference before persisting. That's
    /// unnecessary here: `event` is taken by value, so Rust ownership
    /// already guarantees this call has an uncontested copy.
    pub fn append(&mut self, chat_id: &str, event: HashMap<String, Value>) -> bool {
        let session_key = format!("websocket:{chat_id}");
        if self.forgotten.contains(&session_key) {
            return false;
        }
        match append_transcript_object(&self.webui_dir, &session_key, event) {
            Ok(()) => true,
            Err(e) => {
                log::warn!("webui transcript append failed: {e}");
                false
            }
        }
    }

    /// Best-effort: unlink `chat_id`'s active transcript file and tombstone
    /// its session key so a later, in-flight [`Self::append`] cannot
    /// recreate it (see the [`Self::forgotten`] field doc comment). Called
    /// from the WebSocket `delete_chat` handler alongside
    /// `SessionManager::delete_session`; a missing file is not an error —
    /// there may never have been a WebUI transcript for this chat.
    pub fn forget_session(&mut self, chat_id: &str) {
        let session_key = format!("websocket:{chat_id}");
        self.forgotten.insert(session_key.clone());
        self.turn_sequences.retain(|(c, _), _| c != chat_id);

        let path = webui_transcript_path(&self.webui_dir, &session_key);
        if let Err(e) = fs::remove_file(&path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            log::warn!(
                "webui transcript: failed to unlink {} for delete_chat: {e}",
                path.display()
            );
        }
    }
}

/// Shape a user's text/media/mentions into the `{"event": "user", ...}`
/// transcript record, or `None` when there's nothing to record. Mirrors
/// `build_user_transcript_event` (`webui/transcript.py:898-930`).
pub fn build_user_transcript_event(
    chat_id: &str,
    text: &str,
    media_paths: Option<&[String]>,
    cli_apps: Option<&[HashMap<String, String>]>,
    mcp_presets: Option<&[HashMap<String, String>]>,
) -> Option<HashMap<String, Value>> {
    let paths: Vec<Value> = media_paths
        .unwrap_or(&[])
        .iter()
        .filter(|p| !p.is_empty())
        .map(|p| Value::String(p.clone()))
        .collect();
    if text.is_empty() && paths.is_empty() {
        return None;
    }
    let mut event = HashMap::new();
    event.insert("event".to_string(), Value::String("user".to_string()));
    event.insert("chat_id".to_string(), Value::String(chat_id.to_string()));
    event.insert("text".to_string(), Value::String(text.to_string()));
    if !paths.is_empty() {
        event.insert("media_paths".to_string(), Value::Array(paths));
    }
    if let Some(apps) = cli_apps.filter(|a| !a.is_empty()) {
        event.insert(
            "cli_apps".to_string(),
            serde_json::to_value(apps).unwrap_or(Value::Null),
        );
    }
    if let Some(presets) = mcp_presets.filter(|p| !p.is_empty()) {
        event.insert(
            "mcp_presets".to_string(),
            serde_json::to_value(presets).unwrap_or(Value::Null),
        );
    }
    Some(event)
}

/// Mirrors `_now_ms` (`webui/transcript.py:624-625`).
fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

/// Mirrors `_valid_created_at_ms` (`webui/transcript.py:628-633`). `bool` and
/// `Number` are already distinct `Value` variants, so — unlike Python, where
/// `bool` is an `int` subclass — no explicit bool-exclusion check is needed.
fn valid_created_at_ms(value: Option<&Value>) -> Option<i64> {
    let n = value?.as_f64()?;
    if (0.0..10_000_000_000_000_000.0).contains(&n) {
        Some(n as i64)
    } else {
        None
    }
}

/// Stamp `created_at_ms` onto `obj` if it doesn't already carry a valid one.
/// Mirrors `_record_for_append` (`webui/transcript.py:636-641`).
fn record_for_append(mut obj: HashMap<String, Value>) -> HashMap<String, Value> {
    if valid_created_at_ms(obj.get("created_at_ms")).is_none() {
        obj.insert("created_at_ms".to_string(), Value::from(now_ms()));
    }
    obj
}

/// Compact, non-ASCII-preserving JSON line for one record. Mirrors
/// `_record_json_line` (`webui/transcript.py:163-164`): `serde_json::to_string`
/// is already the compact form (no pretty-printing) and doesn't escape
/// non-ASCII, matching `separators=(",", ":"), ensure_ascii=False`.
fn record_json_line(record: &HashMap<String, Value>) -> std::io::Result<String> {
    serde_json::to_string(record).map_err(std::io::Error::from)
}

/// Map a session key to its active-transcript file path. Mirrors
/// `webui_transcript_path` (`webui/transcript.py:126-128`).
///
/// nanobot's `SessionManager.safe_key` is `safe_filename(key.replace(":", "_"))`
/// — but `safe_filename`'s own unsafe-char set already includes `:`, so
/// pre-replacing it first is a no-op in practice; calling `safe_filename`
/// directly here produces an identical result.
fn webui_transcript_path(webui_dir: &Path, session_key: &str) -> PathBuf {
    let stem = crate::utils::helpers::safe_filename(session_key);
    webui_dir.join(format!("{stem}.jsonl"))
}

/// Atomically append one durable JSON line to the active transcript file.
/// Mirrors `_append_to_active_transcript` (`webui/transcript.py:610-621`).
fn append_to_active_transcript(
    webui_dir: &Path,
    session_key: &str,
    record: &HashMap<String, Value>,
) -> std::io::Result<()> {
    let raw = record_json_line(record)?;
    if raw.len() > MAX_TRANSCRIPT_FILE_BYTES {
        return Err(std::io::Error::other("webui transcript line too large"));
    }
    let path = webui_transcript_path(webui_dir, session_key);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    use std::io::Write;
    file.write_all(raw.as_bytes())?;
    file.write_all(b"\n")?;
    file.flush()?;
    file.sync_data()?;
    Ok(())
}

/// Stamp a timestamp and durably persist one transcript record. Mirrors
/// `append_transcript_object` (`webui/transcript.py:644-648`).
///
/// nanobot rotates the active file into a segment once it grows past
/// `MAX_TRANSCRIPT_FILE_BYTES`, triggered here when `record["event"] ==
/// "turn_end"`. That rotation subsystem isn't ported yet (see module docs),
/// so this only warns — the active transcript will grow unbounded until it's
/// implemented.
fn append_transcript_object(
    webui_dir: &Path,
    session_key: &str,
    obj: HashMap<String, Value>,
) -> std::io::Result<()> {
    let record = record_for_append(obj);
    append_to_active_transcript(webui_dir, session_key, &record)?;
    if record.get("event").and_then(Value::as_str) == Some("turn_end") {
        log::warn!(
            "webui transcript rotation isn't implemented yet; active transcript for \
             '{session_key}' will grow unbounded past {MAX_TRANSCRIPT_FILE_BYTES} bytes"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_webui_turn_id_keeps_a_validly_shaped_string() {
        let value = serde_json::json!("client-turn-123");
        assert_eq!(normalize_webui_turn_id(Some(&value)), "client-turn-123");
    }

    #[test]
    fn normalize_webui_turn_id_trims_whitespace() {
        let value = serde_json::json!("  client-turn-123  ");
        assert_eq!(normalize_webui_turn_id(Some(&value)), "client-turn-123");
    }

    #[test]
    fn normalize_webui_turn_id_generates_a_uuid_when_missing() {
        let id = normalize_webui_turn_id(None);
        assert!(Uuid::parse_str(&id).is_ok());
    }

    #[test]
    fn normalize_webui_turn_id_generates_a_uuid_for_empty_string() {
        let value = serde_json::json!("");
        let id = normalize_webui_turn_id(Some(&value));
        assert!(Uuid::parse_str(&id).is_ok());
    }

    #[test]
    fn normalize_webui_turn_id_generates_a_uuid_for_non_string_value() {
        let value = serde_json::json!(12345);
        let id = normalize_webui_turn_id(Some(&value));
        assert!(Uuid::parse_str(&id).is_ok());
    }

    #[test]
    fn normalize_webui_turn_id_generates_a_uuid_for_overlong_string() {
        let value = serde_json::json!("x".repeat(129));
        let id = normalize_webui_turn_id(Some(&value));
        assert!(Uuid::parse_str(&id).is_ok());
    }

    #[test]
    fn normalize_webui_turn_id_generates_a_uuid_for_disallowed_characters() {
        let value = serde_json::json!("has space");
        let id = normalize_webui_turn_id(Some(&value));
        assert!(Uuid::parse_str(&id).is_ok());
    }

    #[test]
    fn client_turn_metadata_carries_the_normalized_id_under_the_expected_key() {
        let value = serde_json::json!("client-turn-123");
        let metadata = client_turn_metadata(Some(&value));
        assert_eq!(
            metadata.get(WEBUI_TURN_METADATA_KEY),
            Some(&serde_json::Value::String("client-turn-123".to_string()))
        );
    }

    // --- build_user_transcript_event ---

    #[test]
    fn build_user_transcript_event_none_when_no_text_and_no_media() {
        assert!(build_user_transcript_event("chat-1", "", None, None, None).is_none());
    }

    #[test]
    fn build_user_transcript_event_text_only() {
        let event = build_user_transcript_event("chat-1", "hi", None, None, None).unwrap();
        assert_eq!(event.get("event"), Some(&Value::String("user".to_string())));
        assert_eq!(
            event.get("chat_id"),
            Some(&Value::String("chat-1".to_string()))
        );
        assert_eq!(event.get("text"), Some(&Value::String("hi".to_string())));
        assert!(!event.contains_key("media_paths"));
        assert!(!event.contains_key("cli_apps"));
        assert!(!event.contains_key("mcp_presets"));
    }

    #[test]
    fn build_user_transcript_event_media_only_with_empty_text() {
        let media = vec!["a.png".to_string()];
        let event = build_user_transcript_event("chat-1", "", Some(&media), None, None).unwrap();
        assert_eq!(
            event.get("media_paths"),
            Some(&Value::Array(vec![Value::String("a.png".to_string())]))
        );
    }

    #[test]
    fn build_user_transcript_event_includes_non_empty_cli_apps_and_mcp_presets() {
        let apps = vec![HashMap::from([("name".to_string(), "editor".to_string())])];
        let presets = vec![HashMap::from([("name".to_string(), "fs".to_string())])];
        let event =
            build_user_transcript_event("chat-1", "hi", None, Some(&apps), Some(&presets)).unwrap();
        assert!(event.contains_key("cli_apps"));
        assert!(event.contains_key("mcp_presets"));
    }

    #[test]
    fn build_user_transcript_event_omits_empty_cli_apps_and_mcp_presets() {
        let event =
            build_user_transcript_event("chat-1", "hi", None, Some(&[]), Some(&[])).unwrap();
        assert!(!event.contains_key("cli_apps"));
        assert!(!event.contains_key("mcp_presets"));
    }

    // --- valid_created_at_ms / record_for_append ---

    #[test]
    fn valid_created_at_ms_accepts_int_and_float() {
        assert_eq!(valid_created_at_ms(Some(&Value::from(123))), Some(123));
        assert_eq!(valid_created_at_ms(Some(&Value::from(123.0))), Some(123));
    }

    #[test]
    fn valid_created_at_ms_rejects_bool() {
        assert_eq!(valid_created_at_ms(Some(&Value::Bool(true))), None);
    }

    #[test]
    fn valid_created_at_ms_rejects_negative_and_overlong() {
        assert_eq!(valid_created_at_ms(Some(&Value::from(-1))), None);
        assert_eq!(
            valid_created_at_ms(Some(&Value::from(10_000_000_000_000_001.0))),
            None
        );
    }

    #[test]
    fn valid_created_at_ms_rejects_missing_and_non_numeric() {
        assert_eq!(valid_created_at_ms(None), None);
        assert_eq!(
            valid_created_at_ms(Some(&Value::String("123".to_string()))),
            None
        );
    }

    #[test]
    fn record_for_append_preserves_existing_valid_timestamp() {
        let obj = HashMap::from([("created_at_ms".to_string(), Value::from(42))]);
        let record = record_for_append(obj);
        assert_eq!(record.get("created_at_ms"), Some(&Value::from(42)));
    }

    #[test]
    fn record_for_append_stamps_missing_timestamp() {
        let obj = HashMap::new();
        let record = record_for_append(obj);
        let stamped = record.get("created_at_ms").and_then(Value::as_i64).unwrap();
        assert!(stamped > 0);
    }

    #[test]
    fn record_for_append_replaces_invalid_timestamp() {
        let obj = HashMap::from([("created_at_ms".to_string(), Value::Bool(true))]);
        let record = record_for_append(obj);
        assert!(matches!(
            record.get("created_at_ms"),
            Some(Value::Number(_))
        ));
    }

    // --- webui_transcript_path ---

    #[test]
    fn webui_transcript_path_maps_unsafe_characters_through_safe_filename() {
        let dir = PathBuf::from("/data/webui");
        let path = webui_transcript_path(&dir, "websocket:chat/1");
        assert_eq!(path, dir.join("websocket_chat_1.jsonl"));
    }

    // --- append_to_active_transcript / append_transcript_object ---

    #[test]
    fn append_to_active_transcript_writes_one_terminated_json_line() {
        let dir = tempfile::tempdir().unwrap();
        let record = HashMap::from([("event".to_string(), Value::String("user".to_string()))]);
        append_to_active_transcript(dir.path(), "websocket:chat-1", &record).unwrap();
        let path = webui_transcript_path(dir.path(), "websocket:chat-1");
        let contents = std::fs::read_to_string(path).unwrap();
        assert!(contents.ends_with('\n'));
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 1);
        let parsed: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(parsed["event"], "user");
    }

    #[test]
    fn append_to_active_transcript_rejects_oversized_line_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let huge = "x".repeat(MAX_TRANSCRIPT_FILE_BYTES + 1);
        let record = HashMap::from([("text".to_string(), Value::String(huge))]);
        let err = append_to_active_transcript(dir.path(), "websocket:chat-1", &record).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::Other);
        let path = webui_transcript_path(dir.path(), "websocket:chat-1");
        assert!(!path.exists());
    }

    #[test]
    fn append_transcript_object_stamps_timestamp_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let obj = HashMap::from([("event".to_string(), Value::String("user".to_string()))]);
        append_transcript_object(dir.path(), "websocket:chat-1", obj).unwrap();
        let path = webui_transcript_path(dir.path(), "websocket:chat-1");
        let contents = std::fs::read_to_string(path).unwrap();
        let parsed: Value = serde_json::from_str(contents.trim_end()).unwrap();
        assert!(parsed.get("created_at_ms").is_some());
    }

    #[test]
    fn append_transcript_object_on_turn_end_does_not_error() {
        let dir = tempfile::tempdir().unwrap();
        let obj = HashMap::from([("event".to_string(), Value::String("turn_end".to_string()))]);
        // Rotation is deferred (see module docs); this must still succeed.
        assert!(append_transcript_object(dir.path(), "websocket:chat-1", obj).is_ok());
    }

    // --- WebUiTranscriptRecorder: next_turn_seq / annotate_turn ---

    #[test]
    fn next_turn_seq_increments_per_chat_and_turn() {
        let mut recorder = WebUiTranscriptRecorder::new(PathBuf::from("/unused"));
        assert_eq!(recorder.next_turn_seq("chat-1", "turn-1"), 1);
        assert_eq!(recorder.next_turn_seq("chat-1", "turn-1"), 2);
        assert_eq!(recorder.next_turn_seq("chat-1", "turn-2"), 1);
        assert_eq!(recorder.next_turn_seq("chat-2", "turn-1"), 1);
    }

    #[test]
    fn annotate_turn_noops_without_phase() {
        let mut recorder = WebUiTranscriptRecorder::new(PathBuf::from("/unused"));
        let mut event = HashMap::new();
        let metadata = HashMap::from([(
            WEBUI_TURN_METADATA_KEY.to_string(),
            Value::String("turn-1".to_string()),
        )]);
        recorder.annotate_turn("chat-1", &mut event, Some(&metadata), None);
        assert!(event.is_empty());
    }

    #[test]
    fn annotate_turn_noops_without_valid_turn_id() {
        let mut recorder = WebUiTranscriptRecorder::new(PathBuf::from("/unused"));
        let mut event = HashMap::new();
        recorder.annotate_turn("chat-1", &mut event, None, Some("user"));
        assert!(event.is_empty());
    }

    #[test]
    fn annotate_turn_stamps_turn_fields_and_evicts_on_complete() {
        let mut recorder = WebUiTranscriptRecorder::new(PathBuf::from("/unused"));
        let metadata = HashMap::from([(
            WEBUI_TURN_METADATA_KEY.to_string(),
            Value::String("turn-1".to_string()),
        )]);
        let mut event = HashMap::new();
        recorder.annotate_turn("chat-1", &mut event, Some(&metadata), Some("user"));
        assert_eq!(
            event.get("turn_id"),
            Some(&Value::String("turn-1".to_string()))
        );
        assert_eq!(
            event.get("turn_phase"),
            Some(&Value::String("user".to_string()))
        );
        assert_eq!(event.get("turn_seq"), Some(&Value::from(1)));
        assert!(
            recorder
                .turn_sequences
                .contains_key(&("chat-1".to_string(), "turn-1".to_string()))
        );

        let mut complete_event = HashMap::new();
        recorder.annotate_turn(
            "chat-1",
            &mut complete_event,
            Some(&metadata),
            Some("complete"),
        );
        assert_eq!(complete_event.get("turn_seq"), Some(&Value::from(2)));
        assert!(
            !recorder
                .turn_sequences
                .contains_key(&("chat-1".to_string(), "turn-1".to_string()))
        );
    }

    // --- append_user_message ---

    #[test]
    fn append_user_message_skips_bare_stop_command() {
        let dir = tempfile::tempdir().unwrap();
        let mut recorder = WebUiTranscriptRecorder::new(dir.path().to_path_buf());
        let metadata = HashMap::new();
        assert!(!recorder.append_user_message("chat-1", "/stop", &metadata, None, None, None));
        assert!(!webui_transcript_path(dir.path(), "websocket:chat-1").exists());
    }

    #[test]
    fn append_user_message_persists_stop_command_when_media_present() {
        let dir = tempfile::tempdir().unwrap();
        let mut recorder = WebUiTranscriptRecorder::new(dir.path().to_path_buf());
        let metadata = HashMap::new();
        let media = vec!["a.png".to_string()];
        assert!(recorder.append_user_message(
            "chat-1",
            "/stop",
            &metadata,
            Some(&media),
            None,
            None
        ));
    }

    #[test]
    fn append_user_message_skips_empty_text_and_no_media() {
        let dir = tempfile::tempdir().unwrap();
        let mut recorder = WebUiTranscriptRecorder::new(dir.path().to_path_buf());
        let metadata = HashMap::new();
        assert!(!recorder.append_user_message("chat-1", "", &metadata, None, None, None));
        assert!(!webui_transcript_path(dir.path(), "websocket:chat-1").exists());
    }

    #[test]
    fn append_user_message_writes_turn_annotated_record() {
        let dir = tempfile::tempdir().unwrap();
        let mut recorder = WebUiTranscriptRecorder::new(dir.path().to_path_buf());
        let metadata = HashMap::from([(
            WEBUI_TURN_METADATA_KEY.to_string(),
            Value::String("turn-1".to_string()),
        )]);
        assert!(recorder.append_user_message("chat-1", "hello", &metadata, None, None, None));

        let path = webui_transcript_path(dir.path(), "websocket:chat-1");
        let contents = std::fs::read_to_string(path).unwrap();
        let parsed: Value = serde_json::from_str(contents.trim_end()).unwrap();
        assert_eq!(parsed["event"], "user");
        assert_eq!(parsed["chat_id"], "chat-1");
        assert_eq!(parsed["text"], "hello");
        assert_eq!(parsed["turn_id"], "turn-1");
        assert_eq!(parsed["turn_phase"], "user");
        assert_eq!(parsed["turn_seq"], 1);
        assert!(parsed.get("created_at_ms").is_some());
    }

    // --- forget_session ---

    #[test]
    fn forget_session_unlinks_existing_transcript_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut recorder = WebUiTranscriptRecorder::new(dir.path().to_path_buf());
        recorder.append_user_message("chat-1", "hello", &HashMap::new(), None, None, None);
        let path = webui_transcript_path(dir.path(), "websocket:chat-1");
        assert!(path.exists());

        recorder.forget_session("chat-1");
        assert!(!path.exists());
    }

    #[test]
    fn forget_session_on_never_written_chat_does_not_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut recorder = WebUiTranscriptRecorder::new(dir.path().to_path_buf());
        recorder.forget_session("never-written");
        assert!(!webui_transcript_path(dir.path(), "websocket:never-written").exists());
    }

    #[test]
    fn append_after_forget_session_is_a_no_op_and_does_not_recreate_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut recorder = WebUiTranscriptRecorder::new(dir.path().to_path_buf());
        recorder.append_user_message("chat-1", "hello", &HashMap::new(), None, None, None);
        recorder.forget_session("chat-1");
        let path = webui_transcript_path(dir.path(), "websocket:chat-1");
        assert!(!path.exists());

        // A late write that raced past the delete must not resurrect it.
        assert!(!recorder.append_user_message(
            "chat-1",
            "late write",
            &HashMap::new(),
            None,
            None,
            None
        ));
        assert!(!path.exists());
    }

    #[test]
    fn forget_session_only_affects_the_named_chat() {
        let dir = tempfile::tempdir().unwrap();
        let mut recorder = WebUiTranscriptRecorder::new(dir.path().to_path_buf());
        recorder.append_user_message("chat-1", "hello", &HashMap::new(), None, None, None);
        recorder.append_user_message("chat-2", "hi", &HashMap::new(), None, None, None);

        recorder.forget_session("chat-1");

        assert!(!webui_transcript_path(dir.path(), "websocket:chat-1").exists());
        assert!(webui_transcript_path(dir.path(), "websocket:chat-2").exists());
        assert!(recorder.append_user_message(
            "chat-2",
            "still alive",
            &HashMap::new(),
            None,
            None,
            None
        ));
    }
}
