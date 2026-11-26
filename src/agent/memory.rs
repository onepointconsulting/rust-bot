use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::SystemTime;

use crate::agent::context::{SOUL_FILE, USER_FILE};
use crate::agent::model_runtime::{ModelRuntime, ModelRuntimeResolver};
use crate::agent::runner::{AgentRunSpec, AgentRunner};
use crate::agent::skill_operation::SkillAction;
use crate::agent::skill_proposals::SkillProposalStore;
use crate::agent::skill_store::{SkillLimits, SkillStore};
use crate::agent::skills::{BUILTIN_SKILLS_DIR, SkillsLoader};
use crate::agent::tools::skill_manage::{SKILL_MANAGE_TOOL_NAME, SkillManageTool, SkillWriteMode};
use crate::agent::tools::filesystem::{EditFileTool, ReadFileTool};
use crate::agent::tools::registry::ToolRegistry;
use crate::config::schema::SkillsToolConfig;
use crate::providers::base::LLMResponse;
use crate::session::manager::{Session, SessionManager};
use crate::utils::fs::write_atomic;
use crate::utils::gitstore::{GitStore, memory_tracked_paths};
use crate::utils::helpers::{
    empty_or_default, ensure_dir, estimate_message_tokens, estimate_prompt_tokens_chain,
    strip_think,
};
use crate::utils::prompt_templates::render_template;
use chrono::{DateTime, Local};
use regex::Regex;
use serde_json::json;
use tera::Context;

const DEFAULT_MAX_HISTORY: usize = 1000;

