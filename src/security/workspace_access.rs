//! Per-turn workspace scoping: lets one turn use a different project
//! directory / access level than the process's fixed default, without ever
//! moving where session transcripts are stored. Port of nanobot's
//! `nanobot/security/workspace_access.py`. See `agent::workspace_context`
//! for the ambient (task-local) binding that makes this take effect without
//! reconstructing tools.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub use crate::session::keys::WORKSPACE_SCOPE_METADATA_KEY;

/// Env vars used to detect OS-level sandbox enforcement of workspace
/// restriction. rust-bot house prefix, not nanobot's `NANOBOT_*` names.
pub const RUST_BOT_WORKSPACE_SANDBOX_PROVIDER: &str = "RUST_BOT_WORKSPACE_SANDBOX_PROVIDER";
pub const RUST_BOT_WORKSPACE_SANDBOX_ENFORCED: &str = "RUST_BOT_WORKSPACE_SANDBOX_ENFORCED";

/// How much of the filesystem tools operating under this scope may touch.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum WorkspaceAccessMode {
    Restricted,
    Full,
}

impl WorkspaceAccessMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Restricted => "restricted",
            Self::Full => "full",
        }
    }
}

impl fmt::Display for WorkspaceAccessMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl FromStr for WorkspaceAccessMode {
    type Err = WorkspaceScopeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "restricted" => Ok(Self::Restricted),
            "full" => Ok(Self::Full),
            other => Err(WorkspaceScopeError::new(
                400,
                format!("Invalid workspace access_mode '{other}'; expected 'restricted' or 'full'"),
            )),
        }
    }
}

/// Raised only for a *live* client/command request. Never returned for
/// stale persisted metadata — see [`workspace_scope_from_metadata`], which
/// falls back to the default scope instead of propagating this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceScopeError {
    pub status: u16,
    pub message: String,
}

impl WorkspaceScopeError {
    pub fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}

impl fmt::Display for WorkspaceScopeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for WorkspaceScopeError {}

/// Describes *how* restriction is enforced (OS sandbox vs. application-level
/// guard only) — for UI/diagnostic display; not itself an enforcement
/// mechanism (that's [`ToolWorkspace::allowed_root`] and the tools that
/// consult it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceSandboxStatus {
    pub restrict_to_workspace: bool,
    pub workspace_root: PathBuf,
    pub level: String,
    pub enforced: bool,
    pub provider: Option<String>,
    pub provider_label: String,
    pub summary: String,
}

fn env_lookup(environ: Option<&HashMap<String, String>>, key: &str) -> Option<String> {
    match environ {
        Some(map) => map.get(key).cloned(),
        None => std::env::var(key).ok(),
    }
}

/// Report whether `restrict_to_workspace` is backed by an OS-level sandbox
/// (`RUST_BOT_WORKSPACE_SANDBOX_PROVIDER`/`_ENFORCED`) or is
/// application-level guard only. `environ` overrides `std::env::var`
/// lookups for testability.
pub fn workspace_sandbox_status(
    restrict_to_workspace: bool,
    workspace: &Path,
    environ: Option<&HashMap<String, String>>,
) -> WorkspaceSandboxStatus {
    let provider =
        env_lookup(environ, RUST_BOT_WORKSPACE_SANDBOX_PROVIDER).filter(|s| !s.is_empty());
    let enforced_flag = env_lookup(environ, RUST_BOT_WORKSPACE_SANDBOX_ENFORCED)
        .map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false);

    let (level, enforced, provider_label) = if !restrict_to_workspace {
        ("none".to_string(), false, "none".to_string())
    } else if provider.is_some() || enforced_flag {
        (
            "system".to_string(),
            true,
            provider.clone().unwrap_or_else(|| "system".to_string()),
        )
    } else {
        ("application".to_string(), false, "application".to_string())
    };

    let summary = if !restrict_to_workspace {
        "Workspace restriction is off; tools may access the full filesystem.".to_string()
    } else if enforced {
        format!("Workspace access is OS-sandboxed ({provider_label}).")
    } else {
        "Workspace access is restricted at the application level only (no OS sandbox detected)."
            .to_string()
    };

    WorkspaceSandboxStatus {
        restrict_to_workspace,
        workspace_root: workspace.to_path_buf(),
        level,
        enforced,
        provider,
        provider_label,
        summary,
    }
}

