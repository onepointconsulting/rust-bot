//! The environment a child rust-bot is started with.
//!
//! A child must not inherit the supervisor's whole environment (it holds every
//! secret the parent can see), yet it must start: the loader expands `${VAR}`
//! placeholders anywhere in the config, and an unset variable is an error. So
//! the environment is *derived from the config*: exactly the variables the
//! merged config references, a small base set every program needs, and the key
//! variable of each provider the child actually uses.
//!
//! This module only builds the map; the spawn that uses it comes with the
//! parent side of the ACP support.

use std::collections::{BTreeSet, HashMap};

use crate::config::loader::referenced_env_vars;
use crate::config::schema::Config;
use crate::providers::registry::find_by_name;

/// Set on every child so it (and its own children) know how deep the chain is.
pub const DEPTH_ENV_VAR: &str = "RUST_BOT_ACP_DEPTH";

/// How deep in a chain of children this process is: `0` for a process the
/// operator started, the parent's depth plus one for a child (see
/// [`DEPTH_ENV_VAR`], set by the parent when it launches a child).
pub fn current_depth() -> u32 {
    depth_from(std::env::var(DEPTH_ENV_VAR).ok().as_deref())
}

/// The depth a [`DEPTH_ENV_VAR`] value stands for; anything unusable is `0`.
fn depth_from(value: Option<&str>) -> u32 {
    value
        .and_then(|text| text.trim().parse().ok())
        .unwrap_or(0)
}

/// rust-bot's own settings plus `PATH`, which every child needs.
const RUST_BOT_VARS: [&str; 3] = ["PATH", "RUST_LOG", "RUST_LOG_FILE"];

/// Variables the operating system needs to run programs, find temp folders and
/// certificates. Without them `npx`, the shell tool and TLS fail in ways that
/// are hard to diagnose.
#[cfg(windows)]
const OS_VARS: [&str; 7] = [
    "SystemRoot",
    "COMSPEC",
    "PATHEXT",
    "TEMP",
    "TMP",
    "USERPROFILE",
    "APPDATA",
];
#[cfg(not(windows))]
const OS_VARS: [&str; 3] = ["HOME", "TMPDIR", "LANG"];

/// Referenced variables that are not set in the parent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingEnvVars(pub Vec<String>);

impl std::fmt::Display for MissingEnvVars {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the config references environment variables that are not set: {}",
            self.0.join(", ")
        )
    }
}

impl std::error::Error for MissingEnvVars {}

/// `name` in `env`, returning the key as the parent spelled it. Windows
/// variable names are case-insensitive (`Path` is `PATH`).
fn lookup<'a>(env: &'a HashMap<String, String>, name: &str) -> Option<(&'a String, &'a String)> {
    if cfg!(windows) {
        env.iter().find(|(key, _)| key.eq_ignore_ascii_case(name))
    } else {
        env.get_key_value(name)
    }
}

/// Providers the child will call: its own, plus those of its model presets.
/// `auto` selects a provider at runtime from configured keys and names none.
fn used_provider_names(config: &Config) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let own = config.agents.provider.trim();
    if !own.is_empty() && own != "auto" {
        names.insert(own.to_string());
    }
    for preset in config.model_presets.values() {
        if !preset.provider.trim().is_empty() {
            names.insert(preset.provider.clone());
        }
    }
    names
}

