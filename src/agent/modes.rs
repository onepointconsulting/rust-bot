//! Per-session agent composition modes (Standard vs pragmatic Minimal).
//!
//! Mode is a view over the process-wide tool registry and system prompt, not a
//! second catalog. Resolution: session metadata override if present and valid,
//! else the process-wide default, else Standard.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

pub use crate::session::keys::SESSION_AGENT_MODE_METADATA_KEY;

const MINIMAL_TOOLS: &[&str] = &["edit_file", "shell"];
const MINIMAL_BOOTSTRAP_FILES: &[&str] = &["SOUL.md", "USER.md"];
const STANDARD_BOOTSTRAP_FILES: &[&str] = &["AGENTS.md", "SOUL.md", "USER.md", "TOOLS.md"];
const MINIMAL_FALLBACK_PROMPT: &str = "You are a helpful software engineer assistant.";

/// Reserved `/mode` / `set_mode` argument: clear the session override.
pub const RESERVED_AGENT_MODE_NAME: &str = "default";

/// How this session presents tools and assembles the system prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentMode {
    #[default]
    Standard,
    Minimal,
}

impl std::fmt::Display for AgentMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl AgentMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Minimal => "minimal",
        }
    }

    /// Parse a mode name. `"default"` is not a mode — callers treat it as
    /// "clear the session override."
    pub fn parse(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "standard" => Some(Self::Standard),
            "minimal" => Some(Self::Minimal),
            _ => None,
        }
    }

    /// Session override if present and valid, otherwise `default`.
    pub fn resolve(
        default: Self,
        session_metadata: Option<&HashMap<String, serde_json::Value>>,
    ) -> Self {
        let Some(metadata) = session_metadata else {
            return default;
        };
        let Some(raw) = metadata.get(SESSION_AGENT_MODE_METADATA_KEY) else {
            return default;
        };
        let Some(name) = raw.as_str() else {
            return default;
        };
        Self::parse(name).unwrap_or(default)
    }

    /// `None` means every registered tool is visible (Standard).
    pub fn allowed_tool_names(self) -> Option<&'static [&'static str]> {
        match self {
            Self::Standard => None,
            Self::Minimal => Some(MINIMAL_TOOLS),
        }
    }

    pub fn bootstrap_files(self) -> &'static [&'static str] {
        match self {
            Self::Standard => STANDARD_BOOTSTRAP_FILES,
            Self::Minimal => MINIMAL_BOOTSTRAP_FILES,
        }
    }

    pub fn include_identity(self) -> bool {
        matches!(self, Self::Standard)
    }

    pub fn include_memory(self) -> bool {
        matches!(self, Self::Standard)
    }

    pub fn include_skills(self) -> bool {
        matches!(self, Self::Standard)
    }

    pub fn include_recent_history(self) -> bool {
        matches!(self, Self::Standard)
    }

    pub fn include_goal_runtime(self) -> bool {
        matches!(self, Self::Standard)
    }

    pub fn fallback_system_prompt(self) -> Option<&'static str> {
        match self {
            Self::Standard => None,
            Self::Minimal => Some(MINIMAL_FALLBACK_PROMPT),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_standard_and_minimal() {
        assert_eq!(AgentMode::parse("standard"), Some(AgentMode::Standard));
        assert_eq!(AgentMode::parse("MINIMAL"), Some(AgentMode::Minimal));
        assert_eq!(AgentMode::parse("  Minimal  "), Some(AgentMode::Minimal));
    }

    #[test]
    fn parse_rejects_default_and_unknown() {
        assert_eq!(AgentMode::parse("default"), None);
        assert_eq!(AgentMode::parse("ptc"), None);
        assert_eq!(AgentMode::parse(""), None);
    }

    #[test]
    fn resolve_uses_default_when_metadata_missing_or_invalid() {
        assert_eq!(AgentMode::resolve(AgentMode::Minimal, None), AgentMode::Minimal);

        let empty = HashMap::new();
        assert_eq!(
            AgentMode::resolve(AgentMode::Standard, Some(&empty)),
            AgentMode::Standard
        );

        let mut bad = HashMap::new();
        bad.insert(
            SESSION_AGENT_MODE_METADATA_KEY.to_string(),
            serde_json::json!(1),
        );
        assert_eq!(
            AgentMode::resolve(AgentMode::Standard, Some(&bad)),
            AgentMode::Standard
        );

        let mut unknown = HashMap::new();
        unknown.insert(
            SESSION_AGENT_MODE_METADATA_KEY.to_string(),
            serde_json::json!("ptc"),
        );
        assert_eq!(
            AgentMode::resolve(AgentMode::Minimal, Some(&unknown)),
            AgentMode::Minimal
        );
    }

    #[test]
    fn resolve_honors_valid_session_override() {
        let mut meta = HashMap::new();
        meta.insert(
            SESSION_AGENT_MODE_METADATA_KEY.to_string(),
            serde_json::json!("minimal"),
        );
        assert_eq!(
            AgentMode::resolve(AgentMode::Standard, Some(&meta)),
            AgentMode::Minimal
        );
    }

    #[test]
    fn minimal_allow_list_is_shell_and_edit_file() {
        assert_eq!(AgentMode::Standard.allowed_tool_names(), None);
        let names = AgentMode::Minimal.allowed_tool_names().unwrap();
        assert_eq!(names, ["edit_file", "shell"]);
    }

    #[test]
    fn prompt_flags_and_bootstrap_differ_by_mode() {
        let standard = AgentMode::Standard;
        assert!(standard.include_identity());
        assert!(standard.include_memory());
        assert!(standard.include_skills());
        assert!(standard.include_recent_history());
        assert!(standard.include_goal_runtime());
        assert_eq!(
            standard.bootstrap_files(),
            ["AGENTS.md", "SOUL.md", "USER.md", "TOOLS.md"]
        );
        assert!(standard.fallback_system_prompt().is_none());

        let minimal = AgentMode::Minimal;
        assert!(!minimal.include_identity());
        assert!(!minimal.include_memory());
        assert!(!minimal.include_skills());
        assert!(!minimal.include_recent_history());
        assert!(!minimal.include_goal_runtime());
        assert_eq!(minimal.bootstrap_files(), ["SOUL.md", "USER.md"]);
        assert_eq!(
            minimal.fallback_system_prompt(),
            Some("You are a helpful software engineer assistant.")
        );
    }

    #[test]
    fn serde_round_trip_lowercase() {
        assert_eq!(
            serde_json::to_string(&AgentMode::Minimal).unwrap(),
            "\"minimal\""
        );
        assert_eq!(
            serde_json::from_str::<AgentMode>("\"standard\"").unwrap(),
            AgentMode::Standard
        );
    }
}
