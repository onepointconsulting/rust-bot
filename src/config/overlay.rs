//! Config overlay for child agents.
//!
//! A child rust-bot runs on its parent's config plus an **overlay**: a small
//! JSON merge patch (RFC 7386) stored next to the child. The overlay is written
//! by an LLM and sits in a plain file, so it is untrusted input. The rule it is
//! held to: **a child never has more power than its parent.**
//!
//! That is enforced with an *allowlist*, in code. Only the paths listed in
//! [`rule_for`] may appear in an overlay; everything else, including every key a
//! future version adds to the config, is rejected. Keys that could grant power
//! (providers and their URLs or keys, MCP server commands, launch presets,
//! channels) are simply not on the list.
//!
//! Two levels of checking, one rule table:
//!
//! * [`validate_overlay`], when an overlay is written: the allowlist *and* "does
//!   this overlay loosen anything compared with the parent right now?", so the
//!   author gets an error instead of a setting that silently does nothing.
//! * [`load_config_with_overlay`], on every child start: the allowlist again (a
//!   hand-edited file is caught), then **strictest wins**. For settings that can
//!   only be narrowed, the child gets the stricter of parent and overlay. The
//!   config is inherited live, so a parent that tightens later can only make its
//!   children stricter, never break or widen them.
//!
//! Overlay keys are the canonical camelCase names; aliases such as snake_case
//! are not accepted, so there is exactly one spelling to check.

use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::Value;

use crate::config::loader::{load_config, resolve_config_env_vars};
use crate::config::schema::{Config, validate_model_presets};

/// Wildcard entry of `enabledTools`: "every tool of the server".
const ALL_TOOLS: &str = "*";

/// Set once this process runs on an overlay (i.e. it is a child).
static OVERLAY_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Record that this process runs on an overlay.
pub fn mark_overlay_active() {
    OVERLAY_ACTIVE.store(true, Ordering::Relaxed);
}

/// Whether this process runs on an overlay. Commands that rewrite the parent's
/// config file must refuse to run in such a process.
pub fn overlay_is_active() -> bool {
    OVERLAY_ACTIVE.load(Ordering::Relaxed)
}

/// Why an overlay was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverlayError {
    /// The overlay file could not be read or is not a JSON object.
    Unreadable(String),
    /// A path or value is not allowed.
    NotAllowed { path: String, reason: String },
    /// The overlay would loosen a setting compared with the parent.
    Widens { path: String, reason: String },
    /// The merged result is not a valid config.
    Invalid(String),
}

impl std::fmt::Display for OverlayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OverlayError::Unreadable(reason) => write!(f, "overlay cannot be read: {reason}"),
            OverlayError::NotAllowed { path, reason } => {
                write!(f, "overlay setting '{path}' is not allowed: {reason}")
            }
            OverlayError::Widens { path, reason } => {
                write!(
                    f,
                    "overlay setting '{path}' would give the child more than its parent: {reason}"
                )
            }
            OverlayError::Invalid(reason) => {
                write!(f, "overlay produces an invalid config: {reason}")
            }
        }
    }
}

impl std::error::Error for OverlayError {}

fn not_allowed(path: &str, reason: &str) -> OverlayError {
    OverlayError::NotAllowed {
        path: path.to_string(),
        reason: reason.to_string(),
    }
}

fn widens(path: &str, reason: String) -> OverlayError {
    OverlayError::Widens {
        path: path.to_string(),
        reason,
    }
}

// ── the allowlist ───────────────────────────────────────────────────────────

/// What kind of value an allowed path takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rule {
    /// Any non-empty string (e.g. the model name). Cannot grant power.
    FreeString,
    /// The name of a model preset the parent defines.
    PresetName,
    /// `"auto"`, or a provider the parent has configured. The child then uses
    /// the parent's key and URL for it, never new ones.
    ProviderName,
    /// A boolean that may only be narrowed (see [`clamp_to_parent`]).
    NarrowBool,
    /// A list of strings that may only be narrowed.
    StringList,
    /// An inherited MCP server: only `null` (drop it) or an object that narrows
    /// its `enabledTools`.
    DropServer,
}

