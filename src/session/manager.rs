use std::{
    collections::HashMap,
    fs::{self, File},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    sync::{Arc, LazyLock, Mutex},
};

use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};
use regex::Regex;
use serde_json::{Map, Value, json};

use tera::Context;

use crate::{
    config::paths::get_legacy_sessions_dir,
    providers::base::{LLMProviderDyn, LLMResponse},
    session::{
        SESSION_TITLE_METADATA_KEY, history_visibility::is_hidden_history_message,
        keys::COMMAND_KEY,
    },
    utils::helpers::{
        ensure_dir, find_legal_message_start, safe_filename, strip_think, truncate_text,
    },
    utils::prompt_templates::render_template,
};

/// Max stored title length in Unicode scalar values. Matches nanobot `TITLE_MAX_CHARS`.
const TITLE_MAX_CHARS: usize = 60;
/// Reasoning models count thinking tokens against this budget. 96 was enough
/// for a 3–8 word title from a non-reasoning model, but thinking-only replies
/// hit `finish_reason=length` with `content: None` and never persist a title.
const TITLE_GENERATION_MAX_TOKENS: usize = 512;
const TITLE_GENERATION_TEMPERATURE: f32 = 0.2;
const TITLE_INPUT_MAX_CHARS: usize = 1_000;

/// In-memory conversation session record.
#[derive(Clone)]
pub struct Session {
    pub key: String,
    pub messages: Vec<Value>,
    created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub metadata: HashMap<String, Value>,
    pub last_consolidated: usize,
}

impl Session {
    pub fn new(key: String) -> Self {
        Self {
            key,
            messages: Vec::new(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            metadata: HashMap::new(),
            last_consolidated: 0,
        }
    }

    /// Append a chat-style message (`role`, `content`, RFC3339 `timestamp`) and bump `updated_at`.
    ///
    /// `extras` is merged **after** the base fields—same semantics as Python `**kwargs`:
    /// duplicate keys in `extras` override `role`, `content`, and `timestamp`.
    pub fn add_message(
        &mut self,
        role: impl Into<String>,
        content: impl Into<String>,
        extras: Map<String, Value>,
    ) {
        let mut msg = Map::new();
        msg.insert("role".into(), Value::String(role.into()));
        msg.insert("content".into(), Value::String(content.into()));
        msg.insert("timestamp".into(), Value::String(Utc::now().to_rfc3339()));
        msg.extend(extras);

        self.messages.push(Value::Object(msg));
        self.updated_at = Utc::now();
    }

    /// Recent messages for prompting, from `last_consolidated` onward (trimmed and normalized).
    /// Messages tagged with [`COMMAND_KEY`] are omitted.
    ///
    /// * `max_messages` — keep at most this many messages from the **end** of that window before
    ///   user-turn alignment. `None` means 500. **`Some(0)` is treated like `None`** (also 500): a
    ///   literal `0` here used to keep zero rows and surprise callers that expected “default” or
    ///   “unlimited”.
    pub fn get_history(&self, max_messages: Option<usize>) -> Vec<Value> {
        const DEFAULT_CAP: usize = 500;
        let max_messages = match max_messages {
            None | Some(0) => DEFAULT_CAP,
            Some(n) => n,
        };
        let unconsolidated =
            self.messages[(self.last_consolidated).min(self.messages.len())..].to_vec();
        let mut sliced = if unconsolidated.len() > max_messages {
            unconsolidated[unconsolidated.len() - max_messages..].to_vec()
        } else {
            unconsolidated
        };

        // Avoid starting mid-turn when possible.
        for i in 0..sliced.len() {
            if let Some(role) = sliced[i].get("role").and_then(|v| v.as_str()) {
                if role == "user" {
                    sliced = sliced[i..].to_vec();
                    break;
                }
            }
        }

        let mut out: Vec<Value> = Vec::new();
        for message in sliced {
            if message.get(COMMAND_KEY).is_some() {
                continue;
            }
            let mut entry = json!({
                "role": message.get("role").and_then(|v| v.as_str()).unwrap_or(""),
                "content": message.get("content").and_then(|v| v.as_str()).unwrap_or(""),
                "timestamp": message.get("timestamp").and_then(|v| v.as_str()).unwrap_or(""),
            });
            for key in vec!["tool_calls", "tool_call_id", "name", "reasoning_content"] {
                if message.get(key).is_some() {
                    entry[key] = message.get(key).unwrap().clone();
                }
            }
            out.push(entry);
        }
        out
    }

    /// Clears conversation messages and resets consolidation cursor.
    pub fn clear(&mut self) {
        self.messages.clear();
        self.last_consolidated = 0;
        self.updated_at = Utc::now();
    }

    /// Keep a legal recent suffix, mirroring get_history boundary rules.
    pub fn retain_recent_legal_suffix(&mut self, max_messages: usize) {
        if max_messages == 0 {
            self.clear();
            return;
        }
        if self.messages.len() <= max_messages {
            return;
        }

        let prev_last = self.last_consolidated;
        let mut start_idx = self.messages.len() - max_messages;

        // If the cutoff lands mid-turn, extend backward to the nearest user turn.
        while start_idx > 0
            && self.messages[start_idx]
                .get("role")
                .and_then(|v| v.as_str())
                != Some("user")
        {
            start_idx -= 1;
        }

        self.messages = self.messages[start_idx..].to_vec();
        let mut last_consolidated = prev_last.saturating_sub(start_idx);

        let legal_trim = find_legal_message_start(&self.messages);
        if legal_trim > 0 {
            self.messages = self.messages[legal_trim..].to_vec();
            last_consolidated = last_consolidated.saturating_sub(legal_trim);
        }

        self.last_consolidated = last_consolidated;
        self.updated_at = Utc::now();
    }
}

/// Read-only session snapshot (nanobot's `SessionPayload` TypedDict).
#[derive(Debug, Clone)]
pub struct SessionPayload {
    pub key: String,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub metadata: HashMap<String, Value>,
    pub messages: Vec<HashMap<String, Value>>,
}

fn value_as_object_map(value: Value) -> Result<HashMap<String, Value>, String> {
    match value {
        Value::Object(map) => Ok(map.into_iter().collect()),
        _ => Err("session records must be JSON objects".to_string()),
    }
}

/// Rename `src` to `dst`, or copy + remove `src` if rename fails (e.g. cross-volume).
fn migrate_session_file(src: &Path, dst: &Path) -> std::io::Result<()> {
    match fs::rename(src, dst) {
        Ok(()) => Ok(()),
        Err(_) => {
            fs::copy(src, dst)?;
            fs::remove_file(src)?;
            Ok(())
        }
    }
}

fn json_value_as_last_consolidated(v: &Value) -> usize {
    v.as_u64()
        .map(|u| u as usize)
        .or_else(|| v.as_i64().map(|i| i.max(0) as usize))
        .unwrap_or(0)
}

fn title_inputs(session: &Session) -> (String, String) {
    let mut user_text = String::new();
    let mut assistant_text = String::new();
    for message in &session.messages {
        if message.get(COMMAND_KEY) == Some(&Value::Bool(true)) {
            continue;
        }
        if is_hidden_history_message(message) {
            continue;
        }
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let Some(content) = message.get("content").and_then(Value::as_str) else {
            continue;
        };
        if content.trim().is_empty() {
            continue;
        }
        let content = strip_think(content);
        if content.is_empty() {
            continue;
        }
        if role == "user" && user_text.is_empty() {
            user_text = content;
        } else if role == "assistant" && assistant_text.is_empty() {
            assistant_text = content;
        }
        if !user_text.is_empty() && !assistant_text.is_empty() {
            break;
        }
    }
    (user_text, assistant_text)
}

fn title_generation_prompt(
    user_text: &str,
    assistant_text: &str,
) -> Result<(String, String), String> {
    let user_text = truncate_text(user_text, TITLE_INPUT_MAX_CHARS);
    let assistant_text = if assistant_text.is_empty() {
        String::new()
    } else {
        truncate_text(assistant_text, TITLE_INPUT_MAX_CHARS)
    };
    let mut system_ctx = Context::new();
    system_ctx.insert("part", "system");
    let system = render_template("history/title_generation.md", &system_ctx, true)?;

    let mut user_ctx = Context::new();
    user_ctx.insert("part", "user");
    user_ctx.insert("user", &user_text);
    user_ctx.insert("assistant", &assistant_text);
    let user = render_template("history/title_generation.md", &user_ctx, true)?;
    Ok((system, user))
}

/// Failure from [`SessionManager::rename_session`].
#[derive(Debug)]
pub enum RenameSessionError {
    /// No session file (and nothing in cache) for this key.
    NotFound,
    /// The title was written in memory but could not be flushed to disk.
    Save(std::io::Error),
}

pub struct SessionManager {
    pub workspace: PathBuf,
    pub sessions_dir: PathBuf,
    pub legacy_sessions_dir: PathBuf,
    cache: HashMap<String, Session>,
}

impl SessionManager {
    pub fn new(workspace: PathBuf) -> Self {
        Self {
            workspace: workspace.clone(),
            sessions_dir: ensure_dir(workspace.join("sessions")),
            legacy_sessions_dir: get_legacy_sessions_dir(),
            cache: HashMap::new(),
        }
    }

