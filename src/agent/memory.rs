use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::SystemTime;

use crate::agent::context::{SOUL_FILE, USER_FILE};
use crate::providers::base::{LLMProvider, LLMProviderDyn};
use crate::session::manager::SessionManager;
use crate::utils::gitstore::GitStore;
use crate::utils::helpers::{
    ensure_dir, estimate_message_tokens, estimate_prompt_tokens_chain, strip_think,
};
use crate::utils::prompt_templates::render_template;
use chrono::{DateTime, Local};
use regex::Regex;
use serde_json::json;

const DEFAULT_MAX_HISTORY: usize = 1000;

const RAW_MARKER: &'static str = "[RAW]";
const MEMORY_FILE: &'static str = "MEMORY.md";
const HISTORY_FILE: &'static str = "history.jsonl";
const CURSOR_FILE: &'static str = ".cursor";
const DREAM_CURSOR_FILE: &'static str = ".dream_cursor";
const LEGACY_HISTORY_BACKUP: &'static str = "HISTORY.md.bak";

static LEGACY_ENTRY_START: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\[(\d{4}-\d{2}-\d{2}[^\]]*)\]\s*").unwrap());
static LEGACY_TIMESTAMP: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\[(\d{4}-\d{2}-\d{2} \d{2}:\d{2})\]\s*").unwrap());
static LEGACY_RAW_MESSAGE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\[\d{4}-\d{2}-\d{2}[^\]]*\]\s+[A-Z][A-Z0-9_]*(?:\s+\[tools:\s*[^\]]+\])?:")
        .unwrap()
});

pub struct MemoryStore {
    pub workspace: PathBuf,
    pub max_history_entries: usize,
    pub memory_dir: PathBuf,
    pub memory_file: PathBuf,
    pub history_file: PathBuf,
    pub legacy_history_file: PathBuf,
    pub soul_file: PathBuf,
    pub user_file: PathBuf,
    cursor_file: PathBuf,
    dream_cursor_file: PathBuf,
    git: GitStore,
}

impl MemoryStore {
    /// Migration helper: history is considered upgraded when `history.jsonl` exists and is non-empty.
    fn history_file_already_migrated(path: &Path) -> bool {
        match std::fs::metadata(path) {
            Ok(meta) => meta.len() > 0,
            Err(_) => false,
        }
    }

    pub fn new(workspace: PathBuf, max_history_entries: Option<usize>) -> Self {
        let cloned_workspace = workspace.clone();
        let memory_dir = workspace.join("memory");
        ensure_dir(&memory_dir);
        let memory_file = memory_dir.join(MEMORY_FILE);
        let history_file = memory_dir.join(HISTORY_FILE);
        let legacy_history_file = memory_dir.join("HISTORY.md");
        let soul_file = workspace.join(SOUL_FILE);
        let user_file = workspace.join(USER_FILE);
        let cursor_file = memory_dir.join(CURSOR_FILE);
        let dream_cursor_file = memory_dir.join(DREAM_CURSOR_FILE);
        let git = GitStore::new(workspace, vec![]);
        Self {
            workspace: cloned_workspace,
            max_history_entries: max_history_entries.unwrap_or(DEFAULT_MAX_HISTORY),
            memory_dir,
            memory_file,
            history_file,
            legacy_history_file,
            soul_file,
            user_file,
            cursor_file,
            dream_cursor_file,
            git: git,
        }
    }

    /* Migration related methods */

    /// One-time best-effort upgrade from legacy `HISTORY.md` to `history.jsonl`.
    ///
    /// Mirrors Python `_maybe_migrate_legacy_history`:
    /// - No-op when there is nothing to migrate (no legacy file) or migration already happened
    ///   (`history.jsonl` exists with non-zero size).
    /// - Reads legacy text as UTF-8, replacing invalid bytes (Python's `errors="replace"`).
    /// - On parse + write success, also seeds `cursor.json` and `dream_cursor.json` with the last
    ///   cursor so Dream does not replay the entire archive on first start.
    /// - Always renames the legacy file to a fresh `HISTORY.md.bak[.N]` afterwards so the
    ///   migration is not retried.
    /// - All errors after the existence checks are logged and swallowed; this never panics.
    fn maybe_migrate_legacy_history(&self) {
        if !self.legacy_history_file.exists() {
            return;
        }
        if Self::history_file_already_migrated(self.history_file.as_path()) {
            return;
        }

        let legacy_bytes = match std::fs::read(&self.legacy_history_file) {
            Ok(bytes) => bytes,
            Err(e) => {
                log::error!(
                    "Failed to read legacy HISTORY.md for migration ({}): {}",
                    self.legacy_history_file.display(),
                    e
                );
                return;
            }
        };
        let legacy_text = String::from_utf8_lossy(&legacy_bytes).into_owned();

        let entries = self.parse_legacy_history(&legacy_text);
        let entry_count = entries.len();

        if let Err(e) = self.run_legacy_migration(entries) {
            log::error!("Failed to migrate legacy HISTORY.md: {}", e);
            return;
        }

        log::info!(
            "Migrated legacy HISTORY.md to history.jsonl ({} entries)",
            entry_count
        );
    }

    /// Parse legacy `HISTORY.md` text into JSONL-style entry objects (`cursor`, `timestamp`, `content`).
    ///
    /// Mirrors Python `_parse_legacy_history`: normalizes newlines, trims, splits chunks, then prefers
    /// a leading `[YYYY-MM-DD HH:MM]` prefix when it matches [`LEGACY_TIMESTAMP`].
    fn parse_legacy_history(&self, text: &str) -> Vec<serde_json::Value> {
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        let normalized = normalized.trim();
        if normalized.is_empty() {
            return Vec::new();
        }

        let fallback_timestamp = self.legacy_fallback_timestamp();
        let chunks = self.split_legacy_history_chunks(normalized);
        let mut entries = Vec::with_capacity(chunks.len());

        for (cursor, chunk) in chunks.iter().enumerate() {
            let cursor = cursor + 1;
            let mut timestamp = fallback_timestamp.clone();
            let mut content = chunk.clone();

            if let Some(caps) = LEGACY_TIMESTAMP.captures(chunk) {
                if let Some(ts) = caps.get(1) {
                    timestamp = ts.as_str().to_string();
                }
                if let Some(full) = caps.get(0) {
                    let remainder = chunk[full.end()..].trim_start();
                    if !remainder.is_empty() {
                        content = remainder.to_string();
                    }
                }
            }

            entries.push(json!({
                "cursor": cursor,
                "timestamp": timestamp,
                "content": content,
            }));
        }

        entries
    }

