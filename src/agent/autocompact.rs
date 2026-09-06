use std::{
    collections::{HashMap, HashSet},
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
};

use chrono::{DateTime, Utc};

use crate::{
    agent::{memory::Consolidator, model_runtime::ModelRuntime}, session::manager::{Session, SessionManager},
};

struct Summary {
    summary: String,
    last_updated: DateTime<Utc>,
}

pub const NO_SUMMARY: &str = "(nothing)";

pub struct Autocompact {
    sessions: Arc<Mutex<SessionManager>>,
    consolidator: Arc<Consolidator>,
    session_ttl_minutes: i64,
    /// In-flight idle archives. Shared with `'static` archive tasks so they
    /// can clear the flag when they finish.
    archiving: Arc<Mutex<HashSet<String>>>,
    summaries: Arc<Mutex<HashMap<String, Summary>>>,
}

pub enum TsInput {
    DateTime(DateTime<Utc>),
    Str(String),
}

impl Autocompact {
    const RECENT_SUFFIX_MESSAGES: usize = 8;
    const INTERNAL_SESSION_PREFIXES: &[&str; 1] = &["dream:"];

    pub fn new(
        sessions: Arc<Mutex<SessionManager>>,
        consolidator: Arc<Consolidator>,
        session_ttl_minutes: i64,
    ) -> Self {
        Self {
            sessions,
            consolidator,
            session_ttl_minutes,
            archiving: Arc::new(Mutex::new(HashSet::new())),
            summaries: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn is_expired(&self, ts: Option<TsInput>, now: Option<DateTime<Utc>>) -> bool {
        is_session_expired(self.session_ttl_minutes, ts, now)
    }

    fn has_compactable_idle_tail(&self, session_key: &str) -> bool {
        let (tail, key) = {
            let manager = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            let Some(session) = manager.get_session_internal(session_key) else {
                return false;
            };
            let start = session.last_consolidated.min(session.messages.len());
            (session.messages[start..].to_vec(), session.key.clone())
        };
        let mut probe = Session {
            key: key,
            messages: tail,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            metadata: HashMap::new(),
            last_consolidated: 0,
        };
        // The probe always starts with last_consolidated == 0, so nothing in
        // it is already-consolidated: any messages dropped by the retention
        // cut are exactly the ones a real idle-session compaction would need
        // to summarize. Comparing lengths before/after avoids needing a
        // richer return type from retain_recent_legal_suffix (mirrors
        // nanobot's `bool(result.dropped)` for this same probe).
        let before = probe.messages.len();
        probe.retain_recent_legal_suffix(Self::RECENT_SUFFIX_MESSAGES, true);
        before != probe.messages.len()
    }

    fn format_summary(text: &str, last_active: DateTime<Utc>) -> String {
        format!(
            "Previous conversation summary (last active {}):\n{}",
            last_active.to_rfc3339(),
            text
        )
    }

    fn is_internal_session(session_key: &str) -> bool {
        Self::INTERNAL_SESSION_PREFIXES.iter().any(|prefix| session_key.starts_with(prefix))
    }

    fn is_archiving(&self, session_key: &str) -> bool {
        self.archiving
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(session_key)
    }

    fn mark_archiving(&self, session_key: &str) {
        self.archiving
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(session_key.to_string());
    }

    /// Schedule archival for idle sessions, skipping those with in-flight agent tasks.
    pub fn check_expired<F, R>(
        &self,
        schedule_background: F,
        resolve_runtime: R,
        active_session_keys: &[String],
    ) where
        F: Fn(Pin<Box<dyn Future<Output = ()> + Send + 'static>>),
        R: Fn(&Session) -> ModelRuntime,
    {
        log::info!("Auto-compact: checking expired sessions");
        let now = Utc::now();
        // Drop the sessions lock before the probe; `has_compactable_idle_tail`
        // locks the same `Mutex` and std mutexes are not reentrant.
        let listed = {
            let manager = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            manager.list_sessions()
        };
        for session in listed {
            let Some(session_key) = session
                .get("key")
                .and_then(|k| k.as_str())
                .filter(|k| !k.is_empty())
                .map(|k| k.to_string())
            else {
                continue;
            };
            log::info!("Auto-compact: checking session {session_key}");
            if Self::is_internal_session(&session_key)
                || self.is_archiving(&session_key)
                || active_session_keys.iter().any(|k| k == &session_key)
            {
                continue;
            }
            let updated_at_str = session
                .get("updated_at")
                .and_then(|u| u.as_str())
                .unwrap_or_default();
            let is_expired = self.is_expired(Some(TsInput::Str(updated_at_str.to_string())), Some(now));
            let has_compactable_idle_tail = self.has_compactable_idle_tail(&session_key);
            if !is_expired || !has_compactable_idle_tail
            {
                if !is_expired {
                    log::info!("Auto-compact: session {session_key} is not expired");
                }
                if !has_compactable_idle_tail {
                    log::info!("Auto-compact: session {session_key} does not have a compactable idle tail");
                }
                continue;
            }
            let Some(full) = ({
                let manager = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
                manager.get_session_internal(&session_key)
            }) else {
                continue;
            };
            let runtime = resolve_runtime(&full);
            self.mark_archiving(&session_key);
            log::info!("Auto-compact: archiving session {session_key}");
            schedule_background(Box::pin(Self::archive(
                session_key,
                runtime,
                Arc::clone(&self.consolidator),
                Arc::clone(&self.archiving),
                Arc::clone(&self.sessions),
                Arc::clone(&self.summaries),
            )));
        }
    }

    async fn archive(
        session_key: String,
        runtime: ModelRuntime,
        consolidator: Arc<Consolidator>,
        archiving: Arc<Mutex<HashSet<String>>>,
        sessions: Arc<Mutex<SessionManager>>,
        summaries: Arc<Mutex<HashMap<String, Summary>>>,
    ) {
        let _unmark = UnmarkArchiving {
            session_key: session_key.clone(),
            archiving,
        };
        if Self::is_internal_session(&session_key) {
            return;
        }

        let summary = consolidator
            .compact_idle_session(&session_key, runtime, Self::RECENT_SUFFIX_MESSAGES)
            .await;
        // Python: `if summary and summary != "(nothing)"` — None and "" skip.
        if !summary
            .as_deref()
            .is_some_and(|s| !s.is_empty() && s != NO_SUMMARY)
        {
            return;
        }

        let session = {
            let mut manager = sessions.lock().unwrap_or_else(|e| e.into_inner());
            manager.get_or_create_session(&session_key).clone()
        };
        let Some(meta) = session
            .metadata
            .get(SessionManager::LAST_SUMMARY_KEY)
            .and_then(|v| v.as_object())
        else {
            return;
        };
        let (Some(text), Some(last_active_str)) = (
            meta.get("text").and_then(|v| v.as_str()),
            meta.get(SessionManager::LAST_ACTIVE_KEY).and_then(|v| v.as_str()),
        ) else {
            log::error!("Auto-compact: failed for {session_key}: _last_summary missing text or last_active");
            return;
        };
        let last_active = match DateTime::parse_from_rfc3339(last_active_str) {
            Ok(dt) => dt.with_timezone(&Utc),
            Err(e) => {
                log::error!("Auto-compact: failed for {session_key}: {e}");
                return;
            }
        };
        summaries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                session_key,
                Summary {
                    summary: text.to_string(),
                    last_updated: last_active,
                },
            );
    }

    /// Refresh a possibly-stale session and return any pending idle-compaction
    /// summary, pre-formatted and ready to inject into this turn's prompt.
    ///
    /// Mirrors nanobot's `AutoCompact.prepare_session`: internal sessions are
    /// exempted outright (and have their archiving/summary state cleared, in
    /// case one somehow got tagged). Otherwise, if a background archive for
    /// this session is in flight, or the session still looks idle-expired —
    /// `compact_idle_session` never touches `updated_at`, so an idle session
    /// stays "expired" by that check even right after being archived — the
    /// session is reloaded from the shared `SessionManager` so replay
    /// reflects any archiving that has since completed.
    ///
    /// The hot-path in-memory summary (set by a just-finished `archive()` in
    /// this process) is checked first and consumed; the cold path falls back
    /// to the summary already persisted in `session.metadata` (covers a
    /// process restart, where the hot-path map is empty but the summary
    /// survived on disk).
    pub fn prepare_session(&self, mut session: Session, key: &str) -> (Session, Option<String>) {
        if Self::is_internal_session(key) {
            self.archiving
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(key);
            self.summaries
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(key);
            return (session, None);
        }

        if self.is_archiving(key)
            || self.is_expired(Some(TsInput::DateTime(session.updated_at)), None)
        {
            log::info!(
                "Auto-compact: reloading session {key} (archiving={})",
                self.is_archiving(key)
            );
            let mut manager = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            session = manager.get_or_create_session(key).clone();
        }

        // Hot path: summary from in-memory map (process hasn't restarted).
        let hot = self
            .summaries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(key);
        if let Some(entry) = hot {
            let formatted = Self::format_summary(&entry.summary, entry.last_updated);
            return (session, Some(formatted));
        }

        // Cold path: summary persisted in session metadata (process restarted).
        if let Some(meta) = session
            .metadata
            .get(SessionManager::LAST_SUMMARY_KEY)
            .and_then(|v| v.as_object())
        {
            if let (Some(text), Some(last_active_str)) = (
                meta.get("text").and_then(|v| v.as_str()),
                meta.get(SessionManager::LAST_ACTIVE_KEY).and_then(|v| v.as_str()),
            ) {
                if let Ok(dt) = DateTime::parse_from_rfc3339(last_active_str) {
                    let formatted = Self::format_summary(text, dt.with_timezone(&Utc));
                    return (session, Some(formatted));
                }
            }
        }

        (session, None)
    }
}

/// Clears the in-flight flag when the archive task finishes or is dropped
/// (nanobot's `finally: self._archiving.discard(key)`).
struct UnmarkArchiving {
    session_key: String,
    archiving: Arc<Mutex<HashSet<String>>>,
}

impl Drop for UnmarkArchiving {
    fn drop(&mut self) {
        self.archiving
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.session_key);
    }
}

fn is_session_expired(
    session_ttl_minutes: i64,
    ts: Option<TsInput>,
    now: Option<DateTime<Utc>>,
) -> bool {
    if session_ttl_minutes <= 0 {
        return false;
    }
    match ts {
        None => false,
        Some(ts_input) => {
            let ts = match ts_input {
                TsInput::DateTime(dt) => dt,
                TsInput::Str(s) if s.is_empty() => {
                    log::warn!("empty session timestamp; treating as not expired");
                    return false;
                }
                TsInput::Str(s) => match DateTime::parse_from_rfc3339(&s) {
                    Ok(dt) => dt.with_timezone(&Utc),
                    Err(_) => {
                        log::warn!("unparseable session timestamp {s:?}; treating as expired");
                        return true;
                    }
                },
            };
            let now = now.unwrap_or_else(Utc::now);
            let duration = now.signed_duration_since(ts);
            duration.num_minutes() >= session_ttl_minutes
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s)
            .unwrap_or_else(|e| panic!("fixture is not RFC 3339 ({s:?}): {e}"))
            .with_timezone(&Utc)
    }

    fn ts_str(s: impl Into<String>) -> Option<TsInput> {
        Some(TsInput::Str(s.into()))
    }

    fn ts_dt(s: &str) -> Option<TsInput> {
        Some(TsInput::DateTime(utc(s)))
    }

    const NOW: &str = "2026-09-04T08:00:00+00:00";
    const TTL_MINUTES: i64 = 30;

    fn expired(ts: Option<TsInput>) -> bool {
        is_session_expired(TTL_MINUTES, ts, Some(utc(NOW)))
    }

    #[test]
    fn zero_ttl_never_expires() {
        assert!(!is_session_expired(
            0,
            ts_str("2000-01-01T00:00:00+00:00"),
            Some(utc(NOW))
        ));
        assert!(!is_session_expired(
            0,
            ts_str("not-a-timestamp"),
            Some(utc(NOW))
        ));
        assert!(!is_session_expired(0, ts_str(""), Some(utc(NOW))));
        assert!(!is_session_expired(0, None, Some(utc(NOW))));
    }

    #[test]
    fn negative_ttl_never_expires() {
        assert!(!is_session_expired(
            -5,
            ts_str("2000-01-01T00:00:00+00:00"),
            Some(utc(NOW))
        ));
        assert!(!is_session_expired(
            -5,
            ts_str("not-a-timestamp"),
            Some(utc(NOW))
        ));
    }

    #[test]
    fn missing_timestamp_is_not_expired() {
        assert!(!expired(None));
    }

    #[test]
    fn empty_timestamp_is_not_expired() {
        assert!(!expired(ts_str("")));
    }

    #[test]
    fn unparseable_timestamp_is_expired() {
        assert!(expired(ts_str("not-a-timestamp")));
        assert!(expired(ts_str("   ")));
        assert!(expired(ts_str("2026-09-04T07:19:41.379322")));
        assert!(expired(ts_str("2026-09-04 07:19:41.379322+00:00")));
    }

    #[test]
    fn exactly_ttl_is_expired() {
        assert!(expired(ts_str("2026-09-04T07:30:00+00:00")));
        assert!(expired(ts_dt("2026-09-04T07:30:00+00:00")));
    }

    #[test]
    fn one_second_under_ttl_is_not_expired() {
        assert!(!expired(ts_str("2026-09-04T07:30:01+00:00")));
    }

    #[test]
    fn past_ttl_is_expired() {
        assert!(expired(ts_str("2026-09-04T07:29:59+00:00")));
    }

    #[test]
    fn future_timestamp_is_not_expired() {
        assert!(!expired(ts_str("2026-09-04T08:00:01+00:00")));
    }

    #[test]
    fn session_metadata_microseconds_format_parses() {
        let raw = "2026-09-04T07:19:41.379322+00:00";
        assert!(!is_session_expired(
            TTL_MINUTES,
            ts_str(raw),
            Some(utc("2026-09-04T07:49:41.379321+00:00")),
        ));
        assert!(is_session_expired(
            TTL_MINUTES,
            ts_str(raw),
            Some(utc("2026-09-04T07:49:41.379322+00:00")),
        ));
    }

    #[test]
    fn z_suffix_and_offset_are_compared_as_instants() {
        assert!(expired(ts_str("2026-09-04T07:30:00Z")));
        // 10:00+02:00 is 08:00 UTC, so elapsed time is zero.
        assert!(!expired(ts_str("2026-09-04T10:00:00+02:00")));
    }

    #[test]
    fn default_now_uses_the_system_clock() {
        assert!(is_session_expired(
            TTL_MINUTES,
            ts_str("2000-01-01T00:00:00+00:00"),
            None,
        ));
        assert!(!is_session_expired(
            TTL_MINUTES,
            ts_str("2099-01-01T00:00:00+00:00"),
            None,
        ));
    }

    // ── prepare_session tests ───────────────────────────────────────────────

    use crate::agent::memory::{Consolidator, MemoryStore, MessageBuilder};
    use crate::agent::model_runtime::ModelRuntimeResolver;
    use crate::config::schema::Config;
    use crate::providers::base::{
        BoxedProgressCallback, BoxedStreamCallback, GenerationSettings, LLMProviderDyn, LLMResponse,
    };
    use crate::providers::registry::ProviderSpec;
    use tempfile::TempDir;

    /// `LLMProviderDyn` stub that is never actually invoked by these tests —
    /// `prepare_session` never calls the provider; this only satisfies
    /// `Consolidator::new`'s type requirements.
    struct UnusedProvider {
        settings: GenerationSettings,
    }

    impl UnusedProvider {
        fn arc() -> Arc<dyn LLMProviderDyn> {
            Arc::new(Self {
                settings: GenerationSettings::new(),
            })
        }
    }

    #[async_trait::async_trait]
    impl LLMProviderDyn for UnusedProvider {
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
            unreachable!("prepare_session never calls the provider")
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
            unreachable!("prepare_session never calls the provider")
        }
        async fn chat_with_retry(
            &self,
            _: Vec<serde_json::Value>,
            _: Option<Vec<serde_json::Value>>,
            _: Option<String>,
            _: Option<usize>,
            _: Option<f32>,
            _: Option<String>,
            _: Option<serde_json::Value>,
        ) -> LLMResponse {
            unreachable!("prepare_session never calls the provider")
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
            unreachable!("prepare_session never calls the provider")
        }
    }