/// A resolved, effective workspace scope for one turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceScope {
    pub project_path: PathBuf,
    pub access_mode: WorkspaceAccessMode,
    pub restrict_to_workspace: bool,
    pub sandbox_status: WorkspaceSandboxStatus,
    pub source_channel: Option<String>,
}

impl WorkspaceScope {
    pub fn project_name(&self) -> String {
        self.project_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.project_path.display().to_string())
    }

    /// The small dict actually persisted into session metadata.
    pub fn metadata(&self) -> Value {
        json!({
            "project_path": self.project_path.display().to_string(),
            "access_mode": self.access_mode.as_str(),
        })
    }

    /// `metadata()` plus display extras (for UI/diagnostics — not persisted).
    pub fn payload(&self) -> Value {
        let mut v = self.metadata();
        if let Value::Object(ref mut m) = v {
            m.insert("project_name".into(), json!(self.project_name()));
            m.insert(
                "restrict_to_workspace".into(),
                json!(self.restrict_to_workspace),
            );
            m.insert(
                "sandbox_status".into(),
                serde_json::to_value(&self.sandbox_status).unwrap_or(Value::Null),
            );
        }
        v
    }
}

/// What a tool should actually read/write against right now.
#[derive(Debug, Clone)]
pub struct ToolWorkspace {
    pub project_path: Option<PathBuf>,
    pub restrict_to_workspace: bool,
    pub scope: Option<WorkspaceScope>,
}

impl ToolWorkspace {
    pub fn allowed_root(&self) -> Option<PathBuf> {
        if self.restrict_to_workspace {
            self.project_path.clone()
        } else {
            None
        }
    }
}

/// Owns default-scope construction and session-metadata-based override
/// resolution.
///
/// Deliberate deviation from nanobot: nanobot's `for_turn` only honors a
/// session's persisted scope when the turn's channel equals `scoped_channel`
/// (default `"websocket"`) — every other channel always gets the default.
/// rust-bot has no websocket channel yet, so `for_session` below applies
/// unconditionally, regardless of channel. `scoped_channel` is kept as a
/// reserved field for when a future envelope layer adds a *message-level*
/// override, which would then gate on it the way nanobot's `for_turn` does.
#[derive(Debug, Clone)]
pub struct WorkspaceScopeResolver {
    pub default_workspace: PathBuf,
    pub default_restrict_to_workspace: bool,
    pub scoped_channel: String,
}

impl WorkspaceScopeResolver {
    pub fn new(default_workspace: PathBuf, default_restrict_to_workspace: bool) -> Self {
        Self {
            default_workspace,
            default_restrict_to_workspace,
            scoped_channel: "websocket".to_string(),
        }
    }

    pub fn default(&self) -> WorkspaceScope {
        default_workspace_scope(
            &self.default_workspace,
            self.default_restrict_to_workspace,
            None,
        )
    }

    /// Resolve the effective scope for a turn from persisted session
    /// metadata (falls back to `default()` when absent or stale).
    pub fn for_session(&self, session_metadata: Option<&HashMap<String, Value>>) -> WorkspaceScope {
        resolve_effective_workspace_scope(
            session_metadata,
            &self.default_workspace,
            self.default_restrict_to_workspace,
        )
    }
}

pub fn default_access_mode(restrict: bool) -> WorkspaceAccessMode {
    if restrict {
        WorkspaceAccessMode::Restricted
    } else {
        WorkspaceAccessMode::Full
    }
}

pub fn build_workspace_scope(
    project_path: &Path,
    access_mode: WorkspaceAccessMode,
    source_channel: Option<&str>,
) -> WorkspaceScope {
    let restrict_to_workspace = matches!(access_mode, WorkspaceAccessMode::Restricted);
    let sandbox_status = workspace_sandbox_status(restrict_to_workspace, project_path, None);
    WorkspaceScope {
        project_path: project_path.to_path_buf(),
        access_mode,
        restrict_to_workspace,
        sandbox_status,
        source_channel: source_channel.map(str::to_string),
    }
}