    /// Return the mtime of `legacy_history_file` formatted as `"YYYY-MM-DD HH:MM"`.
    ///
    /// Falls back to the current local time when the file metadata is
    /// unavailable (mirrors Python's `except OSError` branch).
    fn legacy_fallback_timestamp(&self) -> String {
        let mtime: SystemTime = self
            .legacy_history_file
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or_else(|_| SystemTime::now());

        DateTime::<Local>::from(mtime)
            .format("%Y-%m-%d %H:%M")
            .to_string()
    }

    fn split_legacy_history_chunks(&self, text: &str) -> Vec<String> {
        let mut chunks: Vec<String> = Vec::new();
        let mut current: Vec<&str> = Vec::new();
        let mut saw_blank_separator = false;

        for line in text.split('\n') {
            if saw_blank_separator && !line.trim().is_empty() && !current.is_empty() {
                chunks.push(current.join("\n").trim().to_string());
                current = vec![line];
                saw_blank_separator = false;
                continue;
            }
            if self.should_start_new_legacy_chunk(line, &current) {
                chunks.push(current.join("\n").trim().to_string());
                current = vec![line];
                saw_blank_separator = false;
                continue;
            }
            current.push(line);
            saw_blank_separator = line.trim().is_empty();
        }

        if !current.is_empty() {
            chunks.push(current.join("\n").trim().to_string());
        }

        chunks.into_iter().filter(|c| !c.is_empty()).collect()
    }

    /// Returns true when `line` should begin a new chunk in the legacy history.
    ///
    /// A new chunk starts when:
    /// - `current` is non-empty AND the line looks like a legacy entry header
    ///   (`[YYYY-MM-DD…]`) BUT is *not* a raw message line
    ///   (`[YYYY-MM-DD…] ROLE…:`), because raw messages are continuations.
    fn should_start_new_legacy_chunk(&self, line: &str, current: &[&str]) -> bool {
        if current.is_empty() {
            return false;
        }
        if !LEGACY_ENTRY_START.is_match(line) {
            return false;
        }
        if self.is_raw_legacy_chunk(current) && LEGACY_RAW_MESSAGE.is_match(line) {
            return false;
        }
        true
    }

    fn is_raw_legacy_chunk(&self, lines: &[&str]) -> bool {
        let mut first_nonempty_option: Option<&str> = None;
        for l in lines {
            if !l.trim().is_empty() {
                first_nonempty_option = Some(l);
                break;
            }
        }
        if let Some(first_nonempty) = first_nonempty_option {
            if let Some(matched) = LEGACY_TIMESTAMP.captures(first_nonempty) {
                let end = matched.get(0).unwrap().end();
                let slice = first_nonempty[end..].trim_start();
                return slice.starts_with(RAW_MARKER);
            }
        }
        false
    }

    /// Return a backup path under [`Self::memory_dir`] that does not exist yet.
    ///
    /// Tries `HISTORY.md.bak`, then `HISTORY.md.bak.2`, `HISTORY.md.bak.3`, … mirroring Python's
    /// `_next_legacy_backup_path`.
    fn next_legacy_backup_path(&self) -> PathBuf {
        let mut candidate = self.memory_dir.join(LEGACY_HISTORY_BACKUP);
        let mut suffix = 2u64;
        while candidate.exists() {
            candidate = self
                .memory_dir
                .join(format!("{LEGACY_HISTORY_BACKUP}.{suffix}"));
            suffix += 1;
        }
        candidate
    }

    /// Inner step shared by `maybe_migrate_legacy_history` so a single `?` ladder can short-circuit
    /// any I/O failure without falling through to the success log line.
    fn run_legacy_migration(&self, entries: Vec<serde_json::Value>) -> io::Result<()> {
        if !entries.is_empty() {
            let last_cursor = entries
                .last()
                .and_then(|e| e.get("cursor"))
                .cloned()
                .unwrap_or_else(|| json!(entries.len()));
            let cursor_str = match &last_cursor {
                serde_json::Value::Number(n) => n.to_string(),
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };

            self.write_entries(entries)?;
            std::fs::write(&self.cursor_file, cursor_str.as_bytes())?;
            std::fs::write(&self.dream_cursor_file, cursor_str.as_bytes())?;
        }

        let backup_path = self.next_legacy_backup_path();
        std::fs::rename(&self.legacy_history_file, &backup_path)?;
        Ok(())
    }

    /* End of migration related methods */

    fn read_safe(path: &PathBuf) -> Option<String> {
        if !path.exists() {
            log::debug!("File does not exist: {}", path.display());
            return None;
        }
        match std::fs::read_to_string(path) {
            Ok(content) => Some(content),
            Err(_) => {
                log::error!("Failed to read file: {}", path.display());
                None
            }
        }
    }

    fn write_safe(content: &str, path: &PathBuf) {
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(path)
        {
            let result = file.write_all(content.as_bytes());
            if result.is_err() {
                log::error!(
                    "Failed to write file: {} due to {}",
                    path.display(),
                    result.err().unwrap()
                );
            }
        } else {
            log::error!("Failed to write file: {}", path.display());
        }
    }

    /* MEMORY.md (long-term facts) */

    pub fn read_memory(&self) -> Option<String> {
        MemoryStore::read_safe(&self.memory_file)
    }

    pub fn write_memory(&self, content: &str) {
        MemoryStore::write_safe(content, &self.memory_file);
    }

    /* SOUL.md */

    pub fn read_soul(&self) -> Option<String> {
        MemoryStore::read_safe(&self.soul_file)
    }

    pub fn write_soul(&self, content: &str) {
        MemoryStore::write_safe(content, &self.soul_file);
    }

    /* USER.md (long-term facts) */

    pub fn read_user(&self) -> Option<String> {
        MemoryStore::read_safe(&self.user_file)
    }

    pub fn write_user(&self, content: &str) {
        MemoryStore::write_safe(content, &self.user_file);
    }

    pub fn get_memory_context(&self) -> String {
        let long_term = self.read_memory().unwrap_or(String::new());
        if long_term.is_empty() {
            return String::new();
        }
        return format!("## Long-term memory:\n{}", long_term);
    }