    /// `MessageBuilder` stub — `prepare_session` never builds a prompt either;
    /// only needed to satisfy `Consolidator::new`.
    struct NoopMessageBuilder;

    impl MessageBuilder for NoopMessageBuilder {
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

    fn test_autocompact(tmp: &TempDir, ttl_minutes: i64) -> (Autocompact, Arc<Mutex<SessionManager>>) {
        let sessions = Arc::new(Mutex::new(SessionManager::new(tmp.path().to_path_buf())));
        let store = Arc::new(MemoryStore::new(tmp.path().to_path_buf(), None));
        let runtime_resolver = Arc::new(ModelRuntimeResolver::new(
            Config::default(),
            UnusedProvider::arc(),
        ));
        let consolidator = Arc::new(Consolidator::new(
            store,
            runtime_resolver,
            sessions.clone(),
            65_536,
            Box::new(NoopMessageBuilder),
            8192,
        ));
        (
            Autocompact::new(sessions.clone(), consolidator, ttl_minutes),
            sessions,
        )
    }

    #[test]
    fn prepare_session_internal_session_returns_none_and_clears_state() {
        let tmp = TempDir::new().unwrap();
        let (ac, _sessions) = test_autocompact(&tmp, 15);
        ac.mark_archiving("dream:20260101-000000");
        ac.summaries.lock().unwrap().insert(
            "dream:20260101-000000".to_string(),
            Summary {
                summary: "stale".to_string(),
                last_updated: Utc::now(),
            },
        );

        let session = Session::new("dream:20260101-000000".into());
        let (returned, summary) = ac.prepare_session(session, "dream:20260101-000000");

        assert!(summary.is_none());
        assert!(!ac.is_archiving("dream:20260101-000000"), "archiving flag must be cleared");
        assert!(
            !ac
                .summaries
                .lock()
                .unwrap()
                .contains_key("dream:20260101-000000"),
            "stale summary must be cleared"
        );
        assert_eq!(returned.key, "dream:20260101-000000");
    }

