//! Compile-time snapshot of `templates/`, used as a fallback when no on-disk
//! `templates/` directory can be found (see
//! [`crate::utils::prompt_templates::resolve_templates_root`]).
//!
//! This lets a standalone binary always seed a fresh workspace and render
//! agent prompts, even without a sibling `templates/` folder.
use std::borrow::Cow;

use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "templates/"]
pub struct BundledTemplates;

/// Read an embedded template as UTF-8 text by its path relative to `templates/`
/// (e.g. `"AGENTS.md"`, `"agent/identity.md"`, `"memory/MEMORY.md"`).
///
/// Paths always use forward slashes, regardless of platform.
pub fn get(path: &str) -> Option<String> {
    let file = BundledTemplates::get(path)?;
    String::from_utf8(file.data.into_owned()).ok()
}

/// Relative paths (forward-slash separated) of every embedded template.
pub fn paths() -> impl Iterator<Item = Cow<'static, str>> {
    BundledTemplates::iter()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_bootstrap_file() {
        let content = get("AGENTS.md");
        assert!(content.is_some(), "AGENTS.md should be embedded");
        assert!(!content.unwrap().is_empty());
    }

    #[test]
    fn test_get_agent_prompt() {
        let content = get("agent/identity.md");
        assert!(content.is_some(), "agent/identity.md should be embedded");
    }

    #[test]
    fn test_get_missing_returns_none() {
        assert!(get("does/not/exist.md").is_none());
    }

    #[test]
    fn test_paths_contains_expected_files() {
        let paths: Vec<String> = paths().map(|p| p.into_owned()).collect();
        assert!(paths.iter().any(|p| p == "AGENTS.md"));
        assert!(paths.iter().any(|p| p == "agent/identity.md"));
        assert!(paths.iter().any(|p| p == "memory/MEMORY.md"));
    }
}