    pub fn append_history(&self, entry: &str) -> u64 {
        let cursor = self.next_cursor();
        let ts = Local::now().format("%Y-%m-%d %H:%M").to_string();
        let mut content = strip_think(entry.trim_end());
        if content.is_empty() {
            content = entry.trim_end().to_string();
        }
        let record = serde_json::json!({"cursor": cursor, "timestamp": ts, "content": content});
        let line = match serde_json::to_string(&record).map(|s| format!("{s}\n")) {
            Ok(line) => line,
            Err(e) => {
                log::error!("Failed to serialize history entry: {}", e);
                return 0;
            }
        };
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.history_file)
        {
            if let Err(e) = file.write_all(line.as_bytes()) {
                log::error!(
                    "Failed to append history file {}: {}",
                    self.history_file.display(),
                    e
                );
                return 0;
            }
        } else {
            log::error!(
                "Failed to open history file: {}",
                self.history_file.display()
            );
            return 0;
        }
        if let Ok(mut f) = File::create(&self.cursor_file) {
            if let Err(e) = write!(f, "{}", cursor) {
                log::error!(
                    "Failed to write cursor file {}: {}",
                    self.cursor_file.display(),
                    e
                );
                return 0;
            }
        } else {
            log::error!("Failed to open cursor file: {}", self.cursor_file.display());
            return 0;
        }
        cursor
    }

    /// Compute the next history cursor counter.
    ///
    /// Mirrors Python `_next_cursor`: prefer [`Self::cursor_file`] if readable as an integer base;
    /// otherwise derive from [`Self::read_last_entry`]'s `"cursor"`; default `1`.
    fn next_cursor(&self) -> u64 {
        if self.cursor_file.exists() {
            match std::fs::read_to_string(&self.cursor_file) {
                Ok(text) => {
                    if let Ok(n) = text.trim().parse::<u64>() {
                        return n.saturating_add(1);
                    }
                }
                Err(_) => {}
            }
        }
        if let Some(last) = self.read_last_entry() {
            if let Some(c) = last.get("cursor") {
                if let Some(n) = Self::parse_entry_cursor(c) {
                    return n.saturating_add(1);
                }
            }
        }
        1
    }

    pub fn read_unprocessed_history(&self, since_cursor: u64) -> Vec<serde_json::Value> {
        self.read_entries()
            .into_iter()
            .filter(|e| {
                e.get("cursor")
                    .and_then(Self::parse_entry_cursor)
                    .unwrap_or(0)
                    > since_cursor
            })
            .collect()
    }

    /// Parse a JSON `cursor` field (supports JSON number or string of digits).
    fn parse_entry_cursor(cursor: &serde_json::Value) -> Option<u64> {
        match cursor {
            serde_json::Value::Number(n) => {
                if let Some(u) = n.as_u64() {
                    return Some(u);
                }
                n.as_f64().map(|f| f as u64)
            }
            serde_json::Value::String(s) => s.trim().parse().ok(),
            _ => None,
        }
    }

    /// Drop oldest entries if the file exceeds *max_history_entries*.
    pub fn compact_history(&self) {
        if self.max_history_entries == 0 {
            return
        }
        let entries = self.read_entries();
        if entries.len() <= self.max_history_entries {
            return;
        }
        let kept = &entries[entries.len() - self.max_history_entries..];
        if let Err(e) = self.write_entries(kept.to_vec()) {
            log::error!("Failed to write history file: {}", e);
        }
    }

    // jsonl helpers

    /// Read all entries from self.history_file as JSONL lines line by line skipping blank lines.
    fn read_entries(&self) -> Vec<serde_json::Value> {
        let file_result = File::open(&self.history_file);
        let mut entries = Vec::new();
        if let Ok(f) = file_result {
            let reader = BufReader::new(f);
            for line_result in reader.lines() {
                if let Ok(line) = line_result {
                    if let Ok(entry) = serde_json::from_str::<serde_json::Value>(&line.trim()) {
                        entries.push(entry);
                    } else {
                        log::error!("Failed to parse JSONL line: {}", line);
                    }
                }
            }
        } else {
            log::error!("Failed to open history file: {}", self.history_file.display());
        }
        entries
    }

    
    /// Read the last entry from [`Self::history_file`] efficiently (tail scan, up to 4 KiB).
    ///
    /// Mirrors Python `_read_last_entry`: opens in binary mode, seeks to end, reads a trailing
    /// window, decodes UTF-8, ignores blank lines, parses the last line as JSON. Returns [`None`]
    /// if the file is missing, empty, has no non-empty lines, or JSON decode fails.
    fn read_last_entry(&self) -> Option<serde_json::Value> {
        const TAIL_BYTES: u64 = 4096;

        let mut file = File::open(&self.history_file).ok()?;
        let size = file.seek(SeekFrom::End(0)).ok()?;
        if size == 0 {
            return None;
        }

        let read_size = std::cmp::min(size, TAIL_BYTES);
        file.seek(SeekFrom::End(-(read_size as i64))).ok()?;

        let mut buf = vec![0u8; read_size as usize];
        file.read_exact(&mut buf).ok()?;

        let data = std::str::from_utf8(&buf).ok()?;
        let lines: Vec<&str> = data.split('\n').filter(|l| !l.trim().is_empty()).collect();
        let last_line = lines.last()?;

        serde_json::from_str(last_line).ok()
    }

    /// Overwrite history.jsonl with the given entries.
    fn write_entries(&self, entries: Vec<serde_json::Value>) -> Result<(), io::Error> {
        let mut file = File::create(&self.history_file)?;
        for entry in entries {
            write!(
                file,
                "{}\n",
                serde_json::to_string(&entry).map_err(io::Error::other)?
            )?;
        }
        Ok(())
    }

    // Dream cursor

    pub fn get_last_dream_cursor(&self) -> u64 {
        if self.dream_cursor_file.exists() {
            match std::fs::read_to_string(&self.dream_cursor_file) {
                Ok(text) => {
                    if let Ok(n) = text.trim().parse::<u64>() {
                        return n;
                    } else {
                        log::error!("Failed to parse dream cursor file: {}", text);
                    }
                }
                Err(_) => {
                    log::error!("Failed to read dream cursor file: {}", self.dream_cursor_file.display());
                }
            }
        }
        return 0;
    }

    pub fn set_last_dream_cursor(&self, cursor: u64) {
        if let Err(e) = std::fs::write(&self.dream_cursor_file, cursor.to_string().as_bytes()) {
            log::error!("Failed to write dream cursor file: {}", e);
        }
    }

    // message formatting utility

    /// Format chat-style message dicts into one line per message.
    ///
    /// Mirrors Python `_format_messages`: skips messages with missing or empty string `content`;
    /// truncates `timestamp` to 16 characters (or uses `"?"`); uppercases `role` (missing →
    /// `"UNKNOWN"`); optional non-empty `tools_used` array becomes ` [tools: a, b]`.
    pub fn format_messages(messages: &[serde_json::Value]) -> String {
        let mut lines = Vec::new();

        for message in messages {
            let Some(content) = message
                .get("content")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
            else {
                continue;
            };

            let tools_suffix = match message.get("tools_used") {
                Some(serde_json::Value::Array(arr)) if !arr.is_empty() => {
                    let joined: Vec<&str> = arr.iter().filter_map(|v| v.as_str()).collect();
                    if joined.is_empty() {
                        String::new()
                    } else {
                        format!(" [tools: {}]", joined.join(", "))
                    }
                }
                _ => String::new(),
            };

            let timestamp = message
                .get("timestamp")
                .and_then(|v| v.as_str())
                .map(|s| {
                    let end = s.len().min(16);
                    &s[..end]
                })
                .unwrap_or("?");

            let role = message
                .get("role")
                .and_then(|v| v.as_str())
                .map(str::to_uppercase)
                .unwrap_or_else(|| "UNKNOWN".into());

            lines.push(format!(
                "[{timestamp}] {role}{tools_suffix}: {content}",
            ));
        }

        lines.join("\n")
    }

    /// Fallback: dump raw messages to history.jsonl without LLM summarization.
    pub fn raw_archive(&self, messages: &[serde_json::Value]) {
        let mut entry = format!("{} {} messages\n", RAW_MARKER, messages.len());
        entry.push_str(&Self::format_messages(messages));
        let cursor = self.append_history(&entry);
        if cursor == 0 {
            log::error!(
                "Memory consolidation degraded: raw-archive failed to persist {} messages (see prior I/O logs)",
                messages.len()
            );
        } else {
            log::warn!(
                "Memory consolidation degraded: raw-archived {} messages",
                messages.len()
            );
        }
    }
}

