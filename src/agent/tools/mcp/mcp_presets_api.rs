//! Built-in MCP server presets: a JSON-driven catalog (bundled defaults, optionally
//! extended/overridden by a user file) plus the logic to materialize a preset into a
//! full [`McpServerConfig`], check whether it's ready to connect, and report why not.
//!
//! Ported from nanobot's `webui/mcp_presets_api.py`, adapted for rust-bot: presets are
//! data (JSON), not hardcoded source, and the action surface is the `/mcp-preset` chat
//! command (see `src/command/builtin.rs`) rather than a WebUI settings HTTP API.

use garde::Validate;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use crate::config::schema::McpServerConfig;
use crate::utils::embedded_templates;
use crate::utils::helpers::expand_tilde_path;

/// Compile-time snapshot of the bundled preset catalog, embedded via
/// [`crate::utils::embedded_templates`] (same mechanism used for `AGENTS.md` etc.).
const DEFAULT_MCP_PRESETS_ASSET: &str = "mcp_presets.default.json";

/// Mirrors nanobot's `_MCP_PRESET_NAME_RE` (`webui/mcp_presets_api.py:28`). Used to
/// validate a user-supplied preset/server name before doing anything with it.
pub static MCP_PRESET_NAME_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^[a-z0-9][a-z0-9_-]{0,63}$").unwrap());

// ── data model ─────────────────────────────────────────────────────────────────

/// Where a preset field value is applied on the resulting `MCPServerConfig`.
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum McpPresetFieldTargetKind {
    Env,
    UrlParam,
    Arg,
    Header,
}

fn default_mcp_preset_field_secret() -> bool {
    true
}
fn default_mcp_preset_field_required() -> bool {
    true
}

/// User-supplied configuration field a built-in MCP preset needs (an API key, token, etc.), and how to plug that value into the resulting MCPServerConfig
#[derive(Debug, Deserialize, Serialize, Validate, Clone)]
#[serde(rename_all = "camelCase")]
pub struct McpPresetField {
    /// The field's identifier, used as the key when a caller supplies a value (e.g. "browserbase_api_key")
    #[serde(alias = "name")]
    #[garde(skip)]
    pub name: String,

    /// Human-readable label shown to the user (e.g. "Browserbase API key")
    #[serde(alias = "label")]
    #[garde(skip)]
    pub label: String,

    /// `(kind, name)` describing where the value is written — e.g. `("env", "BRAVE_API_KEY")`,
    /// `("url_param", "browserbaseApiKey")`, `("arg", "--api-key")`, `("header", "Authorization")`.
    #[serde(alias = "target")]
    #[garde(skip)]
    pub target: (McpPresetFieldTargetKind, String),

    /// Whether the value should be treated/displayed as sensitive (defaults True).
    #[serde(alias = "secret", default = "default_mcp_preset_field_secret")]
    #[garde(skip)]
    pub secret: bool,

    /// Whether the field must be provided when installing the preset (defaults True).
    #[serde(alias = "required", default = "default_mcp_preset_field_required")]
    #[garde(skip)]
    pub required: bool,

    /// The name of an environment variable that can supply the value automatically (also used to report "configured via env" status)
    #[serde(alias = "env_var", default)]
    #[garde(skip)]
    pub env_var: String,

    /// Example text shown in the input field (e.g. "ghp_...")
    #[serde(alias = "placeholder", default)]
    #[garde(skip)]
    pub placeholder: String,
}

/// Transport used by a built-in MCP preset (`stdio` / `streamableHttp` / `sse` / `oauth`).
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum McpPresetTransport {
    Stdio,
    StreamableHttp,
    Sse,
    Oauth,
}

fn default_mcp_preset_install_supported() -> bool {
    true
}

/// Built-in MCP server preset, loaded from the JSON preset catalog (see [`load_mcp_presets`]).
#[derive(Debug, Deserialize, Serialize, Validate, Clone)]
#[serde(rename_all = "camelCase")]
pub struct McpPreset {
    /// Stable identifier (e.g. `"github"`) — also the key it's stored under in `config.tools.mcpServers` once enabled.
    #[serde(alias = "name")]
    #[garde(skip)]
    pub name: String,