/// The rule for an exact overlay path, if that path is allowed.
///
/// This table **is** the allowlist. Adding a path here is a security decision.
fn rule_for(path: &[&str]) -> Option<Rule> {
    match path {
        ["agents", "model"] => Some(Rule::FreeString),
        ["agents", "modelPreset"] => Some(Rule::PresetName),
        ["agents", "provider"] => Some(Rule::ProviderName),
        ["tools", "exec", "enable"] | ["tools", "restrictToWorkspace"] => Some(Rule::NarrowBool),
        ["tools", "disabledTools"] => Some(Rule::StringList),
        ["tools", "mcpServers", _] => Some(Rule::DropServer),
        ["tools", "mcpServers", _, "enabledTools"] => Some(Rule::StringList),
        _ => None,
    }
}

/// Paths that are only containers of allowed paths.
fn is_allowed_container(path: &[&str]) -> bool {
    matches!(
        path,
        ["agents"] | ["tools"] | ["tools", "exec"] | ["tools", "mcpServers"]
    )
}

/// Whether the parent has a usable configuration for provider `name`.
fn provider_is_configured(parent: &Config, name: &str) -> bool {
    let normalized = name.replace('-', "_").to_lowercase();
    parent
        .providers
        .get_by_name(&normalized)
        .is_some_and(|provider| {
            !provider.api_key.is_empty()
                || provider
                    .api_base
                    .as_deref()
                    .is_some_and(|base| !base.is_empty())
        })
}

fn check_leaf(rule: Rule, value: &Value, path: &str, parent: &Config) -> Result<(), OverlayError> {
    match rule {
        Rule::FreeString => value
            .as_str()
            .filter(|text| !text.trim().is_empty())
            .map(|_| ())
            .ok_or_else(|| not_allowed(path, "must be a non-empty string")),
        Rule::PresetName => {
            let name = value
                .as_str()
                .ok_or_else(|| not_allowed(path, "must be a string"))?;
            if parent.model_presets.contains_key(name) {
                Ok(())
            } else {
                Err(not_allowed(
                    path,
                    "must name a model preset that the parent defines",
                ))
            }
        }
        Rule::ProviderName => {
            let name = value
                .as_str()
                .ok_or_else(|| not_allowed(path, "must be a string"))?;
            if name == "auto" || provider_is_configured(parent, name) {
                Ok(())
            } else {
                Err(not_allowed(
                    path,
                    "must be \"auto\" or a provider the parent has configured (an apiKey or apiBase)",
                ))
            }
        }
        // `null` means "delete the key" in a merge patch; the effective value is
        // checked (and clamped) after the merge.
        Rule::NarrowBool => match value {
            Value::Bool(_) | Value::Null => Ok(()),
            _ => Err(not_allowed(path, "must be true or false")),
        },
        Rule::StringList => match value {
            Value::Null => Ok(()),
            Value::Array(items) if items.iter().all(Value::is_string) => Ok(()),
            _ => Err(not_allowed(path, "must be a list of strings")),
        },
        Rule::DropServer => unreachable!("DropServer is handled by the caller"),
    }
}

fn walk_object(
    path: &mut Vec<String>,
    map: &serde_json::Map<String, Value>,
    parent: &Config,
) -> Result<(), OverlayError> {
    for (key, value) in map {
        path.push(key.clone());
        let result = walk(path, value, parent);
        path.pop();
        result?;
    }
    Ok(())
}

fn walk(path: &mut Vec<String>, value: &Value, parent: &Config) -> Result<(), OverlayError> {
    let segments: Vec<&str> = path.iter().map(String::as_str).collect();
    let dotted = segments.join(".");
    match (rule_for(&segments), value) {
        (Some(Rule::DropServer), Value::Null) => Ok(()),
        (Some(Rule::DropServer), Value::Object(map)) => {
            let map = map.clone();
            walk_object(path, &map, parent)
        }
        (Some(Rule::DropServer), _) => Err(not_allowed(
            &dotted,
            "an MCP server can only be dropped (null) or narrowed through enabledTools",
        )),
        (Some(rule), leaf) => check_leaf(rule, leaf, &dotted, parent),
        (None, Value::Object(map)) if is_allowed_container(&segments) => {
            let map = map.clone();
            walk_object(path, &map, parent)
        }
        // An unknown branch: descend anyway, so the error names the exact
        // setting (`providers.openai.apiBase`) instead of just its first word.
        // Nothing below an unknown branch can be allowed, so this always ends in
        // an error; an empty unknown object is refused on the spot.
        (None, Value::Object(map)) if !map.is_empty() => {
            let map = map.clone();
            walk_object(path, &map, parent)
        }
        (None, _) => Err(not_allowed(
            &dotted,
            "this setting cannot be changed by an overlay",
        )),
    }
}

