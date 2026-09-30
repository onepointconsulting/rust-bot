//! Per-session state of the ACP agent: project folder and the running turn.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use futures::future::AbortHandle;

/// Why a turn could not be started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnError {
    /// No such session (never created, or the id is wrong).
    UnknownSession,
    /// The session already has a turn in flight; ACP allows one prompt at a time.
    Busy,
}

struct SessionEntry {
    /// Project folder the session works in (`session/new.cwd`).
    cwd: PathBuf,
    /// Abort handle of the turn currently running, if any.
    running: Option<AbortHandle>,
}

/// Sessions created through `session/new`, shared by the request handlers and the hook.
#[derive(Default)]
pub struct SessionRegistry {
    sessions: Mutex<HashMap<String, SessionEntry>>,
}

impl SessionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    fn sessions(&self) -> std::sync::MutexGuard<'_, HashMap<String, SessionEntry>> {
        self.sessions.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Register a new session working in `cwd`.
    pub fn insert(&self, session_id: String, cwd: PathBuf) {
        self.sessions()
            .insert(session_id, SessionEntry { cwd, running: None });
    }

    /// Project folder of a session.
    pub fn cwd_of(&self, session_id: &str) -> Option<PathBuf> {
        self.sessions()
            .get(session_id)
            .map(|entry| entry.cwd.clone())
    }

    /// Whether the session currently has a turn in flight.
    pub fn is_running(&self, session_id: &str) -> bool {
        self.sessions()
            .get(session_id)
            .is_some_and(|entry| entry.running.is_some())
    }

    /// Mark a turn as running, or explain why it cannot start.
    pub fn begin_turn(&self, session_id: &str, handle: AbortHandle) -> Result<(), TurnError> {
        let mut sessions = self.sessions();
        let entry = sessions
            .get_mut(session_id)
            .ok_or(TurnError::UnknownSession)?;
        if entry.running.is_some() {
            return Err(TurnError::Busy);
        }
        entry.running = Some(handle);
        Ok(())
    }

    /// Mark the session's turn as finished.
    pub fn end_turn(&self, session_id: &str) {
        if let Some(entry) = self.sessions().get_mut(session_id) {
            entry.running = None;
        }
    }

    /// Abort the running turn of a session. Returns whether one was running.
    ///
    /// The turn stays registered as running until [`Self::end_turn`]: a prompt
    /// that arrives while the abort is still taking effect is answered with
    /// [`TurnError::Busy`] instead of overlapping the dying turn.
    pub fn cancel(&self, session_id: &str) -> bool {
        match self
            .sessions()
            .get(session_id)
            .and_then(|entry| entry.running.as_ref())
        {
            Some(handle) => {
                handle.abort();
                true
            }
            None => false,
        }
    }

    /// Abort every running turn (used when the client goes away).
    pub fn cancel_all(&self) {
        for entry in self.sessions().values() {
            if let Some(handle) = &entry.running {
                handle.abort();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::future::{Abortable, abortable, pending};

    fn idle_handle() -> (AbortHandle, Abortable<futures::future::Pending<()>>) {
        let (future, handle) = abortable(pending::<()>());
        (handle, future)
    }

    #[test]
    fn unknown_session_cannot_start_a_turn() {
        let registry = SessionRegistry::new();
        let (handle, _future) = idle_handle();
        assert_eq!(
            registry.begin_turn("nope", handle),
            Err(TurnError::UnknownSession)
        );
    }

    #[test]
    fn a_session_runs_one_turn_at_a_time() {
        let registry = SessionRegistry::new();
        registry.insert("s1".into(), PathBuf::from("/work"));
        let (first, _first_future) = idle_handle();
        let (second, _second_future) = idle_handle();
        assert_eq!(registry.begin_turn("s1", first), Ok(()));
        assert_eq!(registry.begin_turn("s1", second), Err(TurnError::Busy));
        registry.end_turn("s1");
        let (third, _third_future) = idle_handle();
        assert_eq!(registry.begin_turn("s1", third), Ok(()));
    }

    #[test]
    fn is_running_follows_the_turn() {
        let registry = SessionRegistry::new();
        registry.insert("s1".into(), PathBuf::from("/work"));
        assert!(!registry.is_running("s1"));
        let (handle, _future) = idle_handle();
        registry.begin_turn("s1", handle).unwrap();
        assert!(registry.is_running("s1"));
        registry.end_turn("s1");
        assert!(!registry.is_running("s1"));
        assert!(!registry.is_running("unknown"));
    }

    #[test]
    fn cwd_is_remembered_per_session() {
        let registry = SessionRegistry::new();
        registry.insert("s1".into(), PathBuf::from("/a"));
        registry.insert("s2".into(), PathBuf::from("/b"));
        assert_eq!(registry.cwd_of("s1"), Some(PathBuf::from("/a")));
        assert_eq!(registry.cwd_of("s2"), Some(PathBuf::from("/b")));
        assert_eq!(registry.cwd_of("s3"), None);
    }

    #[tokio::test]
    async fn cancel_aborts_only_the_running_turn_of_that_session() {
        let registry = SessionRegistry::new();
        registry.insert("s1".into(), PathBuf::from("/a"));
        registry.insert("s2".into(), PathBuf::from("/b"));
        let (handle_one, future_one) = idle_handle();
        let (handle_two, future_two) = idle_handle();
        registry.begin_turn("s1", handle_one).unwrap();
        registry.begin_turn("s2", handle_two).unwrap();

        assert!(registry.cancel("s1"));
        assert!(future_one.await.is_err(), "s1 turn must be aborted");

        // The aborted turn still counts as running until it reports back.
        let (late, _late_future) = idle_handle();
        assert_eq!(registry.begin_turn("s1", late), Err(TurnError::Busy));
        registry.end_turn("s1");
        assert!(!registry.cancel("s1"), "nothing left to cancel");

        registry.cancel_all();
        assert!(future_two.await.is_err(), "cancel_all aborts s2 too");
    }
}