    /// Human-readable name (e.g. `"GitHub"`).
    #[serde(alias = "display_name")]
    #[garde(skip)]
    pub display_name: String,

    /// Grouping used when listing presets (e.g. `"code"`, `"web"`, `"docs"`).
    #[serde(alias = "category", default)]
    #[garde(skip)]
    pub category: String,

    /// One-line description of what the preset provides.
    #[serde(alias = "description", default)]
    #[garde(skip)]
    pub description: String,

    /// Link to the preset's setup documentation.
    #[serde(alias = "docs_url", default)]
    #[garde(skip)]
    pub docs_url: String,

    /// How the preset connects: stdio, streamableHttp, sse, or oauth.
    #[serde(alias = "transport")]
    #[garde(skip)]
    pub transport: McpPresetTransport,

    /// Whether this preset can actually be materialized/enabled yet (defaults True).
    #[serde(
        alias = "install_supported",
        default = "default_mcp_preset_install_supported"
    )]
    #[garde(skip)]
    pub install_supported: bool,

    /// The `MCPServerConfig` template this preset materializes into, once its fields are resolved.
    #[serde(alias = "server", default)]
    #[garde(skip)]
    pub server: Option<McpServerConfig>,

    /// Configuration fields this preset needs (API keys, tokens, etc.).
    #[serde(alias = "fields", default)]
    #[garde(skip)]
    pub fields: Vec<McpPresetField>,

    /// Free-text note on what's required to use this preset (e.g. `"Docker and GitHub token"`).
    #[serde(alias = "requires", default)]
    #[garde(skip)]
    pub requires: String,

    /// Free-text usage note shown alongside the preset (e.g. auth caveats).
    #[serde(alias = "note", default)]
    #[garde(skip)]
    pub note: String,
}

/// Shape of the JSON preset catalog file (both the embedded default asset and the
/// user override file at `ToolsConfig::mcp_presets_path`).
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
pub struct McpPresetsFile {
    #[serde(default)]
    pub presets: Vec<McpPreset>,
}

// ── errors ─────────────────────────────────────────────────────────────────────

/// Error materializing/validating a preset into a runnable `MCPServerConfig`.
#[derive(Debug)]
pub enum McpPresetError {
    UnknownPreset(String),
    InvalidName(String),
    NotInstallable(String),
    MissingFields(String, Vec<String>),
    InvalidUrl(String, String),
}

impl std::fmt::Display for McpPresetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownPreset(name) => write!(f, "unknown MCP preset '{name}'"),
            Self::InvalidName(name) => write!(f, "invalid MCP preset name '{name}'"),
            Self::NotInstallable(display_name) => write!(f, "{display_name} is not supported yet"),
            Self::MissingFields(display_name, fields) => {
                write!(
                    f,
                    "{display_name} is missing required field(s): {}",
                    fields.join(", ")
                )
            }
            Self::InvalidUrl(url, reason) => write!(f, "invalid MCP server URL '{url}': {reason}"),
        }
    }
}

impl std::error::Error for McpPresetError {}

/// Error loading the MCP preset catalog (bundled defaults and/or the user override file).
#[derive(Debug)]
pub enum McpPresetLoadError {
    Io(std::io::Error),
    Json(serde_json::Error),
    DefaultAssetMissing,
    DefaultAssetCorrupt(serde_json::Error),
    DuplicateName(String),
}

impl std::fmt::Display for McpPresetLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
            Self::Json(e) => write!(f, "invalid MCP presets JSON: {e}"),
            Self::DefaultAssetMissing => {
                write!(
                    f,
                    "bundled default MCP presets asset is missing (this is a bug)"
                )
            }
            Self::DefaultAssetCorrupt(e) => {
                write!(
                    f,
                    "bundled default MCP presets are corrupt (this is a bug): {e}"
                )
            }
            Self::DuplicateName(name) => write!(f, "duplicate MCP preset name '{name}'"),
        }
    }
}

impl std::error::Error for McpPresetLoadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Json(e) | Self::DefaultAssetCorrupt(e) => Some(e),
            Self::DefaultAssetMissing | Self::DuplicateName(_) => None,
        }
    }
}

