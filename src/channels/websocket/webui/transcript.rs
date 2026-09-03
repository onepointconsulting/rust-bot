//! Partial port of nanobot's `webui/transcript.py`. Covers the write path
//! `handle_envelope_message` needs: stamping a client-supplied `turn_id`
//! onto inbound message metadata (`client_turn_metadata`), and persisting
//! user messages to the append-only JSONL transcript (`WebUiTranscriptRecorder`,
//! `append_user_message`, `append`).
//!
//! Deliberately NOT ported yet:
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

use crate::channels::websocket::get_session_id;

/// Maximum size of the active transcript file, past which nanobot rolls
/// older turns into a segment file. Mirrors `_MAX_TRANSCRIPT_FILE_BYTES`
/// (`webui/transcript.py:30`).
const MAX_TRANSCRIPT_FILE_BYTES: usize = 8 * 1024 * 1024;

const TARGET_ACTIVE_TRANSCRIPT_BYTES: usize = (MAX_TRANSCRIPT_FILE_BYTES / 2) as usize;

const WEBUI_FORK_MARKER_EVENT: &str = "fork_marker";

const TRANSCRIPT_ACTIVE_CHUNK_ID: &str = "active";

const TRANSCRIPT_SEGMENT_MANIFEST_VERSION: u32 = 2;

/// Metadata key carrying the WebUI-tracked turn id. Mirrors nanobot's
/// `WEBUI_TURN_METADATA_KEY` (`webui/metadata.py:3`).
pub const WEBUI_TURN_METADATA_KEY: &str = "webui_turn_id";

/// Mirrors nanobot's `_WEBUI_TURN_ID_RE` (`webui/transcript.py:37`).
static WEBUI_TURN_ID_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9._:-]{1,128}$").unwrap());