    /// Get the file path for a session.
    ///
    /// # Arguments
    ///
    /// * `key` - The key of the session.
    ///
    /// # Returns
    ///
    /// The file path for the session.
    fn get_session_path(&self, key: &str) -> PathBuf {
        let safe_key = safe_filename(key);
        return self.sessions_dir.join(format!("{}.jsonl", safe_key));
    }

    fn get_legacy_session_path(&self, key: &str) -> PathBuf {
        let safe_key = safe_filename(key);
        return self.legacy_sessions_dir.join(format!("{}.jsonl", safe_key));
    }

    /// Existing session from cache or disk. Does not create a session and does
    /// not insert a disk load into the cache — that is [`Self::get_or_create_session`].
    pub(crate) fn get_session_internal(&self, session_key: &str) -> Option<Session> {
        if let Some(session) = self.cache.get(session_key) {
            return Some(session.clone());
        }
        self.load(session_key)
    }

    /// Get an existing session or create a new one.
    ///
    /// Cache first; on a miss, load from disk or insert a fresh session.
    pub fn get_or_create_session(&mut self, key: &str) -> &mut Session {
        if !self.cache.contains_key(key) {
            let session = self
                .load(key)
                .unwrap_or_else(|| Session::new(key.to_string()));
            self.cache.insert(key.to_string(), session);
        }
        self.cache.get_mut(key).expect("session is in cache")
    }

    fn load(&self, key: &str) -> Option<Session> {
        let path = self.get_session_path(key);
        if !path.exists() {
            let legacy_path = self.get_legacy_session_path(key);
            if legacy_path.exists() {
                if let Some(parent) = path.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                match migrate_session_file(&legacy_path, &path) {
                    Ok(()) => log::info!("Migrated session {} from legacy path", key),
                    Err(e) => log::error!("Failed to migrate session {}: {:?}", key, e),
                }
            }
        }
        if !path.exists() {
            return None;
        }

        let file = match File::open(&path) {
            Ok(f) => f,
            Err(e) => {
                log::warn!("Failed to open session file {}: {}", path.display(), e);
                return None;
            }
        };

        let mut messages: Vec<Value> = Vec::new();
        let mut metadata: HashMap<String, Value> = HashMap::new();
        let mut created_at = Utc::now();
        let mut last_consolidated: usize = 0;

        let reader = BufReader::new(file);
        for line_result in reader.lines() {
            let line = match line_result {
                Ok(l) => l,
                Err(e) => {
                    log::warn!("Failed reading session {} line: {}", key, e);
                    continue;
                }
            };
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let data: Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(e) => {
                    log::warn!("Skipping invalid JSON in session {}: {}", key, e);
                    continue;
                }
            };
            if let Some(data_type) = data.get("_type")
                && let Some(data_type_str) = data_type.as_str()
                && data_type_str == "metadata"
            {
                let metadata_data = data.get("metadata").cloned().unwrap_or(Value::Null);
                if let Some(meta_obj) = metadata_data.as_object() {
                    for (meta_key, value) in meta_obj.iter() {
                        metadata.insert(meta_key.clone(), value.clone());
                    }
                } else if !metadata_data.is_null() {
                    log::warn!(
                        "Session {} metadata line has non-object metadata field; skipping merge",
                        key
                    );
                }
                if let Some(created_at_val) = data.get("created_at") {
                    if let Some(created_at_str) = created_at_val.as_str() {
                        let parsed =
                            NaiveDateTime::parse_from_str(created_at_str, "%Y-%m-%dT%H:%M:%S%.f")
                                .or_else(|_| {
                                    NaiveDateTime::parse_from_str(
                                        created_at_str,
                                        "%Y-%m-%dT%H:%M:%S",
                                    )
                                });
                        if let Ok(naive_dt) = parsed {
                            created_at = Utc.from_utc_datetime(&naive_dt);
                        } else if let Ok(dt) = DateTime::parse_from_rfc3339(created_at_str) {
                            created_at = dt.with_timezone(&Utc);
                        }
                    }
                }
                if let Some(v) = data.get("last_consolidated") {
                    last_consolidated = json_value_as_last_consolidated(v);
                }
            } else {
                messages.push(data);
            }
        }

        Some(Session {
            key: key.to_string(),
            messages,
            created_at,
            updated_at: Utc::now(),
            metadata,
            last_consolidated,
        })
    }

    /// Save a session to disk and update the cache.
    ///
    /// Writes a single metadata line followed by one line per message (JSONL).
    /// Returns an error if the file cannot be created or written.
    pub fn save(&mut self, session: Session) -> std::io::Result<()> {
        let path = self.get_session_path(&session.key);

        let mut file = File::create(&path)?;
        log::info!("Saving session to {}", path.display());

        let metadata_line = json!({
            "_type": "metadata",
            "key": session.key,
            "created_at": session.created_at.to_rfc3339(),
            "updated_at": session.updated_at.to_rfc3339(),
            "metadata": session.metadata,
            "last_consolidated": session.last_consolidated,
        });
        writeln!(file, "{}", serde_json::to_string(&metadata_line)?)?;

        for msg in &session.messages {
            writeln!(file, "{}", serde_json::to_string(msg)?)?;
        }

        self.cache.insert(session.key.clone(), session);
        Ok(())
    }

    pub fn invalidate(&mut self, key: &str) -> Option<Session> {
        self.cache.remove(key)
    }