pub fn default_workspace_scope(
    workspace: &Path,
    restrict: bool,
    source_channel: Option<&str>,
) -> WorkspaceScope {
    build_workspace_scope(workspace, default_access_mode(restrict), source_channel)
}

/// Validate a client/command-supplied `{ project_path, access_mode }`
/// request. `project_path` falls back to `default_workspace` when omitted
/// (matching nanobot); once resolved it must be an absolute, existing
/// directory. `access_mode` defaults to
/// `default_access_mode(default_restrict_to_workspace)` when omitted. This
/// is the only function in this module that returns `Err`.
pub fn validate_workspace_scope_payload(
    raw: &Value,
    default_workspace: &Path,
    default_restrict_to_workspace: bool,
    source_channel: Option<&str>,
) -> Result<WorkspaceScope, WorkspaceScopeError> {
    if !raw.is_null() && !raw.is_object() {
        return Err(WorkspaceScopeError::new(
            400,
            "workspace_scope must be an object",
        ));
    }

    let project_path_str = raw
        .get("project_path")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let project_path = match project_path_str {
        Some(s) => PathBuf::from(s),
        None => default_workspace.to_path_buf(),
    };
    if !project_path.is_absolute() {
        return Err(WorkspaceScopeError::new(
            400,
            format!(
                "workspace_scope.project_path must be an absolute path: {}",
                project_path.display()
            ),
        ));
    }
    if !project_path.is_dir() {
        return Err(WorkspaceScopeError::new(
            400,
            format!(
                "workspace_scope.project_path does not exist or is not a directory: {}",
                project_path.display()
            ),
        ));
    }

    let access_mode = match raw.get("access_mode").and_then(Value::as_str) {
        Some(s) => s.parse::<WorkspaceAccessMode>()?,
        None => default_access_mode(default_restrict_to_workspace),
    };

    Ok(build_workspace_scope(
        &project_path,
        access_mode,
        source_channel,
    ))
}

/// Read a persisted scope from metadata. Never propagates
/// [`WorkspaceScopeError`] — malformed/stale persisted data silently falls
/// back to the default scope instead (only a *live* validate call raises).
pub fn workspace_scope_from_metadata(
    metadata: &HashMap<String, Value>,
    default_workspace: &Path,
    default_restrict_to_workspace: bool,
    source_channel: Option<&str>,
) -> WorkspaceScope {
    match metadata.get(WORKSPACE_SCOPE_METADATA_KEY) {
        Some(raw) => validate_workspace_scope_payload(
            raw,
            default_workspace,
            default_restrict_to_workspace,
            source_channel,
        )
        .unwrap_or_else(|_| {
            default_workspace_scope(
                default_workspace,
                default_restrict_to_workspace,
                source_channel,
            )
        }),
        None => default_workspace_scope(
            default_workspace,
            default_restrict_to_workspace,
            source_channel,
        ),
    }
}