/// Build the child's environment.
///
/// `merged` is the child's effective config **before** placeholders are
/// expanded, `depth` the child's depth in the chain, and `parent_env` the
/// supervisor's environment (injected so the result is testable).
///
/// Fails with [`MissingEnvVars`] when the config references a variable the
/// parent does not have, so the parent can report it by name instead of
/// spawning a child that dies at startup.
pub fn child_environment(
    merged: &Config,
    depth: u32,
    parent_env: &HashMap<String, String>,
) -> Result<HashMap<String, String>, MissingEnvVars> {
    let raw = serde_json::to_value(merged).unwrap_or_default();
    let referenced = referenced_env_vars(&raw);

    let missing: Vec<String> = referenced
        .iter()
        .filter(|name| lookup(parent_env, name).is_none())
        .cloned()
        .collect();
    if !missing.is_empty() {
        return Err(MissingEnvVars(missing));
    }

    let mut wanted: BTreeSet<String> = referenced;
    wanted.extend(RUST_BOT_VARS.iter().map(|name| name.to_string()));
    wanted.extend(OS_VARS.iter().map(|name| name.to_string()));
    for provider in used_provider_names(merged) {
        if let Some(spec) = find_by_name(&provider)
            && !spec.env_key.is_empty()
        {
            wanted.insert(spec.env_key);
        }
    }

    let mut child = HashMap::new();
    for name in &wanted {
        // Base and provider variables are optional: pass them when the parent has them.
        if let Some((key, value)) = lookup(parent_env, name) {
            child.insert(key.clone(), value.clone());
        }
    }
    child.insert(DEPTH_ENV_VAR.to_string(), depth.to_string());
    Ok(child)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn depth_comes_from_the_variable_and_defaults_to_the_root() {
        assert_eq!(depth_from(None), 0);
        assert_eq!(depth_from(Some("2")), 2);
        assert_eq!(depth_from(Some(" 1 ")), 1);
        for unusable in ["", "x", "-1", "1.5"] {
            assert_eq!(depth_from(Some(unusable)), 0, "{unusable:?}");
        }
    }
    use serde_json::json;

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    fn config(value: serde_json::Value) -> Config {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn a_placeholder_anywhere_in_the_config_reaches_the_child() {
        let merged = config(json!({"tools": {"mcpServers": {"api": {
            "url": "https://mcp.example",
            "headers": {"Authorization": "Bearer ${MCP_HEADERS_JWT}"}
        }}}}));
        let child =
            child_environment(&merged, 1, &env(&[("MCP_HEADERS_JWT", "token-123")])).unwrap();
        assert_eq!(
            child.get("MCP_HEADERS_JWT").map(String::as_str),
            Some("token-123")
        );
    }

    #[test]
    fn variables_the_config_does_not_reference_stay_out() {
        let merged = config(json!({}));
        let parent = env(&[("AWS_SECRET_ACCESS_KEY", "s3cr3t"), ("PATH", "/bin")]);
        let child = child_environment(&merged, 1, &parent).unwrap();
        assert!(!child.contains_key("AWS_SECRET_ACCESS_KEY"));
        assert_eq!(child.get("PATH").map(String::as_str), Some("/bin"));
    }

    #[test]
    fn the_base_set_is_passed_when_the_parent_has_it() {
        let mut pairs: Vec<(&str, &str)> = vec![
            ("PATH", "p"),
            ("RUST_LOG", "debug"),
            ("RUST_LOG_FILE", "f.log"),
        ];
        pairs.extend(OS_VARS.iter().map(|name| (*name, "value")));
        let child = child_environment(&config(json!({})), 1, &env(&pairs)).unwrap();
        for name in RUST_BOT_VARS.iter().chain(OS_VARS.iter()) {
            assert!(child.contains_key(*name), "{name} should be passed");
        }
    }

    #[test]
    fn an_unset_referenced_variable_is_an_error_naming_all_of_them() {
        let merged = config(json!({"tools": {"mcpServers": {"x": {
            "command": "run",
            "env": {"A": "${FIRST_MISSING}", "B": "${SECOND_MISSING}", "C": "${PRESENT}"}
        }}}}));
        let error = child_environment(&merged, 1, &env(&[("PRESENT", "1")])).unwrap_err();
        assert_eq!(
            error,
            MissingEnvVars(vec!["FIRST_MISSING".into(), "SECOND_MISSING".into()])
        );
        let message = error.to_string();
        assert!(
            message.contains("FIRST_MISSING") && message.contains("SECOND_MISSING"),
            "{message}"
        );
    }

    #[test]
    fn only_the_key_variables_of_used_providers_are_passed() {
        let merged = config(json!({
            "agents": {"provider": "openai"},
            "modelPresets": {"claude": {"model": "claude-x", "provider": "anthropic"}}
        }));
        let parent = env(&[
            ("OPENAI_API_KEY", "sk-openai"),
            ("ANTHROPIC_API_KEY", "sk-anthropic"),
            ("OPENROUTER_API_KEY", "sk-unused"),
        ]);
        let child = child_environment(&merged, 1, &parent).unwrap();
        assert!(child.contains_key("OPENAI_API_KEY"));
        assert!(
            child.contains_key("ANTHROPIC_API_KEY"),
            "a preset's provider counts as used"
        );
        assert!(!child.contains_key("OPENROUTER_API_KEY"));
    }

    #[test]
    fn an_absent_provider_key_variable_is_not_an_error() {
        let merged = config(json!({"agents": {"provider": "openai"}}));
        let child = child_environment(&merged, 1, &env(&[])).unwrap();
        assert!(!child.contains_key("OPENAI_API_KEY"));
    }

    #[test]
    fn the_depth_is_always_set() {
        let child = child_environment(&config(json!({})), 3, &env(&[])).unwrap();
        assert_eq!(child.get(DEPTH_ENV_VAR).map(String::as_str), Some("3"));
    }

    #[test]
    fn placeholders_in_keys_are_not_scanned_and_one_value_can_hold_several() {
        let merged = config(json!({"tools": {"mcpServers": {"x": {
            "command": "run",
            "env": {"${NOT_SCANNED}": "${ONE}:${TWO}"}
        }}}}));
        let child = child_environment(
            &merged,
            1,
            &env(&[("ONE", "1"), ("TWO", "2"), ("NOT_SCANNED", "x")]),
        )
        .unwrap();
        assert!(child.contains_key("ONE") && child.contains_key("TWO"));
        assert!(!child.contains_key("NOT_SCANNED"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_variable_names_match_case_insensitively() {
        let child = child_environment(&config(json!({})), 1, &env(&[("Path", "C:\\bin")])).unwrap();
        assert_eq!(child.get("Path").map(String::as_str), Some("C:\\bin"));
    }
}