static TRANSCRIPT_SEGMENT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\d{6}\.jsonl$").unwrap());

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

    /// Map a session key to its active-transcript file path. Mirrors
    /// `webui_transcript_path` (`webui/transcript.py:126-128`).
    ///
    /// nanobot's `SessionManager.safe_key` is `safe_filename(key.replace(":", "_"))`
    /// — but `safe_filename`'s own unsafe-char set already includes `:`, so
    /// pre-replacing it first is a no-op in practice; calling `safe_filename`
    /// directly here produces an identical result.
    fn webui_transcript_path(&self, session_key: &str) -> PathBuf {
        let stem = crate::utils::helpers::safe_filename(session_key);
        self.webui_dir.join(format!("{stem}.jsonl"))
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

    /// Persist one whole outbound turn event (assistant message/progress,
    /// file edits, turn end, ...) with turn id/phase/seq stamping. Thin
    /// public wrapper around [`Self::prepare_and_append`] (`include_source`
    /// and `transcript_overrides` aren't needed by any outbound caller yet)
    /// for use from `WebSocketChannel`'s `send`/`send_file_edit_events`
    /// (`channels/websocket/runtime.rs`), which live in a different module
    /// and can't reach the private method directly.
    pub fn append_turn_event(
        &mut self,
        chat_id: &str,
        event: HashMap<String, Value>,
        metadata: &HashMap<String, Value>,
        phase: &str,
    ) -> bool {
        self.prepare_and_append(chat_id, event, Some(metadata), Some(phase), false, None)
    }

    /// Persist the canonical end of a live stream, never its wire chunks.
    /// Mirrors `_persist_turn_stream_event` (`channels/websocket/runtime.py:1659-1685`),
    /// minus its `_retain_turn_on_transcript_failure` bookkeeping (that
    /// belongs with the turn registry, not this recorder — see
    /// `WebSocketChannel`'s callers).
    ///
    /// A no-op returning `true` when `completed_text` is `None` — every
    /// `send_delta`/`send_reasoning_delta` chunk calls through here, but
    /// only the segment's closing `stream_end`/`send_reasoning_end` call
    /// (which alone has the fully assembled text) should ever reach disk.
    pub fn append_stream_event(
        &mut self,
        chat_id: &str,
        mut event: HashMap<String, Value>,
        completed_text: Option<&str>,
        metadata: &HashMap<String, Value>,
        phase: &str,
    ) -> bool {
        let Some(completed_text) = completed_text else {
            return true;
        };
        event.insert(
            "text".to_string(),
            Value::String(completed_text.to_string()),
        );
        self.prepare_and_append(chat_id, event, Some(metadata), Some(phase), false, None)
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
        let session_key = get_session_id(chat_id);
        if self.forgotten.contains(&session_key) {
            return false;
        }
        match self.append_transcript_object(&session_key, event) {
            Ok(()) => true,
            Err(e) => {
                log::warn!("webui transcript append failed: {e}");
                false
            }
        }
    }

    /// Stamp a timestamp and durably persist one transcript record. Mirrors
    /// `append_transcript_object` (`webui/transcript.py:644-648`).
    ///
    /// nanobot rotates the active file into a segment once it grows past
    /// `MAX_TRANSCRIPT_FILE_BYTES`, triggered here when `record["event"] ==
    /// "turn_end"`.
    fn append_transcript_object(
        &self,
        session_key: &str,
        obj: HashMap<String, Value>,
    ) -> std::io::Result<()> {
        let record = record_for_append(obj);
        self.append_to_active_transcript(session_key, &record)?;
        if record.get("event").and_then(Value::as_str) == Some("turn_end") {
            self.rotate_active_transcript_if_needed(session_key)?;
        }
        Ok(())
    }

    /// Atomically append one durable JSON line to the active transcript file.
    /// Mirrors `_append_to_active_transcript` (`webui/transcript.py:610-621`).
    fn append_to_active_transcript(
        &self,
        session_key: &str,
        record: &HashMap<String, Value>,
    ) -> std::io::Result<()> {
        let raw = record_json_line(record)?;
        if raw.len() > MAX_TRANSCRIPT_FILE_BYTES {
            return Err(std::io::Error::other("webui transcript line too large"));
        }
        let path = self.webui_transcript_path(session_key);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = fs::OpenOptions::new()
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

    /// Best-effort: unlink `chat_id`'s active transcript file and tombstone
    /// its session key so a later, in-flight [`Self::append`] cannot
    /// recreate it (see the [`Self::forgotten`] field doc comment). Called
    /// from the WebSocket `delete_chat` handler alongside
    /// `SessionManager::delete_session`; a missing file is not an error —
    /// there may never have been a WebUI transcript for this chat.
    pub fn forget_session(&mut self, chat_id: &str) {
        let session_key = get_session_id(chat_id);
        self.forgotten.insert(session_key.clone());
        self.turn_sequences.retain(|(c, _), _| c != chat_id);

        let path = self.webui_transcript_path(&session_key);
        if let Err(e) = fs::remove_file(&path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            log::warn!(
                "webui transcript: failed to unlink {} for delete_chat: {e}",
                path.display()
            );
        }
    }

    /// Empty `chat_id`'s durable WebUI transcript without tombstoning the
    /// key — unlike [`Self::forget_session`], later [`Self::append`] calls
    /// must still write. Used by the WebSocket `clear_session` handler:
    /// `attach` prefers transcript history over the `Session`, so leaving
    /// the JSONL in place would resurrect the conversation that was just
    /// wiped.
    pub fn clear_transcript(&mut self, chat_id: &str) {
        let session_key = get_session_id(chat_id);
        self.turn_sequences.retain(|(c, _), _| c != chat_id);
        self.delete_webui_transcript(&session_key);
    }

    /// Copy transcript rows before a zero-based global user-message index.
    /// ``before_user_index == user_count`` copies the full transcript prefix. WebUI
    /// uses that when forking from an assistant reply at the end of a chat.
    pub fn fork_transcript_before_user_index(
        &self,
        source_key: &str,
        target_key: &str,
        before_user_index: usize,
    ) -> bool {
        let lines = self.read_transcript_lines(source_key);
        if lines.is_empty() {
            return false;
        }

        let target_chat_id = chat_id_from_session_key(target_key);
        let mut copied = Vec::new();
        let mut user_index = 0;
        let mut found_target = false;
        for mut row in lines {
            if row.get("event").and_then(Value::as_str) == Some(WEBUI_FORK_MARKER_EVENT) {
                continue;
            }
            if self.is_user_transcript_row(&row) {
                if user_index == before_user_index {
                    found_target = true;
                    break;
                }
                user_index += 1;
            }
            if let Some(chat_id) = target_chat_id.as_ref()
                && let Some(obj) = row.as_object_mut()
            {
                obj.insert("chat_id".to_string(), Value::String(chat_id.clone()));
            }
            copied.push(row);
        }
        if user_index == before_user_index {
            found_target = true;
        }
        if !found_target {
            return false;
        }
        if let Err(e) = self.write_transcript_lines(target_key, copied) {
            log::warn!("webui transcript: fork_transcript_before_user_index write failed: {e}");
            return false;
        }
        true
    }

    /// `attached.history` snapshot built from `session_key`'s durable
    /// transcript rather than a `Session`. Used when a chat fork copies the
    /// JSONL transcript (`fork_transcript_before_user_index` succeeded) — the
    /// new session key has no `Session` file yet, so
    /// [`websocket_chat_history`](crate::channels::websocket::runtime) can't
    /// read it.
    pub fn chat_history(&self, session_key: &str, max_messages: usize) -> Vec<Value> {
        transcript_chat_history(&self.read_transcript_lines(session_key), max_messages)
    }

    /// `pub(crate)` so `WebSocketChannel`'s outbound-persistence tests
    /// (`channels/websocket/runtime.rs`) can assert on raw rows — including
    /// `turn_end`/`file_edit` kinds that [`Self::chat_history`]'s
    /// `attached.history` projection drops, and the raw `reasoning_end`/
    /// `tool_hint` rows it folds into the next answer rather than emitting
    /// as standalone history.
    pub(crate) fn read_transcript_lines(&self, session_key: &str) -> Vec<Value> {
        let mut lines: Vec<Value> = vec![];
        for chunk_id in self.chunk_ids(session_key) {
            if chunk_id == TRANSCRIPT_ACTIVE_CHUNK_ID {
                lines.extend(self.read_transcript_file(&self.webui_transcript_path(session_key)));
            } else {
                lines.extend(
                    self.read_transcript_file(&self.segment_file_path(session_key, &chunk_id)),
                );
            }
        }
        lines
    }

    fn chunk_ids(&self, session_key: &str) -> Vec<String> {
        if let Err(e) = self.rotate_active_transcript_if_needed(session_key) {
            log::warn!("webui transcript: rotate_active_transcript_if_needed failed: {e}");
        }
        let mut ids = self.read_segment_ids(session_key);
        if self.webui_transcript_path(session_key).is_file() {
            ids.push(TRANSCRIPT_ACTIVE_CHUNK_ID.to_string());
        }
        ids
    }

    /// Roll older turns into a segment when the active file is over the cap.
    /// Mirrors `_rotate_active_transcript_if_needed`.
    fn rotate_active_transcript_if_needed(&self, session_key: &str) -> std::io::Result<()> {
        let path = self.webui_transcript_path(session_key);
        if !path.is_file() {
            return Ok(());
        }
        if path
            .metadata()
            .map(|m| m.len() <= MAX_TRANSCRIPT_FILE_BYTES as u64)
            .unwrap_or(true)
        {
            return Ok(());
        }
        let lines = self.read_transcript_file(&path);
        if lines.is_empty() {
            return Ok(());
        }
        let turns = self.split_transcript_turns(lines);
        if turns.len() <= 1 {
            return Ok(());
        }
        let mut keep_start = turns.len() - 1;
        let mut keep_bytes = 0;
        for idx in (0..turns.len()).rev() {
            let turn_bytes = Self::records_bytes(&turns[idx]);
            if idx == turns.len() - 1 || keep_bytes + turn_bytes <= TARGET_ACTIVE_TRANSCRIPT_BYTES {
                keep_start = idx;
                keep_bytes += turn_bytes;
                continue;
            }
            break;
        }

        let moved = &turns[..keep_start];
        let kept = &turns[keep_start..];
        if moved.is_empty() {
            return Ok(());
        }
        self.append_segment_turns(session_key, moved)?;
        self.write_records_to_path(&path, &Self::flatten_turns(kept))
    }

    /// UTF-8 JSONL byte size of `records` (compact JSON plus a newline each).
    /// Mirrors `_records_bytes`.
    fn records_bytes(records: &[Value]) -> usize {
        let mut total = 0;
        for record in records {
            let Ok(line) = serde_json::to_string(record) else {
                continue;
            };
            total += line.len() + 1;
        }
        total
    }

    /// Read one JSONL transcript file, skipping blank lines, invalid JSON, and
    /// non-object records. I/O errors yield an empty list. Mirrors
    /// `_read_transcript_file`.
    fn read_transcript_file(&self, path: &Path) -> Vec<Value> {
        use std::io::{BufRead, BufReader};

        let file = match fs::File::open(path) {
            Ok(file) => file,
            Err(e) => {
                log::warn!(
                    "webui transcript: read transcript failed {}: {e}",
                    path.display()
                );
                return Vec::new();
            }
        };

        let mut lines_out = Vec::new();
        for (line_no, line) in (1usize..).zip(BufReader::new(file).lines()) {
            let line = match line {
                Ok(line) => line,
                Err(e) => {
                    log::warn!(
                        "webui transcript: read transcript failed {}: {e}",
                        path.display()
                    );
                    return Vec::new();
                }
            };
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<Value>(line) {
                Ok(value) if value.is_object() => lines_out.push(value),
                Ok(_) => {}
                Err(_) => {
                    log::warn!(
                        "webui transcript: bad jsonl at {} line {line_no}",
                        path.display()
                    );
                }
            }
        }
        lines_out
    }

    /// Group transcript records into turns bounded by `"event": "turn_end"`.
    /// A trailing group without `turn_end` is kept as an incomplete turn.
    /// Mirrors `_split_transcript_turns`.
    fn split_transcript_turns(&self, lines: Vec<Value>) -> Vec<Vec<Value>> {
        let mut turns = Vec::new();
        let mut current = Vec::new();
        for rec in lines {
            let is_turn_end = rec.get("event").and_then(Value::as_str) == Some("turn_end");
            current.push(rec);
            if is_turn_end {
                turns.push(std::mem::take(&mut current));
            }
        }
        if !current.is_empty() {
            turns.push(current);
        }
        turns
    }

    /// Pack turns into segment files and refresh the manifest. Mirrors
    /// `_append_segment_turns`.
    fn append_segment_turns(&self, session_key: &str, turns: &[Vec<Value>]) -> std::io::Result<()> {
        if turns.is_empty() {
            return Ok(());
        }
        let mut segment_ids = self.read_segment_ids(session_key);
        let mut next_id = segment_ids
            .last()
            .and_then(|id| id.parse::<u32>().ok())
            .map(|id| id.saturating_add(1))
            .unwrap_or(1);
        let mut batch: Vec<Vec<Value>> = Vec::new();
        let mut batch_bytes = 0;
        for turn in turns {
            let turn_bytes = Self::records_bytes(turn);
            if !batch.is_empty() && batch_bytes + turn_bytes > MAX_TRANSCRIPT_FILE_BYTES {
                let segment_id = format!("{next_id:06}");
                self.write_records_to_path(
                    &self.segment_file_path(session_key, &segment_id),
                    &Self::flatten_turns(&batch),
                )?;
                segment_ids.push(segment_id);
                next_id = next_id.saturating_add(1);
                batch.clear();
                batch_bytes = 0;
            }
            batch.push(turn.clone());
            batch_bytes += turn_bytes;
        }
        if !batch.is_empty() {
            let segment_id = format!("{next_id:06}");
            self.write_records_to_path(
                &self.segment_file_path(session_key, &segment_id),
                &Self::flatten_turns(&batch),
            )?;
            segment_ids.push(segment_id);
        }
        self.write_segment_manifest(session_key, &segment_ids)
    }

    /// Segment ids from the session manifest, in manifest order. Mirrors
    /// `_read_segment_ids`.
    fn read_segment_ids(&self, session_key: &str) -> Vec<String> {
        self.read_segment_manifest_entries(session_key)
            .into_iter()
            .filter_map(|entry| entry.get("id").and_then(Value::as_str).map(str::to_string))
            .collect()
    }

    /// Load manifest entries, or rebuild them when the file is missing/stale.
    /// Mirrors `_read_segment_manifest_entries`.
    fn read_segment_manifest_entries(&self, session_key: &str) -> Vec<Value> {
        let rebuild = || match self.rebuilt_segment_manifest_entries(session_key) {
            Ok(entries) => entries,
            Err(e) => {
                log::warn!("webui transcript: rebuilt_segment_manifest_entries failed: {e}");
                Vec::new()
            }
        };

        let directory = self.webui_transcript_segments_dir(session_key);
        if !directory.is_dir() {
            return Vec::new();
        }
        let path = self.webui_transcript_manifest_path(session_key);
        if !path.is_file() {
            return rebuild();
        }

        let Ok(text) = fs::read_to_string(&path) else {
            return rebuild();
        };
        let Ok(data) = serde_json::from_str::<Value>(&text) else {
            return rebuild();
        };
        let Some(manifest) = data.as_object() else {
            return rebuild();
        };
        let version_ok = manifest.get("version").and_then(Value::as_u64)
            == Some(u64::from(TRANSCRIPT_SEGMENT_MANIFEST_VERSION));
        let Some(raw_segments) = manifest.get("segments").and_then(Value::as_array) else {
            return rebuild();
        };
        if !version_ok {
            return rebuild();
        }

        let mut entries = Vec::new();
        for entry in raw_segments {
            let Some(normalized) = self.normalize_manifest_entry(session_key, entry) else {
                return rebuild();
            };
            entries.push(normalized);
        }
        let ids: Vec<String> = entries
            .iter()
            .filter_map(|entry| entry.get("id").and_then(Value::as_str).map(str::to_string))
            .collect();
        if ids != self.segment_ids_on_disk(session_key) {
            return rebuild();
        }
        entries
    }

    fn segment_ids_on_disk(&self, session_key: &str) -> Vec<String> {
        let directory = self.webui_transcript_segments_dir(session_key);
        if !directory.exists() {
            return Vec::new();
        }

        let Ok(entries) = fs::read_dir(&directory) else {
            return Vec::new();
        };

        let mut ids: Vec<String> = entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                if !entry.path().is_file() {
                    return None;
                }
                let name = entry.file_name();
                let name = name.to_str()?;
                if !TRANSCRIPT_SEGMENT_RE.is_match(name) {
                    return None;
                }
                Path::new(name)
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .map(str::to_string)
            })
            .collect();
        ids.sort();
        ids
    }

    fn webui_transcript_segments_dir(&self, session_key: &str) -> PathBuf {
        let stem = crate::utils::helpers::safe_filename(session_key);
        self.webui_dir.join(format!("{stem}.segments"))
    }

    fn webui_transcript_manifest_path(&self, session_key: &str) -> PathBuf {
        self.webui_transcript_segments_dir(session_key)
            .join("manifest.json")
    }

    fn segment_file_path(&self, session_key: &str, segment_id: &str) -> PathBuf {
        return self
            .webui_transcript_segments_dir(session_key)
            .join(format!("{segment_id}.jsonl"));
    }

    /// Flatten turn groups into a single record list. Mirrors `_flatten_turns`.
    fn flatten_turns(turns: &[Vec<Value>]) -> Vec<Value> {
        turns.iter().flatten().cloned().collect()
    }

    /// Write `records` as JSONL via a sibling `.tmp` file, then atomically
    /// replace `path`. Mirrors `_write_records_to_path`.
    fn write_records_to_path(&self, path: &Path, records: &[Value]) -> std::io::Result<()> {
        use std::io::Write;

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let tmp_path = {
            let mut name = path.file_name().unwrap_or_default().to_os_string();
            name.push(".tmp");
            path.with_file_name(name)
        };

        struct RemoveTmp<'a> {
            path: &'a Path,
            keep: bool,
        }
        impl Drop for RemoveTmp<'_> {
            fn drop(&mut self) {
                if !self.keep {
                    let _ = fs::remove_file(self.path);
                }
            }
        }

        let mut tmp_guard = RemoveTmp {
            path: &tmp_path,
            keep: false,
        };

        {
            let mut file = fs::File::create(&tmp_path)?;
            for record in records {
                let raw = serde_json::to_string(record).map_err(std::io::Error::from)?;
                if raw.len() > MAX_TRANSCRIPT_FILE_BYTES {
                    return Err(std::io::Error::other("webui transcript line too large"));
                }
                file.write_all(raw.as_bytes())?;
                file.write_all(b"\n")?;
            }
            file.flush()?;
            file.sync_data()?;
        }

        replace_file(&tmp_path, path)?;
        tmp_guard.keep = true;
        Ok(())
    }

    /// Write `manifest.json` via a sibling `.json.tmp` file, then atomically
    /// replace it. Mirrors `_write_segment_manifest`.
    fn write_segment_manifest(
        &self,
        session_key: &str,
        segment_ids: &[String],
    ) -> std::io::Result<()> {
        fs::create_dir_all(self.webui_transcript_segments_dir(session_key))?;

        let segments: Vec<Value> = segment_ids
            .iter()
            .map(|segment_id| self.segment_manifest_entry(session_key, segment_id))
            .collect();
        let data = serde_json::json!({
            "version": TRANSCRIPT_SEGMENT_MANIFEST_VERSION,
            "segments": segments,
        });

        let path = self.webui_transcript_manifest_path(session_key);
        // Python `path.with_suffix(".json.tmp")` on `manifest.json` → `manifest.json.tmp`.
        let tmp_path = path.with_extension("json.tmp");

        struct RemoveTmp<'a> {
            path: &'a Path,
            keep: bool,
        }
        impl Drop for RemoveTmp<'_> {
            fn drop(&mut self) {
                if !self.keep {
                    let _ = fs::remove_file(self.path);
                }
            }
        }

        let mut tmp_guard = RemoveTmp {
            path: &tmp_path,
            keep: false,
        };

        let mut text = serde_json::to_string_pretty(&data).map_err(std::io::Error::from)?;
        text.push('\n');
        fs::write(&tmp_path, text)?;
        replace_file(&tmp_path, &path)?;
        tmp_guard.keep = true;
        Ok(())
    }

    /// Build one manifest record for a segment file. Mirrors
    /// `_segment_manifest_entry`.
    fn segment_manifest_entry(&self, session_key: &str, segment_id: &str) -> Value {
        let path = self.segment_file_path(session_key, segment_id);
        let lines = self.read_transcript_file(&path);
        let bytes = path.metadata().map(|m| m.len()).unwrap_or(0);
        let user_count = lines
            .iter()
            .filter(|line| self.is_user_transcript_row(line))
            .count();
        let turn_count = self.split_transcript_turns(lines).len();
        serde_json::json!({
            "id": segment_id,
            "bytes": bytes,
            "turn_count": turn_count,
            "user_count": user_count,
        })
    }

    /// Mirrors `_is_user_transcript_row`.
    fn is_user_transcript_row(&self, row: &Value) -> bool {
        row.get("event").and_then(Value::as_str) == Some("user")
            || row.get("role").and_then(Value::as_str) == Some("user")
    }

    /// Rebuild `manifest.json` from segment files on disk, or unlink it when
    /// none remain. Mirrors `_rebuild_segment_manifest`.
    fn rebuild_segment_manifest(&self, session_key: &str) -> std::io::Result<Vec<String>> {
        let segment_ids = self.segment_ids_on_disk(session_key);
        if !segment_ids.is_empty() {
            self.write_segment_manifest(session_key, &segment_ids)?;
        } else {
            match fs::remove_file(self.webui_transcript_manifest_path(session_key)) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        Ok(segment_ids)
    }

    /// Rebuild the manifest from disk, then return one entry per segment.
    /// Mirrors `_rebuilt_segment_manifest_entries`.
    fn rebuilt_segment_manifest_entries(&self, session_key: &str) -> std::io::Result<Vec<Value>> {
        Ok(self
            .rebuild_segment_manifest(session_key)?
            .iter()
            .map(|segment_id| self.segment_manifest_entry(session_key, segment_id))
            .collect())
    }

    /// Validate one manifest segment record against the file on disk.
    /// Mirrors `_normalize_manifest_entry`.
    fn normalize_manifest_entry(&self, session_key: &str, entry: &Value) -> Option<Value> {
        let obj = entry.as_object()?;
        let segment_id = obj.get("id").and_then(Value::as_str)?;
        if !TRANSCRIPT_SEGMENT_RE.is_match(&format!("{segment_id}.jsonl")) {
            return None;
        }
        let segment_path = self.segment_file_path(session_key, segment_id);
        let bytes = non_negative_int(obj.get("bytes"))?;
        let turn_count = non_negative_int(obj.get("turn_count"))?;
        let user_count = non_negative_int(obj.get("user_count"))?;
        let Ok(metadata) = segment_path.metadata() else {
            return None;
        };
        if !segment_path.is_file() || metadata.len() != bytes {
            return None;
        }
        Some(serde_json::json!({
            "id": segment_id,
            "bytes": bytes,
            "turn_count": turn_count,
            "user_count": user_count,
        }))
    }

    fn write_transcript_lines(&self, session_key: &str, rows: Vec<Value>) -> std::io::Result<()> {
        self.delete_webui_transcript(session_key);
        let path = self.webui_transcript_path(session_key);
        self.write_records_to_path(&path, &rows)?;
        self.rotate_active_transcript_if_needed(session_key)?;
        Ok(())
    }

    /// Unlink the active transcript and its segments directory. Mirrors
    /// `delete_webui_transcript` without the legacy thread-path branch.
    fn delete_webui_transcript(&self, session_key: &str) -> bool {
        let mut removed = false;
        let path = self.webui_transcript_path(session_key);
        if path.is_file() {
            match fs::remove_file(&path) {
                Ok(()) => removed = true,
                Err(e) => log::warn!("webui transcript: failed to delete {}: {e}", path.display()),
            }
        }
        let segments_dir = self.webui_transcript_segments_dir(session_key);
        if segments_dir.is_dir() {
            match fs::remove_dir_all(&segments_dir) {
                Ok(()) => removed = true,
                Err(e) => log::warn!(
                    "webui transcript: failed to delete segments {}: {e}",
                    segments_dir.display()
                ),
            }
        }
        removed
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

/// Mirrors `_non_negative_int`. Rejects bools (Python `bool` is an `int`
/// subclass), non-integers, and negatives.
fn non_negative_int(value: Option<&Value>) -> Option<u64> {
    let value = value?;
    if value.is_boolean() {
        return None;
    }
    value
        .as_u64()
        .or_else(|| value.as_i64().and_then(|n| u64::try_from(n).ok()))
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

/// `os.replace`: overwrite `to` with `from`. Unix `rename` already replaces;
/// Windows fails if the destination exists, so remove it first.
fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    match fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(_) if cfg!(windows) => {
            match fs::remove_file(to) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
            fs::rename(from, to)
        }
        Err(e) => Err(e),
    }
}

fn chat_id_from_session_key(session_key: &str) -> Option<String> {
    let chat_id = session_key.strip_prefix("websocket:")?.trim();
    if chat_id.is_empty() {
        None
    } else {
        Some(chat_id.to_string())
    }
}

/// Whether `row` is a display-worthy assistant transcript row: an
/// `"event": "message"` (or `"assistant"`) row, or one carrying
/// `"role": "assistant"` directly — but not a `tool_hint`/`progress`
/// activity row or a `reasoning_end` row, which have no standalone
/// `attached.history` row of their own (see `transcript_chat_history`,
/// which folds them into the next answer's `activity` /
/// `reasoning_content` instead).
fn is_assistant_transcript_row(row: &Value) -> bool {
    let is_message_event = matches!(
        row.get("event").and_then(Value::as_str),
        Some("message") | Some("assistant")
    );
    let is_assistant_role = row.get("role").and_then(Value::as_str) == Some("assistant");
    if !is_message_event && !is_assistant_role {
        return false;
    }
    !matches!(
        row.get("kind").and_then(Value::as_str),
        Some("tool_hint") | Some("progress")
    )
}

/// Shape durable transcript rows for the WebSocket `attached` envelope's
/// `history` field. Companion to `websocket_chat_history`
/// (`channels/websocket/runtime.rs`): the same cap-from-the-end and
/// "start on a user turn" rules, and the same
/// `{role, content, timestamp?, reasoning_content?}` projection, but reading
/// completed `user`/`message` transcript rows instead of `Session` messages.
/// Used when a chat fork copies the JSONL transcript rather than the session
/// file, since the new session key has no `Session` yet to read history
/// from.
///
/// Unlike nanobot's `replay_transcript_to_ui_messages`, this does not fold
/// `delta`/`reasoning_delta`/`file_edit` events into rich UI messages — those
/// rows are simply skipped, since `attached.history` only ever carries plain
/// `user`/`assistant` text (see `websocket_chat_history`'s doc comment)
/// alongside the `activity` array and `reasoning_content` described below.
///
/// `tool_hint`/`progress` ("activity") rows and `reasoning_end` rows are
/// not emitted as standalone history rows either — there is no `{role,
/// content}` shape for them — but unlike the other skipped event kinds they
/// aren't dropped outright: each is buffered and attached to the next answer
/// row for the same turn (the simplest chronological pairing: "whatever
/// hints/reasoning preceded this answer, describe it"). Activity lands as a
/// plain `{"kind", "text"}` object on the answer's `activity` field;
/// assembled reasoning text lands on `reasoning_content`, matching the
/// session-file projection `websocket_chat_history` already sends. The
/// buffers are discarded, not carried forward, when a `user` row starts a
/// new turn before any answer arrived — an aborted turn's hints/reasoning
/// have no answer of their own to describe and shouldn't leak onto the next
/// turn's. Presentation (chip glyph/status, reasoning panel) is left
/// entirely to the frontend, same as it already is for live
/// `tool_hint`/`progress`/`reasoning_*` messages.
fn transcript_chat_history(rows: &[Value], max_messages: usize) -> Vec<Value> {
    if max_messages == 0 {
        return Vec::new();
    }

    let is_user_row = |row: &Value| {
        row.get("event").and_then(Value::as_str) == Some("user")
            || row.get("role").and_then(Value::as_str) == Some("user")
    };
    let is_activity_row = |row: &Value| {
        matches!(
            row.get("kind").and_then(Value::as_str),
            Some("tool_hint") | Some("progress")
        )
    };
    // `send_reasoning_end` persists `{event: "reasoning_end", text, turn_phase:
    // "reasoning"}` — no `kind` field. Match either stamp so a row is not
    // dropped just because one of them is missing.
    let is_reasoning_row = |row: &Value| {
        row.get("event").and_then(Value::as_str) == Some("reasoning_end")
            || row.get("turn_phase").and_then(Value::as_str) == Some("reasoning")
    };

    let mut pending_activity: Vec<Value> = Vec::new();
    let mut pending_reasoning: Vec<String> = Vec::new();
    let mut visible: Vec<(&Value, Vec<Value>, Vec<String>)> = Vec::new();
    for row in rows {
        if is_user_row(row) {
            pending_activity.clear();
            pending_reasoning.clear();
            visible.push((row, Vec::new(), Vec::new()));
            continue;
        }
        if is_activity_row(row) {
            if let Some(text) = row
                .get("text")
                .and_then(Value::as_str)
                .filter(|t| !t.trim().is_empty())
            {
                let kind = row
                    .get("kind")
                    .and_then(Value::as_str)
                    .unwrap_or("progress");
                pending_activity.push(serde_json::json!({"kind": kind, "text": text}));
            }
            continue;
        }
        if is_reasoning_row(row) {
            if let Some(text) = row
                .get("text")
                .and_then(Value::as_str)
                .filter(|t| !t.trim().is_empty())
            {
                pending_reasoning.push(text.to_string());
            }
            continue;
        }
        if is_assistant_transcript_row(row)
            && !row
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .is_empty()
        {
            visible.push((
                row,
                std::mem::take(&mut pending_activity),
                std::mem::take(&mut pending_reasoning),
            ));
        }
    }

    if visible.len() > max_messages {
        visible = visible.split_off(visible.len() - max_messages);
    }
    // Don't open the transcript on a dangling assistant reply.
    if let Some(start) = visible.iter().position(|(row, _, _)| is_user_row(row)) {
        visible = visible.split_off(start);
    }

    visible
        .into_iter()
        .map(|(row, activity, reasoning_parts)| {
            let role = if is_user_row(row) {
                "user"
            } else {
                "assistant"
            };
            let mut entry = serde_json::json!({
                "role": role,
                "content": row.get("text").and_then(Value::as_str).unwrap_or(""),
            });
            if let Some(timestamp) = row.get("timestamp").and_then(Value::as_str) {
                entry["timestamp"] = serde_json::json!(timestamp);
            }
            // Prefer the folded `reasoning_end` text (how the WebUI transcript
            // actually stores a completed trace). Fall back to a
            // `reasoning_content`/`reasoning` field on the answer row itself,
            // which is the session-file shape `websocket_chat_history` reads.
            let reasoning = if reasoning_parts.is_empty() {
                None
            } else {
                Some(reasoning_parts.join("\n\n"))
            }
            .or_else(|| {
                row.get("reasoning_content")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
            })
            .or_else(|| {
                row.get("reasoning")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
            });
            if let Some(reasoning) = reasoning {
                entry["reasoning_content"] = serde_json::json!(reasoning);
            }
            if !activity.is_empty() {
                entry["activity"] = serde_json::json!(activity);
            }
            // Raw (unresolved) media refs: still the absolute on-disk paths
            // `build_user_transcript_event` recorded under `media_paths`, not
            // yet turned into `/v1/media/...` URLs. Kept as a pure, filesystem-
            // free projection here — path -> URL resolution (which needs to
            // check the file still exists) happens once, afterward, in
            // `channels::websocket::runtime::resolve_history_media`, so this
            // function's own tests stay filesystem-free.
            if is_user_row(row)
                && let Some(media_paths) = row.get("media_paths").and_then(Value::as_array)
                && !media_paths.is_empty()
            {
                entry["media"] = serde_json::json!(media_paths);
            }
            entry
        })
        .collect()
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
        let recorder = WebUiTranscriptRecorder::new(dir.clone());
        let path = recorder.webui_transcript_path("websocket:chat/1");
        assert_eq!(path, dir.join("websocket_chat_1.jsonl"));
    }

    // --- append_to_active_transcript / append_transcript_object ---

    #[test]
    fn append_to_active_transcript_writes_one_terminated_json_line() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = WebUiTranscriptRecorder::new(dir.path().to_path_buf());
        let record = HashMap::from([("event".to_string(), Value::String("user".to_string()))]);
        recorder
            .append_to_active_transcript("websocket:chat-1", &record)
            .unwrap();
        let path = recorder.webui_transcript_path("websocket:chat-1");
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
        let recorder = WebUiTranscriptRecorder::new(dir.path().to_path_buf());
        let huge = "x".repeat(MAX_TRANSCRIPT_FILE_BYTES + 1);
        let record = HashMap::from([("text".to_string(), Value::String(huge))]);
        let err = recorder
            .append_to_active_transcript("websocket:chat-1", &record)
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::Other);
        let path = recorder.webui_transcript_path("websocket:chat-1");
        assert!(!path.exists());
    }

    #[test]
    fn append_transcript_object_stamps_timestamp_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = WebUiTranscriptRecorder::new(dir.path().to_path_buf());
        let obj = HashMap::from([("event".to_string(), Value::String("user".to_string()))]);
        recorder
            .append_transcript_object("websocket:chat-1", obj)
            .unwrap();
        let path = recorder.webui_transcript_path("websocket:chat-1");
        let contents = std::fs::read_to_string(path).unwrap();
        let parsed: Value = serde_json::from_str(contents.trim_end()).unwrap();
        assert!(parsed.get("created_at_ms").is_some());
    }

    #[test]
    fn append_transcript_object_on_turn_end_does_not_error() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = WebUiTranscriptRecorder::new(dir.path().to_path_buf());
        let obj = HashMap::from([("event".to_string(), Value::String("turn_end".to_string()))]);
        // Small files skip rotation; turn_end must still succeed.
        assert!(
            recorder
                .append_transcript_object("websocket:chat-1", obj)
                .is_ok()
        );
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

    // --- append_turn_event / append_stream_event ---

    #[test]
    fn append_turn_event_persists_and_stamps_turn_fields() {
        let dir = tempfile::tempdir().unwrap();
        let mut recorder = WebUiTranscriptRecorder::new(dir.path().to_path_buf());
        let metadata = HashMap::from([(
            WEBUI_TURN_METADATA_KEY.to_string(),
            Value::String("turn-1".to_string()),
        )]);
        let event = HashMap::from([
            ("event".to_string(), Value::String("message".to_string())),
            ("text".to_string(), Value::String("hi there".to_string())),
        ]);

        assert!(recorder.append_turn_event("chat-1", event, &metadata, "answer"));

        let rows = recorder.read_transcript_lines("websocket:chat-1");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["text"], "hi there");
        assert_eq!(rows[0]["turn_id"], "turn-1");
        assert_eq!(rows[0]["turn_phase"], "answer");
        assert_eq!(rows[0]["turn_seq"], 1);
    }

    #[test]
    fn append_stream_event_is_a_noop_without_completed_text() {
        let dir = tempfile::tempdir().unwrap();
        let mut recorder = WebUiTranscriptRecorder::new(dir.path().to_path_buf());
        let metadata = HashMap::new();
        let event = HashMap::from([(
            "event".to_string(),
            Value::String("reasoning_end".to_string()),
        )]);

        assert!(recorder.append_stream_event("chat-1", event, None, &metadata, "reasoning"));
        assert!(!recorder.webui_transcript_path("websocket:chat-1").exists());
    }

    #[test]
    fn append_stream_event_persists_completed_text_and_stamps_turn_fields() {
        let dir = tempfile::tempdir().unwrap();
        let mut recorder = WebUiTranscriptRecorder::new(dir.path().to_path_buf());
        let metadata = HashMap::from([(
            WEBUI_TURN_METADATA_KEY.to_string(),
            Value::String("turn-1".to_string()),
        )]);
        let event = HashMap::from([(
            "event".to_string(),
            Value::String("reasoning_end".to_string()),
        )]);

        assert!(recorder.append_stream_event(
            "chat-1",
            event,
            Some("assembled reasoning"),
            &metadata,
            "reasoning"
        ));

        let rows = recorder.read_transcript_lines("websocket:chat-1");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["text"], "assembled reasoning");
        assert_eq!(rows[0]["turn_id"], "turn-1");
        assert_eq!(rows[0]["turn_phase"], "reasoning");
    }

    // --- append_user_message ---

    #[test]
    fn append_user_message_skips_bare_stop_command() {
        let dir = tempfile::tempdir().unwrap();
        let mut recorder = WebUiTranscriptRecorder::new(dir.path().to_path_buf());
        let metadata = HashMap::new();
        assert!(!recorder.append_user_message("chat-1", "/stop", &metadata, None, None, None));
        assert!(!recorder.webui_transcript_path("websocket:chat-1").exists());
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
        assert!(!recorder.webui_transcript_path("websocket:chat-1").exists());
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

        let path = recorder.webui_transcript_path("websocket:chat-1");
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
        let path = recorder.webui_transcript_path("websocket:chat-1");
        assert!(path.exists());

        recorder.forget_session("chat-1");
        assert!(!path.exists());
    }

    #[test]
    fn forget_session_on_never_written_chat_does_not_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut recorder = WebUiTranscriptRecorder::new(dir.path().to_path_buf());
        recorder.forget_session("never-written");
        assert!(
            !recorder
                .webui_transcript_path("websocket:never-written")
                .exists()
        );
    }

    #[test]
    fn append_after_forget_session_is_a_no_op_and_does_not_recreate_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut recorder = WebUiTranscriptRecorder::new(dir.path().to_path_buf());
        recorder.append_user_message("chat-1", "hello", &HashMap::new(), None, None, None);
        recorder.forget_session("chat-1");
        let path = recorder.webui_transcript_path("websocket:chat-1");
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

        assert!(!recorder.webui_transcript_path("websocket:chat-1").exists());
        assert!(recorder.webui_transcript_path("websocket:chat-2").exists());
        assert!(recorder.append_user_message(
            "chat-2",
            "still alive",
            &HashMap::new(),
            None,
            None,
            None
        ));
    }

    #[test]
    fn clear_transcript_unlinks_the_file_but_later_appends_still_write() {
        let dir = tempfile::tempdir().unwrap();
        let mut recorder = WebUiTranscriptRecorder::new(dir.path().to_path_buf());
        recorder.append_user_message("chat-1", "hello", &HashMap::new(), None, None, None);
        let path = recorder.webui_transcript_path("websocket:chat-1");
        assert!(path.exists());

        recorder.clear_transcript("chat-1");
        assert!(!path.exists());
        assert!(recorder.chat_history("websocket:chat-1", 500).is_empty());

        assert!(recorder.append_user_message(
            "chat-1",
            "after clear",
            &HashMap::new(),
            None,
            None,
            None
        ));
        assert!(path.exists());
        let history = recorder.chat_history("websocket:chat-1", 500);
        assert_eq!(history.len(), 1);
        assert_eq!(history[0]["content"], "after clear");
    }

    // --- read_transcript_file ---

    #[test]
    fn read_transcript_file_parses_object_lines() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = WebUiTranscriptRecorder::new(dir.path().to_path_buf());
        let path = dir.path().join("t.jsonl");
        fs::write(&path, "{\"event\":\"user\"}\n{\"event\":\"assistant\"}\n").unwrap();

        let lines = recorder.read_transcript_file(&path);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["event"], "user");
        assert_eq!(lines[1]["event"], "assistant");
    }

    #[test]
    fn read_transcript_file_skips_blank_bad_json_and_non_objects() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = WebUiTranscriptRecorder::new(dir.path().to_path_buf());
        let path = dir.path().join("t.jsonl");
        fs::write(
            &path,
            "\n{\"ok\":true}\nnot-json\n[1,2]\n123\n  \n{\"keep\":1}\n",
        )
        .unwrap();

        let lines = recorder.read_transcript_file(&path);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["ok"], true);
        assert_eq!(lines[1]["keep"], 1);
    }

    #[test]
    fn read_transcript_file_missing_file_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = WebUiTranscriptRecorder::new(dir.path().to_path_buf());
        let path = dir.path().join("missing.jsonl");
        assert!(recorder.read_transcript_file(&path).is_empty());
    }

    // --- split_transcript_turns ---

    #[test]
    fn split_transcript_turns_empty_input_is_empty() {
        let recorder = WebUiTranscriptRecorder::new(PathBuf::from("/unused"));
        assert!(recorder.split_transcript_turns(Vec::new()).is_empty());
    }

    #[test]
    fn split_transcript_turns_splits_on_turn_end_and_keeps_trailing_incomplete() {
        let recorder = WebUiTranscriptRecorder::new(PathBuf::from("/unused"));
        let rec = |event: &str, text: &str| serde_json::json!({"event": event, "text": text});
        let lines = vec![
            rec("user", "a"),
            rec("turn_end", ""),
            rec("user", "b"),
            rec("assistant", "c"),
            rec("turn_end", ""),
            rec("user", "d"),
        ];

        let turns = recorder.split_transcript_turns(lines);
        assert_eq!(turns.len(), 3);
        assert_eq!(turns[0].len(), 2);
        assert_eq!(turns[0][0]["text"], "a");
        assert_eq!(turns[0][1]["event"], "turn_end");
        assert_eq!(turns[1].len(), 3);
        assert_eq!(turns[1][0]["text"], "b");
        assert_eq!(turns[2], vec![rec("user", "d")]);
    }

    // --- records_bytes ---

    #[test]
    fn records_bytes_empty_is_zero() {
        assert_eq!(WebUiTranscriptRecorder::records_bytes(&[]), 0);
    }

    #[test]
    fn records_bytes_counts_utf8_json_plus_newline_per_record() {
        let records = vec![
            serde_json::json!({"event": "user", "text": "café"}),
            serde_json::json!({"event": "turn_end"}),
        ];
        let expected: usize = records
            .iter()
            .map(|record| serde_json::to_string(record).unwrap().len() + 1)
            .sum();
        assert_eq!(WebUiTranscriptRecorder::records_bytes(&records), expected);
        assert!(expected > records.len() * 2);
    }

    // --- flatten_turns ---

    #[test]
    fn flatten_turns_concatenates_nested_records_in_order() {
        let turns = vec![
            vec![
                serde_json::json!({"event": "user", "text": "a"}),
                serde_json::json!({"event": "turn_end"}),
            ],
            vec![serde_json::json!({"event": "user", "text": "b"})],
        ];
        let flat = WebUiTranscriptRecorder::flatten_turns(&turns);
        assert_eq!(flat.len(), 3);
        assert_eq!(flat[0]["text"], "a");
        assert_eq!(flat[1]["event"], "turn_end");
        assert_eq!(flat[2]["text"], "b");
    }

    #[test]
    fn flatten_turns_empty_is_empty() {
        assert!(WebUiTranscriptRecorder::flatten_turns(&[]).is_empty());
    }

    // --- write_records_to_path ---

    #[test]
    fn write_records_to_path_writes_terminated_json_lines_and_creates_parents() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = WebUiTranscriptRecorder::new(dir.path().to_path_buf());
        let path = dir.path().join("nested").join("t.jsonl");
        let records = vec![
            serde_json::json!({"event": "user"}),
            serde_json::json!({"event": "turn_end"}),
        ];
        recorder.write_records_to_path(&path, &records).unwrap();

        let contents = fs::read_to_string(&path).unwrap();
        assert!(contents.ends_with('\n'));
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(
            serde_json::from_str::<Value>(lines[0]).unwrap()["event"],
            "user"
        );
        assert!(!path.with_file_name("t.jsonl.tmp").exists());
    }

    #[test]
    fn write_records_to_path_overwrites_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = WebUiTranscriptRecorder::new(dir.path().to_path_buf());
        let path = dir.path().join("t.jsonl");
        recorder
            .write_records_to_path(&path, &[serde_json::json!({"n": 1})])
            .unwrap();
        recorder
            .write_records_to_path(&path, &[serde_json::json!({"n": 2})])
            .unwrap();

        let parsed: Value =
            serde_json::from_str(fs::read_to_string(&path).unwrap().trim_end()).unwrap();
        assert_eq!(parsed["n"], 2);
    }

    #[test]
    fn write_records_to_path_rejects_oversized_line_without_replacing() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = WebUiTranscriptRecorder::new(dir.path().to_path_buf());
        let path = dir.path().join("t.jsonl");
        recorder
            .write_records_to_path(&path, &[serde_json::json!({"keep": true})])
            .unwrap();

        let huge = "x".repeat(MAX_TRANSCRIPT_FILE_BYTES + 1);
        let err = recorder
            .write_records_to_path(&path, &[serde_json::json!({"text": huge})])
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::Other);
        let parsed: Value =
            serde_json::from_str(fs::read_to_string(&path).unwrap().trim_end()).unwrap();
        assert_eq!(parsed["keep"], true);
        assert!(!path.with_file_name("t.jsonl.tmp").exists());
    }

    // --- segment_manifest_entry ---

    #[test]
    fn segment_manifest_entry_missing_file_is_zeros() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = WebUiTranscriptRecorder::new(dir.path().to_path_buf());
        let entry = recorder.segment_manifest_entry("websocket:chat-1", "000001");
        assert_eq!(entry["id"], "000001");
        assert_eq!(entry["bytes"], 0);
        assert_eq!(entry["turn_count"], 0);
        assert_eq!(entry["user_count"], 0);
    }

    #[test]
    fn segment_manifest_entry_counts_bytes_turns_and_users() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = WebUiTranscriptRecorder::new(dir.path().to_path_buf());
        let records = vec![
            serde_json::json!({"event": "user", "text": "hi"}),
            serde_json::json!({"event": "assistant", "text": "yo"}),
            serde_json::json!({"event": "turn_end"}),
            serde_json::json!({"role": "user", "text": "again"}),
        ];
        let path = recorder.segment_file_path("websocket:chat-1", "000001");
        recorder.write_records_to_path(&path, &records).unwrap();

        let entry = recorder.segment_manifest_entry("websocket:chat-1", "000001");
        assert_eq!(entry["id"], "000001");
        assert_eq!(entry["bytes"], path.metadata().unwrap().len());
        assert_eq!(entry["turn_count"], 2);
        assert_eq!(entry["user_count"], 2);
    }

    // --- write_segment_manifest ---

    #[test]
    fn write_segment_manifest_writes_pretty_json_and_cleans_tmp() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = WebUiTranscriptRecorder::new(dir.path().to_path_buf());
        let records = vec![serde_json::json!({"event": "user", "text": "hi"})];
        let segment_path = recorder.segment_file_path("websocket:chat-1", "000001");
        recorder
            .write_records_to_path(&segment_path, &records)
            .unwrap();

        recorder
            .write_segment_manifest("websocket:chat-1", &["000001".into()])
            .unwrap();

        let path = recorder.webui_transcript_manifest_path("websocket:chat-1");
        let parsed: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(parsed["version"], TRANSCRIPT_SEGMENT_MANIFEST_VERSION);
        assert_eq!(parsed["segments"][0]["id"], "000001");
        assert_eq!(parsed["segments"][0]["user_count"], 1);
        assert!(!path.with_extension("json.tmp").exists());
    }

    #[test]
    fn write_segment_manifest_overwrites_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = WebUiTranscriptRecorder::new(dir.path().to_path_buf());
        recorder
            .write_segment_manifest("websocket:chat-1", &[])
            .unwrap();
        recorder
            .write_segment_manifest("websocket:chat-1", &["000001".into()])
            .unwrap();

        let path = recorder.webui_transcript_manifest_path("websocket:chat-1");
        let parsed: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(parsed["segments"].as_array().unwrap().len(), 1);
        assert_eq!(parsed["segments"][0]["id"], "000001");
    }

    #[test]
    fn append_segment_turns_writes_one_segment_and_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = WebUiTranscriptRecorder::new(dir.path().to_path_buf());
        let turns = vec![vec![
            serde_json::json!({"event": "user", "text": "hi"}),
            serde_json::json!({"event": "turn_end"}),
        ]];
        recorder
            .append_segment_turns("websocket:chat-1", &turns)
            .unwrap();

        let segment = recorder.segment_file_path("websocket:chat-1", "000001");
        assert!(segment.exists());
        let manifest = recorder.webui_transcript_manifest_path("websocket:chat-1");
        let parsed: Value = serde_json::from_str(&fs::read_to_string(&manifest).unwrap()).unwrap();
        assert_eq!(parsed["segments"][0]["id"], "000001");
        assert_eq!(parsed["segments"][0]["user_count"], 1);
    }

    fn bulky_turn(prefix: &str) -> Vec<Value> {
        let text = format!("{prefix}{}", "x".repeat(3 * 1024 * 1024));
        vec![
            serde_json::json!({"event": "user", "text": text}),
            serde_json::json!({"event": "turn_end"}),
        ]
    }

    #[test]
    fn rotate_active_transcript_if_needed_skips_small_files() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = WebUiTranscriptRecorder::new(dir.path().to_path_buf());
        let path = recorder.webui_transcript_path("websocket:chat-1");
        recorder
            .write_records_to_path(&path, &[serde_json::json!({"event": "user", "text": "hi"})])
            .unwrap();
        recorder
            .rotate_active_transcript_if_needed("websocket:chat-1")
            .unwrap();
        assert!(
            !recorder
                .segment_file_path("websocket:chat-1", "000001")
                .exists()
        );
        let contents = fs::read_to_string(&path).unwrap();
        assert!(contents.contains("hi"));
    }

    #[test]
    fn rotate_active_transcript_if_needed_moves_old_turns_into_a_segment() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = WebUiTranscriptRecorder::new(dir.path().to_path_buf());
        let turns = vec![bulky_turn("a"), bulky_turn("b"), bulky_turn("c")];
        let path = recorder.webui_transcript_path("websocket:chat-1");
        recorder
            .write_records_to_path(&path, &WebUiTranscriptRecorder::flatten_turns(&turns))
            .unwrap();
        assert!(path.metadata().unwrap().len() > MAX_TRANSCRIPT_FILE_BYTES as u64);

        recorder
            .rotate_active_transcript_if_needed("websocket:chat-1")
            .unwrap();

        let active = recorder.read_transcript_file(&path);
        let texts: Vec<&str> = active
            .iter()
            .filter_map(|row| row.get("text").and_then(Value::as_str))
            .collect();
        assert!(texts.iter().any(|t| t.starts_with('c')));
        assert!(!texts.iter().any(|t| t.starts_with('a')));
        assert!(
            recorder
                .segment_file_path("websocket:chat-1", "000001")
                .exists()
        );
    }

    // --- rebuild_segment_manifest ---

    #[test]
    fn rebuild_segment_manifest_unlinks_manifest_when_no_segments() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = WebUiTranscriptRecorder::new(dir.path().to_path_buf());
        recorder
            .write_segment_manifest("websocket:chat-1", &[])
            .unwrap();
        let path = recorder.webui_transcript_manifest_path("websocket:chat-1");
        assert!(path.exists());

        let ids = recorder
            .rebuild_segment_manifest("websocket:chat-1")
            .unwrap();
        assert!(ids.is_empty());
        assert!(!path.exists());
    }

    #[test]
    fn rebuild_segment_manifest_rewrites_from_segment_files() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = WebUiTranscriptRecorder::new(dir.path().to_path_buf());
        let segment = recorder.segment_file_path("websocket:chat-1", "000002");
        recorder
            .write_records_to_path(
                &segment,
                &[serde_json::json!({"event": "user", "text": "hi"})],
            )
            .unwrap();

        let ids = recorder
            .rebuild_segment_manifest("websocket:chat-1")
            .unwrap();
        assert_eq!(ids, vec!["000002".to_string()]);
        let parsed: Value = serde_json::from_str(
            &fs::read_to_string(recorder.webui_transcript_manifest_path("websocket:chat-1"))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(parsed["segments"][0]["id"], "000002");
    }

    #[test]
    fn rebuilt_segment_manifest_entries_maps_ids_to_entries() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = WebUiTranscriptRecorder::new(dir.path().to_path_buf());
        let segment = recorder.segment_file_path("websocket:chat-1", "000001");
        recorder
            .write_records_to_path(
                &segment,
                &[serde_json::json!({"event": "user", "text": "hi"})],
            )
            .unwrap();

        let entries = recorder
            .rebuilt_segment_manifest_entries("websocket:chat-1")
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["id"], "000001");
        assert_eq!(entries[0]["user_count"], 1);
    }

    // --- read_segment_manifest_entries ---

    #[test]
    fn read_segment_manifest_entries_missing_dir_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = WebUiTranscriptRecorder::new(dir.path().to_path_buf());
        assert!(
            recorder
                .read_segment_manifest_entries("websocket:chat-1")
                .is_empty()
        );
    }

    #[test]
    fn read_segment_manifest_entries_rebuilds_when_manifest_missing() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = WebUiTranscriptRecorder::new(dir.path().to_path_buf());
        let segment = recorder.segment_file_path("websocket:chat-1", "000001");
        recorder
            .write_records_to_path(&segment, &[serde_json::json!({"event": "user"})])
            .unwrap();

        let entries = recorder.read_segment_manifest_entries("websocket:chat-1");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["id"], "000001");
        assert!(
            recorder
                .webui_transcript_manifest_path("websocket:chat-1")
                .is_file()
        );
    }

    #[test]
    fn read_segment_manifest_entries_rebuilds_corrupt_or_stale_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = WebUiTranscriptRecorder::new(dir.path().to_path_buf());
        let segment = recorder.segment_file_path("websocket:chat-1", "000001");
        recorder
            .write_records_to_path(&segment, &[serde_json::json!({"event": "user"})])
            .unwrap();
        recorder
            .write_segment_manifest("websocket:chat-1", &["000001".into()])
            .unwrap();
        fs::write(
            recorder.webui_transcript_manifest_path("websocket:chat-1"),
            "{not json",
        )
        .unwrap();

        let entries = recorder.read_segment_manifest_entries("websocket:chat-1");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["id"], "000001");
    }

    // --- normalize_manifest_entry ---

    #[test]
    fn normalize_manifest_entry_accepts_matching_on_disk_segment() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = WebUiTranscriptRecorder::new(dir.path().to_path_buf());
        let path = recorder.segment_file_path("websocket:chat-1", "000001");
        recorder
            .write_records_to_path(&path, &[serde_json::json!({"event": "user"})])
            .unwrap();
        let bytes = path.metadata().unwrap().len();
        let entry = serde_json::json!({
            "id": "000001",
            "bytes": bytes,
            "turn_count": 1,
            "user_count": 1,
        });
        let normalized = recorder
            .normalize_manifest_entry("websocket:chat-1", &entry)
            .unwrap();
        assert_eq!(normalized["id"], "000001");
        assert_eq!(normalized["bytes"], bytes);
        assert_eq!(normalized["turn_count"], 1);
        assert_eq!(normalized["user_count"], 1);
    }

    #[test]
    fn normalize_manifest_entry_rejects_invalid_or_stale_entries() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = WebUiTranscriptRecorder::new(dir.path().to_path_buf());
        assert!(
            recorder
                .normalize_manifest_entry("websocket:chat-1", &serde_json::json!([]))
                .is_none()
        );
        assert!(
            recorder
                .normalize_manifest_entry(
                    "websocket:chat-1",
                    &serde_json::json!({"id": "bad", "bytes": 0, "turn_count": 0, "user_count": 0})
                )
                .is_none()
        );
        assert!(
            recorder
                .normalize_manifest_entry(
                    "websocket:chat-1",
                    &serde_json::json!({
                        "id": "000001",
                        "bytes": 0,
                        "turn_count": 0,
                        "user_count": 0
                    })
                )
                .is_none()
        );
    }

    // --- delete_webui_transcript ---

    #[test]
    fn delete_webui_transcript_missing_paths_returns_false() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = WebUiTranscriptRecorder::new(dir.path().to_path_buf());
        assert!(!recorder.delete_webui_transcript("websocket:chat-1"));
    }

    #[test]
    fn delete_webui_transcript_removes_active_file_and_segments_dir() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = WebUiTranscriptRecorder::new(dir.path().to_path_buf());
        let path = recorder.webui_transcript_path("websocket:chat-1");
        recorder
            .write_records_to_path(&path, &[serde_json::json!({"event": "user"})])
            .unwrap();
        let segment = recorder.segment_file_path("websocket:chat-1", "000001");
        recorder
            .write_records_to_path(&segment, &[serde_json::json!({"event": "user"})])
            .unwrap();
        let segments_dir = recorder.webui_transcript_segments_dir("websocket:chat-1");
        assert!(path.is_file());
        assert!(segments_dir.is_dir());

        assert!(recorder.delete_webui_transcript("websocket:chat-1"));
        assert!(!path.exists());
        assert!(!segments_dir.exists());
    }

    // --- fork_transcript_before_user_index ---

    fn write_two_user_source(recorder: &WebUiTranscriptRecorder, key: &str) {
        recorder
            .write_transcript_lines(
                key,
                vec![
                    serde_json::json!({"event": "user", "text": "one", "chat_id": "src"}),
                    serde_json::json!({"event": "turn_end"}),
                    serde_json::json!({"event": WEBUI_FORK_MARKER_EVENT}),
                    serde_json::json!({"event": "user", "text": "two", "chat_id": "src"}),
                    serde_json::json!({"event": "turn_end"}),
                ],
            )
            .unwrap();
    }

    #[test]
    fn fork_transcript_before_user_index_copies_prefix_and_rewrites_chat_id() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = WebUiTranscriptRecorder::new(dir.path().to_path_buf());
        write_two_user_source(&recorder, "websocket:src");

        assert!(recorder.fork_transcript_before_user_index("websocket:src", "websocket:dst", 1));
        let forked = recorder.read_transcript_lines("websocket:dst");
        let texts: Vec<&str> = forked
            .iter()
            .filter_map(|row| row.get("text").and_then(Value::as_str))
            .collect();
        assert_eq!(texts, vec!["one"]);
        assert!(
            forked
                .iter()
                .all(|row| row.get("chat_id").and_then(Value::as_str) == Some("dst"))
        );
        assert!(
            forked.iter().all(
                |row| row.get("event").and_then(Value::as_str) != Some(WEBUI_FORK_MARKER_EVENT)
            )
        );
    }

    #[test]
    fn fork_transcript_before_user_index_at_user_count_copies_full_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = WebUiTranscriptRecorder::new(dir.path().to_path_buf());
        write_two_user_source(&recorder, "websocket:src");

        assert!(recorder.fork_transcript_before_user_index("websocket:src", "websocket:dst", 2));
        let forked = recorder.read_transcript_lines("websocket:dst");
        let texts: Vec<&str> = forked
            .iter()
            .filter_map(|row| row.get("text").and_then(Value::as_str))
            .collect();
        assert_eq!(texts, vec!["one", "two"]);
    }

    #[test]
    fn fork_transcript_before_user_index_rejects_invalid_index_or_empty_source() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = WebUiTranscriptRecorder::new(dir.path().to_path_buf());
        assert!(!recorder.fork_transcript_before_user_index(
            "websocket:missing",
            "websocket:dst",
            0
        ));
        write_two_user_source(&recorder, "websocket:src");
        assert!(!recorder.fork_transcript_before_user_index("websocket:src", "websocket:dst", 3));
    }

    #[test]
    fn chat_history_reads_the_forked_transcript_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = WebUiTranscriptRecorder::new(dir.path().to_path_buf());
        recorder
            .write_transcript_lines(
                "websocket:src",
                vec![
                    serde_json::json!({"event": "user", "text": "one", "chat_id": "src"}),
                    serde_json::json!({"event": "message", "text": "answer one", "chat_id": "src"}),
                    serde_json::json!({"event": "turn_end"}),
                    serde_json::json!({"event": "user", "text": "two", "chat_id": "src"}),
                    serde_json::json!({"event": "turn_end"}),
                ],
            )
            .unwrap();

        assert!(recorder.fork_transcript_before_user_index("websocket:src", "websocket:dst", 1));

        let history = recorder.chat_history("websocket:dst", 500);
        assert_eq!(history.len(), 2);
        assert_eq!(history[0]["role"], "user");
        assert_eq!(history[0]["content"], "one");
        assert_eq!(history[1]["role"], "assistant");
        assert_eq!(history[1]["content"], "answer one");
    }

    // --- transcript_chat_history ---

    #[test]
    fn transcript_chat_history_empty_input_is_empty() {
        assert!(transcript_chat_history(&[], 500).is_empty());
    }

    #[test]
    fn transcript_chat_history_maps_user_and_message_rows() {
        let rows = vec![
            serde_json::json!({"event": "user", "text": "hi"}),
            serde_json::json!({"event": "message", "text": "hello there"}),
        ];

        let history = transcript_chat_history(&rows, 500);

        assert_eq!(history.len(), 2);
        assert_eq!(history[0]["role"], "user");
        assert_eq!(history[0]["content"], "hi");
        assert_eq!(history[1]["role"], "assistant");
        assert_eq!(history[1]["content"], "hello there");
    }

    #[test]
    fn transcript_chat_history_carries_raw_media_paths_on_user_rows() {
        let rows = vec![
            serde_json::json!({
                "event": "user",
                "text": "look at this",
                "media_paths": ["/data/media/websocket/abc.png"],
            }),
            serde_json::json!({"event": "message", "text": "a cat"}),
        ];

        let history = transcript_chat_history(&rows, 500);

        assert_eq!(history.len(), 2);
        assert_eq!(
            history[0]["media"],
            serde_json::json!(["/data/media/websocket/abc.png"])
        );
        // Assistant rows never carry `media` — it's user-only.
        assert!(history[1].get("media").is_none());
    }

    #[test]
    fn transcript_chat_history_omits_media_when_media_paths_absent_or_empty() {
        let rows = vec![
            serde_json::json!({"event": "user", "text": "hi"}),
            serde_json::json!({"event": "user", "text": "hi again", "media_paths": []}),
        ];

        let history = transcript_chat_history(&rows, 500);

        assert!(history[0].get("media").is_none());
        assert!(history[1].get("media").is_none());
    }

    #[test]
    fn transcript_chat_history_drops_turn_end_and_fork_marker_rows_and_folds_activity() {
        let rows = vec![
            serde_json::json!({"event": "user", "text": "hi"}),
            serde_json::json!({"event": "turn_end"}),
            serde_json::json!({"event": WEBUI_FORK_MARKER_EVENT}),
            serde_json::json!({"event": "message", "text": ""}),
            serde_json::json!({"event": "message", "kind": "tool_hint", "text": "read foo.rs"}),
            serde_json::json!({"event": "message", "kind": "progress", "text": "thinking..."}),
            serde_json::json!({"event": "message", "text": "answer"}),
        ];

        let history = transcript_chat_history(&rows, 500);

        // No standalone rows for turn_end/fork-marker/empty-text/activity rows —
        // just the user row and the answer, with the two activity rows folded
        // onto the answer instead of dropped.
        assert_eq!(history.len(), 2);
        assert_eq!(history[0]["content"], "hi");
        assert_eq!(history[1]["content"], "answer");
        assert_eq!(
            history[1]["activity"],
            serde_json::json!([
                {"kind": "tool_hint", "text": "read foo.rs"},
                {"kind": "progress", "text": "thinking..."},
            ])
        );
        assert!(history[0].get("activity").is_none());
    }

    #[test]
    fn transcript_chat_history_pairs_each_activity_run_with_its_own_chronological_answer() {
        // Two rounds within one turn: activity1 must attach only to answer1,
        // activity2 only to answer2 — not lumped together on either.
        let rows = vec![
            serde_json::json!({"event": "user", "text": "hi"}),
            serde_json::json!({"event": "message", "kind": "tool_hint", "text": "round one hint"}),
            serde_json::json!({"event": "message", "text": "round one answer"}),
            serde_json::json!({"event": "message", "kind": "tool_hint", "text": "round two hint"}),
            serde_json::json!({"event": "message", "text": "round two answer"}),
        ];

        let history = transcript_chat_history(&rows, 500);

        assert_eq!(history.len(), 3);
        assert!(history[0].get("activity").is_none());
        assert_eq!(
            history[1]["activity"],
            serde_json::json!([{"kind": "tool_hint", "text": "round one hint"}])
        );
        assert_eq!(
            history[2]["activity"],
            serde_json::json!([{"kind": "tool_hint", "text": "round two hint"}])
        );
    }

    #[test]
    fn transcript_chat_history_folds_reasoning_end_onto_the_next_answer() {
        // Exact persist shape from `send_reasoning_end`: event + assembled
        // text + turn_phase, no `kind`. Must become `reasoning_content` on
        // the answer, not a standalone history row or an activity chip.
        let rows = vec![
            serde_json::json!({"event": "user", "text": "hi"}),
            serde_json::json!({
                "event": "reasoning_end",
                "text": "assembled reasoning",
                "turn_phase": "reasoning",
            }),
            serde_json::json!({"event": "message", "kind": "tool_hint", "text": "read foo.rs"}),
            serde_json::json!({"event": "message", "text": "answer"}),
        ];

        let history = transcript_chat_history(&rows, 500);

        assert_eq!(history.len(), 2);
        assert_eq!(history[1]["content"], "answer");
        assert_eq!(history[1]["reasoning_content"], "assembled reasoning");
        assert_eq!(
            history[1]["activity"],
            serde_json::json!([{"kind": "tool_hint", "text": "read foo.rs"}])
        );
        assert!(history[0].get("reasoning_content").is_none());
    }

    #[test]
    fn transcript_chat_history_reads_on_row_reasoning_when_no_reasoning_end() {
        let rows = vec![
            serde_json::json!({"event": "user", "text": "hi"}),
            serde_json::json!({
                "event": "message",
                "text": "answer",
                "reasoning_content": "think",
            }),
        ];

        let history = transcript_chat_history(&rows, 500);

        assert_eq!(history[1]["reasoning_content"], "think");
    }

    #[test]
    fn transcript_chat_history_pairs_each_reasoning_run_with_its_own_chronological_answer() {
        let rows = vec![
            serde_json::json!({"event": "user", "text": "hi"}),
            serde_json::json!({"event": "reasoning_end", "text": "first thought"}),
            serde_json::json!({"event": "message", "text": "round one"}),
            serde_json::json!({"event": "reasoning_end", "text": "second thought"}),
            serde_json::json!({"event": "message", "text": "round two"}),
        ];

        let history = transcript_chat_history(&rows, 500);

        assert_eq!(history.len(), 3);
        assert_eq!(history[1]["reasoning_content"], "first thought");
        assert_eq!(history[2]["reasoning_content"], "second thought");
    }

    #[test]
    fn transcript_chat_history_discards_orphaned_reasoning_from_an_aborted_turn() {
        let rows = vec![
            serde_json::json!({"event": "user", "text": "first"}),
            serde_json::json!({"event": "reasoning_end", "text": "aborted thought"}),
            serde_json::json!({"event": "user", "text": "second"}),
            serde_json::json!({"event": "message", "text": "second answer"}),
        ];

        let history = transcript_chat_history(&rows, 500);

        assert_eq!(history.len(), 3);
        assert_eq!(history[2]["content"], "second answer");
        assert!(history[2].get("reasoning_content").is_none());
    }

    #[test]
    fn transcript_chat_history_discards_orphaned_activity_from_an_aborted_turn() {
        // An aborted turn's tool hint never gets an answer of its own — it
        // must not leak onto the next turn's unrelated answer.
        let rows = vec![
            serde_json::json!({"event": "user", "text": "first"}),
            serde_json::json!({"event": "message", "kind": "tool_hint", "text": "aborted hint"}),
            serde_json::json!({"event": "user", "text": "second"}),
            serde_json::json!({"event": "message", "text": "second answer"}),
        ];

        let history = transcript_chat_history(&rows, 500);

        assert_eq!(history.len(), 3);
        assert_eq!(history[2]["content"], "second answer");
        assert!(history[2].get("activity").is_none());
    }

    #[test]
    fn transcript_chat_history_caps_from_the_end_and_aligns_to_a_user_turn() {
        let rows = vec![
            serde_json::json!({"event": "user", "text": "old"}),
            serde_json::json!({"event": "message", "text": "old-a"}),
            serde_json::json!({"event": "message", "text": "dangling"}),
            serde_json::json!({"event": "user", "text": "keep"}),
            serde_json::json!({"event": "message", "text": "keep-a"}),
        ];

        let history = transcript_chat_history(&rows, 3);

        let contents: Vec<&str> = history
            .iter()
            .filter_map(|m| m["content"].as_str())
            .collect();
        // Cap of 3 lands on [dangling, keep, keep-a]; align drops dangling.
        assert_eq!(contents, vec!["keep", "keep-a"]);
    }

    #[test]
    fn transcript_chat_history_zero_cap_returns_empty() {
        let rows = vec![serde_json::json!({"event": "user", "text": "hi"})];
        assert!(transcript_chat_history(&rows, 0).is_empty());
    }
}