    pub fn list_sessions(&self) -> Vec<Value> {
        let mut sessions = Vec::new();
        let entries = match std::fs::read_dir(&self.sessions_dir) {
            Ok(e) => e,
            Err(e) => {
                log::warn!("Failed to list sessions dir: {}", e);
                return sessions;
            }
        };
        for path in entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("jsonl"))
        {
            // Read just the metadata line
            if let Ok(file) = File::open(&path) {
                let reader = BufReader::new(file);
                // Read single line from reader
                if let Some(line_result) = reader.lines().next() {
                    if let Ok(line) = line_result {
                        if let Ok(metadata) = serde_json::from_str::<Value>(&line) {
                            if let Some(metadata_type) = metadata.get("_type")
                                && let Some(metadata_type_str) = metadata_type.as_str()
                                && metadata_type_str == "metadata"
                            {
                                if let Some(key) = metadata
                                    .get("key")
                                    .and_then(|v| v.as_str())
                                    .filter(|k| !k.is_empty())
                                {
                                    sessions.push(json!({
                                        "key": key,
                                        "created_at": metadata.get("created_at").and_then(|v| v.as_str()).unwrap_or(""),
                                        "updated_at": metadata.get("updated_at").and_then(|v| v.as_str()).unwrap_or(""),
                                        "path": path.display().to_string(),
                                        "title": listed_session_title(&metadata),
                                    }));
                                }
                            }
                        }
                    }
                }
            }
        }
        sessions.sort_by(|a, b| {
            let a_ts = a.get("updated_at").and_then(|v| v.as_str()).unwrap_or("");
            let b_ts = b.get("updated_at").and_then(|v| v.as_str()).unwrap_or("");
            b_ts.cmp(a_ts)
        });
        sessions
    }

    /// Read-only session view from disk (nanobot's `SessionManager.read`).
    ///
    /// Unlike [`Self::load`], this preserves raw timestamp strings, does not
    /// migrate legacy paths, and on corrupt input attempts [`Self::repair`]
    /// before giving up.
    pub fn read_session_file(&self, key: &str) -> Option<SessionPayload> {
        let path: PathBuf = self.get_session_path(key);
        if !path.exists() {
            return None;
        }

        match self.try_read_session_payload(key, &path) {
            Ok(payload) => Some(payload),
            Err(e) => {
                log::warn!("Failed to read session {}: {}", key, e);
                let repaired = self.repair(key, Some(&path))?;
                log::info!("Recovered read-only session view {} from corrupt file", key);
                Some(Self::session_payload(&repaired))
            }
        }
    }

    fn try_read_session_payload(&self, key: &str, path: &Path) -> Result<SessionPayload, String> {
        let file = File::open(path).map_err(|e| e.to_string())?;
        let reader = BufReader::new(file);

        let mut messages: Vec<HashMap<String, Value>> = Vec::new();
        let mut metadata: HashMap<String, Value> = HashMap::new();
        let mut created_at: Option<String> = None;
        let mut updated_at: Option<String> = None;
        let mut stored_key: Option<String> = None;

        for line_result in reader.lines() {
            let line = line_result.map_err(|e| e.to_string())?;
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let raw_data: Value = serde_json::from_str(line).map_err(|e| e.to_string())?;
            let data = value_as_object_map(raw_data)?;

            if data.get("_type").and_then(|v| v.as_str()) == Some("metadata") {
                metadata = match data.get("metadata") {
                    Some(Value::Object(map)) => {
                        map.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
                    }
                    Some(_) => HashMap::new(),
                    None => HashMap::new(),
                };
                created_at = data
                    .get("created_at")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                updated_at = data
                    .get("updated_at")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                stored_key = data
                    .get("key")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
            } else {
                messages.push(data);
            }
        }

        Ok(SessionPayload {
            key: stored_key.unwrap_or_else(|| key.to_string()),
            created_at,
            updated_at,
            metadata,
            messages,
        })
    }

    /// Best-effort recovery from a corrupt JSONL session file (nanobot's
    /// `SessionManager.repair`). Skips bad lines instead of failing the read.
    fn repair(&self, key: &str, path: Option<&Path>) -> Option<Session> {
        let default_path = self.get_session_path(key);
        let path = path.unwrap_or(default_path.as_path());
        if !path.exists() {
            return None;
        }

        let file = match File::open(path) {
            Ok(f) => f,
            Err(e) => {
                log::warn!("Repair failed for session {}: {}", key, e);
                return None;
            }
        };

        let mut messages: Vec<Value> = Vec::new();
        let mut metadata: HashMap<String, Value> = HashMap::new();
        let mut created_at: Option<DateTime<Utc>> = None;
        let mut updated_at: Option<DateTime<Utc>> = None;
        let mut last_consolidated: usize = 0;
        let mut skipped = 0usize;

        let reader = BufReader::new(file);
        for line_result in reader.lines() {
            let line = match line_result {
                Ok(l) => l,
                Err(_) => {
                    skipped += 1;
                    continue;
                }
            };
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let raw_data: Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(_) => {
                    skipped += 1;
                    continue;
                }
            };
            let Some(data) = raw_data.as_object() else {
                skipped += 1;
                continue;
            };

            if data.get("_type").and_then(|v| v.as_str()) == Some("metadata") {
                metadata = match data.get("metadata") {
                    Some(Value::Object(map)) => {
                        map.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
                    }
                    _ => HashMap::new(),
                };
                if let Some(s) = data
                    .get("created_at")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                {
                    created_at = parse_session_timestamp(s);
                }
                if let Some(s) = data
                    .get("updated_at")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                {
                    updated_at = parse_session_timestamp(s);
                }
                if let Some(v) = data.get("last_consolidated") {
                    last_consolidated = json_value_as_last_consolidated(v);
                }
            } else {
                messages.push(Value::Object(data.clone()));
            }
        }

        if skipped > 0 {
            log::warn!("Skipped {} corrupt lines in session {}", skipped, key);
        }
        if messages.is_empty() && metadata.is_empty() {
            return None;
        }

        Some(Session {
            key: key.to_string(),
            messages,
            created_at: created_at.unwrap_or_else(Utc::now),
            updated_at: updated_at.unwrap_or_else(Utc::now),
            metadata,
            last_consolidated,
        })
    }

    /// Build a [`SessionPayload`] from an in-memory [`Session`].
    fn session_payload(session: &Session) -> SessionPayload {
        let messages = session
            .messages
            .iter()
            .filter_map(|msg| match msg {
                Value::Object(map) => {
                    Some(map.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                }
                _ => None,
            })
            .collect();
        SessionPayload {
            key: session.key.clone(),
            created_at: Some(session.created_at.to_rfc3339()),
            updated_at: Some(session.updated_at.to_rfc3339()),
            metadata: session.metadata.clone(),
            messages,
        }
    }

    /// True when this session has user text and no stored title yet.
    pub fn session_needs_title(&self, session_key: &str) -> bool {
        let Some(session) = self.get_session_internal(session_key) else {
            return false;
        };
        let has_title = session
            .metadata
            .get(SESSION_TITLE_METADATA_KEY)
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.is_empty());
        if has_title {
            return false;
        }
        let (user_text, _) = title_inputs(&session);
        !user_text.is_empty()
    }

    /// Generate and persist a session title. Locks `sessions` only around the
    /// read and the save — the LLM call runs with the lock released.
    pub async fn generate_title(
        sessions: &Mutex<SessionManager>,
        session_key: &str,
        provider: Arc<dyn LLMProviderDyn>,
        model: &str,
    ) -> Option<String> {
        let (system, user) = {
            let manager = sessions.lock().unwrap_or_else(|e| e.into_inner());
            let Some(session) = manager.get_session_internal(session_key) else {
                log::error!("Session not found: {}", session_key);
                return None;
            };
            let title = session
                .metadata
                .get(SESSION_TITLE_METADATA_KEY)
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            if !title.is_empty() {
                return Some(title.to_string());
            }
            let (user_text, assistant_text) = title_inputs(&session);
            if user_text.is_empty() {
                return None;
            }
            match title_generation_prompt(&user_text, &assistant_text) {
                Ok(prompt) => prompt,
                Err(e) => {
                    log::error!("Failed to render title generation template: {e}");
                    return None;
                }
            }
        };
        let response = provider
            .chat_with_retry(
                vec![
                    json!({ "role": "system", "content": system }),
                    json!({ "role": "user", "content": user }),
                ],
                None,
                Some(model.to_string()),
                Some(TITLE_GENERATION_MAX_TOKENS),
                Some(TITLE_GENERATION_TEMPERATURE),
                Some("none".to_string()),
                None,
            )
            .await;
        log::info!("Title generation response: {:?}", response);
        let title = Self::title_from_llm_response(&response);
        if title.is_empty() || title.to_lowercase().starts_with("error") {
            log::debug!(
                "Title generation returned no usable title for {session_key} (finish_reason={})",
                response.finish_reason
            );
            return None;
        }

        let mut manager = sessions.lock().unwrap_or_else(|e| e.into_inner());
        manager.persist_generated_title(session_key, title)
    }

    /// Persist a new display title on an existing session. Does not create a
    /// session — missing keys return [`RenameSessionError::NotFound`].
    pub fn rename_session(
        &mut self,
        session_key: &str,
        title: &str,
    ) -> Result<(), RenameSessionError> {
        let Some(mut session) = self.get_session_internal(session_key) else {
            return Err(RenameSessionError::NotFound);
        };
        session
            .metadata
            .insert(SESSION_TITLE_METADATA_KEY.to_string(), json!(title));
        session.updated_at = Utc::now();
        self.save(session).map_err(RenameSessionError::Save)
    }

    fn persist_generated_title(&mut self, session_key: &str, title: String) -> Option<String> {
        let session = self.get_or_create_session(session_key);
        if let Some(existing) = session
            .metadata
            .get(SESSION_TITLE_METADATA_KEY)
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        {
            return Some(existing.to_string());
        }
        session
            .metadata
            .insert(SESSION_TITLE_METADATA_KEY.to_string(), json!(title.clone()));
        let snapshot = session.clone();
        if let Err(e) = self.save(snapshot) {
            log::error!("Failed to save generated title for {session_key}: {e}");
        }
        Some(title)
    }

    /// Prefer the model's visible `content`. When a reasoning model burns the
    /// token budget on thinking (`content: None`, `finish_reason=length`),
    /// fall back to the last 3–8 word quoted phrase in `reasoning_content`.
    fn title_from_llm_response(response: &LLMResponse) -> String {
        let from_content = Self::clean_generated_title(response.content.clone());
        if !from_content.is_empty() {
            return from_content;
        }
        Self::title_from_reasoning(response.reasoning_content.as_deref())
    }

    fn title_from_reasoning(reasoning: Option<&str>) -> String {
        let Some(reasoning) = reasoning.filter(|s| !s.is_empty()) else {
            return String::new();
        };
        static QUOTE_RE: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r#"["“]([^"”]+)["”]"#).expect("title quote regex"));
        let mut last = String::new();
        for caps in QUOTE_RE.captures_iter(reasoning) {
            let candidate = Self::clean_generated_title(Some(caps[1].to_string()));
            let words = candidate.split_whitespace().count();
            if (3..=8).contains(&words) {
                last = candidate;
            }
        }
        last
    }

    fn clean_generated_title(raw: Option<String>) -> String {
        static TITLE_PREFIX_RE: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r"(?i)^\s*(title|标题)\s*[:：]\s*").expect("title prefix regex")
        });

        let text = raw.unwrap_or_default();
        let text = text.trim();
        if text.is_empty() {
            return String::new();
        }

        let text = TITLE_PREFIX_RE.replace(text, "");
        let text = text
            .trim()
            .trim_matches(|c: char| matches!(c, '"' | '\'' | '`' | '“' | '”' | '‘' | '’'));
        let text = strip_think(text);
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        let text = text
            .trim_end_matches(|c: char| {
                matches!(
                    c,
                    '。' | '.' | '!' | '！' | '?' | '？' | ',' | '，' | ';' | '；' | ':'
                )
            })
            .to_string();

        if text.chars().count() > TITLE_MAX_CHARS {
            let mut truncated: String = text.chars().take(TITLE_MAX_CHARS - 1).collect();
            let end = truncated.trim_end().len();
            truncated.truncate(end);
            truncated.push('…');
            truncated
        } else {
            text
        }
    }
}