/// Check every path and value of `overlay` against the allowlist.
pub fn check_allowed_paths(parent: &Config, overlay: &Value) -> Result<(), OverlayError> {
    let Some(root) = overlay.as_object() else {
        return Err(OverlayError::Unreadable(
            "an overlay must be a JSON object".to_string(),
        ));
    };
    let mut path = Vec::new();
    walk_object(&mut path, root, parent)
}

// ── merging ─────────────────────────────────────────────────────────────────

/// RFC 7386 JSON merge patch: objects merge, `null` deletes a key, anything
/// else (arrays included) replaces.
pub fn json_merge_patch(target: &mut Value, patch: &Value) {
    let Value::Object(patch_map) = patch else {
        *target = patch.clone();
        return;
    };
    if !target.is_object() {
        *target = Value::Object(serde_json::Map::new());
    }
    let Some(target_map) = target.as_object_mut() else {
        return;
    };
    for (key, value) in patch_map {
        if value.is_null() {
            target_map.remove(key);
        } else {
            json_merge_patch(target_map.entry(key.clone()).or_insert(Value::Null), value);
        }
    }
}

/// The parent with `overlay` merged in, exactly as written (not yet clamped).
fn merge_unclamped(parent: &Config, overlay: &Value) -> Result<Config, OverlayError> {
    let mut merged =
        serde_json::to_value(parent).map_err(|e| OverlayError::Invalid(e.to_string()))?;
    json_merge_patch(&mut merged, overlay);
    let config: Config = serde_json::from_value(merged)
        .map_err(|e| OverlayError::Invalid(format!("the merged config does not parse: {e}")))?;
    validate_model_presets(&config).map_err(OverlayError::Invalid)?;
    Ok(config)
}

// ── narrowing rules ─────────────────────────────────────────────────────────

/// Whether `child`'s `enabledTools` is no wider than `parent`'s (`*` = all).
fn enabled_tools_within(child: &[String], parent: &[String]) -> bool {
    if parent.iter().any(|tool| tool == ALL_TOOLS) {
        return true;
    }
    if child.iter().any(|tool| tool == ALL_TOOLS) {
        return false;
    }
    child.iter().all(|tool| parent.contains(tool))
}

/// The narrower of two `enabledTools` lists (`*` = all).
fn narrower_enabled_tools(parent: &[String], child: &[String]) -> Vec<String> {
    let parent_all = parent.iter().any(|tool| tool == ALL_TOOLS);
    let child_all = child.iter().any(|tool| tool == ALL_TOOLS);
    match (parent_all, child_all) {
        (true, true) => vec![ALL_TOOLS.to_string()],
        (true, false) => child.to_vec(),
        (false, true) => parent.to_vec(),
        (false, false) => child
            .iter()
            .filter(|tool| parent.contains(tool))
            .cloned()
            .collect(),
    }
}

/// Entries of `parent` that `merged` no longer has.
fn missing_entries(parent: &[String], merged: &[String]) -> Vec<String> {
    parent
        .iter()
        .filter(|entry| !merged.contains(entry))
        .cloned()
        .collect()
}