struct Consolidator {
    store: MemoryStore,
    provider: Arc<dyn LLMProviderDyn>,
    model: String,
    sessions: SessionManager,
    context_window_tokens: usize,
    max_completion_tokens: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn make_store(tmp: &TempDir) -> MemoryStore {
        MemoryStore::new(tmp.path().to_path_buf(), None)
    }

    #[test]
    fn test_legacy_fallback_timestamp_format() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        // File doesn't exist → falls back to now(); result must still match the pattern.
        let ts = store.legacy_fallback_timestamp();
        assert_eq!(
            ts.len(),
            16,
            "expected 'YYYY-MM-DD HH:MM' (16 chars), got: {ts}"
        );
        assert!(
            ts.chars().nth(4) == Some('-'),
            "expected dash at position 4"
        );
        assert!(
            ts.chars().nth(7) == Some('-'),
            "expected dash at position 7"
        );
        assert!(
            ts.chars().nth(10) == Some(' '),
            "expected space at position 10"
        );
        assert!(
            ts.chars().nth(13) == Some(':'),
            "expected colon at position 13"
        );
    }

    #[test]
    fn test_legacy_fallback_timestamp_uses_file_mtime() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);

        // Create the legacy file so metadata() succeeds
        fs::create_dir_all(&store.memory_dir).unwrap();
        fs::write(&store.legacy_history_file, b"test").unwrap();

        let ts = store.legacy_fallback_timestamp();
        // The timestamp should still match the format
        assert_eq!(ts.len(), 16);

        // The year should be plausible (>= 2024)
        let year: u32 = ts[..4].parse().unwrap();
        assert!(year >= 2024, "unexpected year: {year}");
    }

    #[test]
    fn test_legacy_fallback_timestamp_missing_file_returns_now() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        // legacy_history_file doesn't exist → falls back to SystemTime::now()
        assert!(!store.legacy_history_file.exists());
        let ts = store.legacy_fallback_timestamp();
        let year: u32 = ts[..4].parse().unwrap();
        assert!(year >= 2024);
    }

    #[test]
    fn chunk_start_false_when_current_empty() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        assert!(!store.should_start_new_legacy_chunk("[2024-01-01] Some text", &[]));
    }

    #[test]
    fn chunk_start_true_for_entry_header() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        // Plain timestamped entry header → new chunk
        assert!(
            store.should_start_new_legacy_chunk("[2024-01-01 12:00] Summary line", &["previous"])
        );
    }

    #[test]
    fn chunk_start_false_for_raw_message() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        // Raw message lines (ROLE:) are continuations, not new chunks
        assert!(!store.should_start_new_legacy_chunk(
            "[2024-01-01 12:00] USER: hello",
            &["[2024-01-01 12:00] [RAW] previous"]
        ));
    }

    #[test]
    fn chunk_start_false_for_plain_text() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        assert!(!store.should_start_new_legacy_chunk("Just a plain line", &["previous"]));
    }

    #[test]
    fn empty_string_returns_no_chunks() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        assert_eq!(store.split_legacy_history_chunks(""), Vec::<String>::new());
    }

    #[test]
    fn whitespace_only_returns_no_chunks() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        assert_eq!(
            store.split_legacy_history_chunks("   \n   \n"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn single_entry_returns_one_chunk() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        let text = "[2024-01-01 10:00] Something happened";
        assert_eq!(
            store.split_legacy_history_chunks(text),
            vec!["[2024-01-01 10:00] Something happened"]
        );
    }

    #[test]
    fn two_entries_split_into_two_chunks() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        let text = "[2024-01-01 10:00] First entry\n[2024-01-02 11:00] Second entry";
        let chunks = store.split_legacy_history_chunks(text);
        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].starts_with("[2024-01-01"));
        assert!(chunks[1].starts_with("[2024-01-02"));
    }

    #[test]
    fn blank_line_separator_also_splits() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        // A blank line followed by non-blank text should start a new chunk
        let text = "first chunk line\n\nsecond chunk line";
        let chunks = store.split_legacy_history_chunks(text);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0], "first chunk line");
        assert_eq!(chunks[1], "second chunk line");
    }

    #[test]
    fn trailing_whitespace_stripped_from_chunks() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        let text = "[2024-01-01 10:00] Entry   \n   \n";
        let chunks = store.split_legacy_history_chunks(text);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], "[2024-01-01 10:00] Entry");
    }

    #[test]
    fn raw_message_lines_stay_in_same_chunk() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        // RAW_MESSAGE lines look like entry starts but must NOT split
        let text = "[2024-01-01 10:00] [RAW] Summary\n[2024-01-01 10:01] USER: hello\n[2024-01-01 10:02] AGENT: hi";
        let chunks = store.split_legacy_history_chunks(text);
        println!("chunks: {:?}", chunks);
        // First chunk splits at [2024-01-01 10:00], then the USER/AGENT lines
        // are raw messages so they stay attached to the previous chunk
        assert_eq!(chunks.len(), 1);
    }

    #[test]
    fn multi_line_entry_preserved() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        let text =
            "[2024-01-01 10:00] First entry\ncontinuation line\n[2024-01-02 11:00] Second entry";
        let chunks = store.split_legacy_history_chunks(text);
        assert_eq!(chunks.len(), 2);
        assert_eq!(
            chunks[0],
            "[2024-01-01 10:00] First entry\ncontinuation line"
        );
    }

    #[test]
    fn empty_chunks_filtered_out() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        // Multiple blank lines should not produce empty chunks
        let text = "[2024-01-01 10:00] A\n\n\n[2024-01-02 11:00] B";
        let chunks = store.split_legacy_history_chunks(text);
        assert_eq!(chunks.len(), 2);
    }

    // ── next_legacy_backup_path ─────────────────────────────────────────────────

    #[test]
    fn next_legacy_backup_path_prefers_plain_bak_when_missing() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        let p = store.next_legacy_backup_path();
        assert_eq!(p, store.memory_dir.join("HISTORY.md.bak"));
        assert!(!p.exists());
    }

    #[test]
    fn next_legacy_backup_path_advances_when_bak_exists() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        let first = store.memory_dir.join("HISTORY.md.bak");
        fs::write(&first, b"x").unwrap();

        let p = store.next_legacy_backup_path();
        assert_eq!(p, store.memory_dir.join("HISTORY.md.bak.2"));
        assert!(!p.exists());
    }

    #[test]
    fn next_legacy_backup_path_skips_chain_of_existing_files() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        fs::write(store.memory_dir.join("HISTORY.md.bak"), b"a").unwrap();
        fs::write(store.memory_dir.join("HISTORY.md.bak.2"), b"b").unwrap();
        fs::write(store.memory_dir.join("HISTORY.md.bak.3"), b"c").unwrap();

        assert_eq!(
            store.next_legacy_backup_path(),
            store.memory_dir.join("HISTORY.md.bak.4")
        );
    }

    // ── parse_legacy_history ───────────────────────────────────────────────────

    #[test]
    fn parse_legacy_history_empty_after_trim_returns_no_entries() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        assert!(store.parse_legacy_history("").is_empty());
        assert!(store.parse_legacy_history("   \t\n  ").is_empty());
    }

    #[test]
    fn parse_legacy_history_normalizes_crlf_and_cr() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        let text = "[2024-03-10 09:00]  hello\r\nworld";
        let entries = store.parse_legacy_history(text);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["cursor"], 1);
        assert_eq!(entries[0]["timestamp"], "2024-03-10 09:00");
        assert_eq!(entries[0]["content"], "hello\nworld");
    }

    #[test]
    fn parse_legacy_history_extracts_timestamp_prefix_and_content() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        let text = "[2024-05-01 14:30]    Body line one\nBody line two";
        let entries = store.parse_legacy_history(text);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["timestamp"], "2024-05-01 14:30");
        assert_eq!(entries[0]["content"], "Body line one\nBody line two");
    }

    #[test]
    fn parse_legacy_history_timestamp_only_keeps_full_chunk_as_content() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        let chunk = "[2024-06-15 00:00]";
        let entries = store.parse_legacy_history(chunk);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["timestamp"], "2024-06-15 00:00");
        assert_eq!(entries[0]["content"], chunk);
    }

    #[test]
    fn parse_legacy_history_no_leading_timestamp_uses_fallback_and_full_chunk() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        let text = "Plain archive line\nSecond line";
        let entries = store.parse_legacy_history(text);

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["cursor"], 1);
        assert_eq!(
            entries[0]["content"],
            serde_json::Value::String(text.to_string())
        );

        let ts = entries[0]["timestamp"].as_str().unwrap();
        assert_eq!(ts.len(), 16);
        assert_eq!(&ts[4..5], "-");
        assert_eq!(&ts[10..11], " ");
    }

    #[test]
    fn parse_legacy_history_multiple_chunks_1_based_cursor() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        let text = concat!("[2024-01-01 10:00] First\n\n", "[2024-01-02 11:00] Second");
        let entries = store.parse_legacy_history(text);

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["cursor"], 1);
        assert_eq!(entries[0]["timestamp"], "2024-01-01 10:00");
        assert_eq!(entries[0]["content"], "First");

        assert_eq!(entries[1]["cursor"], 2);
        assert_eq!(entries[1]["timestamp"], "2024-01-02 11:00");
        assert_eq!(entries[1]["content"], "Second");
    }

    #[test]
    fn parse_legacy_history_bracket_line_without_space_in_timestamp_not_matched() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        // LEGACY_TIMESTAMP requires "YYYY-MM-DD HH:MM" inside brackets
        let text = "[2024-01-01] no time component";
        let entries = store.parse_legacy_history(text);

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["content"], text);
        let ts = entries[0]["timestamp"].as_str().unwrap();
        assert_eq!(ts.len(), 16);
    }

    // ── maybe_migrate_legacy_history ──────────────────────────────────────────

    fn read_jsonl_lines(path: &PathBuf) -> Vec<serde_json::Value> {
        let raw = fs::read_to_string(path).unwrap();
        raw.lines()
            .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
            .collect()
    }

    #[test]
    fn migrate_no_legacy_file_is_noop() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        assert!(!store.legacy_history_file.exists());

        store.maybe_migrate_legacy_history();

        assert!(!store.history_file.exists());
        assert!(!store.cursor_file.exists());
        assert!(!store.dream_cursor_file.exists());
    }

    #[test]
    fn migrate_skipped_when_history_jsonl_already_non_empty() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        fs::write(&store.legacy_history_file, b"[2024-01-01 10:00] Old entry").unwrap();
        fs::write(&store.history_file, b"{\"existing\": true}\n").unwrap();

        store.maybe_migrate_legacy_history();

        assert!(store.legacy_history_file.exists(), "legacy file untouched");
        assert!(!store.cursor_file.exists());
        assert!(!store.dream_cursor_file.exists());
        let raw = fs::read_to_string(&store.history_file).unwrap();
        assert_eq!(raw, "{\"existing\": true}\n");
    }

    #[test]
    fn migrate_treats_empty_history_jsonl_as_unmigrated() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        fs::write(&store.legacy_history_file, b"[2024-01-01 10:00] Hello").unwrap();
        fs::write(&store.history_file, b"").unwrap();

        store.maybe_migrate_legacy_history();

        let entries = read_jsonl_lines(&store.history_file);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["timestamp"], "2024-01-01 10:00");
        assert!(!store.legacy_history_file.exists());
    }

    #[test]
    fn migrate_writes_entries_cursor_and_renames_legacy_file() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        let legacy = "[2024-01-01 10:00] First entry\n\n[2024-01-02 11:00] Second entry";
        fs::write(&store.legacy_history_file, legacy).unwrap();

        store.maybe_migrate_legacy_history();

        let entries = read_jsonl_lines(&store.history_file);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["cursor"], 1);
        assert_eq!(entries[1]["cursor"], 2);

        assert_eq!(fs::read_to_string(&store.cursor_file).unwrap(), "2");
        assert_eq!(fs::read_to_string(&store.dream_cursor_file).unwrap(), "2");

        assert!(!store.legacy_history_file.exists());
        let backup = store.memory_dir.join("HISTORY.md.bak");
        assert!(backup.exists());
        assert_eq!(fs::read_to_string(&backup).unwrap(), legacy);
    }

    #[test]
    fn migrate_picks_next_backup_slot_when_default_taken() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        fs::write(&store.legacy_history_file, b"[2024-05-01 09:00] payload").unwrap();
        fs::write(store.memory_dir.join("HISTORY.md.bak"), b"older").unwrap();

        store.maybe_migrate_legacy_history();

        let bak2 = store.memory_dir.join("HISTORY.md.bak.2");
        assert!(bak2.exists(), "should fall through to .bak.2");
        assert_eq!(
            fs::read_to_string(&store.memory_dir.join("HISTORY.md.bak")).unwrap(),
            "older",
            "previous backup must be preserved"
        );
        assert!(!store.legacy_history_file.exists());
    }

    #[test]
    fn migrate_empty_legacy_file_skips_writes_but_renames() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        fs::write(&store.legacy_history_file, b"   \n   \n").unwrap();

        store.maybe_migrate_legacy_history();

        assert!(
            !store.history_file.exists()
                || fs::read_to_string(&store.history_file).unwrap().is_empty()
        );
        assert!(!store.cursor_file.exists());
        assert!(!store.dream_cursor_file.exists());

        assert!(
            !store.legacy_history_file.exists(),
            "legacy file should be renamed"
        );
        assert!(store.memory_dir.join("HISTORY.md.bak").exists());
    }

    #[test]
    fn migrate_preserves_invalid_utf8_via_lossy_read() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        // 0xFF is invalid UTF-8; from_utf8_lossy must replace it instead of failing.
        let mut bytes = b"[2024-04-01 12:00] before \xff after".to_vec();
        bytes.extend_from_slice(b"\n");
        fs::write(&store.legacy_history_file, &bytes).unwrap();

        store.maybe_migrate_legacy_history();

        let entries = read_jsonl_lines(&store.history_file);
        assert_eq!(entries.len(), 1);
        let content = entries[0]["content"].as_str().unwrap();
        assert!(content.starts_with("before"));
        assert!(content.contains("after"));
    }

    // ── read_soul / write_soul ──────────────────────────────────────────────────

    #[test]
    fn read_soul_returns_none_when_missing() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        assert!(!store.soul_file.exists());
        assert_eq!(store.read_soul(), None);
    }

    #[test]
    fn write_soul_truncates_and_read_soul_round_trip() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);

        fs::write(&store.soul_file, "longer old content").unwrap();
        store.write_soul("short");
        assert_eq!(store.read_soul().as_deref(), Some("short"));

        store.write_soul("café ☕\nline2");
        assert_eq!(store.read_soul().unwrap(), "café ☕\nline2");
    }

    // ── read_last_entry ─────────────────────────────────────────────────────────

    #[test]
    fn read_last_entry_missing_file() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        assert!(!store.history_file.exists());
        assert_eq!(store.read_last_entry(), None);
    }

    #[test]
    fn read_last_entry_empty_file() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        fs::write(&store.history_file, b"").unwrap();
        assert_eq!(store.read_last_entry(), None);
    }

    #[test]
    fn read_last_entry_single_line() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        let row = json!({"cursor": 7, "content": "x"});
        fs::write(
            &store.history_file,
            format!("{}\n", serde_json::to_string(&row).unwrap()),
        )
        .unwrap();

        assert_eq!(store.read_last_entry().unwrap(), row);
    }

    #[test]
    fn read_last_entry_picks_last_non_empty_line() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        let a = json!({"i": 1});
        let b = json!({"i": 2, "ok": true});
        let mut body = String::new();
        body.push_str(&format!("{}\n", serde_json::to_string(&a).unwrap()));
        body.push_str("\n  \n");
        body.push_str(&format!("{}\n", serde_json::to_string(&b).unwrap()));
        fs::write(&store.history_file, body).unwrap();

        assert_eq!(store.read_last_entry().unwrap(), b);
    }

    #[test]
    fn read_last_entry_invalid_json_returns_none() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        fs::write(&store.history_file, b"{\"a\": 1}\nnot json\n").unwrap();
        assert_eq!(store.read_last_entry(), None);
    }

    #[test]
    fn read_last_entry_only_blank_lines_returns_none() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        fs::write(&store.history_file, b"  \n\n\t\n").unwrap();
        assert_eq!(store.read_last_entry(), None);
    }

    #[test]
    fn read_last_entry_works_with_tail_larger_than_window() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        let mut body = String::new();
        for i in 0..300 {
            body.push_str(&format!("{{\"cursor\": {i}}}\n"));
        }
        let last = json!({"final": true, "marker": "z"});
        body.push_str(&format!("{}\n", serde_json::to_string(&last).unwrap()));
        assert!(body.len() > 4096, "test setup: tail must exceed 4 KiB");
        fs::write(&store.history_file, body).unwrap();

        assert_eq!(store.read_last_entry().unwrap(), last);
    }

    // ── read_entries ──────────────────────────────────────────────────────────────

    #[test]
    fn read_entries_missing_file_returns_empty() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        assert!(!store.history_file.exists());
        assert!(store.read_entries().is_empty());
    }

    #[test]
    fn read_entries_empty_file_returns_empty() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        fs::write(&store.history_file, b"").unwrap();
        assert!(store.read_entries().is_empty());
    }

    #[test]
    fn read_entries_reads_multiple_lines_in_order() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        let a = json!({"cursor": 1});
        let b = json!({"cursor": 2, "note": "b"});
        let c = json!({"marker": null});
        let mut body = String::new();
        body.push_str(&format!("{}\n", serde_json::to_string(&a).unwrap()));
        body.push_str(&format!("{}\n", serde_json::to_string(&b).unwrap()));
        body.push_str(&format!("{}\n", serde_json::to_string(&c).unwrap()));
        fs::write(&store.history_file, body).unwrap();

        assert_eq!(store.read_entries(), vec![a, b, c]);
    }

    #[test]
    fn read_entries_skips_invalid_and_blank_lines() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        let good = json!({"ok": true});
        let mut body = String::from("totally not json\n");
        body.push_str("\n");
        body.push_str("   \t  \n");
        body.push_str(&format!("{}\n", serde_json::to_string(&good).unwrap()));
        fs::write(&store.history_file, body).unwrap();

        assert_eq!(store.read_entries(), vec![good]);
    }

    #[test]
    fn read_entries_accepts_trimmed_outer_whitespace() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        let row = json!({"cursor": 42});
        let serialized = serde_json::to_string(&row).unwrap();
        fs::write(
            &store.history_file,
            format!("   {serialized}   \n"),
        )
        .unwrap();

        assert_eq!(store.read_entries(), vec![row]);
    }

    #[test]
    fn read_entries_parses_crlf_encoded_lines() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        let row = json!({"x": "payload"});
        let line = serde_json::to_string(&row).unwrap();
        fs::write(&store.history_file, format!("{line}\r\n")).unwrap();

        assert_eq!(store.read_entries(), vec![row]);
    }

    // ── read_unprocessed_history ─────────────────────────────────────────────────

    #[test]
    fn read_unprocessed_history_missing_file_returns_empty() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        if store.history_file.exists() {
            fs::remove_file(&store.history_file).unwrap();
        }
        assert!(!store.history_file.exists());
        assert!(store.read_unprocessed_history(0).is_empty());
        assert!(store.read_unprocessed_history(u64::MAX).is_empty());
    }

    #[test]
    fn read_unprocessed_history_keeps_strictly_greater_than_since() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        let e1 = json!({"cursor": 1, "content": "a"});
        let e2 = json!({"cursor": 2, "content": "b"});
        let e3 = json!({"cursor": 3, "content": "c"});
        let mut body = String::new();
        body.push_str(&format!("{}\n", serde_json::to_string(&e1).unwrap()));
        body.push_str(&format!("{}\n", serde_json::to_string(&e2).unwrap()));
        body.push_str(&format!("{}\n", serde_json::to_string(&e3).unwrap()));
        fs::write(&store.history_file, body).unwrap();

        assert!(store.read_unprocessed_history(3).is_empty());
        assert_eq!(store.read_unprocessed_history(2), vec![e3.clone()]);
        assert_eq!(store.read_unprocessed_history(1), vec![e2.clone(), e3.clone()]);
        assert_eq!(store.read_unprocessed_history(0), vec![e1, e2, e3]);
    }

    #[test]
    fn read_unprocessed_history_excludes_cursor_equal_to_since() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        let at = json!({"cursor": 5, "k": "at"});
        let after = json!({"cursor": 6, "k": "after"});
        fs::write(
            &store.history_file,
            format!(
                "{}\n{}\n",
                serde_json::to_string(&at).unwrap(),
                serde_json::to_string(&after).unwrap(),
            ),
        )
        .unwrap();

        assert_eq!(store.read_unprocessed_history(5), vec![after]);
    }

    #[test]
    fn read_unprocessed_history_parses_string_cursor_like_numeric() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        let legacy = serde_json::from_str::<serde_json::Value>(r#"{"cursor": "14", "note": "legacy"}"#).unwrap();
        fs::write(
            &store.history_file,
            format!("{}\n", serde_json::to_string(&legacy).unwrap()),
        )
        .unwrap();

        assert!(store.read_unprocessed_history(14).is_empty());
        assert_eq!(store.read_unprocessed_history(13), vec![legacy]);
    }

    #[test]
    fn read_unprocessed_history_drops_rows_without_usable_cursor() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        let no_cursor = json!({"content": "no id"});
        let with = json!({"cursor": 2});
        fs::write(
            &store.history_file,
            format!(
                "{}\n{}\n",
                serde_json::to_string(&no_cursor).unwrap(),
                serde_json::to_string(&with).unwrap(),
            ),
        )
        .unwrap();

        let out = store.read_unprocessed_history(0);
        assert_eq!(out, vec![with]);
    }

    // ── next_cursor ─────────────────────────────────────────────────────────────

    fn cursor_path(store: &MemoryStore) -> PathBuf {
        store.memory_dir.join(CURSOR_FILE)
    }

    #[test]
    fn next_cursor_defaults_to_one() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        assert_eq!(store.next_cursor(), 1);
    }

    #[test]
    fn next_cursor_reads_cursor_file_plus_one() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        fs::write(cursor_path(&store), b"12\n").unwrap();
        assert_eq!(store.next_cursor(), 13);
    }

    #[test]
    fn next_cursor_trims_cursor_file() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        fs::write(cursor_path(&store), "  40 \r\n").unwrap();
        assert_eq!(store.next_cursor(), 41);
    }

    #[test]
    fn next_cursor_invalid_cursor_file_falls_back_to_jsonl() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        fs::write(cursor_path(&store), b"not an int").unwrap();
        let row = json!({"cursor": 99, "content": "x"});
        fs::write(
            &store.history_file,
            format!("{}\n", serde_json::to_string(&row).unwrap()),
        )
        .unwrap();
        assert_eq!(store.next_cursor(), 100);
    }

    #[test]
    fn next_cursor_from_jsonl_when_no_cursor_file() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        let row = json!({"cursor": 6, "timestamp": "2024-01-01 00:00"});
        fs::write(
            &store.history_file,
            format!("{}\n", serde_json::to_string(&row).unwrap()),
        )
        .unwrap();
        assert!(!cursor_path(&store).exists());
        assert_eq!(store.next_cursor(), 7);
    }

    #[test]
    fn next_cursor_string_cursor_in_jsonl_entry() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        let line = r#"{"cursor": "15", "note": "legacy"}"#;
        fs::write(&store.history_file, format!("{line}\n")).unwrap();
        assert_eq!(store.next_cursor(), 16);
    }

    // ── get_last_dream_cursor ─────────────────────────────────────────────────────

    fn dream_cursor_path(store: &MemoryStore) -> PathBuf {
        store.memory_dir.join(DREAM_CURSOR_FILE)
    }

    #[test]
    fn get_last_dream_cursor_returns_zero_when_file_missing() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        assert!(!dream_cursor_path(&store).exists());
        assert_eq!(store.get_last_dream_cursor(), 0);
    }

    #[test]
    fn get_last_dream_cursor_reads_unsigned_integer() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        store.set_last_dream_cursor(9042);
        assert_eq!(store.get_last_dream_cursor(), 9042);
    }

    #[test]
    fn get_last_dream_cursor_trims_whitespace() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        fs::write(&store.dream_cursor_file, "  18 \r\n").unwrap();
        assert_eq!(store.get_last_dream_cursor(), 18);
    }

    #[test]
    fn get_last_dream_cursor_invalid_or_empty_returns_zero() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        fs::write(&store.dream_cursor_file, b"not-a-number").unwrap();
        assert_eq!(store.get_last_dream_cursor(), 0);

        fs::write(&store.dream_cursor_file, b"   ").unwrap();
        assert_eq!(store.get_last_dream_cursor(), 0);
    }

    // ── format_messages ───────────────────────────────────────────────────────────

    #[test]
    fn format_messages_skips_missing_or_empty_content() {
        let messages = vec![
            json!({"role": "user", "timestamp": "2026-01-01 10:00", "content": ""}),
            json!({"role": "user", "timestamp": "2026-01-01 10:01"}),
            json!({"role": "assistant", "content": "ok", "timestamp": "2026-01-01 10:02"}),
        ];
        let out = MemoryStore::format_messages(&messages);
        assert_eq!(
            out,
            "[2026-01-01 10:02] ASSISTANT: ok"
        );
    }

    #[test]
    fn format_messages_joins_lines_and_tools_used() {
        let messages = vec![
            json!({
                "content": "Hello, world!",
                "timestamp": "2026-03-25 10:00:00",
                "role": "user",
                "tools_used": serde_json::Value::Array(vec![]),
            }),
            json!({
                "content": "Done.",
                "timestamp": "2026-03-25 10:00:01",
                "role": "assistant",
                "tools_used": ["grep", "read_file"],
            }),
        ];
        let out = MemoryStore::format_messages(&messages);
        assert_eq!(
            out,
            concat!(
                "[2026-03-25 10:00] USER: Hello, world!\n",
                "[2026-03-25 10:00] ASSISTANT [tools: grep, read_file]: Done.",
            )
        );
    }

    #[test]
    fn format_messages_truncates_timestamp_and_unknown_role() {
        let messages = vec![
            json!({
                "content": "x",
                "timestamp": "12345678901234567890",
                "role": "tool",
            }),
            json!({"content": "no role", "timestamp": "short"}),
        ];
        let out = MemoryStore::format_messages(&messages);
        assert_eq!(
            out,
            "[1234567890123456] TOOL: x\n[short] UNKNOWN: no role"
        );
    }

    // ── append_history ────────────────────────────────────────────────────────────

    #[test]
    fn append_history_first_entry_writes_jsonl_cursor_and_returns_one() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);

        let c = store.append_history("hello world");
        assert_eq!(c, 1);

        let entries = read_jsonl_lines(&store.history_file);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["cursor"], 1);
        assert_eq!(entries[0]["content"], "hello world");
        let ts = entries[0]["timestamp"].as_str().unwrap();
        assert_eq!(ts.len(), 16);

        assert_eq!(fs::read_to_string(cursor_path(&store)).unwrap(), "1");
    }

    #[test]
    fn append_history_twice_increments_cursor_and_appends_newline_records() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);

        assert_eq!(store.append_history("first"), 1);
        assert_eq!(store.append_history("second"), 2);

        let entries = read_jsonl_lines(&store.history_file);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["cursor"], 1);
        assert_eq!(entries[0]["content"], "first");
        assert_eq!(entries[1]["cursor"], 2);
        assert_eq!(entries[1]["content"], "second");

        let raw = fs::read_to_string(&store.history_file).unwrap();
        assert!(raw.ends_with('\n'));
        assert_eq!(raw.lines().count(), 2);

        assert_eq!(fs::read_to_string(cursor_path(&store)).unwrap(), "2");
    }

    #[test]
    fn append_history_continues_from_existing_jsonl_without_cursor_file() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        let row = json!({"cursor": 10, "timestamp": "2026-05-07 09:00", "content": "seed"});
        fs::write(
            &store.history_file,
            format!("{}\n", serde_json::to_string(&row).unwrap()),
        )
        .unwrap();
        assert!(!cursor_path(&store).exists());

        assert_eq!(store.append_history("after seed"), 11);

        let entries = read_jsonl_lines(&store.history_file);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1]["cursor"], 11);
        assert_eq!(entries[1]["content"], "after seed");

        assert_eq!(fs::read_to_string(cursor_path(&store)).unwrap(), "11");

        for entry in entries {
            println!("entry: {}", serde_json::to_string(&entry).unwrap());
        }
    }
}
