//! Which stored sessions `rust-bot agent --resume` offers, and how they are
//! described (label, relative age, file size, preview).
//!
//! Only CLI sessions are listed. Websocket, ACP, cron, API and other channel
//! sessions share the same `sessions/` folder, but resuming them from the CLI
//! would race the process that owns them (the gateway caches sessions and
//! rewrites the whole file on every save, and nothing locks the files).

use std::fs;

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::channels::registry::discover_channel_names;
use crate::channels::websocket::CHANNEL_NAME as WEBSOCKET_CHANNEL_NAME;
use crate::session::history_visibility::is_hidden_history_message;
use crate::session::keys::COMMAND_KEY;
use crate::session::manager::{SessionManager, parse_session_timestamp};

/// Session sources that are not the interactive CLI, besides the channels
/// listed by the channel registry and the websocket channel.
const NON_CLI_SESSION_SOURCES: &[&str] = &["acp", "cron", "api", "system", "heartbeat"];

/// A stored CLI session the user can pick to resume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumableSession {
    pub key: String,
    pub title: Option<String>,
    pub updated_at: DateTime<Utc>,
    pub size_bytes: u64,
}

impl ResumableSession {
    /// Title when the session has one, otherwise its key.
    pub fn display_label(&self) -> &str {
        self.title.as_deref().unwrap_or(&self.key)
    }
}

/// How a `--resume <query>` argument resolves against the listed sessions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeResolution {
    /// Resume this session directly, without showing the picker.
    Unique(String),
    /// Ambiguous or empty: open the picker with the query pre-filled.
    Picker,
}

/// True when `key` belongs to the interactive CLI: `cli:*` keys and bare
/// names given with `-s` (e.g. `ems`). Keys whose source (the part before the
/// first `:`, or the whole key) is another channel or an automation are not.
pub fn is_cli_session_key(key: &str) -> bool {
    let source = key.split(':').next().unwrap_or(key);
    let is_channel = source == WEBSOCKET_CHANNEL_NAME || discover_channel_names().contains(&source);
    !is_channel && !NON_CLI_SESSION_SOURCES.contains(&source)
}

/// Stored CLI sessions, most recently updated first.
///
/// Sessions stamped with a websocket owner are skipped even when their key
/// looks like a CLI key, since a browser client owns them.
pub fn resumable_cli_sessions(manager: &SessionManager) -> Vec<ResumableSession> {
    manager
        .list_sessions()
        .iter()
        .filter_map(resumable_session_from_listing)
        .collect()
}

/// Convert one [`SessionManager::list_sessions`] entry, or `None` when it is
/// not a resumable CLI session.
fn resumable_session_from_listing(entry: &Value) -> Option<ResumableSession> {
    let key = entry.get("key").and_then(Value::as_str)?;
    if !is_cli_session_key(key) {
        return None;
    }
    let owned_by_websocket_client = entry
        .get("owner_client_id")
        .and_then(Value::as_str)
        .is_some_and(|owner| !owner.is_empty());
    if owned_by_websocket_client {
        return None;
    }
    let title = entry
        .get("title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .map(str::to_string);
    let updated_at = entry
        .get("updated_at")
        .and_then(Value::as_str)
        .and_then(parse_session_timestamp)
        .unwrap_or(DateTime::<Utc>::UNIX_EPOCH);
    let size_bytes = entry
        .get("path")
        .and_then(Value::as_str)
        .and_then(|path| fs::metadata(path).ok())
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    Some(ResumableSession {
        key: key.to_string(),
        title,
        updated_at,
        size_bytes,
    })
}

/// True when every whitespace-separated term of `query` appears,
/// case-insensitively, in the session's title or key. An empty query matches.
pub fn session_matches_query(session: &ResumableSession, query: &str) -> bool {
    let key = session.key.to_lowercase();
    let title = session.title.as_deref().unwrap_or("").to_lowercase();
    query
        .split_whitespace()
        .map(str::to_lowercase)
        .all(|term| key.contains(&term) || title.contains(&term))
}

/// Resolve a `--resume` query: an exact key wins, then a query matching
/// exactly one session. Anything else (empty, no match, several matches)
/// opens the picker.
pub fn resolve_resume_query(sessions: &[ResumableSession], query: &str) -> ResumeResolution {
    let query = query.trim();
    if query.is_empty() {
        return ResumeResolution::Picker;
    }
    if let Some(exact) = sessions.iter().find(|session| session.key == query) {
        return ResumeResolution::Unique(exact.key.clone());
    }
    let mut matches = sessions
        .iter()
        .filter(|session| session_matches_query(session, query));
    match (matches.next(), matches.next()) {
        (Some(only), None) => ResumeResolution::Unique(only.key.clone()),
        _ => ResumeResolution::Picker,
    }
}

/// Human-readable file size with one decimal above bytes: `512B`,
/// `678.5KB`, `1.2MB`, `3.0GB`.
pub fn format_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes}B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit_index = 0;
    while value >= 1024.0 && unit_index + 1 < UNITS.len() {
        value /= 1024.0;
        unit_index += 1;
    }
    format!("{value:.1}{}", UNITS[unit_index])
}