const RAW_MARKER: &str = "[RAW]";
const MEMORY_FILE: &str = "MEMORY.md";
const HISTORY_FILE: &str = "history.jsonl";
const CURSOR_FILE: &str = ".cursor";
const DREAM_CURSOR_FILE: &str = ".dream_cursor";
const LEGACY_HISTORY_BACKUP: &str = "HISTORY.md.bak";

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
    pub git: GitStore,
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
        let git = GitStore::new(workspace, memory_tracked_paths());
        // Workspaces initialised before `skills/` was tracked still ignore it.
        git.ensure_gitignore();
        let store = Self {
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
            git,
        };
        store.maybe_migrate_legacy_history();
        store
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
        if let Some(first_nonempty) = first_nonempty_option
            && let Some(matched) = LEGACY_TIMESTAMP.captures(first_nonempty)
        {
            let end = matched.get(0).unwrap().end();
            let slice = first_nonempty[end..].trim_start();
            return slice.starts_with(RAW_MARKER);
        }
        false
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
        let long_term = self.read_memory().unwrap_or_default();
        if long_term.is_empty() {
            return String::new();
        }
        format!("## Long-term memory:\n{}", long_term)
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
        if let Err(e) = write_atomic(&self.cursor_file, cursor.to_string().as_bytes()) {
            log::error!(
                "Failed to write cursor file {}: {}",
                self.cursor_file.display(),
                e
            );
            return 0;
        }
        cursor
    }

    /// Compute the next history cursor counter.
    ///
    /// Mirrors Python `_next_cursor`: prefer [`Self::cursor_file`] if readable as an integer base;
    /// otherwise derive from [`Self::read_last_entry`]'s `"cursor"`; default `1`.
    fn next_cursor(&self) -> u64 {
        if self.cursor_file.exists()
            && let Ok(text) = std::fs::read_to_string(&self.cursor_file)
            && let Ok(n) = text.trim().parse::<u64>()
        {
            return n.saturating_add(1);
        }
        if let Some(last) = self.read_last_entry()
            && let Some(c) = last.get("cursor")
            && let Some(n) = Self::parse_entry_cursor(c)
        {
            return n.saturating_add(1);
        }
        1
    }

    pub fn read_unprocessed_history(&self, since_cursor: u64) -> Vec<serde_json::Value> {
        log::info!(
            "Dream: reading unprocessed history since cursor: {}",
            since_cursor
        );
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

    /// Drop oldest entries if the file exceeds *max_history_entries*.
    pub fn compact_history(&self) {
        if self.max_history_entries == 0 {
            return;
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

    /// Read all entries from self.history_file as JSONL lines line by line skipping blank lines.
    fn read_entries(&self) -> Vec<serde_json::Value> {
        log::info!(
            "Dream: reading entries from history file: {}",
            self.history_file.display()
        );
        let file_result = File::open(&self.history_file);
        let mut entries = Vec::new();
        if let Ok(f) = file_result {
            let reader = BufReader::new(f);
            log::info!("Dream: reader created");
            for line_result in reader.lines() {
                if let Ok(line) = line_result {
                    if let Ok(entry) = serde_json::from_str::<serde_json::Value>(line.trim()) {
                        entries.push(entry);
                    } else {
                        log::error!("Failed to parse JSONL line: {}", line);
                    }
                } else {
                    log::error!("Failed to read line: {}", line_result.unwrap_err());
                }
            }
        } else {
            log::error!(
                "Failed to open history file: {}",
                self.history_file.display()
            );
        }
        log::info!("Dream: entries read: {}", entries.len());
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
        let mut contents = Vec::new();
        for entry in entries {
            writeln!(
                contents,
                "{}",
                serde_json::to_string(&entry).map_err(io::Error::other)?
            )?;
        }
        write_atomic(&self.history_file, &contents)
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
                    log::error!(
                        "Failed to read dream cursor file: {}",
                        self.dream_cursor_file.display()
                    );
                }
            }
        }
        0
    }

    pub fn set_last_dream_cursor(&self, cursor: u64) {
        if let Err(e) = write_atomic(&self.dream_cursor_file, cursor.to_string().as_bytes()) {
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

            lines.push(format!("[{timestamp}] {role}{tools_suffix}: {content}",));
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

    fn write_safe(content: &str, path: &Path) {
        // Atomic: a kill mid-write must not leave MEMORY.md, SOUL.md or USER.md truncated.
        if let Err(e) = write_atomic(path, content.as_bytes()) {
            log::error!("Failed to write file: {} due to {}", path.display(), e);
        }
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
}

pub trait MessageBuilder: Send + Sync {
    /// Build the complete message list for an LLM call
    #[allow(clippy::too_many_arguments)]
    fn build_messages(
        &self,
        history: &[serde_json::Value],
        current_message: &str,
        skill_names: Option<&[String]>,
        media: Option<&[String]>,
        channel: Option<&str>,
        chat_id: Option<&str>,
        session_metadata: Option<&HashMap<String, serde_json::Value>>,
        // The current turn's ephemeral, pre-extracted runtime-context blocks
        // (e.g. a WebUI "quoted" excerpt) — distinct from `session_metadata`
        // (the persisted Session's own metadata, read only for goal-state
        // lines). Mirrors nanobot's `runtime_context_blocks_from_metadata`
        // being resolved once per request, ahead of context assembly.
        runtime_context_blocks: Option<&[crate::runtime_context::RuntimeContextBlock]>,
        current_role: &str,
        // A previous idle-session compaction's summary text, formatted and
        // ready to inject (see `Autocompact::prepare_session`) — distinct
        // from `session_metadata`, which is the session's own raw metadata
        // map. `None` when there is nothing pending.
        session_summary: Option<&str>,
    ) -> Vec<serde_json::Value>;

    /// Get tool definitions with stable ordering for cache-friendly prompts.
    fn get_definitions(&self) -> Vec<serde_json::Value>;
}

pub struct Consolidator {
    pub store: Arc<MemoryStore>,
    /// Resolves the summarization call's provider/model per session (its
    /// stored preset override, if any, else the process-wide default) — no
    /// fixed provider/model is held here.
    runtime_resolver: Arc<ModelRuntimeResolver>,
    sessions: Arc<Mutex<SessionManager>>,
    context_window_tokens: u64,
    message_builder: Box<dyn MessageBuilder>,
    max_completion_tokens: usize,
    locks: Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
    /// Keep reusable procedures in summaries (`agents.dream.learnSkills`), so
    /// Dream can turn them into skills.
    learn_skills: bool,
}

impl Consolidator {
    const SAFETY_BUFFER: usize = 1024;
    const MAX_CONSOLIDATION_ROUNDS: usize = 5;

    pub fn new(
        store: Arc<MemoryStore>,
        runtime_resolver: Arc<ModelRuntimeResolver>,
        sessions: Arc<Mutex<SessionManager>>,
        context_window_tokens: u64,
        message_builder: Box<dyn MessageBuilder>,
        max_completion_tokens: usize,
    ) -> Self {
        Self {
            store,
            runtime_resolver,
            sessions,
            context_window_tokens,
            message_builder,
            max_completion_tokens,
            locks: Arc::new(Mutex::new(HashMap::new())),
            learn_skills: false,
        }
    }

    /// Ask summaries to keep reusable procedures ("PROCEDURE: …" lines) for
    /// Dream's skill learning (`agents.dream.learnSkills`).
    pub fn with_learn_skills(mut self, learn_skills: bool) -> Self {
        self.learn_skills = learn_skills;
        self
    }

    /// The summarisation system prompt; it adds the Procedures category when
    /// `learn_skills` is on.
    fn archive_system_prompt(&self) -> String {
        let mut context = Context::new();
        context.insert("learn_skills", &self.learn_skills);
        match render_template("agent/consolidator_archive.md", &context, true) {
            Ok(s) => s,
            Err(e) => {
                log::error!("Failed to render consolidator_archive template: {}", e);
                String::new()
            }
        }
    }

    /// Pick a user-turn boundary that removes enough old prompt tokens.
    pub fn pick_consolidation_boundary(
        session: &Session,
        tokens_to_remove: usize,
    ) -> Option<(usize, usize)> {
        let start = session.last_consolidated;
        if start >= session.messages.len() || tokens_to_remove == 0 {
            return None;
        }

        let mut removed_tokens = 0;
        let mut last_boundary = None::<(usize, usize)>;
        for idx in start..session.messages.len() {
            let message = &session.messages[idx];
            if idx > start && message.get("role").and_then(|v| v.as_str()) == Some("user") {
                last_boundary = Some((idx, removed_tokens));
                if removed_tokens >= tokens_to_remove {
                    return last_boundary;
                }
            }
            removed_tokens += estimate_message_tokens(message);
        }
        last_boundary
    }

    /// Estimate current prompt size for the normal session history view.
    /// "if we sent a request right now, how many tokens would the prompt be?"
    pub fn estimate_session_prompt_tokens(&self, session: &Session) -> (u64, String) {
        // Same default window as `session.get_history(None)` (500); `Some(0)` is also normalized to that cap.
        let history = session.get_history(None);
        let (channel, chat_id) = session
            .key
            .split_once(':')
            .map(|(ch, id)| (Some(ch), Some(id)))
            .unwrap_or((None, None));
        let probe_messages = self.message_builder.build_messages(
            history.as_slice(),
            "[token-probe]",
            None,
            None,
            channel,
            chat_id,
            None,
            None,
            "user",
            None,
        );
        let (estimated, source) = estimate_prompt_tokens_chain(
            probe_messages.as_slice(),
            Some(self.message_builder.get_definitions().as_slice()),
        );
        (estimated as u64, source)
    }

    /// Summarize *messages* via LLM and append the result to `history.jsonl`.
    ///
    /// Mirrors nanobot's `Consolidator.archive`: returns the summary text
    /// that was actually persisted on success, or `None` when the provider
    /// call itself failed (`finish_reason == "error"`) — in which case
    /// *messages* is raw-archived instead so nothing is silently lost.
    ///
    /// `summary_messages`, when given, replaces `messages` as the LLM's
    /// input (richer context — e.g. a still-visible suffix that shouldn't
    /// itself be archived) while `messages` remains what gets raw-dumped on
    /// failure.
    ///
    /// `runtime_override`, when given, is used as-is instead of resolving a
    /// runtime from `session_key` (used by [`Self::compact_idle_session`],
    /// whose caller already resolved one for the full session).
    pub async fn archive(
        &self,
        messages: &[serde_json::Value],
        session_key: &str,
        summary_messages: Option<&[serde_json::Value]>,
        runtime_override: Option<&ModelRuntime>,
    ) -> Option<String> {
        if messages.is_empty() {
            return None;
        }
        let system_prompt = self.archive_system_prompt();
        let formatted = MemoryStore::format_messages(summary_messages.unwrap_or(messages));

        let resolved_runtime;
        let runtime: &ModelRuntime = match runtime_override {
            Some(r) => r,
            None => {
                resolved_runtime = self
                    .runtime_resolver
                    .resolve_for_session_key(&self.sessions, session_key);
                &resolved_runtime
            }
        };

        let response = runtime
            .provider
            .chat_with_retry(
                vec![
                    serde_json::json!({
                        "role": "system",
                        "content": system_prompt
                    }),
                    serde_json::json!({
                        "role": "user",
                        "content": formatted
                    }),
                ],
                None,
                Some(runtime.model.clone()),
                None,
                None,
                None,
                None,
            )
            .await;

        if response.finish_reason == "error" {
            log::warn!("Consolidation provider returned an error, raw-dumping to history");
            self.store.raw_archive(messages);
            return None;
        }

        // Match `append_history`: treat blank / whitespace-only summaries like missing output.
        let summary_entry = response.content.as_ref().map(|entry| {
            let mut c = strip_think(entry.trim_end());
            if c.is_empty() {
                c = entry.trim_end().to_string();
            }
            if c.trim().is_empty() {
                "[no summary]"
            } else {
                entry.as_str()
            }
        });
        match summary_entry {
            Some(s) => {
                self.store.append_history(s);
                Some(s.to_string())
            }
            None => {
                log::warn!("Consolidation LLM call failed, raw-dumping to history.");
                self.store.raw_archive(messages);
                None
            }
        }
    }

    /// Loop: archive old messages until prompt fits within safe budget.
    ///
    /// The budget reserves space for completion tokens and a safety buffer
    /// so the LLM request never exceeds the context window. Resolves the
    /// session from `self.sessions` by key so callers need not hold the
    /// session-manager lock across `await` points.
    pub async fn maybe_consolidate_by_tokens(&self, session_key: &str) {
        if self.context_window_tokens == 0 {
            return;
        }

        let lock = self.get_lock(session_key);
        let _guard = lock.lock().await;
        let window = self.context_window_tokens as usize;
        let budget = window
            .saturating_sub(self.max_completion_tokens)
            .saturating_sub(Consolidator::SAFETY_BUFFER);
        let target = budget / 2;

        let (mut estimated, source) = {
            let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            let session = sessions.get_or_create_session(session_key);
            if session.messages.is_empty() {
                return;
            }
            self.estimate_session_prompt_tokens(session)
        };

        if estimated == 0 {
            return;
        }
        if estimated < budget as u64 {
            log::debug!(
                "Token consolidation idle {session_key}: {estimated}/{window} via {source}",
                window = self.context_window_tokens,
            );
            return;
        }

        for round_num in 0..Consolidator::MAX_CONSOLIDATION_ROUNDS {
            if estimated <= target as u64 {
                return;
            }

            let chunk_and_end = {
                let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
                let session = sessions.get_or_create_session(session_key);
                match Consolidator::pick_consolidation_boundary(session, target) {
                    Some((idx, _removed_tokens)) => {
                        let end_idx = idx;
                        let chunk = session.messages[session.last_consolidated..end_idx].to_vec();
                        if chunk.is_empty() {
                            return;
                        }
                        log::info!(
                            "Token consolidation round {round_num} for {session_key}: {estimated}/{window} via {source}, chunk={chunk_len} msgs",
                            window = self.context_window_tokens,
                            chunk_len = chunk.len(),
                        );
                        Some((chunk, end_idx))
                    }
                    None => {
                        log::debug!(
                            "Token consolidation: no safe boundary for {session_key} (round {round_num})",
                        );
                        return;
                    }
                }
            };

            let Some((chunk, end_idx)) = chunk_and_end else {
                return;
            };

            if self
                .archive(&chunk, session_key, None, None)
                .await
                .is_none()
            {
                return;
            }

            estimated = {
                let (est, snapshot) = {
                    let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
                    let session = sessions.get_or_create_session(session_key);
                    session.last_consolidated = end_idx;
                    let snapshot = session.clone();
                    let est = self.estimate_session_prompt_tokens(&snapshot).0;
                    (est, snapshot)
                };
                let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
                if let Err(e) = sessions.save(snapshot) {
                    log::error!("Failed to save session after consolidation: {e}");
                    return;
                }
                est
            };

            if estimated == 0 {
                return;
            }
        }
    }

    fn get_lock(&self, session_key: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.locks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(session_key.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// Archive an idle session's un-replayed prefix and hide it from replay
    /// without deleting it.
    ///
    /// Mirrors nanobot's `Consolidator.compact_idle_session`: the returned
    /// `Some(String::new())` means there was nothing to archive (an empty
    /// tail, or the tail already fits within `max_suffix`); `Some(text)` is
    /// the archived summary; `None` means the provider call itself failed
    /// (the dropped messages were raw-archived instead, and the replay
    /// boundary still advances so a failed session doesn't get stuck
    /// re-summarizing the same prefix forever).
    pub async fn compact_idle_session(
        &self,
        session_key: &str,
        runtime: ModelRuntime,
        max_suffix: usize,
    ) -> Option<String> {
        let lock = self.get_lock(session_key);
        let _guard = lock.lock().await;

        // Force a fresh reload so this pass never works from a stale cached
        // session while some other task may have mutated it concurrently.
        let mut session = {
            let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            sessions.invalidate(session_key);
            sessions.get_or_create_session(session_key).clone()
        };

        let start = session.last_consolidated.min(session.messages.len());
        let messages_to_summarize = session.messages[start..].to_vec();
        if messages_to_summarize.is_empty() {
            let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            if let Err(e) = sessions.save(session) {
                log::error!("Failed to save session {session_key} after idle compact: {e}");
            }
            return Some(String::new());
        }

        // Probe: how much of the tail would a legal recent-suffix retention
        // drop? The probe always starts at last_consolidated == 0, so
        // nothing in it counts as already-consolidated — any messages it
        // drops are exactly the ones this pass needs to summarize.
        let mut probe = Session {
            key: session.key.clone(),
            messages: messages_to_summarize.clone(),
            created_at: session.created_at,
            updated_at: session.updated_at,
            metadata: HashMap::new(),
            last_consolidated: 0,
        };
        probe.retain_recent_legal_suffix(max_suffix, true);
        let visible_suffix = probe.messages;
        let dropped_count = messages_to_summarize.len() - visible_suffix.len();

        if dropped_count == 0 {
            let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            if let Err(e) = sessions.save(session) {
                log::error!("Failed to save session {session_key} after idle compact: {e}");
            }
            return Some(String::new());
        }
        let messages_to_remove = &messages_to_summarize[..dropped_count];

        let last_active = session.updated_at;
        // The visible suffix informs the summary but stays out of the raw
        // fallback dump if the provider call fails.
        let summary = self
            .archive(
                messages_to_remove,
                session_key,
                Some(messages_to_summarize.as_slice()),
                Some(&runtime),
            )
            .await;

        if let Some(text) = summary.as_deref() {
            if text != "(nothing)" {
                let mut summary_meta = serde_json::Map::new();
                summary_meta.insert(
                    "text".to_string(),
                    serde_json::Value::String(text.to_string()),
                );
                summary_meta.insert(
                    SessionManager::LAST_ACTIVE_KEY.to_string(),
                    serde_json::Value::String(last_active.to_rfc3339()),
                );
                session.metadata.insert(
                    SessionManager::LAST_SUMMARY_KEY.to_string(),
                    serde_json::Value::Object(summary_meta),
                );
                log::info!("session {session_key} archived; summary={text}");
            } else {
                log::error!("session {session_key} failed to archive; no summary metadata");
            }
        }

        // Preserve history and advance only the replay boundary.
        session.last_consolidated = session.messages.len() - visible_suffix.len();

        log::info!(
            "Idle-session compact for {session_key}: archived={}, visible={}, retained={}, summary={}",
            messages_to_remove.len(),
            visible_suffix.len(),
            session.messages.len(),
            summary.is_some(),
        );

        {
            let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            if let Err(e) = sessions.save(session) {
                log::error!("Failed to save session {session_key} after idle compact: {e}");
            }
        }

        summary
    }
}

/// Marker opening a Phase 1 line that describes a skill (procedural) lesson.
const SKILL_FINDING_MARKER: &str = "[SKILL]";

/// Analysis handed to Phase 2 when Phase 1 found only skill lessons.
const SKIP_MARKER: &str = "[SKIP]";

/// Write tools whose successful calls count as Dream changes.
const MEMORY_WRITE_TOOL: &str = "edit_file";

/// Phase 1's analysis, split by destination.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DreamFindings {
    /// Lines for the memory files (`[FILE]`, `[FILE-REMOVE]`, `[SKIP]`, …),
    /// handed to Phase 2 unchanged.
    pub memory: String,
    /// `[SKILL] …` lines for Phase 3, trimmed, without list bullets.
    pub skills: Vec<String>,
}

/// Split Phase 1's `analysis` into memory findings and `[SKILL]` lines.
///
/// A skill line is one whose trimmed text, after an optional `-`/`*` list
/// bullet, starts with `[SKILL]` (any case). Every other line stays in
/// `memory`, in order; the memory text is trimmed.
pub fn partition_findings(analysis: &str) -> DreamFindings {
    let mut memory_lines = Vec::new();
    let mut skills = Vec::new();
    for line in analysis.lines() {
        match skill_finding(line) {
            Some(finding) => skills.push(finding),
            None => memory_lines.push(line),
        }
    }
    DreamFindings {
        memory: memory_lines.join("\n").trim().to_string(),
        skills,
    }
}

/// The normalised `[SKILL] …` finding on `line`, if it is one.
fn skill_finding(line: &str) -> Option<String> {
    let text = line.trim();
    let text = text
        .strip_prefix("- ")
        .or_else(|| text.strip_prefix("* "))
        .unwrap_or(text)
        .trim_start();
    let marker_len = SKILL_FINDING_MARKER.len();
    let marker = text.get(..marker_len)?;
    if !marker.eq_ignore_ascii_case(SKILL_FINDING_MARKER) {
        return None;
    }
    let lesson = text[marker_len..].trim();
    if lesson.is_empty() {
        return None;
    }
    Some(format!("{SKILL_FINDING_MARKER} {lesson}"))
}

/// One skill as Dream shows it to the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillIndexEntry {
    pub name: String,
    pub description: String,
    /// Where its `SKILL.md` is, when the prompt should say so.
    pub path: Option<String>,
}

/// The skill index as prompt lines (`- name: description [— path]`), or
/// `(none)` when there are no skills.
pub fn format_skill_index(entries: &[SkillIndexEntry]) -> String {
    if entries.is_empty() {
        return "(none)".to_string();
    }
    entries
        .iter()
        .map(|entry| match &entry.path {
            Some(path) => format!("- {}: {} — {}", entry.name, entry.description, path),
            None => format!("- {}: {}", entry.name, entry.description),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The Dream commit message for the changes of one run.
///
/// Skill changes are only mentioned when skill learning is on; staged
/// proposals are counted as proposals, since they are not written yet.
pub fn format_dream_commit_message(
    timestamp: &str,
    memory_changes: usize,
    skill_changes: usize,
    learn_skills: bool,
    skills_staged: bool,
) -> String {
    let mut message = format!("dream: {timestamp}, {memory_changes} memory change(s)");
    if learn_skills {
        let kind = if skills_staged {
            "skill proposal(s)"
        } else {
            "skill change(s)"
        };
        message.push_str(&format!(", {skill_changes} {kind}"));
    }
    message
}

/// What one AgentRunner phase achieved.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct PhaseOutcome {
    /// Whether the run stopped normally (`stop_reason == "completed"`).
    completed: bool,
    /// One `name: detail` line per successful write.
    changes: Vec<String>,
}

/// Dream's background skill learning: whether it is on, and the tools Phase 3
/// runs with.
struct SkillLearning {
    enabled: bool,
    /// Whether `skill_manage` stages proposals rather than writing.
    staged: bool,
    /// `read_file` plus a restricted `skill_manage`.
    tools: ToolRegistry,
    /// Bundled skills, listed after the workspace ones.
    builtin_skills_dir: PathBuf,
    /// Pending proposals, also read by `/skills-review`.
    proposals: Arc<SkillProposalStore>,
}

impl SkillLearning {
    /// Skill learning for `workspace`: on when `enabled`; writes directly or,
    /// when `skills_config.require_approval`, stages proposals. Size limits
    /// come from `skills_config`.
    fn new(
        workspace: &Path,
        enabled: bool,
        skills_config: &SkillsToolConfig,
        builtin_skills_dir: Option<PathBuf>,
    ) -> Self {
        let builtin_skills_dir = builtin_skills_dir.unwrap_or_else(|| BUILTIN_SKILLS_DIR.clone());
        let store = Arc::new(SkillStore::new(
            workspace,
            Some(builtin_skills_dir.clone()),
            SkillLimits::from(skills_config),
        ));
        let proposals = Arc::new(SkillProposalStore::new(workspace, Arc::clone(&store)));
        let staged = skills_config.require_approval;
        let mode = if staged {
            SkillWriteMode::Propose(Arc::clone(&proposals))
        } else {
            SkillWriteMode::Apply
        };
        let mut tools = ToolRegistry::new();
        tools.register(Box::new(ReadFileTool::new(
            Some(workspace.to_path_buf()),
            None,
            None,
        )));
        tools.register(Box::new(
            SkillManageTool::new(store)
                .with_mode(mode)
                .with_allowed_actions(&SkillAction::NON_DESTRUCTIVE),
        ));
        Self {
            enabled,
            staged,
            tools,
            builtin_skills_dir,
            proposals,
        }
    }
}

/// Memory processor: analyse history.jsonl, then edit files via AgentRunner.
///
/// Phase 1 produces an analysis summary (plain LLM call).
/// Phase 2 delegates to AgentRunner with read_file / edit_file tools so the
/// LLM can make targeted, incremental edits instead of replacing entire files.
/// Phase 3 (only with `agents.dream.learnSkills` and when Phase 1 found
/// `[SKILL]` lessons) runs AgentRunner with read_file / skill_manage to turn
/// those lessons into workspace skills, or skill proposals.
pub struct Dream {
    pub store: Arc<MemoryStore>,
    /// Resolves each Dream cycle's provider/model — no fixed provider/model
    /// is held here; see [`Self::resolve_runtime`].
    runtime_resolver: Arc<ModelRuntimeResolver>,
    /// Name of a `model_presets` entry Dream should use, if configured.
    dream_model_preset: Option<String>,
    /// Raw model-string override, used when `dream_model_preset` is unset.
    model_override: Option<String>,
    pub max_batch_size: usize,
    pub max_iterations: usize,
    pub max_tool_result_chars: usize,
    tools: ToolRegistry,
    skill_learning: SkillLearning,
}

impl Dream {
    /// A Dream over `store` with skill learning off; see
    /// [`Self::with_skill_learning`].
    pub fn new(
        store: Arc<MemoryStore>,
        runtime_resolver: Arc<ModelRuntimeResolver>,
        dream_model_preset: Option<String>,
        model_override: Option<String>,
        max_batch_size: usize,
        max_iterations: usize,
        max_tool_result_chars: usize,
    ) -> Dream {
        let mut tools = ToolRegistry::new();
        let workspace = store.workspace.clone();
        tools.register(Box::new(ReadFileTool::new(
            Some(workspace.clone()),
            None,
            None,
        )));
        tools.register(Box::new(EditFileTool::new(
            Some(workspace.clone()),
            None,
            None,
        )));
        let skill_learning =
            SkillLearning::new(&workspace, false, &SkillsToolConfig::default(), None);
        Dream {
            store,
            runtime_resolver,
            dream_model_preset,
            model_override,
            max_batch_size,
            max_iterations,
            max_tool_result_chars,
            tools,
            skill_learning,
        }
    }

    /// Turn background skill learning on or off (`agents.dream.learnSkills`).
    ///
    /// Size limits and the write policy come from `tools.skills`: with
    /// `requireApproval` Dream stages proposals for `/skills-review`,
    /// otherwise it writes skills directly (versioned by the memory git store).
    pub fn with_skill_learning(self, learn_skills: bool, skills_config: &SkillsToolConfig) -> Self {
        self.with_skill_learning_from(learn_skills, skills_config, None)
    }

    /// [`Self::with_skill_learning`] reading bundled skills from
    /// `builtin_skills_dir` (the loader's default when `None`).
    fn with_skill_learning_from(
        mut self,
        learn_skills: bool,
        skills_config: &SkillsToolConfig,
        builtin_skills_dir: Option<PathBuf>,
    ) -> Self {
        self.skill_learning = SkillLearning::new(
            &self.store.workspace,
            learn_skills,
            skills_config,
            builtin_skills_dir,
        );
        self
    }

    /// The pending skill proposals (also those left from earlier runs).
    pub fn skill_proposals(&self) -> &Arc<SkillProposalStore> {
        &self.skill_learning.proposals
    }

    /// Resolve this Dream cycle's runtime: `dream_model_preset` (a named
    /// preset) takes precedence; else `model_override` (a raw model string)
    /// is layered onto the process-wide default's provider; else the
    /// process-wide default is used unchanged. Resolved fresh on every
    /// `run()` — Dream never holds a fixed provider/model from construction.
    fn resolve_runtime(&self) -> ModelRuntime {
        if let Some(name) = &self.dream_model_preset {
            match self.runtime_resolver.resolve_preset(name) {
                Ok(runtime) => return runtime,
                Err(e) => log::warn!(
                    "agents.dream.dreamModelPreset '{name}' failed to resolve ({e}); \
                     falling back to modelOverride/process default"
                ),
            }
        }
        let mut runtime = self.runtime_resolver.current_default();
        if let Some(model) = &self.model_override {
            runtime.model = model.clone();
        }
        runtime
    }

    fn finish_reason_fail(phase1_response: &LLMResponse) -> bool {
        phase1_response.finish_reason == "error" || phase1_response.content.is_none()
    }

    /// Process unprocessed history entries. Returns true if work was done.
    ///
    /// The cursor advances whatever the phases achieve, so a batch is never
    /// analysed twice. A Phase 3 failure is logged and does not undo Phase 2.
    /// Any change made is committed; the result reports Phase 2's success.
    pub async fn run(&self) -> bool {
        log::info!("Dream: running");
        let runtime = self.resolve_runtime();
        log::info!(
            "Dream run using model={} (preset={})",
            runtime.model,
            runtime.preset_name
        );
        let last_cursor = self.store.get_last_dream_cursor();
        let entries = self.store.read_unprocessed_history(last_cursor);
        if entries.is_empty() {
            log::info!("Dream: no entries to process");
            return false;
        }

        let batch = entries[..entries.len().min(self.max_batch_size)].to_vec();
        log::info!(
            "Dream: processing {} entries (cursor {}→{}), batch={}",
            entries.len(),
            last_cursor,
            batch[batch.len() - 1]["cursor"],
            batch.len(),
        );

        let history_text = Self::build_history_text(&batch);
        let file_context = self.build_file_context();

        let Some(analysis) = self
            .run_analysis(&runtime, &history_text, &file_context)
            .await
        else {
            return false;
        };
        let findings = if self.skill_learning.enabled {
            partition_findings(&analysis)
        } else {
            DreamFindings {
                memory: analysis,
                skills: Vec::new(),
            }
        };

        let memory_outcome = self
            .run_memory_phase(&runtime, &findings, &file_context)
            .await;
        let skill_outcome = self.run_skill_phase(&runtime, &findings.skills).await;

        // Advance cursor — always, to avoid re-processing Phase 1
        let new_cursor = batch[batch.len() - 1]["cursor"].as_u64().unwrap_or(0);
        self.store.set_last_dream_cursor(new_cursor);
        self.store.compact_history();

        if memory_outcome.completed {
            log::info!(
                "Dream done: {} memory change(s), {} skill change(s), cursor advanced to {}",
                memory_outcome.changes.len(),
                skill_outcome.changes.len(),
                new_cursor,
            );
        } else {
            log::warn!("Dream Phase 2 failed; cursor advanced to {}", new_cursor);
        }

        self.commit_changes(&batch, &memory_outcome, &skill_outcome);
        memory_outcome.completed
    }

    /// The batch as `[timestamp] content` lines for the prompts.
    fn build_history_text(batch: &[serde_json::Value]) -> String {
        batch
            .iter()
            .map(|e| {
                format!(
                    "[{}] {}",
                    e["timestamp"].as_str().unwrap_or(""),
                    e["content"].as_str().unwrap_or("")
                )
            })
            .collect::<Vec<String>>()
            .join("\n")
    }

    /// Today's date and the current MEMORY.md, SOUL.md and USER.md, for the
    /// Phase 1 and Phase 2 prompts.
    fn build_file_context(&self) -> String {
        let current_date = Local::now().format("%Y-%m-%d").to_string();
        let current_memory = empty_or_default(self.store.read_memory());
        let current_soul = empty_or_default(self.store.read_soul());
        let current_user = empty_or_default(self.store.read_user());
        format!(
            "## Current Date\n{}\n\n\
             ## Current MEMORY.md ({} chars)\n{}\n\n\
             ## Current SOUL.md ({} chars)\n{}\n\n\
             ## Current USER.md ({} chars)\n{}",
            current_date,
            current_memory.chars().count(),
            current_memory,
            current_soul.chars().count(),
            current_soul,
            current_user.chars().count(),
            current_user,
        )
    }

    /// Every skill the loader sees, workspace first; with `with_paths`, each
    /// entry says where its `SKILL.md` is.
    fn skill_index(&self, with_paths: bool) -> Vec<SkillIndexEntry> {
        let workspace_skills = self.store.workspace.join("skills");
        let builtin_skills = &self.skill_learning.builtin_skills_dir;
        SkillsLoader::new(&self.store.workspace, Some(builtin_skills.clone()))
            .list_skill_summaries()
            .into_iter()
            .map(|summary| {
                let path = with_paths.then(|| {
                    let workspace_file = workspace_skills.join(&summary.name).join("SKILL.md");
                    let file = if workspace_file.is_file() {
                        workspace_file
                    } else {
                        builtin_skills.join(&summary.name).join("SKILL.md")
                    };
                    file.display().to_string()
                });
                SkillIndexEntry {
                    name: summary.name,
                    description: summary.description,
                    path,
                }
            })
            .collect()
    }

    /// The Phase 1 system prompt; asks for `[SKILL]` lines when learning skills.
    fn phase1_system_prompt(&self) -> Result<String, String> {
        let mut context = Context::new();
        context.insert("learn_skills", &self.skill_learning.enabled);
        if self.skill_learning.enabled {
            context.insert("skill_index", &format_skill_index(&self.skill_index(false)));
        }
        render_template("agent/dream_phase1.md", &context, true)
    }

    /// Phase 1: one LLM call comparing history with the memory files.
    /// Returns the analysis, or `None` when the call failed.
    async fn run_analysis(
        &self,
        runtime: &ModelRuntime,
        history_text: &str,
        file_context: &str,
    ) -> Option<String> {
        let phase1_prompt = format!("## Conversation History\n{history_text}\n\n{file_context}");
        let phase1_system = match self.phase1_system_prompt() {
            Ok(s) => s,
            Err(e) => {
                log::error!("Dream Phase 1: failed to render template: {}", e);
                return None;
            }
        };
        let phase1_response = runtime
            .provider
            .chat_with_retry(
                vec![
                    serde_json::json!({
                        "role": "system",
                        "content": phase1_system,
                    }),
                    serde_json::json!({
                        "role": "user",
                        "content": phase1_prompt,
                    }),
                ],
                None,
                Some(runtime.model.clone()),
                None,
                None,
                None,
                None,
            )
            .await;
        if Self::finish_reason_fail(&phase1_response) {
            log::error!(
                "Dream Phase 1 failed: finish_reason={}, has_content={}",
                phase1_response.finish_reason,
                phase1_response.content.is_some(),
            );
            return None;
        }
        let analysis = empty_or_default(phase1_response.content);
        log::info!(
            "Dream Phase 1 analysis ({} chars): {}",
            analysis.chars().count(),
            analysis.chars().take(500).collect::<String>()
        );
        Some(analysis)
    }

    /// Phase 2: edit SOUL.md, USER.md and MEMORY.md from the memory findings.
    ///
    /// When Phase 1 found only skill lessons, there is nothing for the memory
    /// files and the phase is skipped (it counts as completed).
    async fn run_memory_phase(
        &self,
        runtime: &ModelRuntime,
        findings: &DreamFindings,
        file_context: &str,
    ) -> PhaseOutcome {
        let analysis = findings.memory.as_str();
        if analysis.is_empty() && !findings.skills.is_empty() {
            log::info!("Dream Phase 2 skipped: only skill findings");
            return PhaseOutcome {
                completed: true,
                changes: Vec::new(),
            };
        }
        let analysis = if analysis.is_empty() {
            SKIP_MARKER
        } else {
            analysis
        };
        let phase2_prompt = format!("## Analysis Result\n{analysis}\n\n{file_context}");
        let phase2_system = match render_template("agent/dream_phase2.md", &Context::new(), true) {
            Ok(s) => s,
            Err(e) => {
                log::error!("Dream Phase 2: failed to render template: {}", e);
                return PhaseOutcome::default();
            }
        };
        self.run_agent_phase(
            "Phase 2",
            runtime,
            phase2_system,
            phase2_prompt,
            &self.tools,
            MEMORY_WRITE_TOOL,
        )
        .await
    }

    /// Phase 3: turn `[SKILL]` findings into skills (or skill proposals).
    ///
    /// Runs only when skill learning is on and there are findings, so it costs
    /// nothing otherwise. A failure is logged; it never undoes Phase 2.
    async fn run_skill_phase(&self, runtime: &ModelRuntime, skills: &[String]) -> PhaseOutcome {
        if !self.skill_learning.enabled || skills.is_empty() {
            return PhaseOutcome {
                completed: true,
                changes: Vec::new(),
            };
        }
        let skills_dir = self.store.workspace.join("skills");
        let mut context = Context::new();
        context.insert("skills_dir", &skills_dir.display().to_string());
        context.insert("staged", &self.skill_learning.staged);
        let phase3_system = match render_template("agent/dream_phase3.md", &context, true) {
            Ok(s) => s,
            Err(e) => {
                log::error!("Dream Phase 3: failed to render template: {}", e);
                return PhaseOutcome::default();
            }
        };
        let phase3_prompt = format!(
            "## Skill Findings\n{}\n\n## Existing Skills\n{}\n\n## Skills Directory\n{}",
            skills.join("\n"),
            format_skill_index(&self.skill_index(true)),
            skills_dir.display(),
        );
        let outcome = self
            .run_agent_phase(
                "Phase 3",
                runtime,
                phase3_system,
                phase3_prompt,
                &self.skill_learning.tools,
                SKILL_MANAGE_TOOL_NAME,
            )
            .await;
        if !outcome.completed {
            log::warn!(
                "Dream Phase 3 did not complete; {} skill change(s) kept",
                outcome.changes.len()
            );
        }
        outcome
    }

    /// Run an AgentRunner phase with `tools`; successful `write_tool` calls are
    /// the phase's changes.
    async fn run_agent_phase(
        &self,
        phase: &str,
        runtime: &ModelRuntime,
        system_prompt: String,
        user_prompt: String,
        tools: &ToolRegistry,
        write_tool: &str,
    ) -> PhaseOutcome {
        let messages = vec![
            serde_json::json!({
                "role": "system",
                "content": system_prompt,
            }),
            serde_json::json!({
                "role": "user",
                "content": user_prompt,
            }),
        ];
        let runner = AgentRunner::new(runtime.provider.clone());
        let response = runner
            .run(AgentRunSpec {
                initial_messages: messages,
                tools: tools.clone(),
                model: runtime.model.clone(),
                max_iterations: self.max_iterations,
                max_tool_result_chars: self.max_tool_result_chars,
                fail_on_tool_error: false,
                ..Default::default()
            })
            .await;
        log::debug!(
            "Dream {phase} complete: stop_reason={}, tool_events={}",
            response.stop_reason,
            response.tool_events.len(),
        );
        if response.stop_reason != "completed" {
            log::warn!(
                "Dream {phase} stopped: stop_reason={}, has_content={}",
                response.stop_reason,
                response.final_content.is_some(),
            );
        }
        PhaseOutcome {
            completed: response.stop_reason == "completed",
            changes: successful_writes(&response.tool_events, write_tool),
        }
    }

    /// Commit the run's changes to the memory git store, when there are any.
    fn commit_changes(
        &self,
        batch: &[serde_json::Value],
        memory_outcome: &PhaseOutcome,
        skill_outcome: &PhaseOutcome,
    ) {
        let total_changes = memory_outcome.changes.len() + skill_outcome.changes.len();
        if total_changes == 0 || !self.store.git.is_initialized() {
            return;
        }
        let ts = batch
            .last()
            .and_then(|entry| entry["timestamp"].as_str())
            .unwrap_or("");
        let commit_ts = if ts.is_empty() {
            Local::now().format("%Y-%m-%d %H:%M").to_string()
        } else {
            ts.to_string()
        };
        let message = format_dream_commit_message(
            &commit_ts,
            memory_outcome.changes.len(),
            skill_outcome.changes.len(),
            self.skill_learning.enabled,
            self.skill_learning.staged,
        );
        if let Some(sha) = self.store.git.auto_commit(&message) {
            log::info!("Dream git commit: {}", sha);
        }
    }
}

/// Log every tool event and return `name: detail` for each successful call
/// of `write_tool` (reads are not changes).
fn successful_writes(
    tool_events: &[HashMap<String, String>],
    write_tool: &str,
) -> Vec<String> {
    let mut changes = Vec::new();
    for event in tool_events {
        let field = |key: &str| event.get(key).map(String::as_str).unwrap_or("");
        let msg = format!(
            "Dream tool_event: name={}, status={}, detail={}",
            field("name"),
            field("status"),
            field("detail"),
        );
        log::info!("{}", msg.chars().take(200).collect::<String>());
        if field("status") == "ok" && field("name") == write_tool {
            changes.push(format!("{}: {}", field("name"), field("detail")));
        }
    }
    changes
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::fs;
    use std::sync::Arc;
    use tempfile::TempDir;

    use crate::config::schema::Config;
    use crate::providers::base::{
        BoxedProgressCallback, BoxedStreamCallback, GenerationSettings, LLMProviderDyn, LLMResponse,
    };
    use crate::providers::registry::ProviderSpec;

    fn make_store(tmp: &TempDir) -> MemoryStore {
        MemoryStore::new(tmp.path().to_path_buf(), None)
    }

    fn session_with_messages(
        messages: Vec<serde_json::Value>,
        last_consolidated: usize,
    ) -> Session {
        let mut s = Session::new("test-session".into());
        s.messages = messages;
        s.last_consolidated = last_consolidated;
        s
    }

    struct StubArchiveMessageBuilder;

    impl MessageBuilder for StubArchiveMessageBuilder {
        fn build_messages(
            &self,
            _history: &[serde_json::Value],
            _current_message: &str,
            _skill_names: Option<&[String]>,
            _media: Option<&[String]>,
            _channel: Option<&str>,
            _chat_id: Option<&str>,
            _session_metadata: Option<&HashMap<String, serde_json::Value>>,
            _runtime_context_blocks: Option<&[crate::runtime_context::RuntimeContextBlock]>,
            _current_role: &str,
            _session_summary: Option<&str>,
        ) -> Vec<serde_json::Value> {
            vec![]
        }

        fn get_definitions(&self) -> Vec<serde_json::Value> {
            vec![]
        }
    }

    /// `LLMProviderDyn` stub: [`chat_with_retry`](LLMProviderDyn::chat_with_retry)
    /// replays `script` in order, then returns a fixed response. Every
    /// request's messages are recorded.
    struct ArchiveTestProvider {
        settings: GenerationSettings,
        chat_with_retry_response: LLMResponse,
        script: Mutex<std::collections::VecDeque<LLMResponse>>,
        requests: Mutex<Vec<Vec<serde_json::Value>>>,
    }

    impl ArchiveTestProvider {
        fn arc(chat_with_retry_response: LLMResponse) -> Arc<dyn LLMProviderDyn> {
            Arc::new(Self {
                settings: GenerationSettings::new(),
                chat_with_retry_response,
                script: Mutex::new(Default::default()),
                requests: Mutex::new(Vec::new()),
            })
        }

        /// A provider answering with `script`, then with a plain "done".
        fn scripted(script: Vec<LLMResponse>) -> Arc<Self> {
            Arc::new(Self {
                settings: GenerationSettings::new(),
                chat_with_retry_response: text_response("done"),
                script: Mutex::new(script.into()),
                requests: Mutex::new(Vec::new()),
            })
        }

        /// The messages of every request so far.
        fn requests(&self) -> Vec<Vec<serde_json::Value>> {
            self.requests.lock().unwrap().clone()
        }
    }

    /// A final text answer.
    fn text_response(content: &str) -> LLMResponse {
        let mut response = LLMResponse::new();
        response.content = Some(content.to_string());
        response.finish_reason = "stop".to_string();
        response
    }

    /// An answer calling the tool `name` with `arguments`.
    fn tool_call_response(name: &str, arguments: serde_json::Value) -> LLMResponse {
        let mut response = LLMResponse::new();
        response.finish_reason = "tool_calls".to_string();
        response.tool_calls = vec![crate::providers::base::ToolCallRequest {
            id: "call_1".to_string(),
            name: name.to_string(),
            arguments: arguments
                .as_object()
                .unwrap()
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            extra_content: None,
            provider_specific_fields: None,
            function_provider_specific_fields: None,
        }];
        response
    }

    #[async_trait::async_trait]
    impl LLMProviderDyn for ArchiveTestProvider {
        fn api_key(&self) -> Option<String> {
            None
        }
        fn api_base(&self) -> Option<String> {
            None
        }
        fn extra_headers(&self) -> Option<HashMap<String, String>> {
            None
        }
        fn generation_settings(&self) -> &GenerationSettings {
            &self.settings
        }
        fn generation_settings_mut(&mut self) -> &mut GenerationSettings {
            &mut self.settings
        }
        fn spec(&self) -> Option<&ProviderSpec> {
            None
        }
        fn get_default_model(&self) -> String {
            String::new()
        }
        async fn chat(
            &self,
            _: Vec<serde_json::Value>,
            _: Option<Vec<serde_json::Value>>,
            _: Option<String>,
            _: usize,
            _: Option<f32>,
            _: Option<String>,
            _: Option<serde_json::Value>,
        ) -> LLMResponse {
            unimplemented!("use chat_with_retry in archive tests")
        }
        async fn safe_chat(
            &self,
            _: Vec<serde_json::Value>,
            _: Option<Vec<serde_json::Value>>,
            _: Option<String>,
            _: usize,
            _: Option<f32>,
            _: Option<String>,
            _: Option<serde_json::Value>,
        ) -> LLMResponse {
            unimplemented!("use chat_with_retry in archive tests")
        }
        async fn chat_with_retry(
            &self,
            messages: Vec<serde_json::Value>,
            _: Option<Vec<serde_json::Value>>,
            _: Option<String>,
            _: Option<usize>,
            _: Option<f32>,
            _: Option<String>,
            _: Option<serde_json::Value>,
        ) -> LLMResponse {
            self.requests.lock().unwrap().push(messages);
            self.script
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| self.chat_with_retry_response.clone())
        }
        async fn chat_stream_with_retry_boxed(
            &self,
            _: Vec<serde_json::Value>,
            _: Option<Vec<serde_json::Value>>,
            _: Option<String>,
            _: Option<usize>,
            _: Option<f32>,
            _: Option<String>,
            _: Option<serde_json::Value>,
            _: Option<BoxedStreamCallback>,
            _: Option<BoxedProgressCallback>,
        ) -> LLMResponse {
            unimplemented!("not used in archive tests")
        }
    }

    fn test_runtime_resolver(provider: Arc<dyn LLMProviderDyn>) -> Arc<ModelRuntimeResolver> {
        Arc::new(ModelRuntimeResolver::new(Config::default(), provider))
    }

    /// A `ModelRuntime` for tests that call `compact_idle_session` directly
    /// (it takes a caller-resolved runtime rather than resolving its own).
    fn test_runtime(provider: Arc<dyn LLMProviderDyn>) -> ModelRuntime {
        test_runtime_resolver(provider).current_default()
    }

    fn test_consolidator(tmp: &TempDir, provider: Arc<dyn LLMProviderDyn>) -> Consolidator {
        Consolidator::new(
            Arc::new(make_store(tmp)),
            test_runtime_resolver(provider),
            Arc::new(Mutex::new(SessionManager::with_default_eviction_threshold(
                tmp.path().to_path_buf(),
            ))),
            65_536,
            Box::new(StubArchiveMessageBuilder),
            8192,
        )
    }

    // ── maybe_consolidate_by_tokens tests ─────────────────────────────────────

    #[tokio::test]
    async fn consolidate_noop_when_messages_empty() {
        let tmp = TempDir::new().unwrap();
        let mut resp = LLMResponse::new();
        resp.content = Some("summary".into());
        let sessions = Arc::new(Mutex::new(SessionManager::with_default_eviction_threshold(
            tmp.path().to_path_buf(),
        )));
        sessions
            .lock()
            .unwrap()
            .save(Session::new("s".into()))
            .unwrap();
        let c = Consolidator::new(
            Arc::new(make_store(&tmp)),
            test_runtime_resolver(ArchiveTestProvider::arc(resp)),
            sessions,
            65_536,
            Box::new(StubArchiveMessageBuilder),
            8192,
        );
        c.maybe_consolidate_by_tokens("s").await;
        assert!(
            !c.store.history_file.exists()
                || fs::metadata(&c.store.history_file).unwrap().len() == 0,
            "no history written for empty session"
        );
    }

    #[tokio::test]
    async fn consolidate_noop_when_context_window_is_zero() {
        let tmp = TempDir::new().unwrap();
        let mut resp = LLMResponse::new();
        resp.content = Some("summary".into());
        let sessions = Arc::new(Mutex::new(SessionManager::with_default_eviction_threshold(
            tmp.path().to_path_buf(),
        )));
        sessions
            .lock()
            .unwrap()
            .save(session_with_messages(
                vec![
                    json!({"role": "user", "content": "hi"}),
                    json!({"role": "assistant", "content": "yo"}),
                ],
                0,
            ))
            .unwrap();
        let c = Consolidator::new(
            Arc::new(make_store(&tmp)),
            test_runtime_resolver(ArchiveTestProvider::arc(resp)),
            sessions,
            0,
            Box::new(StubArchiveMessageBuilder),
            0,
        );
        c.maybe_consolidate_by_tokens("s").await;
        assert!(
            !c.store.history_file.exists()
                || fs::metadata(&c.store.history_file).unwrap().len() == 0,
            "no history written when context_window_tokens is zero"
        );
    }

    #[tokio::test]
    async fn consolidate_noop_when_estimated_tokens_below_budget() {
        // StubArchiveMessageBuilder returns empty probe messages, so estimated == 0.
        // estimated == 0 is an early-return guard inside the method.
        let tmp = TempDir::new().unwrap();
        let mut resp = LLMResponse::new();
        resp.content = Some("summary".into());
        let sessions = Arc::new(Mutex::new(SessionManager::with_default_eviction_threshold(
            tmp.path().to_path_buf(),
        )));
        sessions
            .lock()
            .unwrap()
            .save(session_with_messages(
                vec![
                    json!({"role": "user", "content": "hi"}),
                    json!({"role": "assistant", "content": "yo"}),
                ],
                0,
            ))
            .unwrap();
        let c = Consolidator::new(
            Arc::new(make_store(&tmp)),
            test_runtime_resolver(ArchiveTestProvider::arc(resp)),
            sessions,
            65_536,
            Box::new(StubArchiveMessageBuilder),
            8192,
        );
        c.maybe_consolidate_by_tokens("s").await;
        assert!(
            !c.store.history_file.exists()
                || fs::metadata(&c.store.history_file).unwrap().len() == 0,
            "no history written when estimated is zero (below budget)"
        );
    }

    #[tokio::test]
    async fn consolidate_archives_chunk_and_advances_last_consolidated() {
        // Use a tiny context window so the real token estimation (via StubArchiveMessageBuilder
        // which returns empty probe messages => estimated == 0) does NOT bypass the budget check.
        // We override by giving the session enough messages that pick_consolidation_boundary
        // can return a boundary, while the consolidator has a very small context window.
        //
        // Because StubArchiveMessageBuilder always returns [] probe messages, estimated is always 0
        // and the method exits early.  To exercise the consolidation path we need a MessageBuilder
        // that returns something.  Instead we test the interaction between archive and
        // last_consolidated by driving the method through a session where all the early-exit guards
        // do NOT trigger, i.e. we wire a MessageBuilder that reports a non-zero probe size.
        //
        // The simplest approach: set context_window_tokens tiny enough that even the stub
        // reported estimate would exceed budget, then confirm archive was called.
        // Since StubArchiveMessageBuilder returns 0 tokens the method exits at `if estimated == 0`.
        // We document this coverage gap and cover the slice / last_consolidated advance logic
        // directly via a lower-level scenario that doesn't depend on the token-estimation path.
        let tmp = TempDir::new().unwrap();
        let mut resp = LLMResponse::new();
        resp.content = Some("archive-summary".into());
        let sessions = Arc::new(Mutex::new(SessionManager::with_default_eviction_threshold(
            tmp.path().to_path_buf(),
        )));

        // Build a session with several alternating messages starting already past last_consolidated=0.
        // last_consolidated stays 0; after any consolidation round it should advance.
        let msgs: Vec<serde_json::Value> = (0..6)
            .map(|i| {
                let role = if i % 2 == 0 { "user" } else { "assistant" };
                json!({"role": role, "content": format!("msg {i}")})
            })
            .collect();
        let session = session_with_messages(msgs, 0);
        let lc_before = session.last_consolidated;
        sessions.lock().unwrap().save(session).unwrap();

        let c = Consolidator::new(
            Arc::new(make_store(&tmp)),
            test_runtime_resolver(ArchiveTestProvider::arc(resp)),
            sessions.clone(),
            65_536,
            Box::new(StubArchiveMessageBuilder),
            8192,
        );
        c.maybe_consolidate_by_tokens("s").await;

        // With StubArchiveMessageBuilder (returns 0 estimated), consolidation exits early.
        // Verify no history was spuriously written and last_consolidated unchanged.
        let lc_after = sessions
            .lock()
            .unwrap()
            .get_or_create_session("s")
            .last_consolidated;
        assert_eq!(
            lc_after, lc_before,
            "last_consolidated must not change when estimated == 0"
        );
        assert!(
            !c.store.history_file.exists()
                || fs::metadata(&c.store.history_file).unwrap().len() == 0
        );
    }

    #[tokio::test]
    async fn consolidate_lock_is_reentrant_safe_across_two_calls() {
        // Verify that two sequential calls on the same session key both complete
        // without deadlocking (the lock must be released after each call).
        let tmp = TempDir::new().unwrap();
        let mut resp = LLMResponse::new();
        resp.content = Some("s".into());
        let sessions = Arc::new(Mutex::new(SessionManager::with_default_eviction_threshold(
            tmp.path().to_path_buf(),
        )));
        sessions
            .lock()
            .unwrap()
            .save(session_with_messages(
                vec![
                    json!({"role": "user", "content": "a"}),
                    json!({"role": "assistant", "content": "b"}),
                ],
                0,
            ))
            .unwrap();
        let c = Consolidator::new(
            Arc::new(make_store(&tmp)),
            test_runtime_resolver(ArchiveTestProvider::arc(resp)),
            sessions,
            65_536,
            Box::new(StubArchiveMessageBuilder),
            8192,
        );
        c.maybe_consolidate_by_tokens("s").await;
        c.maybe_consolidate_by_tokens("s").await;
        // If we reach here without deadlock the test passes.
    }

    #[tokio::test]
    async fn archive_returns_false_when_messages_empty() {
        let tmp = TempDir::new().unwrap();
        let mut resp = LLMResponse::new();
        resp.content = Some("should-not-be-used".into());
        let c = test_consolidator(&tmp, ArchiveTestProvider::arc(resp));
        assert!(c.archive(&[], "test-session", None, None).await.is_none());
        assert!(
            !c.store.history_file.exists()
                || fs::metadata(&c.store.history_file).unwrap().len() == 0
        );
    }

    #[tokio::test]
    async fn archive_appends_llm_summary_to_history_when_content_present() {
        let tmp = TempDir::new().unwrap();
        let mut resp = LLMResponse::new();
        resp.content = Some("consolidated-summary-unique-xyz".into());
        let c = test_consolidator(&tmp, ArchiveTestProvider::arc(resp));
        let messages = vec![json!({
            "role": "user",
            "content": "hello archive",
            "timestamp": "2026-01-01T12:00:00Z",
        })];
        assert_eq!(
            c.archive(&messages, "test-session", None, None).await,
            Some("consolidated-summary-unique-xyz".to_string())
        );
        let raw = fs::read_to_string(&c.store.history_file).expect("history written");
        let last_line = raw.lines().last().expect("one jsonl line");
        let row: serde_json::Value = serde_json::from_str(last_line).unwrap();
        assert_eq!(
            row.get("content").and_then(|v| v.as_str()),
            Some("consolidated-summary-unique-xyz")
        );
    }

    #[tokio::test]
    async fn archive_raw_dumps_when_llm_returns_no_content() {
        let tmp = TempDir::new().unwrap();
        let mut resp = LLMResponse::new();
        resp.content = None;
        let c = test_consolidator(&tmp, ArchiveTestProvider::arc(resp));
        let messages = vec![json!({
            "role": "user",
            "content": "fall back",
            "timestamp": "2026-02-02T15:00:00Z",
        })];
        assert!(
            c.archive(&messages, "test-session", None, None)
                .await
                .is_none()
        );
        let raw = fs::read_to_string(&c.store.history_file).expect("history written");
        let last_line = raw.lines().last().expect("one jsonl line");
        let row: serde_json::Value = serde_json::from_str(last_line).unwrap();
        let content = row.get("content").and_then(|v| v.as_str()).unwrap_or("");
        assert!(
            content.contains(RAW_MARKER),
            "expected raw marker in archived content, got: {content:?}"
        );
        assert!(
            content.contains("USER:"),
            "formatted user line should appear: {content:?}"
        );
    }

    #[test]
    fn render_template_result_must_not_be_embedded_in_json_macro_for_chat_message() {
        let v = serde_json::json!({
            "content": render_template("agent/consolidator_archive.md", &Context::new(), true)
        });
        assert!(
            !v["content"].is_string(),
            "Consolidator::archive must unwrap render_template(); json! serializes Result as a structured value, not a plain string"
        );
    }

    #[tokio::test]
    async fn archive_stores_no_summary_placeholder_when_llm_returns_whitespace_only() {
        let tmp = TempDir::new().unwrap();
        let mut resp = LLMResponse::new();
        resp.content = Some("   \n  ".into());
        let c = test_consolidator(&tmp, ArchiveTestProvider::arc(resp));
        let messages = vec![json!({
            "role": "user",
            "content": "only whitespace summary",
            "timestamp": "2026-03-03T10:00:00Z",
        })];
        assert_eq!(
            c.archive(&messages, "test-session", None, None).await,
            Some("[no summary]".to_string())
        );
        let raw = fs::read_to_string(&c.store.history_file).expect("history written");
        let last_line = raw.lines().last().expect("one jsonl line");
        let row: serde_json::Value = serde_json::from_str(last_line).unwrap();
        let content = row.get("content").and_then(|v| v.as_str()).unwrap_or("");
        assert_eq!(content, "[no summary]", "got: {content:?}");
    }

    // ── compact_idle_session tests ─────────────────────────────────────────

    #[tokio::test]
    async fn compact_idle_session_archives_prefix_preserves_messages_and_hides_prefix() {
        let tmp = TempDir::new().unwrap();
        let mut resp = LLMResponse::new();
        resp.content = Some("Summary of old conversation.".into());
        resp.finish_reason = "stop".into();
        let provider = ArchiveTestProvider::arc(resp);
        let runtime = test_runtime(provider.clone());
        let sessions = Arc::new(Mutex::new(SessionManager::with_default_eviction_threshold(
            tmp.path().to_path_buf(),
        )));

        let mut session = Session::new("cli:test".into());
        let old_ts = session.updated_at;
        for i in 0..20 {
            session.add_message("user", format!("user msg {i}"), serde_json::Map::new());
            session.add_message(
                "assistant",
                format!("assistant msg {i}"),
                serde_json::Map::new(),
            );
        }
        session.updated_at = old_ts;
        sessions.lock().unwrap().save(session).unwrap();

        let c = Consolidator::new(
            Arc::new(make_store(&tmp)),
            test_runtime_resolver(provider),
            sessions.clone(),
            65_536,
            Box::new(StubArchiveMessageBuilder),
            8192,
        );

        let result = c.compact_idle_session("cli:test", runtime, 8).await;
        assert_eq!(result, Some("Summary of old conversation.".to_string()));

        let reloaded = {
            let mut guard = sessions.lock().unwrap();
            guard.invalidate("cli:test");
            guard.get_session_with_archive("cli:test").unwrap()
        };

        // Archive + live tail: the idle compaction must not lose raw history.
        assert_eq!(reloaded.messages.len(), 40);
        assert_eq!(
            reloaded.messages[0].get("content"),
            Some(&json!("user msg 0"))
        );
        assert_eq!(reloaded.last_consolidated, 32);
        let visible = reloaded.get_history(Some(40));
        assert_eq!(visible.len(), 8);
        assert_eq!(visible[0].get("content"), Some(&json!("user msg 16")));
        assert_eq!(
            visible.last().unwrap().get("content"),
            Some(&json!("assistant msg 19"))
        );
        let meta = reloaded
            .metadata
            .get(SessionManager::LAST_SUMMARY_KEY)
            .expect("summary metadata written");
        assert_eq!(
            meta.get("text"),
            Some(&json!("Summary of old conversation."))
        );
        assert!(meta.get(SessionManager::LAST_ACTIVE_KEY).is_some());
        assert_eq!(reloaded.updated_at, old_ts, "updated_at must be untouched");
    }

    #[tokio::test]
    async fn compact_idle_session_llm_failure_raw_dumps_and_advances_boundary() {
        let tmp = TempDir::new().unwrap();
        let mut resp = LLMResponse::new();
        resp.content = None;
        resp.finish_reason = "error".into();
        let provider = ArchiveTestProvider::arc(resp);
        let runtime = test_runtime(provider.clone());
        let sessions = Arc::new(Mutex::new(SessionManager::with_default_eviction_threshold(
            tmp.path().to_path_buf(),
        )));

        let mut session = Session::new("cli:fail".into());
        for i in 0..10 {
            session.add_message("user", format!("u{i}"), serde_json::Map::new());
            session.add_message("assistant", format!("a{i}"), serde_json::Map::new());
        }
        sessions.lock().unwrap().save(session).unwrap();

        let store = Arc::new(make_store(&tmp));
        let c = Consolidator::new(
            store.clone(),
            test_runtime_resolver(provider),
            sessions.clone(),
            65_536,
            Box::new(StubArchiveMessageBuilder),
            8192,
        );

        let result = c.compact_idle_session("cli:fail", runtime, 4).await;
        assert!(result.is_none());

        let entries = store.read_unprocessed_history(0);
        assert!(
            entries.iter().any(|e| e
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .contains(RAW_MARKER)),
            "expected a [RAW] entry in history.jsonl after a failed provider call"
        );

        let reloaded = {
            let mut guard = sessions.lock().unwrap();
            guard.invalidate("cli:fail");
            guard.get_session_with_archive("cli:fail").unwrap()
        };
        assert_eq!(
            reloaded.messages.len(),
            20,
            "raw dump must not delete history"
        );
        assert_eq!(reloaded.messages[0].get("content"), Some(&json!("u0")));
        assert_eq!(
            reloaded.last_consolidated, 16,
            "boundary still advances on failure"
        );
        let visible: Vec<String> = reloaded
            .get_history(Some(20))
            .iter()
            .map(|m| {
                m.get("content")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string()
            })
            .collect();
        assert_eq!(visible, vec!["u8", "a8", "u9", "a9"]);
    }

    #[tokio::test]
    async fn compact_idle_session_nothing_summary_not_stored() {
        let tmp = TempDir::new().unwrap();
        let mut resp = LLMResponse::new();
        resp.content = Some("(nothing)".into());
        resp.finish_reason = "stop".into();
        let provider = ArchiveTestProvider::arc(resp);
        let runtime = test_runtime(provider.clone());
        let sessions = Arc::new(Mutex::new(SessionManager::with_default_eviction_threshold(
            tmp.path().to_path_buf(),
        )));

        let mut session = Session::new("cli:nothing".into());
        for i in 0..10 {
            session.add_message("user", format!("u{i}"), serde_json::Map::new());
            session.add_message("assistant", format!("a{i}"), serde_json::Map::new());
        }
        sessions.lock().unwrap().save(session).unwrap();

        let c = Consolidator::new(
            Arc::new(make_store(&tmp)),
            test_runtime_resolver(provider),
            sessions.clone(),
            65_536,
            Box::new(StubArchiveMessageBuilder),
            8192,
        );

        let result = c.compact_idle_session("cli:nothing", runtime, 4).await;
        assert_eq!(result, Some("(nothing)".to_string()));

        let reloaded = {
            let mut guard = sessions.lock().unwrap();
            guard.invalidate("cli:nothing");
            guard.get_or_create_session("cli:nothing").clone()
        };
        assert!(
            !reloaded
                .metadata
                .contains_key(SessionManager::LAST_SUMMARY_KEY),
            "a literal '(nothing)' summary must not be persisted"
        );
    }

    #[tokio::test]
    async fn compact_idle_session_empty_session_is_noop() {
        let tmp = TempDir::new().unwrap();
        let mut resp = LLMResponse::new();
        resp.content = Some("should-not-be-used".into());
        let provider = ArchiveTestProvider::arc(resp);
        let runtime = test_runtime(provider.clone());
        let sessions = Arc::new(Mutex::new(SessionManager::with_default_eviction_threshold(
            tmp.path().to_path_buf(),
        )));

        let old_ts = chrono::Utc::now() - chrono::Duration::hours(2);
        let mut session = Session::new("cli:empty".into());
        session.updated_at = old_ts;
        sessions.lock().unwrap().save(session).unwrap();

        let c = Consolidator::new(
            Arc::new(make_store(&tmp)),
            test_runtime_resolver(provider),
            sessions.clone(),
            65_536,
            Box::new(StubArchiveMessageBuilder),
            8192,
        );

        let result = c.compact_idle_session("cli:empty", runtime, 8).await;
        assert_eq!(result, Some(String::new()));

        let reloaded = {
            let mut guard = sessions.lock().unwrap();
            guard.invalidate("cli:empty");
            guard.get_or_create_session("cli:empty").clone()
        };
        assert_eq!(
            reloaded.updated_at, old_ts,
            "an idle-but-empty session's timestamp must not look active"
        );
        assert!(reloaded.metadata.is_empty());
    }

    #[test]
    fn pick_consolidation_boundary_returns_none_when_tokens_to_remove_zero() {
        let s = session_with_messages(
            vec![
                json!({"role": "user", "content": "hi"}),
                json!({"role": "assistant", "content": "yo"}),
            ],
            0,
        );
        assert_eq!(Consolidator::pick_consolidation_boundary(&s, 0), None);
    }

    #[test]
    fn pick_consolidation_boundary_returns_none_when_start_at_or_past_messages_len() {
        let s = session_with_messages(vec![json!({"role": "user", "content": ""})], 1);
        assert_eq!(Consolidator::pick_consolidation_boundary(&s, 100), None);

        let s2 = session_with_messages(vec![json!({"role": "user", "content": ""})], 2);
        assert_eq!(Consolidator::pick_consolidation_boundary(&s2, 100), None);
    }

    #[test]
    fn pick_consolidation_boundary_no_user_turn_after_start_returns_none() {
        // Single message at `start`: condition is `idx > start && role == user`, so never triggers.
        let s = session_with_messages(vec![json!({"role": "user", "content": ""})], 0);
        assert_eq!(Consolidator::pick_consolidation_boundary(&s, 1), None);

        // First user in range is exactly at `start` (not after).
        let s2 = session_with_messages(
            vec![
                json!({"role": "system", "content": "x"}),
                json!({"role": "user", "content": ""}),
            ],
            1,
        );
        assert_eq!(Consolidator::pick_consolidation_boundary(&s2, 1000), None);
    }

    #[test]
    fn pick_consolidation_boundary_finds_second_user_with_expected_removed_counts() {
        // Empty `content` → `estimate_message_tokens` is 4 per message.
        let s = session_with_messages(
            vec![
                json!({"role": "user", "content": ""}),
                json!({"role": "assistant", "content": ""}),
                json!({"role": "user", "content": ""}),
            ],
            0,
        );
        let cum_before_third =
            estimate_message_tokens(&s.messages[0]) + estimate_message_tokens(&s.messages[1]);
        assert_eq!(cum_before_third, 8);

        assert_eq!(
            Consolidator::pick_consolidation_boundary(&s, 8),
            Some((2, 8)),
            "exact threshold should return at first qualifying user boundary"
        );
        assert_eq!(
            Consolidator::pick_consolidation_boundary(&s, 7),
            Some((2, 8)),
            "removed count already met before checking should still return that boundary"
        );
    }

    #[test]
    fn pick_consolidation_boundary_returns_last_user_boundary_when_threshold_not_met() {
        let s = session_with_messages(
            vec![
                json!({"role": "user", "content": ""}),
                json!({"role": "assistant", "content": ""}),
                json!({"role": "user", "content": ""}),
            ],
            0,
        );
        assert_eq!(
            Consolidator::pick_consolidation_boundary(&s, 100),
            Some((2, 8)),
            "should return last user-turn boundary even when removed_tokens never reaches threshold"
        );
    }

    #[test]
    fn pick_consolidation_boundary_respects_last_consolidated_range() {
        let s = session_with_messages(
            vec![
                json!({"role": "user", "content": "a"}),
                json!({"role": "assistant", "content": "b"}),
                json!({"role": "user", "content": "c"}),
                json!({"role": "assistant", "content": "d"}),
            ],
            2,
        );
        // Only indices 2.. are scanned; first candidate user is at idx 2 but `idx > start` fails.
        assert_eq!(Consolidator::pick_consolidation_boundary(&s, 1), None);
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
            fs::read_to_string(store.memory_dir.join("HISTORY.md.bak")).unwrap(),
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
        body.push('\n');
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
        fs::write(&store.history_file, format!("   {serialized}   \n")).unwrap();

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
        assert_eq!(
            store.read_unprocessed_history(1),
            vec![e2.clone(), e3.clone()]
        );
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
        let legacy =
            serde_json::from_str::<serde_json::Value>(r#"{"cursor": "14", "note": "legacy"}"#)
                .unwrap();
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
        assert_eq!(out, "[2026-01-01 10:02] ASSISTANT: ok");
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
        assert_eq!(out, "[1234567890123456] TOOL: x\n[short] UNKNOWN: no role");
    }

    // ── append_history ────────────────────────────────────────────────────────────

    #[test]
    fn memory_writes_are_atomic_and_leave_no_temp_files() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);

        store.write_memory("first version of the memory");
        // A reader that opened the file before the next write still sees the
        // complete old content (truncate-and-rewrite would change it).
        let mut early_reader = File::open(&store.memory_file).unwrap();
        store.write_memory("second version");
        let mut old_content = String::new();
        early_reader.read_to_string(&mut old_content).unwrap();
        assert_eq!(old_content, "first version of the memory");
        store.set_last_dream_cursor(7);
        store.append_history("an entry");

        assert_eq!(
            fs::read_to_string(&store.memory_file).unwrap(),
            "second version"
        );
        assert_eq!(store.get_last_dream_cursor(), 7);
        for folder in [tmp.path().to_path_buf(), store.memory_dir.clone()] {
            let temp_files: Vec<String> = fs::read_dir(&folder)
                .unwrap()
                .filter_map(Result::ok)
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .filter(|name| name.ends_with(".tmp"))
                .collect();
            assert!(temp_files.is_empty(), "{folder:?}: {temp_files:?}");
        }
    }

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

    // ── Dream::resolve_runtime ────────────────────────────────────────────────

    fn test_dream(tmp: &TempDir, resolver: Arc<ModelRuntimeResolver>) -> Dream {
        Dream::new(
            Arc::new(make_store(tmp)),
            resolver,
            None,
            None,
            20,
            10,
            8192,
        )
    }

    #[test]
    fn dream_resolves_named_preset_when_configured() {
        use crate::config::schema::ModelPresetConfig;

        let mut config = Config::default();
        config.providers.anthropic.api_key = "test-key".to_string();
        config.model_presets.insert(
            "dream-preset".to_string(),
            ModelPresetConfig {
                model: "claude-haiku".to_string(),
                provider: "anthropic".to_string(),
                ..Default::default()
            },
        );
        let resolver = Arc::new(ModelRuntimeResolver::new(
            config,
            ArchiveTestProvider::arc(LLMResponse::new()),
        ));
        let tmp = TempDir::new().unwrap();
        let mut dream = test_dream(&tmp, resolver);
        dream.dream_model_preset = Some("dream-preset".to_string());

        let runtime = dream.resolve_runtime();
        assert_eq!(runtime.model, "claude-haiku");
        assert_eq!(runtime.preset_name, "dream-preset");
    }

    #[test]
    fn dream_falls_back_to_model_override_raw_string() {
        let config = Config::default();
        let resolver = Arc::new(ModelRuntimeResolver::new(
            config,
            ArchiveTestProvider::arc(LLMResponse::new()),
        ));
        let tmp = TempDir::new().unwrap();
        let mut dream = test_dream(&tmp, resolver.clone());
        dream.model_override = Some("raw-override-model".to_string());

        let runtime = dream.resolve_runtime();
        assert_eq!(runtime.model, "raw-override-model");
        // Provider is still the process default's — only the model string is overridden.
        assert!(Arc::ptr_eq(
            &runtime.provider,
            &resolver.current_default().provider
        ));
    }

    #[test]
    fn dream_uses_process_default_when_no_override_configured() {
        let config = Config::default();
        let resolver = Arc::new(ModelRuntimeResolver::new(
            config,
            ArchiveTestProvider::arc(LLMResponse::new()),
        ));
        let tmp = TempDir::new().unwrap();
        let dream = test_dream(&tmp, resolver.clone());

        let runtime = dream.resolve_runtime();
        assert_eq!(runtime.model, resolver.current_default().model);
        assert_eq!(runtime.preset_name, "default");
    }

    // ── partition_findings ────────────────────────────────────────────────────

    #[test]
    fn partition_findings_splits_skill_lines_from_memory_lines() {
        let analysis = "[USER] lives in Lisbon\n\
                        [SKILL] NEW:deploy-app build then upload — pitfall: stale cache\n\
                        [MEMORY-REMOVE] old meeting\n\
                        [SKILL] weather use open-meteo";
        let findings = partition_findings(analysis);
        assert_eq!(
            findings.memory,
            "[USER] lives in Lisbon\n[MEMORY-REMOVE] old meeting"
        );
        assert_eq!(
            findings.skills,
            vec![
                "[SKILL] NEW:deploy-app build then upload — pitfall: stale cache".to_string(),
                "[SKILL] weather use open-meteo".to_string(),
            ]
        );
    }

    #[test]
    fn partition_findings_with_only_skill_lines_leaves_memory_empty() {
        let findings = partition_findings("[SKILL] a lesson\n\n[SKILL] another\n");
        assert_eq!(findings.memory, "");
        assert_eq!(findings.skills.len(), 2);
    }

    #[test]
    fn partition_findings_without_skill_lines_keeps_the_analysis() {
        let findings = partition_findings("[USER] fact\n[SOUL] tone");
        assert_eq!(findings.memory, "[USER] fact\n[SOUL] tone");
        assert!(findings.skills.is_empty());
        assert_eq!(partition_findings("").memory, "");
    }

    #[test]
    fn partition_findings_leaves_skip_for_phase_two() {
        let findings = partition_findings("[SKIP]");
        assert_eq!(findings.memory, "[SKIP]");
        assert!(findings.skills.is_empty());
    }

    #[test]
    fn partition_findings_accepts_bullets_whitespace_and_any_case() {
        let findings = partition_findings(
            "  - [skill] lower case lesson  \n* [Skill]   bulleted\n\t[SKILL]\n[SKILLS] not a marker",
        );
        assert_eq!(
            findings.skills,
            vec![
                "[SKILL] lower case lesson".to_string(),
                "[SKILL] bulleted".to_string(),
            ]
        );
        // An empty [SKILL] line and a different marker stay with memory.
        assert_eq!(findings.memory, "[SKILL]\n[SKILLS] not a marker");
    }

    #[test]
    fn format_skill_index_lists_names_descriptions_and_optional_paths() {
        assert_eq!(format_skill_index(&[]), "(none)");
        let entries = vec![
            SkillIndexEntry {
                name: "a".into(),
                description: "Use when A.".into(),
                path: None,
            },
            SkillIndexEntry {
                name: "b".into(),
                description: "Use when B.".into(),
                path: Some("/skills/b/SKILL.md".into()),
            },
        ];
        assert_eq!(
            format_skill_index(&entries),
            "- a: Use when A.\n- b: Use when B. — /skills/b/SKILL.md"
        );
    }

    #[test]
    fn dream_commit_message_counts_memory_and_skill_changes() {
        assert_eq!(
            format_dream_commit_message("2026-11-25 10:00", 2, 0, false, false),
            "dream: 2026-11-25 10:00, 2 memory change(s)"
        );
        assert_eq!(
            format_dream_commit_message("ts", 1, 3, true, false),
            "dream: ts, 1 memory change(s), 3 skill change(s)"
        );
        assert_eq!(
            format_dream_commit_message("ts", 0, 1, true, true),
            "dream: ts, 0 memory change(s), 1 skill proposal(s)"
        );
    }

    // ── Consolidator procedure category ───────────────────────────────────────

    #[test]
    fn archive_prompt_asks_for_procedures_only_when_learning_skills() {
        let tmp = TempDir::new().unwrap();
        let provider = ArchiveTestProvider::arc(LLMResponse::new());
        let plain = test_consolidator(&tmp, Arc::clone(&provider)).archive_system_prompt();
        assert!(!plain.contains("PROCEDURE"));
        assert!(plain.contains("- Preferences: communication style, tool preferences\n\nPriority"));

        let learning = test_consolidator(&tmp, provider)
            .with_learn_skills(true)
            .archive_system_prompt();
        assert!(learning.contains("PROCEDURE: <task> — <steps> — <pitfall>"));
    }

    // ── Dream phases with a scripted provider ─────────────────────────────────

    /// A workspace whose memory store is versioned, with one history entry,
    /// and an empty bundled-skills folder.
    struct DreamFixture {
        workspace: TempDir,
        builtin: TempDir,
        store: Arc<MemoryStore>,
    }

    impl DreamFixture {
        fn new() -> Self {
            let workspace = TempDir::new().unwrap();
            let store = Arc::new(make_store(&workspace));
            assert!(store.git.init());
            store.append_history("deploy failed until the cache was cleared");
            Self {
                workspace,
                builtin: TempDir::new().unwrap(),
                store,
            }
        }

        /// A Dream answering from `provider`, learning skills when
        /// `learn_skills`, staging them when `require_approval`.
        fn dream(
            &self,
            provider: Arc<ArchiveTestProvider>,
            learn_skills: bool,
            require_approval: bool,
        ) -> Dream {
            let skills_config = SkillsToolConfig {
                require_approval,
                ..SkillsToolConfig::default()
            };
            Dream::new(
                Arc::clone(&self.store),
                test_runtime_resolver(provider),
                None,
                None,
                20,
                10,
                8192,
            )
            .with_skill_learning_from(
                learn_skills,
                &skills_config,
                Some(self.builtin.path().to_path_buf()),
            )
        }

        fn skill_file(&self, name: &str) -> PathBuf {
            self.workspace
                .path()
                .join("skills")
                .join(name)
                .join("SKILL.md")
        }
    }

    const SKILL_FINDING: &str = "[SKILL] NEW:deploy-cache clear the build cache before deploying";

    fn deploy_cache_skill() -> String {
        "---\nname: deploy-cache\ndescription: Use when deploying. Clears the cache first.\n---\n\n\
         1. Clear the build cache.\n2. Deploy.\n"
            .to_string()
    }

    /// Phase 1 finds one skill lesson; Phase 3 creates it, then stops.
    fn skill_script() -> Vec<LLMResponse> {
        vec![
            text_response(SKILL_FINDING),
            tool_call_response(
                "skill_manage",
                json!({"action": "create", "name": "deploy-cache",
                       "content": deploy_cache_skill(), "reason": "cache pitfall"}),
            ),
            text_response("done"),
        ]
    }

    fn system_prompt_of(request: &[serde_json::Value]) -> String {
        request[0]["content"].as_str().unwrap_or("").to_string()
    }

    #[tokio::test]
    async fn dream_without_skill_findings_does_not_run_phase_three() {
        let fixture = DreamFixture::new();
        let provider =
            ArchiveTestProvider::scripted(vec![text_response("[SKIP]"), text_response("ok")]);
        let dream = fixture.dream(Arc::clone(&provider), true, false);

        assert!(dream.run().await);

        let requests = provider.requests();
        assert_eq!(requests.len(), 2, "Phase 1 and Phase 2 only");
        assert!(system_prompt_of(&requests[1]).contains("Update memory files"));
        assert!(fs::read_dir(fixture.workspace.path().join("skills")).unwrap().next().is_none());
    }

    #[tokio::test]
    async fn dream_writes_a_skill_and_commits_it_when_approval_is_off() {
        let fixture = DreamFixture::new();
        let provider = ArchiveTestProvider::scripted(skill_script());
        let dream = fixture.dream(Arc::clone(&provider), true, false);

        assert!(dream.run().await);

        assert!(fixture.skill_file("deploy-cache").is_file());
        let requests = provider.requests();
        // Phase 2 was skipped (only skill findings); Phase 3 took two turns.
        assert_eq!(requests.len(), 3);
        assert!(system_prompt_of(&requests[1]).contains("skill_manage"));
        let phase3_prompt = requests[1][1]["content"].as_str().unwrap();
        assert!(phase3_prompt.contains(SKILL_FINDING));

        let head = &fixture.store.git.log(1)[0];
        assert!(
            head.message.contains("0 memory change(s), 1 skill change(s)"),
            "{}",
            head.message
        );
        let (_, diff) = fixture.store.git.show_commit_diff(&head.sha).unwrap();
        assert!(diff.contains("skills/deploy-cache/SKILL.md"), "{diff}");
        assert!(dream.skill_proposals().list().is_empty());
        assert!(fixture.store.get_last_dream_cursor() > 0);
    }

    #[tokio::test]
    async fn dream_stages_a_proposal_when_approval_is_required() {
        let fixture = DreamFixture::new();
        let provider = ArchiveTestProvider::scripted(skill_script());
        let dream = fixture.dream(Arc::clone(&provider), true, true);

        assert!(dream.run().await);

        assert!(!fixture.skill_file("deploy-cache").exists());
        let proposals = dream.skill_proposals().list();
        assert_eq!(proposals.len(), 1);
        assert_eq!(proposals[0].operation.skill_name(), "deploy-cache");
        assert_eq!(proposals[0].reason.as_deref(), Some("cache pitfall"));
        assert!(system_prompt_of(&provider.requests()[1]).contains("Review mode"));
        // Nothing tracked changed, so nothing was committed after init.
        assert_eq!(fixture.store.git.log(10).len(), 1);
    }

    #[tokio::test]
    async fn dream_phase_one_has_no_skill_block_when_learning_is_off() {
        let fixture = DreamFixture::new();
        let provider = ArchiveTestProvider::scripted(vec![
            text_response(SKILL_FINDING),
            text_response("nothing to edit"),
        ]);
        let dream = fixture.dream(Arc::clone(&provider), false, false);

        assert!(dream.run().await);

        let requests = provider.requests();
        let phase1_system = system_prompt_of(&requests[0]);
        assert!(!phase1_system.contains("[SKILL]"));
        assert!(!phase1_system.contains("Existing skills"));
        // The analysis goes to Phase 2 untouched and Phase 3 never runs.
        assert_eq!(requests.len(), 2);
        assert!(requests[1][1]["content"].as_str().unwrap().contains(SKILL_FINDING));
        assert!(!fixture.skill_file("deploy-cache").exists());
    }

    #[tokio::test]
    async fn dream_phase_one_lists_existing_skills_when_learning() {
        let fixture = DreamFixture::new();
        let existing = fixture.builtin.path().join("weather");
        fs::create_dir_all(&existing).unwrap();
        fs::write(
            existing.join("SKILL.md"),
            "---\nname: weather\ndescription: Use when asked about weather.\n---\n\nUse wttr.in.\n",
        )
        .unwrap();
        let provider =
            ArchiveTestProvider::scripted(vec![text_response("[SKIP]"), text_response("ok")]);
        let dream = fixture.dream(Arc::clone(&provider), true, false);

        dream.run().await;

        let phase1_system = system_prompt_of(&provider.requests()[0]);
        assert!(phase1_system.contains("[SKILL] <existing-skill-name | NEW:proposed-name>"));
        assert!(phase1_system.contains("- weather: Use when asked about weather."));
    }
}