    #[test]
    fn prepare_session_hot_path_pops_in_memory_summary() {
        let tmp = TempDir::new().unwrap();
        let (ac, sessions) = test_autocompact(&tmp, 15);
        let session = Session::new("cli:test".into());
        sessions.lock().unwrap().save(session.clone()).unwrap();

        let last_active = Utc::now();
        ac.summaries.lock().unwrap().insert(
            "cli:test".to_string(),
            Summary {
                summary: "User said hello.".to_string(),
                last_updated: last_active,
            },
        );

        let (_returned, summary) = ac.prepare_session(session, "cli:test");

        let summary = summary.expect("hot-path summary should be returned");
        assert!(summary.contains("User said hello."));
        assert!(summary.contains("Previous conversation summary"));
        assert!(
            !ac.summaries.lock().unwrap().contains_key("cli:test"),
            "hot-path entry must be consumed (popped)"
        );
    }

    #[test]
    fn prepare_session_cold_path_reads_summary_from_metadata() {
        let tmp = TempDir::new().unwrap();
        let (ac, sessions) = test_autocompact(&tmp, 15);
        let mut session = Session::new("cli:test".into());
        let last_active = Utc::now();
        session.metadata.insert(
            SessionManager::LAST_SUMMARY_KEY.to_string(),
            serde_json::json!({
                "text": "Persisted summary.",
                (SessionManager::LAST_ACTIVE_KEY): last_active.to_rfc3339(),
            }),
        );
        sessions.lock().unwrap().save(session.clone()).unwrap();

        // Hot-path map is empty (simulates a process restart), so this must
        // fall through to reading `session.metadata` directly.
        let (_returned, summary) = ac.prepare_session(session, "cli:test");

        let summary = summary.expect("cold-path summary should be returned");
        assert!(summary.contains("Persisted summary."));
        assert!(summary.contains("Previous conversation summary"));
    }