/// The last `max_turns` displayable turns of `key` as `(role, text)` pairs,
/// read from the session file without loading or caching the session.
pub fn session_preview(
    manager: &SessionManager,
    key: &str,
    max_turns: usize,
) -> Vec<(String, String)> {
    let Some(payload) = manager.read_session_file(key) else {
        return Vec::new();
    };
    let messages: Vec<Value> = payload
        .messages
        .into_iter()
        .map(|message| Value::Object(message.into_iter().collect()))
        .collect();
    recent_cli_transcript(&messages, max_turns)
}

/// Last `max_messages` displayable turns: `user`/`assistant` only, skipping
/// slash commands, hidden/automation rows, and empty text. The window starts
/// on a user turn when one exists, so a replay does not open mid-reply.
pub fn recent_cli_transcript(messages: &[Value], max_messages: usize) -> Vec<(String, String)> {
    if max_messages == 0 {
        return Vec::new();
    }
    let mut visible: Vec<(String, String)> = messages
        .iter()
        .filter(|message| {
            let role = message.get("role").and_then(Value::as_str).unwrap_or("");
            (role == "user" || role == "assistant")
                && message.get(COMMAND_KEY).is_none()
                && !is_hidden_history_message(message)
        })
        .filter_map(|message| {
            let role = message.get("role").and_then(Value::as_str)?;
            let text = persisted_message_text(message.get("content"));
            if text.is_empty() {
                None
            } else {
                Some((role.to_string(), text))
            }
        })
        .collect();

    if visible.len() > max_messages {
        let start = visible.len() - max_messages;
        visible = visible.split_off(start);
    }
    if let Some(start) = visible.iter().position(|(role, _)| role == "user")
        && start > 0
    {
        visible = visible.split_off(start);
    }
    visible
}