impl From<std::io::Error> for McpPresetLoadError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<serde_json::Error> for McpPresetLoadError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

// ── ingestion ──────────────────────────────────────────────────────────────────

/// Ensure no two presets in `presets` share a `name` (case-sensitive, matching how
/// presets are keyed in `config.tools.mcpServers`).
fn check_duplicate_names<'a>(
    presets: impl Iterator<Item = &'a McpPreset>,
) -> Result<(), McpPresetLoadError> {
    let mut seen = HashSet::new();
    for preset in presets {
        if !seen.insert(preset.name.clone()) {
            return Err(McpPresetLoadError::DuplicateName(preset.name.clone()));
        }
    }
    Ok(())
}

fn load_default_presets() -> Result<Vec<McpPreset>, McpPresetLoadError> {
    let raw = embedded_templates::get(DEFAULT_MCP_PRESETS_ASSET)
        .ok_or(McpPresetLoadError::DefaultAssetMissing)?;
    let file: McpPresetsFile =
        serde_json::from_str(&raw).map_err(McpPresetLoadError::DefaultAssetCorrupt)?;
    check_duplicate_names(file.presets.iter())?;
    Ok(file.presets)
}

/// Load the effective MCP preset catalog: the bundled defaults, merged by `name` with
/// whatever is found at `path` (tilde-expanded). A user entry overrides a bundled
/// preset with the same name; new names are pure additions. A missing user file means
/// "defaults only" — this loader never writes to disk.
pub fn load_mcp_presets(path: &str) -> Result<Vec<McpPreset>, McpPresetLoadError> {
    let mut by_name: HashMap<String, McpPreset> = HashMap::new();
    for preset in load_default_presets()? {
        by_name.insert(preset.name.clone(), preset);
    }

    let expanded = expand_tilde_path(path);
    match std::fs::read_to_string(expanded.as_ref()) {
        Ok(contents) => {
            let file: McpPresetsFile = serde_json::from_str(&contents)?;
            check_duplicate_names(file.presets.iter())?;
            for preset in file.presets {
                by_name.insert(preset.name.clone(), preset);
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err.into()),
    }

    let mut presets: Vec<McpPreset> = by_name.into_values().collect();
    presets.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(presets)
}

// ── materialization ────────────────────────────────────────────────────────────

/// Find `--flag value` or `--flag=value` in an argv-style slice. Mirrors nanobot's
/// `_arg_value` (`webui/mcp_presets_api.py:523-530`).
fn arg_value(args: &[String], flag: &str) -> Option<String> {
    let prefix = format!("{flag}=");
    let mut i = 0;
    while i < args.len() {
        let item = &args[i];
        if item == flag {
            return args.get(i + 1).cloned();
        }
        if let Some(v) = item.strip_prefix(&prefix) {
            return Some(v.to_string());
        }
        i += 1;
    }
    None
}

/// Replace any existing `--flag`/`--flag=value` occurrence and append the flag pair at
/// the end. Mirrors nanobot's `_with_arg_value` (`webui/mcp_presets_api.py:533-548`).
fn with_arg_value(args: &[String], flag: &str, value: &str) -> Vec<String> {
    let prefix = format!("{flag}=");
    let mut out = Vec::with_capacity(args.len() + 2);
    let mut skip_next = false;
    for item in args {
        if skip_next {
            skip_next = false;
            continue;
        }
        if item == flag {
            skip_next = true;
            continue;
        }
        if item.starts_with(&prefix) {
            continue;
        }
        out.push(item.clone());
    }
    out.push(flag.to_string());
    out.push(value.to_string());
    out
}

/// Read a query parameter's first value from a URL string.
fn url_query_param(url: &str, key: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    parsed
        .query_pairs()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
}

/// Replace-or-append a query parameter on a URL string. Mirrors nanobot's
/// `_url_with_param` (`webui/mcp_presets_api.py:507-520`).
fn url_with_param(url: &str, key: &str, value: &str) -> Result<String, McpPresetError> {
    let mut parsed = url::Url::parse(url)
        .map_err(|e| McpPresetError::InvalidUrl(url.to_string(), e.to_string()))?;
    let existing: Vec<(String, String)> = parsed
        .query_pairs()
        .filter(|(k, _)| k != key)
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    {
        let mut qp = parsed.query_pairs_mut();
        qp.clear();
        for (k, v) in &existing {
            qp.append_pair(k, v);
        }
        qp.append_pair(key, value);
    }
    Ok(parsed.to_string())
}

/// Read a field's current value out of an existing `McpServerConfig`, per its target
/// kind. Mirrors nanobot's `_field_value_from_config` (`webui/mcp_presets_api.py:551-568`).
fn field_value_from_config(
    field: &McpPresetField,
    cfg: Option<&McpServerConfig>,
) -> Option<String> {
    let cfg = cfg?;
    let (kind, target_name) = &field.target;
    match kind {
        McpPresetFieldTargetKind::Env => {
            cfg.env.get(target_name).filter(|v| !v.is_empty()).cloned()
        }
        McpPresetFieldTargetKind::Header => cfg
            .headers
            .get(target_name)
            .filter(|v| !v.is_empty())
            .cloned(),
        McpPresetFieldTargetKind::Arg => arg_value(&cfg.args, target_name),
        McpPresetFieldTargetKind::UrlParam => {
            if cfg.url.is_empty() {
                None
            } else {
                url_query_param(&cfg.url, target_name)
            }
        }
    }
}

/// Whether a field already has a usable value — either in `cfg` or via its env var.
/// Mirrors nanobot's `_field_configured` (`webui/mcp_presets_api.py:571-575`).
fn field_configured(field: &McpPresetField, cfg: Option<&McpServerConfig>) -> bool {
    if field_value_from_config(field, cfg).is_some() {
        return true;
    }
    !field.env_var.is_empty()
        && std::env::var(&field.env_var)
            .map(|v| !v.is_empty())
            .unwrap_or(false)
}

/// Resolve a field's value with precedence: caller-supplied override > existing config
/// value > `${ENV_VAR}` placeholder string (never the raw resolved secret — this lets a
/// secret live only in an environment variable while `config.json` stores just the
/// `${VAR}` reference). Mirrors nanobot's `_resolve_field_value`
/// (`webui/mcp_presets_api.py:590-603`).
fn resolve_field_value(
    field: &McpPresetField,
    overrides: &HashMap<String, String>,
    existing: Option<&McpServerConfig>,
) -> Option<String> {
    if let Some(provided) = overrides
        .get(&field.name)
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        return Some(provided.to_string());
    }
    if let Some(current) = field_value_from_config(field, existing) {
        return Some(current);
    }
    if !field.env_var.is_empty()
        && std::env::var(&field.env_var)
            .map(|v| !v.is_empty())
            .unwrap_or(false)
    {
        return Some(format!("${{{}}}", field.env_var));
    }
    None
}