/// Apply "strictest wins" to `merged`: for every narrow-only setting the child
/// gets the stricter of the parent's value and its own.
pub fn clamp_to_parent(parent: &Config, merged: &mut Config) {
    merged.tools.exec.enable = parent.tools.exec.enable && merged.tools.exec.enable;
    merged.tools.restrict_to_workspace =
        parent.tools.restrict_to_workspace || merged.tools.restrict_to_workspace;

    let mut disabled = parent.tools.disabled_tools.clone();
    for tool in &merged.tools.disabled_tools {
        if !disabled.contains(tool) {
            disabled.push(tool.clone());
        }
    }
    merged.tools.disabled_tools = disabled;

    // An overlay can drop an inherited server but never add or change one.
    merged
        .tools
        .mcp_servers
        .retain(|name, _| parent.tools.mcp_servers.contains_key(name));
    for (name, server) in merged.tools.mcp_servers.iter_mut() {
        if let Some(inherited) = parent.tools.mcp_servers.get(name) {
            server.enabled_tools =
                narrower_enabled_tools(&inherited.enabled_tools, &server.enabled_tools);
        }
    }
}

/// Reject a merged config that loosens a narrow-only setting of the parent.
fn check_not_wider(parent: &Config, merged: &Config) -> Result<(), OverlayError> {
    if merged.tools.exec.enable && !parent.tools.exec.enable {
        return Err(widens(
            "tools.exec.enable",
            "the parent has the shell tool disabled".to_string(),
        ));
    }
    if !merged.tools.restrict_to_workspace && parent.tools.restrict_to_workspace {
        return Err(widens(
            "tools.restrictToWorkspace",
            "the parent restricts tools to the workspace (null resets it to the default)"
                .to_string(),
        ));
    }
    let missing = missing_entries(&parent.tools.disabled_tools, &merged.tools.disabled_tools);
    if !missing.is_empty() {
        return Err(widens(
            "tools.disabledTools",
            format!(
                "it must keep the parent's disabled tools, missing: {}",
                missing.join(", ")
            ),
        ));
    }
    for (name, server) in &merged.tools.mcp_servers {
        if let Some(inherited) = parent.tools.mcp_servers.get(name)
            && !enabled_tools_within(&server.enabled_tools, &inherited.enabled_tools)
        {
            return Err(widens(
                &format!("tools.mcpServers.{name}.enabledTools"),
                "it lists tools the parent does not enable".to_string(),
            ));
        }
    }
    Ok(())
}

// ── entry points ────────────────────────────────────────────────────────────

/// Check an overlay when it is written (`acp_create_agent` / `acp_update_agent`):
/// the allowlist, and that nothing is looser than the parent's **current** config.
pub fn validate_overlay(parent: &Config, overlay: &Value) -> Result<(), OverlayError> {
    check_allowed_paths(parent, overlay)?;
    let merged = merge_unclamped(parent, overlay)?;
    check_not_wider(parent, &merged)
}

/// The child's effective config: the allowlist, the merge, then strictest wins.
///
/// Placeholders (`${VAR}`) are still in place; nothing has been expanded.
pub fn effective_config(parent: &Config, overlay: &Value) -> Result<Config, OverlayError> {
    check_allowed_paths(parent, overlay)?;
    let mut merged = merge_unclamped(parent, overlay)?;
    clamp_to_parent(parent, &mut merged);
    Ok(merged)
}

/// Read an overlay file.
pub fn read_overlay(path: &Path) -> Result<Value, OverlayError> {
    let text = fs::read_to_string(path)
        .map_err(|e| OverlayError::Unreadable(format!("{}: {e}", path.display())))?;
    serde_json::from_str(&text)
        .map_err(|e| OverlayError::Unreadable(format!("{} is not valid JSON: {e}", path.display())))
}

