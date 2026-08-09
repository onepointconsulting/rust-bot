//! Validation, authorization, and persistence for *requested* workspace-scope
//! changes — the layer nanobot's `dispatch_envelope` calls into. Port of
//! `nanobot/webui/workspaces.py`.
//!
//! Builds on the core model in `security::workspace_access` (Layer 1) and
//! will eventually be called from the not-yet-implemented websocket envelope
//! dispatcher; nothing in `channels::websocket` calls into this module yet.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::{
    config::paths::get_webui_dir,
    security::{
        WorkspaceAccessMode, WorkspaceScope, WorkspaceScopeError, WORKSPACE_SCOPE_METADATA_KEY,
        build_workspace_scope, default_workspace_scope, validate_workspace_scope_payload,
        workspace_scope_from_metadata,
    },
    session::manager::SessionManager,
    utils::helpers::write_text_atomic,
};

const WEBUI_WORKSPACE_STATE_SCHEMA_VERSION: u16 = 1;
const MAX_STATE_FILE_BYTES: u64 = 128 * 1024;
const WEBUI_SCOPE_CHANNEL: &str = "websocket";

/// Allow a remote request only when it keeps the project and does not add access.
fn scope_change_is_non_escalating(current: &WorkspaceScope, requested: &WorkspaceScope) -> bool {
    requested.project_path == current.project_path
        && (!current.restrict_to_workspace || requested.restrict_to_workspace)
}

pub fn webui_workspace_state_path() -> PathBuf {
    get_webui_dir().join("workspace-state.json")
}

/// The WebUI-wide default access mode toggle — a *different* two-state
/// concept than [`WorkspaceAccessMode`]: `Default` means "defer to the
/// process config," not "restricted."
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DefaultAccessMode {
    Default,
    Full,
}

