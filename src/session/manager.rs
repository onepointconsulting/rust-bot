use std::{
    collections::HashMap,
    fs::{self, File},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
};

use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};
use serde_json::{Map, Value, json};

use crate::{
    config::paths::get_legacy_sessions_dir,
    utils::helpers::{ensure_dir, find_legal_message_start, safe_filename},
};

/// In-memory conversation session record.
pub struct Session {
    pub key: String,
    pub messages: Vec<Value>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    metadata: HashMap<String, Value>,
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

    pub fn get_history(&self, max_messages: Option<usize>) -> Vec<Value> {
        let max_messages = max_messages.unwrap_or(500);
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

    /// Get an existing session or create a new one.
    ///
    /// # Arguments
    ///
    /// * `key` - The key of the session.
    ///
    /// # Returns
    ///
    /// The session.
    fn get_or_create_session(&mut self, key: &str) -> &mut Session {
        if self.cache.contains_key(key) {
            return self.cache.get_mut(key).unwrap();
        }
        let mut session_opt = self.load(key);
        if let None = &mut session_opt {
            session_opt = Some(Session::new(key.to_string()));
        }
        let session = session_opt.unwrap();
        self.cache.insert(key.to_string(), session);
        return self.cache.get_mut(key).expect("just inserted");
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
                            if let Some(metadata_type) = metadata.get("_type") && let Some(metadata_type_str) = metadata_type.as_str() && metadata_type_str == "metadata" {
                                if let Some(key) = metadata.get("key").and_then(|v| v.as_str()).filter(|k| !k.is_empty()) {
                                    sessions.push(json!({
                                        "key": key,
                                        "created_at": metadata.get("created_at").and_then(|v| v.as_str()).unwrap_or(""),
                                        "updated_at": metadata.get("updated_at").and_then(|v| v.as_str()).unwrap_or(""),
                                        "path": path.display().to_string(),
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
    }

    #[test]
    fn list_sessions_missing_datetime_fields_use_empty_strings() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::new(dir.path().join("ws"));
        let path = mgr.sessions_dir.join("partial.jsonl");
        fs::write(
            &path,
            r#"{"_type":"metadata","key":"partial"}"#,
        )
        .unwrap();

        let listed = mgr.list_sessions();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0]["created_at"], json!(""));
        assert_eq!(listed[0]["updated_at"], json!(""));
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
        fs::write(
            &path_b,
            r#"{"_type":"metadata","key":"no_ts"}"#,
        )
        .unwrap();

        let listed = mgr.list_sessions();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0]["key"], json!("has_ts"));
        assert_eq!(listed[1]["key"], json!("no_ts"));
    }
}
