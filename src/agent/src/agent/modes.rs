//! Per-session agent composition modes (Standard, pragmatic Minimal, and
//! NoMcp, which is Standard without the MCP-provided tools).
//!
//! Mode is a view over the process-wide tool registry and system prompt, not a
//! second catalog. Resolution: session metadata override if present and valid,
//! else the process-wide default, else Standard.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::agent::tools::registry::ToolRegistry;

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
    /// Standard prompt and tools, minus every `mcp_*` tool.
    #[serde(rename = "no_mcp")]
    NoMcp,
}

impl std::fmt::Display for AgentMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl AgentMode {
    /// Every mode, in the order clients should list them.
    pub const ALL: [Self; 3] = [Self::Standard, Self::Minimal, Self::NoMcp];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Minimal => "minimal",
            Self::NoMcp => "no_mcp",
        }
    }

    /// Comma-separated mode names for user-facing "available modes" errors.
    pub fn available_names() -> String {
        Self::ALL.map(Self::as_str).join(", ")
    }

    /// Parse a mode name. `"default"` is not a mode — callers treat it as
    /// "clear the session override." Spaces and hyphens are accepted in place
    /// of underscores, so `no_mcp`, `no-mcp` and `no mcp` are the same mode.
    pub fn parse(name: &str) -> Option<Self> {
        let normalized = name.trim().to_ascii_lowercase().replace([' ', '-'], "_");
        match normalized.as_str() {
            "standard" => Some(Self::Standard),
            "minimal" => Some(Self::Minimal),
            "no_mcp" => Some(Self::NoMcp),
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

    /// Whether the tool called `name` is visible in this mode. A predicate
    /// rather than a static allow-list because NoMcp is "everything except
    /// `mcp_*`", which a fixed list of names cannot express (MCP tools are
    /// registered at runtime).
    pub fn allows_tool(self, name: &str) -> bool {
        match self {
            Self::Standard => true,
            Self::Minimal => MINIMAL_TOOLS.contains(&name),
            Self::NoMcp => !ToolRegistry::is_mcp_tool_name(name),
        }
    }

    /// The tools of `registry` that this mode exposes.
    pub fn restrict_tools(self, registry: &ToolRegistry) -> ToolRegistry {
        registry.restrict_by(|name| self.allows_tool(name))
    }

    /// Standard prompt composition: everything except Minimal's pared-down one.
    fn uses_standard_prompt(self) -> bool {
        matches!(self, Self::Standard | Self::NoMcp)
    }

    pub fn bootstrap_files(self) -> &'static [&'static str] {
        if self.uses_standard_prompt() {
            STANDARD_BOOTSTRAP_FILES
        } else {
            MINIMAL_BOOTSTRAP_FILES
        }
    }

    pub fn include_identity(self) -> bool {
        self.uses_standard_prompt()
    }

    pub fn include_memory(self) -> bool {
        self.uses_standard_prompt()
    }

    pub fn include_skills(self) -> bool {
        self.uses_standard_prompt()
    }

    pub fn include_recent_history(self) -> bool {
        self.uses_standard_prompt()
    }

    pub fn include_goal_runtime(self) -> bool {
        self.uses_standard_prompt()
    }

    pub fn fallback_system_prompt(self) -> Option<&'static str> {
        if self.uses_standard_prompt() {
            None
        } else {
            Some(MINIMAL_FALLBACK_PROMPT)
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
        assert_eq!(
            AgentMode::resolve(AgentMode::Minimal, None),
            AgentMode::Minimal
        );

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
    fn parse_accepts_no_mcp_spellings() {
        for spelling in ["no_mcp", "NO_MCP", "no mcp", "  No-Mcp  "] {
            assert_eq!(
                AgentMode::parse(spelling),
                Some(AgentMode::NoMcp),
                "{spelling}"
            );
        }
        assert_eq!(AgentMode::parse("nomcp"), None);
    }

    #[test]
    fn as_str_round_trips_through_parse_for_every_mode() {
        for mode in AgentMode::ALL {
            assert_eq!(AgentMode::parse(mode.as_str()), Some(mode));
        }
    }

    #[test]
    fn available_names_lists_every_mode() {
        assert_eq!(AgentMode::available_names(), "standard, minimal, no_mcp");
    }

    #[test]
    fn resolve_honors_a_no_mcp_session_override() {
        let mut meta = HashMap::new();
        meta.insert(
            SESSION_AGENT_MODE_METADATA_KEY.to_string(),
            serde_json::json!("no_mcp"),
        );
        assert_eq!(
            AgentMode::resolve(AgentMode::Standard, Some(&meta)),
            AgentMode::NoMcp
        );
    }

    #[test]
    fn standard_allows_every_tool() {
        assert!(AgentMode::Standard.allows_tool("shell"));
        assert!(AgentMode::Standard.allows_tool("mcp_search"));
    }

    #[test]
    fn minimal_allows_only_shell_and_edit_file() {
        assert!(AgentMode::Minimal.allows_tool("edit_file"));
        assert!(AgentMode::Minimal.allows_tool("shell"));
        assert!(!AgentMode::Minimal.allows_tool("web_search"));
        assert!(!AgentMode::Minimal.allows_tool("mcp_search"));
    }

    #[test]
    fn no_mcp_hides_only_mcp_prefixed_tools() {
        assert!(AgentMode::NoMcp.allows_tool("shell"));
        assert!(AgentMode::NoMcp.allows_tool("web_search"));
        assert!(!AgentMode::NoMcp.allows_tool("mcp_search"));
        assert!(!AgentMode::NoMcp.allows_tool("mcp_github_create_issue"));
    }

    #[test]
    fn no_mcp_uses_the_standard_prompt_composition() {
        let mode = AgentMode::NoMcp;
        assert!(mode.include_identity());
        assert!(mode.include_memory());
        assert!(mode.include_skills());
        assert!(mode.include_recent_history());
        assert!(mode.include_goal_runtime());
        assert_eq!(
            mode.bootstrap_files(),
            AgentMode::Standard.bootstrap_files()
        );
        assert!(mode.fallback_system_prompt().is_none());
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
        assert_eq!(
            serde_json::to_string(&AgentMode::NoMcp).unwrap(),
            "\"no_mcp\""
        );
        assert_eq!(
            serde_json::from_str::<AgentMode>("\"no_mcp\"").unwrap(),
            AgentMode::NoMcp
        );
    }
}