/// Flatten a persisted `content` field. Image turns are stored as a block
/// array, so `as_str()` would print them blank. Placeholders such as
/// `[image: path]` stay in the text — the CLI has no thumbnail to show instead.
pub fn persisted_message_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.trim().to_string(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::keys::{SESSION_TITLE_METADATA_KEY, SESSION_WEBSOCKET_OWNER_CLIENT_ID_KEY};
    use serde_json::{Map, json};

    fn session(key: &str, title: Option<&str>) -> ResumableSession {
        ResumableSession {
            key: key.to_string(),
            title: title.map(str::to_string),
            updated_at: Utc::now(),
            size_bytes: 0,
        }
    }

    #[test]
    fn cli_keys_and_bare_names_are_cli_sessions() {
        for key in ["cli:direct", "cli:other", "ems", "research-chat"] {
            assert!(is_cli_session_key(key), "{key} should be a CLI session");
        }
    }

    #[test]
    fn channel_and_automation_keys_are_not_cli_sessions() {
        for key in [
            "websocket:abc",
            "acp:123",
            "cron:job-1",
            "api:default",
            "system:x",
            "heartbeat",
            "email:someone@example.com",
            "whatsapp:+44123",
        ] {
            assert!(
                !is_cli_session_key(key),
                "{key} should not be a CLI session"
            );
        }
    }

    /// Persist a session with one user message (padding sets the file size).
    fn store_session(
        manager: &mut SessionManager,
        key: &str,
        metadata: &[(&str, &str)],
        updated_at: DateTime<Utc>,
        padding: usize,
    ) {
        let mut session = manager.get_or_create_session(key).clone();
        session.add_message("user", "x".repeat(padding), Map::new());
        for (name, value) in metadata {
            session.metadata.insert(name.to_string(), json!(value));
        }
        session.updated_at = updated_at;
        manager.save(session).expect("save session");
    }

    #[test]
    fn resumable_cli_sessions_filters_sorts_and_measures() {
        let dir = tempfile::tempdir().unwrap();
        let mut manager = SessionManager::new(dir.path().to_path_buf(), 24, 1);
        let now = Utc::now();
        store_session(
            &mut manager,
            "cli:direct",
            &[],
            now - chrono::Duration::days(2),
            10,
        );
        store_session(
            &mut manager,
            "ems",
            &[(SESSION_TITLE_METADATA_KEY, "EMS report")],
            now - chrono::Duration::hours(1),
            5000,
        );
        store_session(&mut manager, "websocket:chat", &[], now, 10);
        store_session(&mut manager, "cron:job", &[], now, 10);
        store_session(
            &mut manager,
            "browser-owned",
            &[(SESSION_WEBSOCKET_OWNER_CLIENT_ID_KEY, "client-1")],
            now,
            10,
        );

        let sessions = resumable_cli_sessions(&manager);
        let keys: Vec<&str> = sessions.iter().map(|s| s.key.as_str()).collect();
        assert_eq!(keys, vec!["ems", "cli:direct"]);
        assert_eq!(sessions[0].title.as_deref(), Some("EMS report"));
        assert_eq!(sessions[1].title, None);
        assert!(sessions[0].size_bytes > 5000);
        assert!(sessions[1].size_bytes > 0);
        assert!(sessions[0].size_bytes > sessions[1].size_bytes);
    }

    #[test]
    fn display_label_falls_back_to_key() {
        assert_eq!(session("ems", Some("Report")).display_label(), "Report");
        assert_eq!(session("ems", None).display_label(), "ems");
    }

    #[test]
    fn format_size_picks_the_unit() {
        assert_eq!(format_size(0), "0B");
        assert_eq!(format_size(1023), "1023B");
        assert_eq!(format_size(1024), "1.0KB");
        assert_eq!(format_size(694_784), "678.5KB");
        assert_eq!(format_size(1_258_291), "1.2MB");
        assert_eq!(format_size(3 * 1024 * 1024 * 1024), "3.0GB");
    }

    #[test]
    fn query_matching_needs_every_term_in_title_or_key() {
        let report = session("ems", Some("HMRC R&D evidence report"));
        assert!(session_matches_query(&report, ""));
        assert!(session_matches_query(&report, "hmrc"));
        assert!(session_matches_query(&report, "REPORT ems"));
        assert!(session_matches_query(&report, "  evidence   hmrc "));
        assert!(!session_matches_query(&report, "hmrc missing"));
        let untitled = session("cli:direct", None);
        assert!(session_matches_query(&untitled, "direct"));
        assert!(!session_matches_query(&untitled, "report"));
    }

    #[test]
    fn resolve_resume_query_prefers_exact_key_then_single_match() {
        let sessions = vec![
            session("ems", Some("Report")),
            session("ems-scripting", Some("Scripts")),
            session("cli:direct", Some("Sidebar plan")),
        ];
        assert_eq!(
            resolve_resume_query(&sessions, "ems"),
            ResumeResolution::Unique("ems".to_string())
        );
        assert_eq!(
            resolve_resume_query(&sessions, "sidebar"),
            ResumeResolution::Unique("cli:direct".to_string())
        );
        assert_eq!(
            resolve_resume_query(&sessions, "em"),
            ResumeResolution::Picker
        );
        assert_eq!(
            resolve_resume_query(&sessions, "nothing"),
            ResumeResolution::Picker
        );
        assert_eq!(
            resolve_resume_query(&sessions, "  "),
            ResumeResolution::Picker
        );
    }

    #[test]
    fn session_preview_reads_recent_turns_from_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut manager = SessionManager::new(dir.path().to_path_buf(), 24, 1);
        let mut stored = manager.get_or_create_session("ems").clone();
        stored.add_message("user", "first question", Map::new());
        stored.add_message("assistant", "first answer", Map::new());
        stored.add_message("user", "second question", Map::new());
        manager.save(stored).unwrap();

        let fresh = SessionManager::new(dir.path().to_path_buf(), 24, 1);
        assert_eq!(
            session_preview(&fresh, "ems", 2),
            vec![("user".to_string(), "second question".to_string())]
        );
        assert!(session_preview(&fresh, "missing", 6).is_empty());
    }

    #[test]
    fn recent_cli_transcript_keeps_the_tail_and_starts_on_a_user_turn() {
        let messages = vec![
            json!({"role": "user", "content": "oldest"}),
            json!({"role": "assistant", "content": "old reply"}),
            json!({"role": "assistant", "content": "dangling"}),
            json!({"role": "user", "content": "latest question"}),
            json!({"role": "assistant", "content": "latest answer"}),
        ];
        let recent = recent_cli_transcript(&messages, 3);
        assert_eq!(
            recent,
            vec![
                ("user".to_string(), "latest question".to_string()),
                ("assistant".to_string(), "latest answer".to_string()),
            ]
        );
    }

    #[test]
    fn recent_cli_transcript_skips_tools_commands_hidden_and_empty() {
        let messages = vec![
            json!({"role": "user", "content": "hi"}),
            json!({"role": "assistant", "content": "", "tool_calls": [{"id": "c1"}]}),
            json!({"role": "tool", "content": "tool output"}),
            json!({"role": "user", "content": "/help", "_command": true}),
            json!({"role": "assistant", "content": "help text", "_command": true}),
            json!({"role": "user", "content": "cron", "_hidden_history": true}),
            json!({
                "role": "user",
                "content": [
                    {"type": "text", "text": "see this"},
                    {"type": "text", "text": "[image: /tmp/shot.png]"},
                ],
            }),
            json!({"role": "assistant", "content": "saw it"}),
        ];
        let recent = recent_cli_transcript(&messages, 10);
        assert_eq!(
            recent,
            vec![
                ("user".to_string(), "hi".to_string()),
                (
                    "user".to_string(),
                    "see this\n[image: /tmp/shot.png]".to_string()
                ),
                ("assistant".to_string(), "saw it".to_string()),
            ]
        );
    }

    #[test]
    fn recent_cli_transcript_empty_when_nothing_is_visible() {
        assert!(recent_cli_transcript(&[], 10).is_empty());
        assert!(recent_cli_transcript(&[json!({"role": "tool", "content": "x"})], 10).is_empty());
        assert!(recent_cli_transcript(&[json!({"role": "user", "content": "hi"})], 0).is_empty());
    }
}