impl DefaultAccessMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Full => "full",
        }
    }

    /// Parses `"default"`/`"full"`; the legacy `"restricted"` value silently
    /// remaps to `Default`, matching nanobot's `write_webui_default_access_mode`.
    fn parse_with_legacy_remap(s: &str) -> Option<Self> {
        match s {
            "default" | "restricted" => Some(Self::Default),
            "full" => Some(Self::Full),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct WebuiWorkspaceState {
    schema_version: u16,
    default_access_mode: DefaultAccessMode,
    updated_at: Option<String>,
}

impl Default for WebuiWorkspaceState {
    fn default() -> Self {
        Self {
            schema_version: WEBUI_WORKSPACE_STATE_SCHEMA_VERSION,
            default_access_mode: DefaultAccessMode::Default,
            updated_at: None,
        }
    }
}

fn load_state_from(path: &Path) -> WebuiWorkspaceState {
    match std::fs::metadata(path) {
        Ok(meta) if meta.len() > MAX_STATE_FILE_BYTES => {
            log::warn!("webui workspace state too large, ignoring: {}", path.display());
            return WebuiWorkspaceState::default();
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return WebuiWorkspaceState::default();
        }
        Err(e) => {
            log::warn!("read webui workspace state failed {}: {e}", path.display());
            return WebuiWorkspaceState::default();
        }
    }

    match std::fs::read_to_string(path) {
        Ok(contents) => serde_json::from_str(&contents).unwrap_or_else(|e| {
            log::warn!("read webui workspace state failed {}: {e}", path.display());
            WebuiWorkspaceState::default()
        }),
        Err(e) => {
            log::warn!("read webui workspace state failed {}: {e}", path.display());
            WebuiWorkspaceState::default()
        }
    }
}

fn save_state_to(path: &Path, state: &WebuiWorkspaceState) -> Result<(), String> {
    let contents = serde_json::to_string_pretty(state).map_err(|e| e.to_string())?;
    if contents.len() as u64 > MAX_STATE_FILE_BYTES {
        return Err("workspace state is too large".to_string());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    write_text_atomic(path, &contents).map_err(|e| e.to_string())
}

fn read_default_access_mode_at(path: &Path) -> DefaultAccessMode {
    load_state_from(path).default_access_mode
}

/// Returns whether the mode actually changed (matches nanobot's bool return).
fn write_default_access_mode_at(path: &Path, mode: DefaultAccessMode) -> Result<bool, String> {
    let mut state = load_state_from(path);
    let changed = state.default_access_mode != mode;
    if changed {
        state.default_access_mode = mode;
        state.updated_at = Some(chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string());
        save_state_to(path, &state)?;
    }
    Ok(changed)
}

pub fn read_webui_default_access_mode() -> DefaultAccessMode {
    read_default_access_mode_at(&webui_workspace_state_path())
}

pub fn write_webui_default_access_mode(mode: DefaultAccessMode) -> Result<bool, String> {
    write_default_access_mode_at(&webui_workspace_state_path(), mode)
}

pub fn default_scope_for_webui(
    default_workspace: &Path,
    default_restrict_to_workspace: bool,
) -> WorkspaceScope {
    match read_webui_default_access_mode() {
        DefaultAccessMode::Default => default_workspace_scope(
            default_workspace,
            default_restrict_to_workspace,
            Some(WEBUI_SCOPE_CHANNEL),
        ),
        DefaultAccessMode::Full => build_workspace_scope(
            default_workspace,
            WorkspaceAccessMode::Full,
            Some(WEBUI_SCOPE_CHANNEL),
        ),
    }
}

pub fn workspaces_payload(
    default_workspace: &Path,
    default_restrict_to_workspace: bool,
    controls_available: bool,
) -> serde_json::Value {
    let mode = read_webui_default_access_mode();
    let scope = default_scope_for_webui(default_workspace, default_restrict_to_workspace);
    serde_json::json!({
        "schema_version": WEBUI_WORKSPACE_STATE_SCHEMA_VERSION,
        "default_access_mode": mode.as_str(),
        "default_scope": scope.payload(),
        "controls": {
            "can_change_project": controls_available,
            "can_use_full_access": controls_available,
        },
    })
}

/// Owns validation, escalation-authorization, and persistence for a
/// *requested* workspace-scope change. Port of nanobot's
/// `WebUIWorkspaceController`, renamed since nothing here is actually
/// web-specific — only its future caller (the websocket envelope
/// dispatcher) is. Takes `&mut SessionManager` per call rather than owning
/// one, matching every other session-touching method in this codebase
/// (`AgentLoop::set_session_model_preset`, `set_session_workspace_scope`).
#[derive(Clone)]
pub struct WorkspaceRequestHandler {
    default_workspace: PathBuf,
    default_restrict_to_workspace: bool,
}

impl WorkspaceRequestHandler {
    pub fn new(default_workspace: PathBuf, default_restrict_to_workspace: bool) -> Self {
        Self { default_workspace, default_restrict_to_workspace }
    }

    pub fn default_scope(&self) -> WorkspaceScope {
        default_scope_for_webui(&self.default_workspace, self.default_restrict_to_workspace)
    }

    /// Resolve a session's persisted scope override, if any, else the
    /// WebUI-aware default. Uses `get_or_create_session` (the only session
    /// lookup this codebase has); unlike nanobot's `read_session_metadata`
    /// this can create an empty session record for a chat_id with no prior
    /// history — a minor, accepted tradeoff (see the plan notes).
    pub fn scope_for_session_key(
        &self,
        session_manager: &mut SessionManager,
        session_key: &str,
    ) -> WorkspaceScope {
        let session = session_manager.get_or_create_session(session_key);
        workspace_scope_from_metadata(
            &session.metadata,
            &self.default_workspace,
            self.default_restrict_to_workspace,
            Some(WEBUI_SCOPE_CHANNEL),
        )
    }

    pub fn payload(&self, controls_available: bool) -> serde_json::Value {
        workspaces_payload(
            &self.default_workspace,
            self.default_restrict_to_workspace,
            controls_available,
        )
    }

    /// Validate an envelope's requested scope against the current one
    /// (session override if `session_key` is `Some`, else the default),
    /// enforcing the localhost-only escalation rule when `controls_available`
    /// is `false`.
    pub fn scope_from_envelope(
        &self,
        session_manager: &mut SessionManager,
        envelope: &HashMap<String, serde_json::Value>,
        session_key: Option<&str>,
        controls_available: bool,
    ) -> Result<WorkspaceScope, WorkspaceScopeError> {
        let current = match session_key {
            Some(key) => self.scope_for_session_key(session_manager, key),
            None => self.default_scope(),
        };
        let scope = match envelope.get(WORKSPACE_SCOPE_METADATA_KEY) {
            None => current.clone(),
            Some(raw) => validate_workspace_scope_payload(
                raw,
                &self.default_workspace,
                self.default_restrict_to_workspace,
                Some(WEBUI_SCOPE_CHANNEL),
            )?,
        };
        if !controls_available && !scope_change_is_non_escalating(&current, &scope) {
            return Err(WorkspaceScopeError::new(403, "workspace controls are localhost-only"));
        }
        Ok(scope)
    }

    pub fn scope_for_new_chat(
        &self,
        session_manager: &mut SessionManager,
        envelope: &HashMap<String, serde_json::Value>,
        controls_available: bool,
    ) -> Result<WorkspaceScope, WorkspaceScopeError> {
        self.scope_from_envelope(session_manager, envelope, None, controls_available)
    }

    /// `chat_running` hard-rejects (`409`) — you can't swap the sandbox out
    /// from under a running turn.
    pub fn scope_for_set_request(
        &self,
        session_manager: &mut SessionManager,
        envelope: &HashMap<String, serde_json::Value>,
        chat_id: &str,
        chat_running: bool,
        controls_available: bool,
    ) -> Result<WorkspaceScope, WorkspaceScopeError> {
        if chat_running {
            return Err(WorkspaceScopeError::new(409, "chat_running"));
        }
        let session_key = format!("websocket:{chat_id}");
        self.scope_from_envelope(session_manager, envelope, Some(&session_key), controls_available)
    }

    /// Like [`Self::scope_for_set_request`], but only rejects (`409`) while
    /// `chat_running` if the envelope's requested scope actually *differs*
    /// from what's already persisted — a same-value resend mid-turn is
    /// harmless and allowed.
    pub fn scope_for_message(
        &self,
        session_manager: &mut SessionManager,
        envelope: &HashMap<String, serde_json::Value>,
        chat_id: &str,
        chat_running: bool,
        controls_available: bool,
    ) -> Result<WorkspaceScope, WorkspaceScopeError> {
        let session_key = format!("websocket:{chat_id}");
        let scope =
            self.scope_from_envelope(session_manager, envelope, Some(&session_key), controls_available)?;
        if envelope.contains_key(WORKSPACE_SCOPE_METADATA_KEY)
            && chat_running
            && scope.metadata() != self.scope_for_session_key(session_manager, &session_key).metadata()
        {
            return Err(WorkspaceScopeError::new(409, "chat_running"));
        }
        Ok(scope)
    }

    /// Persist `scope` for `chat_id`'s websocket session, tagging it as a
    /// WebUI session. Deliberately separate from
    /// `AgentLoop::set_session_workspace_scope` (the generic, channel-agnostic
    /// path our `/workspace` command uses) — this one hardcodes the
    /// `websocket:{chat_id}` session-key format and the `webui` tag, both
    /// specific to the future envelope-driven caller.
    pub fn persist_scope(&self, session_manager: &mut SessionManager, chat_id: &str, scope: &WorkspaceScope) {
        let session_key = format!("websocket:{chat_id}");
        let session = session_manager.get_or_create_session(&session_key);
        session.metadata.insert("webui".to_string(), serde_json::json!(true));
        session
            .metadata
            .insert(WORKSPACE_SCOPE_METADATA_KEY.to_string(), scope.metadata());
        let snapshot = session.clone();
        if let Err(e) = session_manager.save(snapshot) {
            log::error!("Failed to save session after persisting workspace scope: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn state_path(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("workspace-state.json")
    }

    // --- DefaultAccessMode ---

    #[test]
    fn default_access_mode_parses_default_full_and_legacy_restricted() {
        assert_eq!(DefaultAccessMode::parse_with_legacy_remap("default"), Some(DefaultAccessMode::Default));
        assert_eq!(DefaultAccessMode::parse_with_legacy_remap("full"), Some(DefaultAccessMode::Full));
        assert_eq!(DefaultAccessMode::parse_with_legacy_remap("restricted"), Some(DefaultAccessMode::Default));
    }

    #[test]
    fn default_access_mode_rejects_unknown_string() {
        assert_eq!(DefaultAccessMode::parse_with_legacy_remap("bogus"), None);
    }

    #[test]
    fn default_access_mode_as_str_matches_parse() {
        assert_eq!(DefaultAccessMode::Default.as_str(), "default");
        assert_eq!(DefaultAccessMode::Full.as_str(), "full");
    }

    // --- state file ---

    #[test]
    fn load_state_from_missing_file_returns_default() {
        let dir = tempfile::tempdir().unwrap();
        let state = load_state_from(&state_path(&dir));
        assert_eq!(state, WebuiWorkspaceState::default());
    }

    #[test]
    fn load_state_from_corrupted_file_resets_to_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = state_path(&dir);
        std::fs::write(&path, b"not valid json").unwrap();
        let state = load_state_from(&path);
        assert_eq!(state, WebuiWorkspaceState::default());
    }

    #[test]
    fn load_state_from_oversized_file_resets_to_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = state_path(&dir);
        let oversized = "x".repeat(MAX_STATE_FILE_BYTES as usize + 1);
        std::fs::write(&path, oversized).unwrap();
        let state = load_state_from(&path);
        assert_eq!(state, WebuiWorkspaceState::default());
    }

    #[test]
    fn write_default_access_mode_at_reports_whether_it_changed() {
        let dir = tempfile::tempdir().unwrap();
        let path = state_path(&dir);

        let changed = write_default_access_mode_at(&path, DefaultAccessMode::Full).unwrap();
        assert!(changed);
        let state = load_state_from(&path);
        assert_eq!(state.default_access_mode, DefaultAccessMode::Full);
        assert!(state.updated_at.is_some());

        let changed_again = write_default_access_mode_at(&path, DefaultAccessMode::Full).unwrap();
        assert!(!changed_again);
    }

    #[test]
    fn default_scope_for_webui_reflects_full_override() {
        let dir = tempfile::tempdir().unwrap();
        let path = state_path(&dir);
        write_default_access_mode_at(&path, DefaultAccessMode::Full).unwrap();
        let state = load_state_from(&path);

        // Exercise the same branch `default_scope_for_webui` uses, without
        // depending on the process-global `get_webui_dir()` path.
        let scope = match state.default_access_mode {
            DefaultAccessMode::Default => {
                default_workspace_scope(dir.path(), true, Some(WEBUI_SCOPE_CHANNEL))
            }
            DefaultAccessMode::Full => {
                build_workspace_scope(dir.path(), WorkspaceAccessMode::Full, Some(WEBUI_SCOPE_CHANNEL))
            }
        };
        assert_eq!(scope.access_mode, WorkspaceAccessMode::Full);
        assert!(!scope.restrict_to_workspace);
    }

    // --- scope_change_is_non_escalating ---

    fn scope_at(dir: &Path, mode: WorkspaceAccessMode) -> WorkspaceScope {
        build_workspace_scope(dir, mode, None)
    }

    #[test]
    fn scope_change_is_non_escalating_rejects_project_change() {
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();
        let current = scope_at(dir_a.path(), WorkspaceAccessMode::Restricted);
        let requested = scope_at(dir_b.path(), WorkspaceAccessMode::Restricted);
        assert!(!scope_change_is_non_escalating(&current, &requested));
    }

    #[test]
    fn scope_change_is_non_escalating_rejects_privilege_escalation() {
        let dir = tempfile::tempdir().unwrap();
        let current = scope_at(dir.path(), WorkspaceAccessMode::Restricted);
        let requested = scope_at(dir.path(), WorkspaceAccessMode::Full);
        assert!(!scope_change_is_non_escalating(&current, &requested));
    }

    #[test]
    fn scope_change_is_non_escalating_allows_same_project_tightening() {
        let dir = tempfile::tempdir().unwrap();
        let current = scope_at(dir.path(), WorkspaceAccessMode::Full);
        let requested = scope_at(dir.path(), WorkspaceAccessMode::Restricted);
        assert!(scope_change_is_non_escalating(&current, &requested));
    }

    // --- WorkspaceRequestHandler ---

    fn handler_and_sessions(default_dir: &Path) -> (WorkspaceRequestHandler, SessionManager) {
        let handler = WorkspaceRequestHandler::new(default_dir.to_path_buf(), true);
        let sessions = SessionManager::new(default_dir.to_path_buf());
        (handler, sessions)
    }

    #[test]
    fn scope_from_envelope_rejects_escalation_when_controls_unavailable() {
        let default_dir = tempfile::tempdir().unwrap();
        let other_dir = tempfile::tempdir().unwrap();
        let (handler, mut sessions) = handler_and_sessions(default_dir.path());

        let mut envelope = HashMap::new();
        envelope.insert(
            WORKSPACE_SCOPE_METADATA_KEY.to_string(),
            json!({"project_path": other_dir.path().display().to_string(), "access_mode": "full"}),
        );

        let err = handler
            .scope_from_envelope(&mut sessions, &envelope, None, false)
            .unwrap_err();
        assert_eq!(err.status, 403);
    }

    #[test]
    fn scope_from_envelope_allows_escalation_when_controls_available() {
        let default_dir = tempfile::tempdir().unwrap();
        let other_dir = tempfile::tempdir().unwrap();
        let (handler, mut sessions) = handler_and_sessions(default_dir.path());

        let mut envelope = HashMap::new();
        envelope.insert(
            WORKSPACE_SCOPE_METADATA_KEY.to_string(),
            json!({"project_path": other_dir.path().display().to_string(), "access_mode": "full"}),
        );

        let scope = handler
            .scope_from_envelope(&mut sessions, &envelope, None, true)
            .unwrap();
        assert_eq!(scope.project_path, other_dir.path());
    }

    #[test]
    fn scope_for_set_request_rejects_when_chat_running() {
        let default_dir = tempfile::tempdir().unwrap();
        let (handler, mut sessions) = handler_and_sessions(default_dir.path());
        let envelope = HashMap::new();

        let err = handler
            .scope_for_set_request(&mut sessions, &envelope, "chat-1", true, true)
            .unwrap_err();
        assert_eq!(err.status, 409);
    }

    #[test]
    fn scope_for_message_allows_resend_of_same_scope_while_running() {
        let default_dir = tempfile::tempdir().unwrap();
        let (handler, mut sessions) = handler_and_sessions(default_dir.path());

        let scope = handler.default_scope();
        handler.persist_scope(&mut sessions, "chat-1", &scope);

        let mut envelope = HashMap::new();
        envelope.insert(WORKSPACE_SCOPE_METADATA_KEY.to_string(), scope.metadata());

        let result = handler.scope_for_message(&mut sessions, &envelope, "chat-1", true, true);
        assert!(result.is_ok());
    }

    #[test]
    fn scope_for_message_rejects_actual_change_while_running() {
        let default_dir = tempfile::tempdir().unwrap();
        let other_dir = tempfile::tempdir().unwrap();
        let (handler, mut sessions) = handler_and_sessions(default_dir.path());

        let scope = handler.default_scope();
        handler.persist_scope(&mut sessions, "chat-1", &scope);

        let mut envelope = HashMap::new();
        envelope.insert(
            WORKSPACE_SCOPE_METADATA_KEY.to_string(),
            json!({"project_path": other_dir.path().display().to_string(), "access_mode": "restricted"}),
        );

        let err = handler
            .scope_for_message(&mut sessions, &envelope, "chat-1", true, true)
            .unwrap_err();
        assert_eq!(err.status, 409);
    }

    #[test]
    fn persist_scope_then_scope_for_session_key_roundtrips() {
        let default_dir = tempfile::tempdir().unwrap();
        let project_dir = tempfile::tempdir().unwrap();
        let (handler, mut sessions) = handler_and_sessions(default_dir.path());

        let scope = build_workspace_scope(project_dir.path(), WorkspaceAccessMode::Full, None);
        handler.persist_scope(&mut sessions, "chat-1", &scope);

        let reloaded = handler.scope_for_session_key(&mut sessions, "websocket:chat-1");
        assert_eq!(reloaded.project_path, project_dir.path());
        assert_eq!(reloaded.access_mode, WorkspaceAccessMode::Full);

        let session = sessions.get_or_create_session("websocket:chat-1");
        assert_eq!(session.metadata.get("webui"), Some(&json!(true)));
    }
}