/// Session-metadata-only resolution (see the module-level deviation note on
/// [`WorkspaceScopeResolver`] — no message-metadata/channel gating yet;
/// that's reserved for a future envelope layer).
pub fn resolve_effective_workspace_scope(
    session_metadata: Option<&HashMap<String, Value>>,
    default_workspace: &Path,
    default_restrict_to_workspace: bool,
) -> WorkspaceScope {
    match session_metadata {
        Some(meta) => workspace_scope_from_metadata(
            meta,
            default_workspace,
            default_restrict_to_workspace,
            None,
        ),
        None => default_workspace_scope(default_workspace, default_restrict_to_workspace, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn access_mode_from_str_roundtrip_and_rejects_unknown() {
        assert_eq!(
            "restricted".parse::<WorkspaceAccessMode>().unwrap(),
            WorkspaceAccessMode::Restricted
        );
        assert_eq!(
            "full".parse::<WorkspaceAccessMode>().unwrap(),
            WorkspaceAccessMode::Full
        );
        let err = "bogus".parse::<WorkspaceAccessMode>().unwrap_err();
        assert_eq!(err.status, 400);
    }

    #[test]
    fn access_mode_display_matches_as_str() {
        assert_eq!(WorkspaceAccessMode::Restricted.to_string(), "restricted");
        assert_eq!(WorkspaceAccessMode::Full.to_string(), "full");
    }

    #[test]
    fn build_workspace_scope_sets_restrict_flag_from_access_mode() {
        let dir = tempfile::tempdir().unwrap();
        let restricted = build_workspace_scope(dir.path(), WorkspaceAccessMode::Restricted, None);
        assert!(restricted.restrict_to_workspace);
        let full = build_workspace_scope(dir.path(), WorkspaceAccessMode::Full, None);
        assert!(!full.restrict_to_workspace);
    }

    #[test]
    fn default_workspace_scope_restricted_vs_full() {
        let dir = tempfile::tempdir().unwrap();
        let scope = default_workspace_scope(dir.path(), true, None);
        assert_eq!(scope.access_mode, WorkspaceAccessMode::Restricted);
        let scope = default_workspace_scope(dir.path(), false, None);
        assert_eq!(scope.access_mode, WorkspaceAccessMode::Full);
    }

    #[test]
    fn validate_workspace_scope_payload_falls_back_to_default_workspace_when_project_path_omitted()
    {
        let default_dir = tempfile::tempdir().unwrap();
        let scope =
            validate_workspace_scope_payload(&json!({}), default_dir.path(), false, None).unwrap();
        assert_eq!(scope.project_path, default_dir.path());
    }

    #[test]
    fn validate_workspace_scope_payload_rejects_non_object_raw() {
        let default_dir = tempfile::tempdir().unwrap();
        let err = validate_workspace_scope_payload(
            &json!("not an object"),
            default_dir.path(),
            false,
            None,
        )
        .unwrap_err();
        assert_eq!(err.status, 400);

        let err =
            validate_workspace_scope_payload(&json!([1, 2, 3]), default_dir.path(), false, None)
                .unwrap_err();
        assert_eq!(err.status, 400);
    }

    #[test]
    fn validate_workspace_scope_payload_rejects_relative_path() {
        let default_dir = tempfile::tempdir().unwrap();
        let err = validate_workspace_scope_payload(
            &json!({"project_path": "relative/dir"}),
            default_dir.path(),
            false,
            None,
        )
        .unwrap_err();
        assert_eq!(err.status, 400);
    }

    #[test]
    fn validate_workspace_scope_payload_rejects_nonexistent_directory() {
        let default_dir = tempfile::tempdir().unwrap();
        let missing = std::env::temp_dir().join("rust-bot-workspace-access-test-missing-dir");
        let err = validate_workspace_scope_payload(
            &json!({"project_path": missing.display().to_string()}),
            default_dir.path(),
            false,
            None,
        )
        .unwrap_err();
        assert_eq!(err.status, 400);
    }

    #[test]
    fn validate_workspace_scope_payload_accepts_existing_absolute_dir_and_defaults_access_mode() {
        let default_dir = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let scope = validate_workspace_scope_payload(
            &json!({"project_path": dir.path().display().to_string()}),
            default_dir.path(),
            true,
            None,
        )
        .unwrap();
        assert_eq!(scope.access_mode, WorkspaceAccessMode::Restricted);
        assert_eq!(scope.project_path, dir.path());
    }

    #[test]
    fn validate_workspace_scope_payload_rejects_invalid_access_mode_string() {
        let default_dir = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let err = validate_workspace_scope_payload(
            &json!({"project_path": dir.path().display().to_string(), "access_mode": "bogus"}),
            default_dir.path(),
            false,
            None,
        )
        .unwrap_err();
        assert_eq!(err.status, 400);
    }

    #[test]
    fn workspace_scope_from_metadata_returns_default_when_key_absent() {
        let default_dir = tempfile::tempdir().unwrap();
        let metadata: HashMap<String, Value> = HashMap::new();
        let scope = workspace_scope_from_metadata(&metadata, default_dir.path(), false, None);
        assert_eq!(scope.project_path, default_dir.path());
    }

    #[test]
    fn workspace_scope_from_metadata_falls_back_silently_on_malformed_entry() {
        let default_dir = tempfile::tempdir().unwrap();
        let mut metadata: HashMap<String, Value> = HashMap::new();
        metadata.insert(
            WORKSPACE_SCOPE_METADATA_KEY.to_string(),
            json!({"project_path": "relative/not/absolute"}),
        );
        let scope = workspace_scope_from_metadata(&metadata, default_dir.path(), false, None);
        assert_eq!(scope.project_path, default_dir.path());
    }

    #[test]
    fn resolve_effective_workspace_scope_prefers_session_override() {
        let default_dir = tempfile::tempdir().unwrap();
        let override_dir = tempfile::tempdir().unwrap();
        let mut metadata: HashMap<String, Value> = HashMap::new();
        metadata.insert(
            WORKSPACE_SCOPE_METADATA_KEY.to_string(),
            json!({"project_path": override_dir.path().display().to_string(), "access_mode": "full"}),
        );
        let scope = resolve_effective_workspace_scope(Some(&metadata), default_dir.path(), true);
        assert_eq!(scope.project_path, override_dir.path());
        assert_eq!(scope.access_mode, WorkspaceAccessMode::Full);
    }

    #[test]
    fn resolve_effective_workspace_scope_returns_default_when_no_session_metadata() {
        let default_dir = tempfile::tempdir().unwrap();
        let scope = resolve_effective_workspace_scope(None, default_dir.path(), true);
        assert_eq!(scope.project_path, default_dir.path());
        assert_eq!(scope.access_mode, WorkspaceAccessMode::Restricted);
    }

    #[test]
    fn workspace_sandbox_status_reports_application_level_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let status = workspace_sandbox_status(true, dir.path(), Some(&HashMap::new()));
        assert_eq!(status.level, "application");
        assert!(!status.enforced);
    }

    #[test]
    fn workspace_sandbox_status_reports_none_when_unrestricted() {
        let dir = tempfile::tempdir().unwrap();
        let status = workspace_sandbox_status(false, dir.path(), Some(&HashMap::new()));
        assert_eq!(status.level, "none");
        assert!(!status.enforced);
    }

    #[test]
    fn workspace_sandbox_status_reports_system_when_env_provider_set() {
        let dir = tempfile::tempdir().unwrap();
        let environ = env_map(&[(RUST_BOT_WORKSPACE_SANDBOX_PROVIDER, "bwrap")]);
        let status = workspace_sandbox_status(true, dir.path(), Some(&environ));
        assert_eq!(status.level, "system");
        assert!(status.enforced);
        assert_eq!(status.provider.as_deref(), Some("bwrap"));
    }

    #[test]
    fn tool_workspace_allowed_root_none_when_unrestricted() {
        let tw = ToolWorkspace {
            project_path: Some(PathBuf::from("/some/path")),
            restrict_to_workspace: false,
            scope: None,
        };
        assert_eq!(tw.allowed_root(), None);
    }

    #[test]
    fn tool_workspace_allowed_root_some_when_restricted() {
        let tw = ToolWorkspace {
            project_path: Some(PathBuf::from("/some/path")),
            restrict_to_workspace: true,
            scope: None,
        };
        assert_eq!(tw.allowed_root(), Some(PathBuf::from("/some/path")));
    }

    #[test]
    fn workspace_scope_metadata_vs_payload_shapes() {
        let dir = tempfile::tempdir().unwrap();
        let scope = build_workspace_scope(dir.path(), WorkspaceAccessMode::Restricted, None);

        let metadata = scope.metadata();
        let metadata_obj = metadata.as_object().unwrap();
        assert_eq!(metadata_obj.len(), 2);
        assert!(metadata_obj.contains_key("project_path"));
        assert!(metadata_obj.contains_key("access_mode"));

        let payload = scope.payload();
        let payload_obj = payload.as_object().unwrap();
        assert!(payload_obj.contains_key("project_name"));
        assert!(payload_obj.contains_key("restrict_to_workspace"));
        assert!(payload_obj.contains_key("sandbox_status"));
    }

    #[test]
    fn workspace_scope_resolver_default_and_for_session() {
        let dir = tempfile::tempdir().unwrap();
        let resolver = WorkspaceScopeResolver::new(dir.path().to_path_buf(), true);
        assert_eq!(resolver.default().project_path, dir.path());
        assert_eq!(resolver.for_session(None).project_path, dir.path());
    }
}