    #[test]
    fn prepare_session_no_summary_anywhere_returns_none() {
        let tmp = TempDir::new().unwrap();
        let (ac, sessions) = test_autocompact(&tmp, 15);
        let session = Session::new("cli:test".into());
        sessions.lock().unwrap().save(session.clone()).unwrap();

        let (_returned, summary) = ac.prepare_session(session, "cli:test");

        assert!(summary.is_none());
    }

    #[test]
    fn prepare_session_reloads_session_when_archiving_in_flight() {
        let tmp = TempDir::new().unwrap();
        let (ac, sessions) = test_autocompact(&tmp, 15);

        // The persisted version has a message the caller's stale local copy
        // does not — proof that `prepare_session` reloaded rather than using
        // the passed-in session as-is.
        let mut persisted = Session::new("cli:test".into());
        persisted.add_message("user", "fresh from disk", serde_json::Map::new());
        sessions.lock().unwrap().save(persisted).unwrap();

        ac.mark_archiving("cli:test");
        let stale_local = Session::new("cli:test".into());
        assert!(stale_local.messages.is_empty());

        let (returned, _summary) = ac.prepare_session(stale_local, "cli:test");

        assert_eq!(returned.messages.len(), 1);
        assert_eq!(
            returned.messages[0].get("content"),
            Some(&serde_json::json!("fresh from disk"))
        );
    }

    #[test]
    fn prepare_session_reloads_session_when_still_idle_expired() {
        let tmp = TempDir::new().unwrap();
        let (ac, sessions) = test_autocompact(&tmp, 15);

        let mut persisted = Session::new("cli:test".into());
        persisted.add_message("user", "fresh from disk", serde_json::Map::new());
        persisted.updated_at = Utc::now() - chrono::Duration::minutes(20);
        sessions.lock().unwrap().save(persisted.clone()).unwrap();

        // Caller's local copy is stale (empty messages) but shares the same
        // old `updated_at`, so `is_expired` alone (no archiving flag needed)
        // must trigger the reload — matches nanobot's `compact_idle_session`
        // never touching `updated_at`, so an archived session still reads
        // as expired right after being archived.
        let mut stale_local = Session::new("cli:test".into());
        stale_local.updated_at = persisted.updated_at;

        let (returned, _summary) = ac.prepare_session(stale_local, "cli:test");

        assert_eq!(returned.messages.len(), 1);
    }
}