/// Materialize `preset` into a full `McpServerConfig`, applying `overrides` (caller-
/// supplied `field_name -> value` pairs, e.g. parsed from `/mcp-preset enable` args)
/// and falling back to `existing`'s current values (so re-enabling an already-
/// configured preset preserves previously entered values). Mirrors nanobot's
/// `_materialize_server` (`webui/mcp_presets_api.py:606-630`).
///
/// Collects *every* missing required field into one error, rather than failing on the
/// first, so the caller can report the full list at once.
pub fn materialize_preset_server(
    preset: &McpPreset,
    overrides: &HashMap<String, String>,
    existing: Option<&McpServerConfig>,
) -> Result<McpServerConfig, McpPresetError> {
    let Some(template) = &preset.server else {
        return Err(McpPresetError::NotInstallable(preset.display_name.clone()));
    };
    if !preset.install_supported {
        return Err(McpPresetError::NotInstallable(preset.display_name.clone()));
    }

    let mut cfg = template.clone();
    let mut missing: Vec<String> = Vec::new();
    for field in &preset.fields {
        let value = resolve_field_value(field, overrides, existing);
        match value {
            None if field.required => {
                missing.push(field.label.clone());
            }
            None => {}
            Some(value) => {
                let (kind, target_name) = &field.target;
                match kind {
                    McpPresetFieldTargetKind::Env => {
                        cfg.env.insert(target_name.clone(), value);
                    }
                    McpPresetFieldTargetKind::Header => {
                        cfg.headers.insert(target_name.clone(), value);
                    }
                    McpPresetFieldTargetKind::Arg => {
                        cfg.args = with_arg_value(&cfg.args, target_name, &value);
                    }
                    McpPresetFieldTargetKind::UrlParam => {
                        cfg.url = url_with_param(&cfg.url, target_name, &value)?;
                    }
                }
            }
        }
    }
    if !missing.is_empty() {
        return Err(McpPresetError::MissingFields(
            preset.display_name.clone(),
            missing,
        ));
    }
    Ok(cfg)
}