/// Load the parent config, apply the overlay and expand `${VAR}` placeholders.
///
/// The order is the point: raw parent config, validate and merge the overlay,
/// and only then expand environment variables. The allowlist therefore sees
/// placeholders, never secrets, and expanded secrets exist only in memory.
///
/// Like [`load_config`] this panics when the parent file is malformed JSON; the
/// caller turns that into an error.
pub fn load_config_with_overlay(
    parent_path: &Path,
    overlay_path: &Path,
) -> Result<Config, OverlayError> {
    let parent = load_config(Some(parent_path.to_path_buf()));
    let overlay = read_overlay(overlay_path)?;
    let merged = effective_config(&parent, &overlay)?;
    resolve_config_env_vars(&merged).map_err(OverlayError::Invalid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A parent with a configured provider, one preset and one MCP server.
    fn parent() -> Config {
        serde_json::from_value(json!({
            "agents": {"provider": "openai", "model": "gpt-4o"},
            "providers": {"openai": {"apiKey": "sk-parent-secret", "apiBase": "https://api.parent.example"}},
            "modelPresets": {"cheap": {"model": "gpt-4o-mini", "provider": "openai"}},
            "tools": {
                "mcpServers": {
                    "docs": {"command": "npx", "args": ["docs-server"], "enabledTools": ["*"]},
                    "db": {"command": "db-mcp", "enabledTools": ["query", "schema"]}
                }
            }
        }))
        .unwrap()
    }

    fn not_allowed_path(result: Result<(), OverlayError>) -> String {
        match result {
            Err(OverlayError::NotAllowed { path, .. }) => path,
            other => panic!("expected NotAllowed, got {other:?}"),
        }
    }

    fn widens_path(result: Result<(), OverlayError>) -> String {
        match result {
            Err(OverlayError::Widens { path, .. }) => path,
            other => panic!("expected Widens, got {other:?}"),
        }
    }

    // ── allowlist ───────────────────────────────────────────────────────────

    #[test]
    fn the_read_only_reviewer_overlay_is_accepted() {
        let overlay = json!({
            "tools": {
                "exec": {"enable": false},
                "restrictToWorkspace": true,
                "disabledTools": ["write_file", "edit_file"]
            }
        });
        assert_eq!(validate_overlay(&parent(), &overlay), Ok(()));
    }

    #[test]
    fn model_preset_and_configured_provider_are_accepted() {
        let overlay = json!({"agents": {"model": "gpt-4o-mini", "modelPreset": "cheap", "provider": "openai"}});
        assert_eq!(validate_overlay(&parent(), &overlay), Ok(()));
        assert_eq!(
            validate_overlay(&parent(), &json!({"agents": {"provider": "auto"}})),
            Ok(())
        );
    }

    #[test]
    fn provider_keys_urls_and_headers_can_never_be_set() {
        for overlay in [
            json!({"providers": {"openai": {"apiBase": "https://evil.example"}}}),
            json!({"providers": {"openai": {"apiKey": "sk-stolen"}}}),
            json!({"providers": {"openai": {"extraHeaders": {"x": "y"}}}}),
            json!({"providers": {"custom": {"apiBase": "https://evil.example"}}}),
        ] {
            let path = not_allowed_path(validate_overlay(&parent(), &overlay));
            assert!(path.starts_with("providers"), "{path}");
        }
    }

    #[test]
    fn a_new_mcp_server_or_a_changed_inherited_one_is_rejected() {
        let new_server =
            json!({"tools": {"mcpServers": {"evil": {"command": "sh", "args": ["-c", "x"]}}}});
        assert_eq!(
            not_allowed_path(validate_overlay(&parent(), &new_server)),
            "tools.mcpServers.evil.command"
        );
        let changed = json!({"tools": {"mcpServers": {"docs": {"command": "sh"}}}});
        assert_eq!(
            not_allowed_path(validate_overlay(&parent(), &changed)),
            "tools.mcpServers.docs.command"
        );
        let env = json!({"tools": {"mcpServers": {"docs": {"env": {"A": "b"}}}}});
        assert_eq!(
            not_allowed_path(validate_overlay(&parent(), &env)),
            "tools.mcpServers.docs.env.A"
        );
        let not_null = json!({"tools": {"mcpServers": {"docs": "gone"}}});
        assert!(validate_overlay(&parent(), &not_null).is_err());
    }

    #[test]
    fn launch_presets_channels_and_unknown_paths_are_rejected() {
        for (overlay, expected) in [
            (
                json!({"tools": {"acp": {"launchPresets": {"x": {"command": ["sh"]}}}}}),
                "tools.acp.launchPresets.x.command",
            ),
            (
                json!({"channels": {"websocket": {"enabled": true}}}),
                "channels.websocket.enabled",
            ),
            (
                json!({"tools": {"exec": {"enabled": false}}}),
                "tools.exec.enabled",
            ),
            (
                json!({"tools": {"exec": {"sandbox": ""}}}),
                "tools.exec.sandbox",
            ),
            (
                json!({"agents": {"workspace": "/elsewhere"}}),
                "agents.workspace",
            ),
            (
                json!({"tools": {"restrict_to_workspace": true}}),
                "tools.restrict_to_workspace",
            ),
            (json!({"toolz": {}}), "toolz"),
        ] {
            assert_eq!(
                not_allowed_path(validate_overlay(&parent(), &overlay)),
                expected
            );
        }
    }

    #[test]
    fn an_unconfigured_provider_or_unknown_preset_is_rejected() {
        let unconfigured = json!({"agents": {"provider": "anthropic"}});
        assert_eq!(
            not_allowed_path(validate_overlay(&parent(), &unconfigured)),
            "agents.provider"
        );
        let unknown_preset = json!({"agents": {"modelPreset": "nope"}});
        assert_eq!(
            not_allowed_path(validate_overlay(&parent(), &unknown_preset)),
            "agents.modelPreset"
        );
    }

    #[test]
    fn wrong_value_types_are_rejected() {
        for overlay in [
            json!({"tools": {"exec": {"enable": "yes"}}}),
            json!({"tools": {"disabledTools": "write_file"}}),
            json!({"tools": {"disabledTools": [1, 2]}}),
            json!({"agents": {"model": ""}}),
            json!({"agents": {"model": 5}}),
            json!("not an object"),
            json!([]),
        ] {
            assert!(
                validate_overlay(&parent(), &overlay).is_err(),
                "should reject {overlay}"
            );
        }
    }

    #[test]
    fn dropping_a_server_and_narrowing_its_tools_is_accepted() {
        let overlay =
            json!({"tools": {"mcpServers": {"docs": null, "db": {"enabledTools": ["query"]}}}});
        assert_eq!(validate_overlay(&parent(), &overlay), Ok(()));
        let effective = effective_config(&parent(), &overlay).unwrap();
        assert!(!effective.tools.mcp_servers.contains_key("docs"));
        assert_eq!(
            effective.tools.mcp_servers["db"].enabled_tools,
            vec!["query"]
        );
        // The server's command is the parent's, untouched.
        assert_eq!(effective.tools.mcp_servers["db"].command, "db-mcp");
    }

    // ── widening (write-time feedback) ──────────────────────────────────────

    #[test]
    fn enabling_what_the_parent_disabled_is_rejected_when_written() {
        let mut strict_parent = parent();
        strict_parent.tools.exec.enable = false;
        strict_parent.tools.restrict_to_workspace = true;
        strict_parent.tools.disabled_tools = vec!["shell".to_string()];

        assert_eq!(
            widens_path(validate_overlay(
                &strict_parent,
                &json!({"tools": {"exec": {"enable": true}}})
            )),
            "tools.exec.enable"
        );
        assert_eq!(
            widens_path(validate_overlay(
                &strict_parent,
                &json!({"tools": {"restrictToWorkspace": false}})
            )),
            "tools.restrictToWorkspace"
        );
        // null deletes the key, which falls back to the looser default.
        assert_eq!(
            widens_path(validate_overlay(
                &strict_parent,
                &json!({"tools": {"restrictToWorkspace": null}})
            )),
            "tools.restrictToWorkspace"
        );
        assert_eq!(
            widens_path(validate_overlay(
                &strict_parent,
                &json!({"tools": {"disabledTools": ["write_file"]}})
            )),
            "tools.disabledTools"
        );
        assert_eq!(
            widens_path(validate_overlay(
                &strict_parent,
                &json!({"tools": {"disabledTools": null}})
            )),
            "tools.disabledTools"
        );
    }

    #[test]
    fn listing_tools_the_parent_does_not_enable_is_rejected_when_written() {
        let wider =
            json!({"tools": {"mcpServers": {"db": {"enabledTools": ["query", "drop_table"]}}}});
        assert_eq!(
            widens_path(validate_overlay(&parent(), &wider)),
            "tools.mcpServers.db.enabledTools"
        );
        let all = json!({"tools": {"mcpServers": {"db": {"enabledTools": ["*"]}}}});
        assert_eq!(
            widens_path(validate_overlay(&parent(), &all)),
            "tools.mcpServers.db.enabledTools"
        );
        // Under a server that enables everything, any list is narrower.
        let narrow = json!({"tools": {"mcpServers": {"docs": {"enabledTools": ["search"]}}}});
        assert_eq!(validate_overlay(&parent(), &narrow), Ok(()));
    }

    // ── strictest wins (start-time) ─────────────────────────────────────────

    #[test]
    fn a_parent_that_tightens_later_only_makes_the_child_stricter() {
        // The overlay was valid when written (the parent disabled nothing) ...
        let overlay = json!({"tools": {"disabledTools": ["write_file"]}});
        assert_eq!(validate_overlay(&parent(), &overlay), Ok(()));
        // ... then the parent tightens. The child keeps both, and still starts.
        let mut tightened = parent();
        tightened.tools.disabled_tools = vec!["shell".to_string()];
        tightened.tools.restrict_to_workspace = true;
        tightened.tools.exec.enable = false;

        let effective = effective_config(&tightened, &overlay).unwrap();

        assert_eq!(effective.tools.disabled_tools, vec!["shell", "write_file"]);
        assert!(effective.tools.restrict_to_workspace);
        assert!(!effective.tools.exec.enable);
    }

    #[test]
    fn a_widening_value_at_start_is_clamped_not_fatal() {
        let mut strict_parent = parent();
        strict_parent.tools.exec.enable = false;
        strict_parent.tools.restrict_to_workspace = true;
        strict_parent.tools.disabled_tools = vec!["shell".to_string()];
        // A hand-edited overlay tries to loosen everything.
        let tampered = json!({"tools": {
            "exec": {"enable": true},
            "restrictToWorkspace": null,
            "disabledTools": []
        }});

        let effective = effective_config(&strict_parent, &tampered).unwrap();

        assert!(!effective.tools.exec.enable);
        assert!(effective.tools.restrict_to_workspace);
        assert_eq!(effective.tools.disabled_tools, vec!["shell"]);
    }

    #[test]
    fn enabled_tools_combine_to_the_narrower_list() {
        let all = vec!["*".to_string()];
        let some = vec!["a".to_string(), "b".to_string()];
        let other = vec!["b".to_string(), "c".to_string()];
        assert_eq!(narrower_enabled_tools(&all, &all), all);
        assert_eq!(narrower_enabled_tools(&all, &some), some);
        assert_eq!(narrower_enabled_tools(&some, &all), some);
        assert_eq!(narrower_enabled_tools(&some, &other), vec!["b"]);
        assert!(enabled_tools_within(&some, &all));
        assert!(enabled_tools_within(&["a".to_string()], &some));
        assert!(!enabled_tools_within(&other, &some));
        assert!(!enabled_tools_within(&all, &some));
    }

    #[test]
    fn everything_not_in_the_overlay_is_inherited_unchanged() {
        let overlay = json!({"agents": {"model": "gpt-4o-mini"}});
        let effective = effective_config(&parent(), &overlay).unwrap();
        let parent = parent();
        assert_eq!(effective.agents.model, "gpt-4o-mini");
        assert_eq!(
            effective.providers.openai.api_key,
            parent.providers.openai.api_key
        );
        assert_eq!(
            effective.providers.openai.api_base,
            parent.providers.openai.api_base
        );
        assert_eq!(
            effective.tools.mcp_servers.len(),
            parent.tools.mcp_servers.len()
        );
        assert_eq!(effective.agents.provider, parent.agents.provider);
    }

    // ── merge patch ─────────────────────────────────────────────────────────

    #[test]
    fn merge_patch_follows_rfc_7386() {
        let mut target = json!({"a": {"b": 1, "c": 2}, "list": [1, 2, 3], "gone": true, "keep": 1});
        json_merge_patch(
            &mut target,
            &json!({"a": {"b": 9, "d": 4}, "list": [7], "gone": null}),
        );
        assert_eq!(
            target,
            json!({"a": {"b": 9, "c": 2, "d": 4}, "list": [7], "keep": 1})
        );
        let mut scalar = json!({"a": 1});
        json_merge_patch(&mut scalar, &json!("replaced"));
        assert_eq!(scalar, json!("replaced"));
        let mut not_object = json!({"a": 1});
        json_merge_patch(&mut not_object, &json!({"a": {"b": 2}}));
        assert_eq!(not_object, json!({"a": {"b": 2}}));
    }

    // ── files and environment ───────────────────────────────────────────────

    fn write(dir: &Path, name: &str, value: &Value) -> std::path::PathBuf {
        let path = dir.join(name);
        fs::write(&path, serde_json::to_string_pretty(value).unwrap()).unwrap();
        path
    }

    #[test]
    fn placeholders_survive_validation_and_are_expanded_only_at_the_end() {
        // CARGO_MANIFEST_DIR is always set while cargo runs tests.
        let expected = std::env::var("CARGO_MANIFEST_DIR").expect("set by cargo");
        let mut with_placeholder = parent();
        with_placeholder.providers.openai.api_key = "${CARGO_MANIFEST_DIR}".to_string();

        // The effective config (what the allowlist and merge see) still holds the placeholder.
        let effective = effective_config(&with_placeholder, &json!({})).unwrap();
        assert_eq!(effective.providers.openai.api_key, "${CARGO_MANIFEST_DIR}");

        // Expansion happens after, on the merged config.
        let dir = tempfile::tempdir().unwrap();
        let parent_path = dir.path().join("config.json");
        fs::write(
            &parent_path,
            serde_json::to_string(&with_placeholder).unwrap(),
        )
        .unwrap();
        let overlay_path = write(
            dir.path(),
            "overlay.json",
            &json!({"agents": {"model": "gpt-4o-mini"}}),
        );
        let loaded = load_config_with_overlay(&parent_path, &overlay_path).unwrap();
        assert_eq!(loaded.providers.openai.api_key, expected);
        assert_eq!(loaded.agents.model, "gpt-4o-mini");
    }

    #[test]
    fn an_unset_referenced_variable_is_an_error_naming_it() {
        let mut broken = parent();
        broken.providers.openai.api_key = "${RUST_BOT_SURELY_UNSET_VARIABLE_4711}".to_string();
        let dir = tempfile::tempdir().unwrap();
        let parent_path = dir.path().join("config.json");
        fs::write(&parent_path, serde_json::to_string(&broken).unwrap()).unwrap();
        let overlay_path = write(dir.path(), "overlay.json", &json!({}));

        let error = load_config_with_overlay(&parent_path, &overlay_path).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("RUST_BOT_SURELY_UNSET_VARIABLE_4711"),
            "{error}"
        );
    }

    #[test]
    fn a_missing_or_malformed_overlay_file_is_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        let parent_path = write(
            dir.path(),
            "config.json",
            &serde_json::to_value(parent()).unwrap(),
        );
        let missing = load_config_with_overlay(&parent_path, &dir.path().join("nope.json"));
        assert!(matches!(missing, Err(OverlayError::Unreadable(_))));

        let broken = dir.path().join("broken.json");
        fs::write(&broken, "{ not json").unwrap();
        assert!(matches!(
            load_config_with_overlay(&parent_path, &broken),
            Err(OverlayError::Unreadable(_))
        ));
    }

    #[test]
    fn a_tampered_overlay_file_fails_the_load_with_the_validation_error() {
        let dir = tempfile::tempdir().unwrap();
        let parent_path = write(
            dir.path(),
            "config.json",
            &serde_json::to_value(parent()).unwrap(),
        );
        let overlay_path = write(
            dir.path(),
            "overlay.json",
            &json!({"providers": {"openai": {"apiBase": "https://evil.example"}}}),
        );
        let error = load_config_with_overlay(&parent_path, &overlay_path).unwrap_err();
        assert!(
            error.to_string().contains("providers.openai.apiBase"),
            "{error}"
        );
    }

    #[test]
    fn an_empty_overlay_changes_nothing() {
        let effective = effective_config(&parent(), &json!({})).unwrap();
        assert_eq!(
            serde_json::to_value(&effective).unwrap(),
            serde_json::to_value(parent()).unwrap()
        );
    }
}