/// Title stored on the JSONL metadata line (`metadata.title`), if any.
fn listed_session_title(metadata_line: &Value) -> String {
    metadata_line
        .get("metadata")
        .and_then(|m| m.get(SESSION_TITLE_METADATA_KEY))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn parse_session_timestamp(s: &str) -> Option<DateTime<Utc>> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Utc));
    }
    let parsed = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f")
        .or_else(|_| NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S"));
    parsed.ok().map(|naive| Utc.from_utc_datetime(&naive))
}

/// Format persisted sessions for CLI or chat command output.
pub fn format_sessions_list(sessions: &[Value], current_key: Option<&str>) -> String {
    if sessions.is_empty() {
        return "No sessions available.".to_string();
    }

    let mut lines = vec!["Sessions (most recent first):".to_string()];
    for entry in sessions {
        let key = entry
            .get("key")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let updated = entry
            .get("updated_at")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty());
        let current = current_key == Some(key);
        let suffix = if current { " (current)" } else { "" };
        let title = entry
            .get("title")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty());
        let label = match title {
            Some(title) => format!("{title} [{key}]"),
            None => key.to_string(),
        };
        match updated {
            Some(ts) => lines.push(format!("- {label}{suffix} — updated {ts}")),
            None => lines.push(format!("- {label}{suffix}")),
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::HIDDEN_HISTORY_KEY;

    fn fixture_message(role: &str, content: &str) -> Value {
        json!({
            "role": role,
            "content": content,
            "timestamp": "2026-01-01T00:00:00Z",
        })
    }

    #[test]
    fn get_history_empty_returns_empty_vec() {
        let session = Session::new("s1".into());
        assert!(session.get_history(None).is_empty());
        assert!(session.get_history(Some(10)).is_empty());
    }

    #[test]
    fn get_history_skips_suffix_before_last_consolidated() {
        let mut session = Session::new("s1".into());
        session.messages.push(fixture_message("user", "before"));
        session
            .messages
            .push(fixture_message("assistant", "middle"));
        session.messages.push(fixture_message("user", "after"));
        session.last_consolidated = 1;
        // Window is [assistant, user]; alignment keeps from first user only.
        let h = session.get_history(None);
        assert_eq!(h.len(), 1);
        assert_eq!(h[0]["content"], json!("after"));
    }

    #[test]
    fn get_history_max_messages_keeps_tail_of_window() {
        let mut session = Session::new("s1".into());
        session.messages.push(fixture_message("assistant", "m0"));
        session.messages.push(fixture_message("user", "m1"));
        session.messages.push(fixture_message("assistant", "m2"));
        session.messages.push(fixture_message("user", "m3"));
        session.messages.push(fixture_message("assistant", "m4"));
        let h = session.get_history(Some(2));
        // Tail last two: user m3, assistant m4 — first message is already user → both kept.
        assert_eq!(h.len(), 2);
        assert_eq!(h[0]["role"], json!("user"));
        assert_eq!(h[0]["content"], json!("m3"));
        assert_eq!(h[1]["role"], json!("assistant"));
        assert_eq!(h[1]["content"], json!("m4"));
    }

    #[test]
    fn get_history_some_zero_matches_none_default_cap() {
        let mut session = Session::new("s1".into());
        session.messages.push(fixture_message("user", "m1"));
        let with_none = session.get_history(None);
        let with_zero = session.get_history(Some(0));
        assert_eq!(with_none, with_zero);
        assert!(
            !with_none.is_empty(),
            "Some(0) is normalized to the same default cap as None"
        );
    }

    #[test]
    fn get_history_alignment_drops_prefix_until_first_user() {
        let mut session = Session::new("s1".into());
        session.messages.push(fixture_message("assistant", "lead"));
        session.messages.push(fixture_message("user", "prompt"));
        session.messages.push(fixture_message("assistant", "reply"));
        let h = session.get_history(Some(50));
        assert_eq!(h.len(), 2);
        assert_eq!(h[0]["role"], json!("user"));
        assert_eq!(h[0]["content"], json!("prompt"));
        assert_eq!(h[1]["content"], json!("reply"));
    }

    #[test]
    fn get_history_forward_whitelisted_optional_fields() {
        let mut session = Session::new("s1".into());
        let mut m = Map::new();
        m.insert("role".into(), json!("assistant"));
        m.insert("content".into(), json!("x"));
        m.insert("timestamp".into(), json!("t"));
        m.insert("tool_call_id".into(), json!("id-9"));
        m.insert(
            "tool_calls".into(),
            json!([{"type": "function", "id": "c1"}]),
        );
        session.messages.push(Value::Object(m));

        let h = session.get_history(None);
        assert_eq!(h.len(), 1);
        assert_eq!(h[0]["tool_call_id"], json!("id-9"));
        assert_eq!(
            h[0]["tool_calls"],
            json!([{"type": "function", "id": "c1"}])
        );
        assert!(h[0].get("name").is_none());
    }

    #[test]
    fn get_history_omits_command_messages() {
        let mut session = Session::new("s1".into());
        session.messages.push(fixture_message("user", "hello"));
        let mut command = Map::new();
        command.insert("role".into(), json!("user"));
        command.insert("content".into(), json!("/status"));
        command.insert("timestamp".into(), json!("2026-01-01T00:00:00Z"));
        command.insert(COMMAND_KEY.to_string(), json!(true));
        session.messages.push(Value::Object(command));
        session.messages.push(fixture_message("assistant", "hi"));

        let h = session.get_history(None);
        assert_eq!(h.len(), 2);
        assert_eq!(h[0]["content"], json!("hello"));
        assert_eq!(h[1]["content"], json!("hi"));
        assert!(h.iter().all(|m| m.get(COMMAND_KEY).is_none()));
    }

    #[test]
    fn test_add_message() {
        let session_key = "test".to_string();
        let mut session = Session::new(session_key.clone());
        session.add_message("user", "Hello, world!", Map::new());
        assert_eq!(session.messages.len(), 1);
        assert_eq!(
            session.messages[0].get("role").unwrap().as_str().unwrap(),
            "user"
        );
        assert_eq!(
            session.messages[0]
                .get("content")
                .unwrap()
                .as_str()
                .unwrap(),
            "Hello, world!"
        );
        assert_eq!(session.key, session_key);
    }

    #[test]
    fn clear_removes_messages_and_resets_last_consolidated() {
        let mut session = Session::new("k1".into());
        session.add_message("user", "a", Map::new());
        session.add_message("assistant", "b", Map::new());
        session.last_consolidated = 1;
        session.clear();
        assert!(session.messages.is_empty());
        assert_eq!(session.last_consolidated, 0);
        assert!(session.get_history(None).is_empty());
    }

    #[test]
    fn clear_preserves_key_and_metadata() {
        let mut session = Session::new("persist-key".into());
        session.metadata.insert("trace".into(), json!("v1"));
        session.add_message("user", "x", Map::new());
        session.clear();
        assert_eq!(session.key, "persist-key");
        assert_eq!(session.metadata.get("trace"), Some(&json!("v1")));
    }

    #[test]
    fn clear_bumps_updated_at() {
        let mut session = Session::new("k2".into());
        let before = session.updated_at;
        std::thread::sleep(std::time::Duration::from_millis(20));
        session.clear();
        assert!(
            session.updated_at > before,
            "expected updated_at to advance after clear"
        );
    }

    #[test]
    fn retain_recent_remaps_last_consolidated_after_prefix_drop() {
        let mut session = Session::new("k".into());
        session.messages.push(fixture_message("user", "u0"));
        session.messages.push(fixture_message("assistant", "a1"));
        session.messages.push(fixture_message("user", "u2"));
        session.messages.push(fixture_message("assistant", "a3"));
        session.messages.push(fixture_message("user", "u4"));
        session.last_consolidated = 3;
        let history_before = session.get_history(None);

        session.retain_recent_legal_suffix(2);
        assert_eq!(session.messages.len(), 3);
        assert_eq!(
            session.messages[0].get("content"),
            Some(&json!("u2")),
            "user-alignment should include the user before a mid-turn cut"
        );

        assert_eq!(session.last_consolidated, 1);
        let history_after = session.get_history(None);
        assert_eq!(history_before, history_after);
    }

    #[test]
    fn retain_recent_max_zero_behaves_like_clear() {
        let mut session = Session::new("k".into());
        session.messages.push(fixture_message("user", "x"));
        session.last_consolidated = 1;
        session.retain_recent_legal_suffix(0);
        assert!(session.messages.is_empty());
        assert_eq!(session.last_consolidated, 0);
    }

    /// Orphan tool after assistant-only prefix: `find_legal_message_start` can advance past the
    /// illegal prefix once that prefix is retained (here the count cap still keeps the whole vec,
    /// but legal trim drops everything before the first safe index).
    #[test]
    fn retain_recent_applies_legal_trim_for_assistant_led_suffix() {
        let mut orphan = Map::new();
        orphan.insert("role".into(), json!("tool"));
        orphan.insert("content".into(), json!("orphan"));
        orphan.insert("timestamp".into(), json!("t"));
        orphan.insert("tool_call_id".into(), json!("unknown-id"));

        let mut session = Session::new("k".into());
        session
            .messages
            .push(fixture_message("assistant", "lead-a"));
        session
            .messages
            .push(fixture_message("assistant", "lead-b"));
        session.messages.push(Value::Object(orphan));
        session.messages.push(fixture_message("user", "hi"));
        session.last_consolidated = 0;

        session.retain_recent_legal_suffix(3);
        assert_eq!(session.messages.len(), 1);
        assert_eq!(session.messages[0].get("content"), Some(&json!("hi")));
        assert_eq!(session.last_consolidated, 0);
    }

    #[test]
    fn load_reads_jsonl_metadata_and_messages() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::new(dir.path().join("ws"));
        let path = mgr
            .sessions_dir
            .join(format!("{}.jsonl", safe_filename("s1")));
        let body = concat!(
            r#"{"_type":"metadata","metadata":{"t":"v"},"created_at":"2026-01-10T08:00:00","last_consolidated":2}"#,
            "\n",
            r#"{"role":"user","content":"hi"}"#,
            "\n",
        );
        fs::write(&path, body).unwrap();
        let s = mgr.load("s1").expect("load");
        assert_eq!(s.metadata.get("t"), Some(&json!("v")));
        assert_eq!(s.last_consolidated, 2);
        assert_eq!(s.messages.len(), 1);
        assert_eq!(s.messages[0]["content"], json!("hi"));
    }

    #[test]
    fn load_non_object_metadata_does_not_panic_and_keeps_messages() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::new(dir.path().join("ws"));
        let path = mgr
            .sessions_dir
            .join(format!("{}.jsonl", safe_filename("s2")));
        fs::write(
            &path,
            "{\"_type\":\"metadata\",\"metadata\":[]}\n{\"role\":\"user\",\"content\":\"ok\"}\n",
        )
        .unwrap();
        let s = mgr.load("s2").expect("load");
        assert!(s.metadata.is_empty());
        assert_eq!(s.messages.len(), 1);
        assert_eq!(s.messages[0]["content"], json!("ok"));
    }

    #[test]
    fn load_skips_invalid_json_line_and_keeps_following_lines() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::new(dir.path().join("ws"));
        let path = mgr
            .sessions_dir
            .join(format!("{}.jsonl", safe_filename("s3")));
        fs::write(&path, "not json\n{\"role\":\"user\",\"content\":\"x\"}\n").unwrap();
        let s = mgr.load("s3").expect("load");
        assert_eq!(s.messages.len(), 1);
        assert_eq!(s.messages[0]["content"], json!("x"));
    }

    #[test]
    fn save_writes_metadata_line_and_messages_as_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = SessionManager::new(dir.path().join("ws"));
        let mut session = Session::new("k1".into());
        session.add_message("user", "hello", Map::new());
        session.add_message("assistant", "world", Map::new());
        session.metadata.insert("x".into(), json!(42));
        session.last_consolidated = 1;

        mgr.save(session).expect("save");

        let path = mgr
            .sessions_dir
            .join(format!("{}.jsonl", safe_filename("k1")));
        let content = fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();

        // First line is the metadata record.
        assert_eq!(lines.len(), 3);
        let meta: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(meta["_type"], json!("metadata"));
        assert_eq!(meta["key"], json!("k1"));
        assert_eq!(meta["metadata"]["x"], json!(42));
        assert_eq!(meta["last_consolidated"], json!(1));
        assert!(meta["created_at"].as_str().is_some());
        assert!(meta["updated_at"].as_str().is_some());

        // Subsequent lines are the messages.
        let msg0: Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(msg0["role"], json!("user"));
        assert_eq!(msg0["content"], json!("hello"));

        let msg1: Value = serde_json::from_str(lines[2]).unwrap();
        assert_eq!(msg1["role"], json!("assistant"));
        assert_eq!(msg1["content"], json!("world"));
    }

    #[test]
    fn save_updates_cache_so_next_load_is_not_needed() {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = SessionManager::new(dir.path().join("ws"));
        let mut session = Session::new("k2".into());
        session.add_message("user", "ping", Map::new());

        mgr.save(session).expect("save");

        assert!(
            mgr.cache.contains_key("k2"),
            "cache should hold the session after save"
        );
    }

    #[test]
    fn save_then_load_round_trips_fields() {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = SessionManager::new(dir.path().join("ws"));
        let mut session = Session::new("k3".into());
        session.add_message("user", "a", Map::new());
        session.add_message("assistant", "b", Map::new());
        session.metadata.insert("env".into(), json!("test"));
        session.last_consolidated = 1;
        let saved_created_at = session.created_at;

        mgr.save(session).expect("save");

        // Remove from cache to force a real disk read.
        mgr.cache.remove("k3");
        let loaded = mgr.load("k3").expect("load");

        assert_eq!(loaded.key, "k3");
        assert_eq!(loaded.messages.len(), 2);
        assert_eq!(loaded.messages[0]["role"], json!("user"));
        assert_eq!(loaded.messages[1]["role"], json!("assistant"));
        assert_eq!(loaded.metadata.get("env"), Some(&json!("test")));
        assert_eq!(loaded.last_consolidated, 1);
        // created_at survives the round-trip (compare at second granularity).
        let diff = (loaded.created_at - saved_created_at).num_seconds().abs();
        assert!(diff < 2, "created_at should survive the round-trip");
    }

    #[test]
    fn save_overwrites_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = SessionManager::new(dir.path().join("ws"));
        let path = mgr
            .sessions_dir
            .join(format!("{}.jsonl", safe_filename("k4")));

        // Write some stale content first.
        fs::write(&path, "stale content\n").unwrap();

        let session = Session::new("k4".into());
        mgr.save(session).expect("save");

        let content = fs::read_to_string(&path).unwrap();
        // Stale content must be gone; first line must be valid metadata JSON.
        let first_line = content.lines().next().unwrap();
        let meta: Value = serde_json::from_str(first_line).unwrap();
        assert_eq!(meta["_type"], json!("metadata"));
    }

    // ── list_sessions ─────────────────────────────────────────────────────────

    #[test]
    fn list_sessions_empty_directory() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::new(dir.path().join("ws"));
        assert!(mgr.list_sessions().is_empty());
    }

    #[test]
    fn list_sessions_sorted_by_updated_at_descending() {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = SessionManager::new(dir.path().join("ws"));

        let mut older = Session::new("older".into());
        older.updated_at = Utc.with_ymd_and_hms(2026, 5, 1, 10, 0, 0).unwrap();
        mgr.save(older).unwrap();

        let mut newer = Session::new("newer".into());
        newer.updated_at = Utc.with_ymd_and_hms(2026, 5, 11, 18, 0, 0).unwrap();
        mgr.save(newer).unwrap();

        let listed = mgr.list_sessions();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0]["key"], json!("newer"));
        assert_eq!(listed[1]["key"], json!("older"));
    }

    #[test]
    fn list_sessions_ignores_non_jsonl_files() {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = SessionManager::new(dir.path().join("ws"));
        fs::write(mgr.sessions_dir.join("readme.txt"), "x").unwrap();
        let s = Session::new("only".into());
        mgr.save(s).unwrap();
        mgr.cache.clear();

        let listed = mgr.list_sessions();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0]["key"], json!("only"));
    }

    #[test]
    fn list_sessions_skips_invalid_json_first_line() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::new(dir.path().join("ws"));
        let path = mgr
            .sessions_dir
            .join(format!("{}.jsonl", safe_filename("bad")));
        fs::write(&path, "not-json\n").unwrap();

        assert!(mgr.list_sessions().is_empty());
    }

    #[test]
    fn list_sessions_skips_first_line_not_metadata_type() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::new(dir.path().join("ws"));
        let path = mgr.sessions_dir.join("other.jsonl");
        fs::write(
            &path,
            r#"{"_type":"other","key":"x","updated_at":"2026-01-01T00:00:00Z"}"#,
        )
        .unwrap();

        assert!(mgr.list_sessions().is_empty());
    }

    #[test]
    fn list_sessions_skips_metadata_without_key() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::new(dir.path().join("ws"));
        let path = mgr.sessions_dir.join("nokey.jsonl");
        fs::write(
            &path,
            r#"{"_type":"metadata","created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-02T00:00:00Z"}"#,
        )
        .unwrap();

        assert!(mgr.list_sessions().is_empty());
    }

    #[test]
    fn list_sessions_skips_empty_string_key() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::new(dir.path().join("ws"));
        let path = mgr.sessions_dir.join("emptykey.jsonl");
        fs::write(
            &path,
            r#"{"_type":"metadata","key":"","updated_at":"2026-01-01T00:00:00Z"}"#,
        )
        .unwrap();

        assert!(mgr.list_sessions().is_empty());
    }

    #[test]
    fn list_sessions_returns_created_at_path_and_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::new(dir.path().join("ws"));
        let path = mgr
            .sessions_dir
            .join(format!("{}.jsonl", safe_filename("meta-t")));
        fs::write(
            &path,
            r#"{"_type":"metadata","key":"meta-t","created_at":"2026-04-01T12:00:00+00:00","updated_at":"2026-04-02T15:30:00+00:00"}"#,
        )
        .unwrap();

        let listed = mgr.list_sessions();
        assert_eq!(listed.len(), 1);
        let e = &listed[0];
        assert_eq!(e["key"], json!("meta-t"));
        assert_eq!(e["created_at"], json!("2026-04-01T12:00:00+00:00"));
        assert_eq!(e["updated_at"], json!("2026-04-02T15:30:00+00:00"));
        assert_eq!(e["path"], json!(path.display().to_string()));
        assert_eq!(e["title"], json!(""));
    }

    #[test]
    fn list_sessions_missing_datetime_fields_use_empty_strings() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::new(dir.path().join("ws"));
        let path = mgr.sessions_dir.join("partial.jsonl");
        fs::write(&path, r#"{"_type":"metadata","key":"partial"}"#).unwrap();

        let listed = mgr.list_sessions();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0]["created_at"], json!(""));
        assert_eq!(listed[0]["updated_at"], json!(""));
        assert_eq!(listed[0]["title"], json!(""));
    }

    #[test]
    fn list_sessions_empty_updated_at_sorts_after_rfc3339() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::new(dir.path().join("ws"));

        let path_a = mgr.sessions_dir.join("a.jsonl");
        fs::write(
            &path_a,
            r#"{"_type":"metadata","key":"has_ts","updated_at":"2026-06-01T00:00:00Z"}"#,
        )
        .unwrap();
        let path_b = mgr.sessions_dir.join("b.jsonl");
        fs::write(&path_b, r#"{"_type":"metadata","key":"no_ts"}"#).unwrap();

        let listed = mgr.list_sessions();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0]["key"], json!("has_ts"));
        assert_eq!(listed[1]["key"], json!("no_ts"));
    }

    #[test]
    fn format_sessions_list_marks_current_and_shows_updated_at() {
        let sessions = vec![
            json!({
                "key": "cli:direct",
                "updated_at": "2026-06-01T00:00:00Z",
            }),
            json!({
                "key": "other:session",
                "updated_at": "2026-06-02T00:00:00Z",
            }),
        ];
        let text = format_sessions_list(&sessions, Some("cli:direct"));
        assert!(text.contains("cli:direct (current) — updated 2026-06-01T00:00:00Z"));
        assert!(text.contains("- other:session — updated 2026-06-02T00:00:00Z"));
    }

    #[test]
    fn format_sessions_list_includes_title_when_present() {
        let sessions = vec![json!({
            "key": "websocket:chat-1",
            "updated_at": "2026-06-01T00:00:00Z",
            "title": "Fix the login bug",
        })];
        let text = format_sessions_list(&sessions, None);
        assert!(
            text.contains("- Fix the login bug [websocket:chat-1] — updated 2026-06-01T00:00:00Z")
        );
    }

    #[test]
    fn list_sessions_includes_generated_title() {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = SessionManager::new(dir.path().join("ws"));
        {
            let session = mgr.get_or_create_session("chat");
            session.metadata.insert(
                SESSION_TITLE_METADATA_KEY.to_string(),
                json!("Fix the login bug"),
            );
            let snapshot = session.clone();
            mgr.save(snapshot).unwrap();
        }
        mgr.cache.clear();

        let listed = mgr.list_sessions();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0]["title"], json!("Fix the login bug"));
    }

    #[test]
    fn rename_session_persists_title_and_overwrites_existing() {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = SessionManager::new(dir.path().join("ws"));
        mgr.save(Session::new("chat".to_string())).unwrap();

        mgr.rename_session("chat", "First title").unwrap();
        {
            let session = mgr.get_session_internal("chat").expect("session");
            assert_eq!(
                session.metadata.get(SESSION_TITLE_METADATA_KEY),
                Some(&json!("First title"))
            );
        }

        mgr.rename_session("chat", "Renamed").unwrap();
        mgr.cache.clear();
        let listed = mgr.list_sessions();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0]["title"], json!("Renamed"));
    }

    #[test]
    fn rename_session_errors_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = SessionManager::new(dir.path().join("ws"));
        assert!(matches!(
            mgr.rename_session("missing", "Nope"),
            Err(RenameSessionError::NotFound)
        ));
        assert!(mgr.list_sessions().is_empty());
    }

    #[test]
    fn format_sessions_list_empty() {
        assert_eq!(format_sessions_list(&[], None), "No sessions available.");
    }

    // ── read_session_file ──────────────────────────────────────────────────

    #[test]
    fn read_session_payload_returns_none_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::new(dir.path().join("ws"));
        assert!(mgr.read_session_file("missing").is_none());
    }

    #[test]
    fn read_session_payload_preserves_raw_timestamps_and_messages() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::new(dir.path().join("ws"));
        let path = mgr
            .sessions_dir
            .join(format!("{}.jsonl", safe_filename("r1")));
        let body = concat!(
            r#"{"_type":"metadata","key":"r1","metadata":{"t":"v"},"created_at":"2026-01-10T08:00:00","updated_at":"2026-01-11T09:00:00"}"#,
            "\n",
            r#"{"role":"user","content":"hi"}"#,
            "\n",
        );
        fs::write(&path, body).unwrap();

        let payload = mgr.read_session_file("r1").expect("payload");
        assert_eq!(payload.key, "r1");
        assert_eq!(payload.created_at.as_deref(), Some("2026-01-10T08:00:00"));
        assert_eq!(payload.updated_at.as_deref(), Some("2026-01-11T09:00:00"));
        assert_eq!(payload.metadata.get("t"), Some(&json!("v")));
        assert_eq!(payload.messages.len(), 1);
        assert_eq!(payload.messages[0].get("content"), Some(&json!("hi")));
    }

    #[test]
    fn read_session_payload_falls_back_to_key_when_metadata_omits_it() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::new(dir.path().join("ws"));
        let path = mgr
            .sessions_dir
            .join(format!("{}.jsonl", safe_filename("fallback-key")));
        fs::write(
            &path,
            r#"{"_type":"metadata","metadata":{}}
{"role":"user","content":"x"}
"#,
        )
        .unwrap();

        let payload = mgr.read_session_file("fallback-key").expect("payload");
        assert_eq!(payload.key, "fallback-key");
    }

    #[test]
    fn read_session_payload_repairs_corrupt_lines() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::new(dir.path().join("ws"));
        let path = mgr
            .sessions_dir
            .join(format!("{}.jsonl", safe_filename("corrupt")));
        fs::write(
            &path,
            concat!(
                r#"{"_type":"metadata","key":"corrupt","metadata":{"ok":true}}"#,
                "\n",
                "not json\n",
                r#"{"role":"user","content":"recovered"}"#,
                "\n",
            ),
        )
        .unwrap();

        let payload = mgr.read_session_file("corrupt").expect("repaired");
        assert_eq!(payload.key, "corrupt");
        assert_eq!(payload.messages.len(), 1);
        assert_eq!(
            payload.messages[0].get("content"),
            Some(&json!("recovered"))
        );
        assert_eq!(payload.metadata.get("ok"), Some(&json!(true)));
    }

    #[test]
    fn clean_generated_title_empty_and_none() {
        assert_eq!(SessionManager::clean_generated_title(None), "");
        assert_eq!(
            SessionManager::clean_generated_title(Some("   ".into())),
            ""
        );
    }

    #[test]
    fn clean_generated_title_strips_prefix_quotes_think_and_punct() {
        assert_eq!(
            SessionManager::clean_generated_title(Some("Title: \"Hello world!\"".into())),
            "Hello world"
        );
        assert_eq!(
            SessionManager::clean_generated_title(Some("标题：  调试会话。".into())),
            "调试会话"
        );
        assert_eq!(
            SessionManager::clean_generated_title(Some(
                "<think>secret</think> Fix login bug".into()
            )),
            "Fix login bug"
        );
    }

    #[test]
    fn clean_generated_title_collapses_whitespace_and_truncates() {
        assert_eq!(
            SessionManager::clean_generated_title(Some("foo   \n\t  bar".into())),
            "foo bar"
        );
        let long = "a".repeat(TITLE_MAX_CHARS + 10);
        let cleaned = SessionManager::clean_generated_title(Some(long));
        assert_eq!(cleaned.chars().count(), TITLE_MAX_CHARS);
        assert!(cleaned.ends_with('…'));
        assert_eq!(
            cleaned
                .chars()
                .take(TITLE_MAX_CHARS - 1)
                .collect::<String>(),
            "a".repeat(TITLE_MAX_CHARS - 1)
        );
    }

    #[test]
    fn title_from_llm_response_prefers_content_over_reasoning() {
        let response = LLMResponse {
            content: Some("Athens Weather Right Now".into()),
            tool_calls: Vec::new(),
            finish_reason: "stop".into(),
            usage: HashMap::new(),
            reasoning_content: Some(
                "Possible: \"Current Weather in Athens Greece\". Use \"Weather in Athens Greece Now\"."
                    .into(),
            ),
            thinking_blocks: None,
        };
        assert_eq!(
            SessionManager::title_from_llm_response(&response),
            "Athens Weather Right Now"
        );
    }

    #[test]
    fn title_from_llm_response_extracts_last_quoted_title_from_reasoning() {
        let response = LLMResponse {
            content: None,
            tool_calls: Vec::new(),
            finish_reason: "length".into(),
            usage: HashMap::new(),
            reasoning_content: Some(
                "We need a concise title. Possible: \"Current Weather in Athens Greece\" \
                 but that's 4 words. Or \"Athens Weather Right Now\" that's 4. \
                 Use \"Weather in Athens Greece Now\" 4."
                    .into(),
            ),
            thinking_blocks: None,
        };
        assert_eq!(
            SessionManager::title_from_llm_response(&response),
            "Weather in Athens Greece Now"
        );
    }

    #[test]
    fn title_from_llm_response_ignores_short_or_long_quoted_reasoning() {
        let response = LLMResponse {
            content: Some("   ".into()),
            tool_calls: Vec::new(),
            finish_reason: "length".into(),
            usage: HashMap::new(),
            reasoning_content: Some(
                "Too short: \"Hi\". Too long: \"This is a nine word candidate that should not win\"."
                    .into(),
            ),
            thinking_blocks: None,
        };
        assert_eq!(SessionManager::title_from_llm_response(&response), "");
    }

    #[test]
    fn title_inputs_empty_session() {
        let session = Session::new("s1".into());
        assert_eq!(title_inputs(&session), (String::new(), String::new()));
    }

    #[test]
    fn title_inputs_takes_first_user_and_assistant() {
        let mut session = Session::new("s1".into());
        session.messages.push(fixture_message("user", "  hello  "));
        session
            .messages
            .push(fixture_message("assistant", "hi there"));
        session.messages.push(fixture_message("user", "later user"));
        session
            .messages
            .push(fixture_message("assistant", "later assistant"));
        assert_eq!(
            title_inputs(&session),
            ("hello".to_string(), "hi there".to_string())
        );
    }

    #[test]
    fn title_inputs_skips_commands_hidden_empty_and_think_only() {
        let mut session = Session::new("s1".into());
        session.messages.push(json!({
            "role": "user",
            "content": "/help",
            "_command": true,
        }));
        session.messages.push(json!({
            "role": "assistant",
            "content": "Available commands",
            "_command": true,
        }));
        session.messages.push(json!({
            "role": "user",
            "content": "hidden prompt",
            "_hidden_history": true,
        }));
        session.messages.push(fixture_message("user", "   "));
        session
            .messages
            .push(fixture_message("user", "<think>secret</think>"));
        session.messages.push(json!({
            "role": "user",
            "content": ["not", "a", "string"],
        }));
        session.messages.push(fixture_message(
            "user",
            "<think>scratch</think> real question",
        ));
        session
            .messages
            .push(fixture_message("assistant", "real answer"));
        assert_eq!(session.messages[0][COMMAND_KEY], json!(true));
        assert_eq!(session.messages[2][HIDDEN_HISTORY_KEY], json!(true));
        assert_eq!(
            title_inputs(&session),
            ("real question".to_string(), "real answer".to_string())
        );
    }

    #[test]
    fn title_inputs_user_only_leaves_assistant_empty() {
        let mut session = Session::new("s1".into());
        session
            .messages
            .push(fixture_message("user", "just a question"));
        assert_eq!(
            title_inputs(&session),
            ("just a question".to_string(), String::new())
        );
    }

    #[test]
    fn title_generation_prompt_includes_user_and_assistant() {
        let (system, user) =
            title_generation_prompt("Fix the login bug", "Reset the token").unwrap();
        assert!(system.contains("Return only the title text"));
        assert!(!system.contains("User:"));
        assert!(user.contains("User: Fix the login bug"));
        assert!(user.contains("Assistant: Reset the token"));
        assert!(user.contains("3 to 8 words"));
    }

    #[test]
    fn title_generation_prompt_omits_empty_assistant() {
        let (_, user) = title_generation_prompt("Fix the login bug", "").unwrap();
        assert!(user.contains("User: Fix the login bug"));
        assert!(!user.contains("Assistant:"));
    }

    #[test]
    fn session_needs_title_false_when_missing_titled_or_empty() {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = SessionManager::new(dir.path().to_path_buf());
        assert!(!mgr.session_needs_title("missing"));

        mgr.get_or_create_session("empty");
        assert!(!mgr.session_needs_title("empty"));

        {
            let session = mgr.get_or_create_session("empty");
            session.messages.push(fixture_message("user", "hello"));
            session
                .metadata
                .insert(SESSION_TITLE_METADATA_KEY.to_string(), json!("Existing"));
        }
        assert!(!mgr.session_needs_title("empty"));
    }

    #[test]
    fn session_needs_title_true_when_user_text_and_no_title() {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = SessionManager::new(dir.path().to_path_buf());
        {
            let session = mgr.get_or_create_session("chat");
            session.messages.push(fixture_message("user", "hello"));
        }
        assert!(mgr.session_needs_title("chat"));
    }
}