// ── status / validation ────────────────────────────────────────────────────────

/// Readiness of a preset given its (possibly absent) configured `McpServerConfig`.
/// Mirrors nanobot's status strings (`webui/mcp_presets_api.py:652-659`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpPresetStatus {
    NotInstalled,
    ComingSoon,
    MissingCredentials,
    MissingDependency,
    Configured,
}

impl McpPresetStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotInstalled => "not_installed",
            Self::ComingSoon => "coming_soon",
            Self::MissingCredentials => "missing_credentials",
            Self::MissingDependency => "missing_dependency",
            Self::Configured => "configured",
        }
    }
}

/// Whether `command` resolves to a real executable — either on `PATH` or as a literal
/// (tilde-expandable) file path. Mirrors nanobot's `_command_available`
/// (`webui/mcp_presets_api.py:633-639`).
fn command_available(command: &str) -> bool {
    if command.is_empty() {
        return false;
    }
    if which::which(command).is_ok() {
        return true;
    }
    let expanded = expand_tilde_path(command);
    std::path::Path::new(expanded.as_ref()).is_file()
}

/// Determine a preset's readiness. Mirrors nanobot's `_status_for`
/// (`webui/mcp_presets_api.py:652-659`).
pub fn status_for(preset: &McpPreset, cfg: Option<&McpServerConfig>) -> McpPresetStatus {
    let Some(cfg) = cfg else {
        return if preset.install_supported {
            McpPresetStatus::NotInstalled
        } else {
            McpPresetStatus::ComingSoon
        };
    };
    if preset
        .fields
        .iter()
        .any(|f| f.required && !field_configured(f, Some(cfg)))
    {
        return McpPresetStatus::MissingCredentials;
    }
    if !cfg.command.is_empty() && !command_available(&cfg.command) {
        return McpPresetStatus::MissingDependency;
    }
    McpPresetStatus::Configured
}

// ── message-attachment mentions (WebUI-facing, currently unused by any channel) ──

/// Sanitize structured MCP preset mentions sent by a client. Currently a no-op stub —
/// no channel populates `mcp_presets` on an inbound envelope yet (see
/// `src/channels/websocket/runtime.rs`, which mirrors this for `cli_apps` via
/// `normalize_cli_app_mentions`). Left unimplemented intentionally; wiring this up is a
/// separate, self-contained follow-up unrelated to the `/mcp-preset` command.
pub fn normalize_mcp_preset_mentions(
    raw: Option<&serde_json::Value>,
) -> Vec<HashMap<String, String>> {
    let Some(serde_json::Value::Array(_items)) = raw else {
        return vec![];
    };
    vec![]
}

// ── tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn field(
        name: &str,
        target: (McpPresetFieldTargetKind, &str),
        required: bool,
        env_var: &str,
    ) -> McpPresetField {
        McpPresetField {
            name: name.to_string(),
            label: format!("{name} label"),
            target: (target.0, target.1.to_string()),
            secret: true,
            required,
            env_var: env_var.to_string(),
            placeholder: String::new(),
        }
    }

    fn preset_with_fields(
        server: Option<McpServerConfig>,
        fields: Vec<McpPresetField>,
    ) -> McpPreset {
        McpPreset {
            name: "test-preset".to_string(),
            display_name: "Test Preset".to_string(),
            category: String::new(),
            description: String::new(),
            docs_url: String::new(),
            transport: McpPresetTransport::Stdio,
            install_supported: true,
            server,
            fields,
            requires: String::new(),
            note: String::new(),
        }
    }

    fn stdio_server(command: &str, args: &[&str]) -> McpServerConfig {
        McpServerConfig {
            command: command.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            ..McpServerConfig::default()
        }
    }

    // ── loader ────────────────────────────────────────────────────────────────

    #[test]
    fn load_mcp_presets_missing_file_returns_defaults_only() {
        let presets = load_mcp_presets("/definitely/not/a/real/path/mcp_presets.json").unwrap();
        assert!(presets.iter().any(|p| p.name == "github"));
        assert!(presets.iter().any(|p| p.name == "playwright"));
        assert_eq!(presets.len(), load_default_presets().unwrap().len());
    }

    #[test]
    fn load_mcp_presets_malformed_json_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.json");
        std::fs::write(&path, "not json").unwrap();
        let err = load_mcp_presets(path.to_str().unwrap()).unwrap_err();
        assert!(matches!(err, McpPresetLoadError::Json(_)));
    }

    #[test]
    fn load_mcp_presets_user_file_overrides_default_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp_presets.json");
        std::fs::write(
            &path,
            r#"{"presets":[{"name":"github","displayName":"GitHub Fork","transport":"stdio"}]}"#,
        )
        .unwrap();
        let presets = load_mcp_presets(path.to_str().unwrap()).unwrap();
        let github = presets.iter().find(|p| p.name == "github").unwrap();
        assert_eq!(github.display_name, "GitHub Fork");
        // Other defaults remain untouched.
        assert!(presets.iter().any(|p| p.name == "playwright"));
    }

    #[test]
    fn load_mcp_presets_user_file_duplicate_name_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp_presets.json");
        std::fs::write(
            &path,
            r#"{"presets":[
                {"name":"foo","displayName":"Foo","transport":"stdio"},
                {"name":"foo","displayName":"Foo2","transport":"stdio"}
            ]}"#,
        )
        .unwrap();
        let err = load_mcp_presets(path.to_str().unwrap()).unwrap_err();
        assert!(matches!(err, McpPresetLoadError::DuplicateName(name) if name == "foo"));
    }

    #[test]
    fn default_asset_has_no_duplicate_names() {
        let presets = load_default_presets().unwrap();
        assert!(check_duplicate_names(presets.iter()).is_ok());
        assert_eq!(presets.len(), 13);
    }

    // ── resolve_field_value precedence ──────────────────────────────────────

    #[test]
    fn resolve_field_value_prefers_override_over_config_over_env() {
        let f = field(
            "token",
            (McpPresetFieldTargetKind::Env, "TOKEN"),
            true,
            "MY_ENV_VAR",
        );
        let mut overrides = HashMap::new();
        overrides.insert("token".to_string(), "override-value".to_string());
        let existing = stdio_server("cmd", &[]);
        assert_eq!(
            resolve_field_value(&f, &overrides, Some(&existing)),
            Some("override-value".to_string())
        );
    }

    #[test]
    fn resolve_field_value_falls_back_to_existing_config_value() {
        let f = field(
            "token",
            (McpPresetFieldTargetKind::Env, "TOKEN"),
            true,
            "MY_ENV_VAR",
        );
        let overrides = HashMap::new();
        let mut existing = stdio_server("cmd", &[]);
        existing
            .env
            .insert("TOKEN".to_string(), "existing-value".to_string());
        assert_eq!(
            resolve_field_value(&f, &overrides, Some(&existing)),
            Some("existing-value".to_string())
        );
    }

    #[test]
    fn resolve_field_value_falls_back_to_env_var_placeholder() {
        let var_name = "RUST_BOT_TEST_MCP_PRESET_ENV_VAR";
        unsafe {
            std::env::set_var(var_name, "actual-secret");
        }
        let f = field(
            "token",
            (McpPresetFieldTargetKind::Env, "TOKEN"),
            true,
            var_name,
        );
        let overrides = HashMap::new();
        let result = resolve_field_value(&f, &overrides, None);
        unsafe {
            std::env::remove_var(var_name);
        }
        assert_eq!(result, Some(format!("${{{var_name}}}")));
    }

    #[test]
    fn resolve_field_value_none_when_nothing_supplied() {
        let f = field("token", (McpPresetFieldTargetKind::Env, "TOKEN"), true, "");
        let overrides = HashMap::new();
        assert_eq!(resolve_field_value(&f, &overrides, None), None);
    }

    // ── materialize_preset_server: one test per target kind ────────────────

    #[test]
    fn materialize_preset_server_env_target_sets_env_map() {
        let preset = preset_with_fields(
            Some(stdio_server("cmd", &[])),
            vec![field(
                "token",
                (McpPresetFieldTargetKind::Env, "TOKEN"),
                true,
                "",
            )],
        );
        let mut overrides = HashMap::new();
        overrides.insert("token".to_string(), "abc123".to_string());
        let cfg = materialize_preset_server(&preset, &overrides, None).unwrap();
        assert_eq!(cfg.env.get("TOKEN"), Some(&"abc123".to_string()));
    }

    #[test]
    fn materialize_preset_server_header_target_sets_headers_map() {
        let mut server = McpServerConfig {
            url: "https://example.com/mcp".to_string(),
            ..McpServerConfig::default()
        };
        server.transport_type = Some(crate::config::schema::McpTransportType::StreamableHttp);
        let preset = preset_with_fields(
            Some(server),
            vec![field(
                "auth",
                (McpPresetFieldTargetKind::Header, "Authorization"),
                true,
                "",
            )],
        );
        let mut overrides = HashMap::new();
        overrides.insert("auth".to_string(), "Bearer xyz".to_string());
        let cfg = materialize_preset_server(&preset, &overrides, None).unwrap();
        assert_eq!(
            cfg.headers.get("Authorization"),
            Some(&"Bearer xyz".to_string())
        );
    }

    #[test]
    fn materialize_preset_server_arg_target_appends_flag_pair() {
        let preset = preset_with_fields(
            Some(stdio_server("npx", &["-y", "@some/pkg"])),
            vec![field(
                "api_key",
                (McpPresetFieldTargetKind::Arg, "--api-key"),
                false,
                "",
            )],
        );
        let mut overrides = HashMap::new();
        overrides.insert("api_key".to_string(), "ctx7_abc".to_string());
        let cfg = materialize_preset_server(&preset, &overrides, None).unwrap();
        assert_eq!(cfg.args, vec!["-y", "@some/pkg", "--api-key", "ctx7_abc"]);
    }

    #[test]
    fn materialize_preset_server_url_param_target_sets_query_string() {
        let server = McpServerConfig {
            url: "https://mcp.example.com/mcp".to_string(),
            ..McpServerConfig::default()
        };
        let preset = preset_with_fields(
            Some(server),
            vec![field(
                "api_key",
                (McpPresetFieldTargetKind::UrlParam, "exampleApiKey"),
                true,
                "",
            )],
        );
        let mut overrides = HashMap::new();
        overrides.insert("api_key".to_string(), "bb_live_123".to_string());
        let cfg = materialize_preset_server(&preset, &overrides, None).unwrap();
        assert_eq!(
            url_query_param(&cfg.url, "exampleApiKey"),
            Some("bb_live_123".to_string())
        );
    }

    #[test]
    fn materialize_preset_server_missing_required_field_errors() {
        let preset = preset_with_fields(
            Some(stdio_server("cmd", &[])),
            vec![field(
                "token",
                (McpPresetFieldTargetKind::Env, "TOKEN"),
                true,
                "",
            )],
        );
        let overrides = HashMap::new();
        let err = materialize_preset_server(&preset, &overrides, None).unwrap_err();
        match err {
            McpPresetError::MissingFields(_, fields) => {
                assert_eq!(fields, vec!["token label".to_string()]);
            }
            other => panic!("expected MissingFields, got {other:?}"),
        }
    }

    #[test]
    fn materialize_preset_server_preserves_existing_value_when_not_overridden() {
        let preset = preset_with_fields(
            Some(stdio_server("cmd", &[])),
            vec![field(
                "token",
                (McpPresetFieldTargetKind::Env, "TOKEN"),
                true,
                "",
            )],
        );
        let mut existing = stdio_server("cmd", &[]);
        existing
            .env
            .insert("TOKEN".to_string(), "already-set".to_string());
        let overrides = HashMap::new();
        let cfg = materialize_preset_server(&preset, &overrides, Some(&existing)).unwrap();
        assert_eq!(cfg.env.get("TOKEN"), Some(&"already-set".to_string()));
    }

    #[test]
    fn materialize_preset_server_not_installable_when_no_server_template() {
        let preset = preset_with_fields(None, vec![]);
        let overrides = HashMap::new();
        let err = materialize_preset_server(&preset, &overrides, None).unwrap_err();
        assert!(matches!(err, McpPresetError::NotInstallable(_)));
    }

    // ── argv / URL helpers ───────────────────────────────────────────────────

    #[test]
    fn arg_value_finds_flag_space_form() {
        let args = vec!["-y".to_string(), "--api-key".to_string(), "abc".to_string()];
        assert_eq!(arg_value(&args, "--api-key"), Some("abc".to_string()));
    }

    #[test]
    fn arg_value_finds_flag_equals_form() {
        let args = vec!["--api-key=abc".to_string()];
        assert_eq!(arg_value(&args, "--api-key"), Some("abc".to_string()));
    }

    #[test]
    fn with_arg_value_replaces_existing_flag() {
        let args = vec![
            "--api-key".to_string(),
            "old".to_string(),
            "--full".to_string(),
        ];
        let result = with_arg_value(&args, "--api-key", "new");
        assert_eq!(result, vec!["--full", "--api-key", "new"]);
    }

    #[test]
    fn url_with_param_replaces_existing_query_param() {
        let url = url_with_param("https://example.com/mcp?foo=old&bar=1", "foo", "new").unwrap();
        assert_eq!(url_query_param(&url, "foo"), Some("new".to_string()));
        assert_eq!(url_query_param(&url, "bar"), Some("1".to_string()));
    }

    #[test]
    fn url_query_param_reads_first_value() {
        let url = "https://example.com/mcp?key=value1&key=value2";
        assert_eq!(url_query_param(url, "key"), Some("value1".to_string()));
        assert_eq!(url_query_param(url, "missing"), None);
    }

    // ── status_for ───────────────────────────────────────────────────────────

    #[test]
    fn status_for_not_installed_when_no_existing_config() {
        let preset = preset_with_fields(Some(stdio_server("cmd", &[])), vec![]);
        assert_eq!(status_for(&preset, None), McpPresetStatus::NotInstalled);
    }

    #[test]
    fn status_for_missing_credentials_when_required_field_unconfigured() {
        let preset = preset_with_fields(
            Some(stdio_server("cmd", &[])),
            vec![field(
                "token",
                (McpPresetFieldTargetKind::Env, "TOKEN"),
                true,
                "",
            )],
        );
        let cfg = stdio_server("cmd", &[]);
        assert_eq!(
            status_for(&preset, Some(&cfg)),
            McpPresetStatus::MissingCredentials
        );
    }

    #[test]
    fn status_for_missing_dependency_when_command_not_on_path() {
        let preset = preset_with_fields(
            Some(stdio_server("definitely-not-a-real-binary-xyz", &[])),
            vec![],
        );
        let cfg = stdio_server("definitely-not-a-real-binary-xyz", &[]);
        assert_eq!(
            status_for(&preset, Some(&cfg)),
            McpPresetStatus::MissingDependency
        );
    }

    #[test]
    fn status_for_configured_when_all_satisfied() {
        let preset = preset_with_fields(
            Some(stdio_server("cmd", &[])),
            vec![field(
                "token",
                (McpPresetFieldTargetKind::Env, "TOKEN"),
                true,
                "",
            )],
        );
        let mut cfg = stdio_server("cmd", &[]);
        cfg.command = String::new(); // no dependency check when there's no command
        cfg.env.insert("TOKEN".to_string(), "abc".to_string());
        assert_eq!(status_for(&preset, Some(&cfg)), McpPresetStatus::Configured);
    }

    #[test]
    fn check_duplicate_names_detects_repeat() {
        let a = preset_with_fields(None, vec![]);
        let mut b = preset_with_fields(None, vec![]);
        b.name = a.name.clone();
        assert!(check_duplicate_names(vec![&a, &b].into_iter()).is_err());
    }
}
