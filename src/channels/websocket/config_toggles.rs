//! Switching tools and MCP servers on or off in the raw text of `config.json`, for the
//! WebUI's Settings dialog.
//!
//! The edits work on the JSON text (key order is preserved) and touch only
//! `tools.disabledTools` and `tools.mcpServers.<name>.enabled`. Nothing is round-tripped
//! through the typed `Config`, so `${ENV_VAR}` placeholders and every other setting stay
//! exactly as the user wrote them. Everything here is a pure function of strings, which
//! keeps it unit-testable; saving (validation, backup, atomic write) is
//! [`super::config_file::save_config_text`]'s job and the envelope handlers in
//! `runtime.rs` add the access checks.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use super::config_file::{read_config_text, save_config_text};
use crate::agent::tools::registry::ToolCatalogueEntry;
use crate::agent::tools::tool_settings_api::ToolSettingsSummary;

/// Tools that cannot be switched off from the dialog: other parts of the gateway call
/// them directly (the REST API sends replies through `message`).
pub const PROTECTED_TOOLS: &[&str] = &["message"];

/// Longest tool or server name the dialog will write into the file.
const MAX_NAME_LENGTH: usize = 128;

/// Spellings the configuration schema accepts for `tools.disabledTools`.
const DISABLED_TOOLS_KEYS: &[&str] = &["disabledTools", "disabled_tools"];
/// Spellings the configuration schema accepts for `tools.mcpServers`.
const MCP_SERVERS_KEYS: &[&str] = &["mcpServers", "mcp_servers"];

/// Why a toggle was not applied. Messages name the tool or server only; they never
/// contain any other part of the file.
#[derive(Debug, PartialEq, Eq)]
pub enum ToggleError {
    /// The file is not valid JSON, or not shaped the way the toggle expects.
    InvalidConfig(String),
    /// The name is empty, too long, or contains characters tools and servers never use.
    InvalidName,
    /// The tool is one the gateway needs (see [`PROTECTED_TOOLS`]).
    ProtectedTool,
    /// No MCP server with that name is configured.
    UnknownServer,
}

impl ToggleError {
    /// Stable code the browser switches on.
    pub fn detail(&self) -> &'static str {
        match self {
            Self::InvalidConfig(_) => "invalid_config",
            Self::InvalidName => "invalid_name",
            Self::ProtectedTool => "protected_tool",
            Self::UnknownServer => "unknown_server",
        }
    }

    /// Human-readable explanation shown in the dialog.
    pub fn message(&self) -> String {
        match self {
            Self::InvalidConfig(reason) => reason.clone(),
            Self::InvalidName => "Invalid name.".to_string(),
            Self::ProtectedTool => {
                "This tool is required by the gateway and cannot be switched off.".to_string()
            }
            Self::UnknownServer => "No MCP server with that name is configured.".to_string(),
        }
    }
}

/// An MCP server as configured in the file, for the dialog's list.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ConfiguredMcpServer {
    pub name: String,
    /// The server's `enabled` flag; `true` when the key is absent.
    pub enabled: bool,
    /// `stdio`, `sse` or `streamableHttp`, as written or detected; `unknown` otherwise.
    /// Never the command line, URL, headers or environment: those may hold credentials.
    pub transport: String,
}

/// Whether `name` is acceptable as a tool or server name: 1–128 characters from
/// letters, digits, `_`, `-`, `.` and spaces.
pub fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_LENGTH
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ' '))
}

/// Whether the dialog may not switch `tool_name` off (see [`PROTECTED_TOOLS`]).
pub fn is_protected_tool(tool_name: &str) -> bool {
    PROTECTED_TOOLS.contains(&tool_name)
}

fn parse_root(text: &str) -> Result<Value, ToggleError> {
    let root: Value = serde_json::from_str(text)
        .map_err(|e| ToggleError::InvalidConfig(format!("Invalid JSON: {e}")))?;
    if root.is_object() {
        Ok(root)
    } else {
        Err(ToggleError::InvalidConfig(
            "The configuration is not a JSON object.".to_string(),
        ))
    }
}

/// The first of `keys` present in `object`, or the first key when none is.
fn existing_or_default_key<'a>(object: &Map<String, Value>, keys: &[&'a str]) -> &'a str {
    keys.iter()
        .copied()
        .find(|key| object.contains_key(*key))
        .unwrap_or(keys[0])
}

/// `root.tools` as a mutable object, created when missing.
fn tools_object_mut(root: &mut Value) -> Result<&mut Map<String, Value>, ToggleError> {
    let root_object = root.as_object_mut().expect("parse_root checked this");
    root_object
        .entry("tools")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| ToggleError::InvalidConfig("\"tools\" is not an object.".to_string()))
}

/// Serialize `root` the way the file was written: pretty-printed, with the trailing
/// newline of `original` kept.
fn render(root: &Value, original: &str) -> String {
    let mut text = serde_json::to_string_pretty(root).expect("a Value always serializes");
    if original.ends_with('\n') {
        text.push('\n');
    }
    text
}

/// Names in `tools.disabledTools` of the config `text`, in file order.
pub fn disabled_tool_names(text: &str) -> Result<Vec<String>, ToggleError> {
    let root = parse_root(text)?;
    let Some(tools) = root.get("tools").and_then(Value::as_object) else {
        return Ok(Vec::new());
    };
    let key = existing_or_default_key(tools, DISABLED_TOOLS_KEYS);
    Ok(tools
        .get(key)
        .and_then(Value::as_array)
        .map(|names| {
            names
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default())
}

/// The MCP servers configured in `text`, in file order.
pub fn configured_mcp_servers(text: &str) -> Result<Vec<ConfiguredMcpServer>, ToggleError> {
    let root = parse_root(text)?;
    let Some(tools) = root.get("tools").and_then(Value::as_object) else {
        return Ok(Vec::new());
    };
    let key = existing_or_default_key(tools, MCP_SERVERS_KEYS);
    let Some(servers) = tools.get(key).and_then(Value::as_object) else {
        return Ok(Vec::new());
    };
    Ok(servers
        .iter()
        .map(|(name, server)| ConfiguredMcpServer {
            name: name.clone(),
            enabled: server
                .get("enabled")
                .and_then(Value::as_bool)
                .unwrap_or(true),
            transport: transport_of(server),
        })
        .collect())
}

/// The transport of one server entry: its `transportType` (or `type`) when set, else
/// detected from `command` (stdio) or `url`.
fn transport_of(server: &Value) -> String {
    let declared = ["transportType", "type"]
        .iter()
        .find_map(|key| server.get(*key).and_then(Value::as_str))
        .filter(|value| matches!(*value, "stdio" | "sse" | "streamableHttp"));
    if let Some(declared) = declared {
        return declared.to_string();
    }
    let has_text = |key: &str| {
        server
            .get(key)
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty())
    };
    if has_text("command") {
        "stdio".to_string()
    } else if has_text("url") {
        "streamableHttp".to_string()
    } else {
        "unknown".to_string()
    }
}

/// `text` with the tool `name` added to (`disabled`) or removed from (`!disabled`)
/// `tools.disabledTools`. `None` when the file already says so, so the caller can skip
/// the write. Switching a protected tool off is refused; switching one on is always fine.
pub fn with_tool_disabled(
    text: &str,
    name: &str,
    disabled: bool,
) -> Result<Option<String>, ToggleError> {
    if !is_valid_name(name) {
        return Err(ToggleError::InvalidName);
    }
    if disabled && is_protected_tool(name) {
        return Err(ToggleError::ProtectedTool);
    }
    let mut root = parse_root(text)?;
    let tools = tools_object_mut(&mut root)?;
    let key = existing_or_default_key(tools, DISABLED_TOOLS_KEYS).to_string();
    let entry = tools.entry(key).or_insert_with(|| Value::Array(Vec::new()));
    let names = entry.as_array_mut().ok_or_else(|| {
        ToggleError::InvalidConfig("\"disabledTools\" is not a list.".to_string())
    })?;

    let currently_disabled = names.iter().any(|n| n.as_str() == Some(name));
    if currently_disabled == disabled {
        return Ok(None);
    }
    if disabled {
        names.push(Value::String(name.to_string()));
    } else {
        names.retain(|n| n.as_str() != Some(name));
    }
    Ok(Some(render(&root, text)))
}

/// `text` with `tools.mcpServers.<name>.enabled` set to `enabled`. `None` when the
/// server already has that value (an absent flag counts as `true`). The server must
/// already be configured: the dialog never creates one.
pub fn with_mcp_server_enabled(
    text: &str,
    name: &str,
    enabled: bool,
) -> Result<Option<String>, ToggleError> {
    if !is_valid_name(name) {
        return Err(ToggleError::InvalidName);
    }
    let mut root = parse_root(text)?;
    let tools = tools_object_mut(&mut root)?;
    let key = existing_or_default_key(tools, MCP_SERVERS_KEYS).to_string();
    let server = tools
        .get_mut(&key)
        .and_then(Value::as_object_mut)
        .and_then(|servers| servers.get_mut(name))
        .ok_or(ToggleError::UnknownServer)?;
    let server = server.as_object_mut().ok_or_else(|| {
        ToggleError::InvalidConfig("The MCP server entry is not an object.".to_string())
    })?;

    let current = server
        .get("enabled")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    if current == enabled {
        return Ok(None);
    }
    server.insert("enabled".to_string(), Value::Bool(enabled));
    Ok(Some(render(&root, text)))
}

/// What the dialog shows for one built-in tool.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ToolSettingRow {
    pub name: String,
    pub description: String,
    /// Whether `config.json` leaves the tool on (what the toggle shows).
    pub enabled: bool,
    /// Whether the tool is on in the running process. Differs from `enabled` while a
    /// change waits for a restart.
    pub active: bool,
    /// `false` for tools the gateway needs (see [`PROTECTED_TOOLS`]).
    pub toggleable: bool,
    /// The configuration form, for a row that stands for a whole tool section.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config: Option<ToolSettingsSummary>,
}

/// What the dialog shows for one configured MCP server.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct McpServerSettingRow {
    pub name: String,
    pub description: String,
    /// Whether `config.json` leaves the server on (what the toggle shows).
    pub enabled: bool,
    /// Whether the running process was started with the server on.
    pub active: bool,
}

/// Description shown for a tool named in `disabledTools` that this process does not know
/// (its feature is off in the configuration, or it was removed).
const UNKNOWN_TOOL_DESCRIPTION: &str = "Not available in this process.";

/// The tool rows: every tool of the running `catalogue` plus any name in `disabled_names`
/// the catalogue lacks, sorted A–Z ignoring case.
pub fn tool_setting_rows(
    catalogue: &[ToolCatalogueEntry],
    disabled_names: &[String],
) -> Vec<ToolSettingRow> {
    let is_disabled = |name: &str| disabled_names.iter().any(|n| n == name);
    let mut rows: Vec<ToolSettingRow> = catalogue
        .iter()
        .map(|entry| ToolSettingRow {
            name: entry.name.clone(),
            description: entry.description.clone(),
            enabled: !is_disabled(&entry.name),
            active: entry.active,
            toggleable: !is_protected_tool(&entry.name),
            config: None,
        })
        .collect();
    let known: HashSet<&str> = catalogue.iter().map(|e| e.name.as_str()).collect();
    let mut seen_unknown: HashSet<&str> = HashSet::new();
    for name in disabled_names {
        if known.contains(name.as_str()) || !seen_unknown.insert(name.as_str()) {
            continue;
        }
        rows.push(ToolSettingRow {
            name: name.clone(),
            description: UNKNOWN_TOOL_DESCRIPTION.to_string(),
            enabled: false,
            active: false,
            toggleable: true,
            config: None,
        });
    }
    rows.sort_by_key(|row| row.name.to_lowercase());
    rows
}

/// Fold the tool section forms into `rows`: the rows of the tools a section owns are
/// replaced by one row for the section, whose switch is the section's own `enable`
/// setting and which carries the form. `active` is whether any owned tool is running
/// (a section that is off registers none of them). Result is sorted A–Z.
pub fn merge_section_rows(
    rows: Vec<ToolSettingRow>,
    sections: Vec<ToolSettingsSummary>,
) -> Vec<ToolSettingRow> {
    let owned: HashSet<&str> = sections
        .iter()
        .flat_map(|section| section.tool_names.iter().map(String::as_str))
        .collect();
    let mut merged: Vec<ToolSettingRow> = rows
        .iter()
        .filter(|row| !owned.contains(row.name.as_str()))
        .cloned()
        .collect();
    for section in sections {
        // A section without a switch (general settings) is always in effect.
        let active = section.enabled.is_none()
            || rows
                .iter()
                .any(|row| row.active && section.tool_names.contains(&row.name));
        merged.push(ToolSettingRow {
            name: section.name.clone(),
            description: section.description.clone(),
            enabled: section.enabled.unwrap_or(true),
            active,
            toggleable: true,
            config: Some(section),
        });
    }
    merged.sort_by_key(|row| row.name.to_lowercase());
    merged
}

/// The MCP server rows: every configured server, sorted A–Z ignoring case.
/// `running` holds the servers the process was started with switched on.
pub fn mcp_server_setting_rows(
    configured: &[ConfiguredMcpServer],
    running: &HashSet<String>,
) -> Vec<McpServerSettingRow> {
    let mut rows: Vec<McpServerSettingRow> = configured
        .iter()
        .map(|server| McpServerSettingRow {
            name: server.name.clone(),
            description: format!("MCP server ({})", server.transport),
            enabled: server.enabled,
            active: running.contains(&server.name),
        })
        .collect();
    rows.sort_by_key(|row| row.name.to_lowercase());
    rows
}

/// Why a toggle could not be saved.
#[derive(Debug, PartialEq, Eq)]
pub enum ApplyError {
    /// The edit itself was refused.
    Toggle(ToggleError),
    /// Reading, validating, backing up or writing the file failed; the file is untouched.
    Save(String),
}

/// Result of a saved toggle.
#[derive(Debug, PartialEq, Eq)]
pub struct ApplyOutcome {
    /// Whether the file changed at all (a toggle to the current state needs no restart).
    pub changed: bool,
    /// The backup of the previous file, when one was made.
    pub backup: Option<PathBuf>,
}

/// Read the config at `path`, apply `edit` to its text and, if that changes anything,
/// save it through [`save_config_text`] (validated, previous file backed up, atomic).
/// The caller holds the config write lock so no other edit lands in between.
pub fn apply_edit(
    path: &Path,
    edit: impl FnOnce(&str) -> Result<Option<String>, ToggleError>,
) -> Result<ApplyOutcome, ApplyError> {
    let text = read_config_text(path)
        .map_err(|e| ApplyError::Save(format!("Could not read the configuration file: {e}")))?;
    let Some(updated) = edit(&text).map_err(ApplyError::Toggle)? else {
        return Ok(ApplyOutcome {
            changed: false,
            backup: None,
        });
    };
    let backup = save_config_text(path, &updated).map_err(ApplyError::Save)?;
    Ok(ApplyOutcome {
        changed: true,
        backup,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, active: bool) -> ToolCatalogueEntry {
        ToolCatalogueEntry {
            name: name.to_string(),
            description: format!("{name} tool"),
            active,
        }
    }

    #[test]
    fn tool_rows_show_the_file_state_and_the_running_state_separately() {
        let catalogue = vec![entry("grep", true), entry("shell", false)];

        let rows = tool_setting_rows(&catalogue, &["shell".to_string(), "grep".to_string()]);

        let grep = rows.iter().find(|r| r.name == "grep").unwrap();
        assert!(
            !grep.enabled && grep.active,
            "disabled in file, still running"
        );
        let shell = rows.iter().find(|r| r.name == "shell").unwrap();
        assert!(!shell.enabled && !shell.active);
    }

    #[test]
    fn tool_rows_are_sorted_ignoring_case_and_flag_protected_tools() {
        let catalogue = vec![
            entry("write_file", true),
            entry("Edit", true),
            entry("message", true),
        ];

        let rows = tool_setting_rows(&catalogue, &[]);

        let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["Edit", "message", "write_file"]);
        assert!(!rows[1].toggleable);
        assert!(rows[0].toggleable);
    }

    #[test]
    fn tool_rows_list_disabled_names_the_process_does_not_know_once() {
        let rows = tool_setting_rows(
            &[entry("grep", true)],
            &["web_search".to_string(), "web_search".to_string()],
        );

        let unknown: Vec<&ToolSettingRow> =
            rows.iter().filter(|r| r.name == "web_search").collect();
        assert_eq!(unknown.len(), 1);
        assert!(!unknown[0].enabled && !unknown[0].active);
        assert_eq!(unknown[0].description, UNKNOWN_TOOL_DESCRIPTION);
    }

    #[test]
    fn section_rows_replace_the_rows_of_the_tools_they_own() {
        use crate::agent::tools::tool_settings_api::tool_settings_overview;
        use crate::config::schema::ToolsConfig;

        let mut tools = ToolsConfig::default();
        tools.image_generation.enable = true;
        let rows = tool_setting_rows(&[entry("grep", true), entry("image_generation", true)], &[]);

        let merged = merge_section_rows(rows, tool_settings_overview(&tools));

        let names: Vec<&str> = merged.iter().map(|r| r.name.as_str()).collect();
        assert!(names.contains(&"grep") && names.contains(&"imageGeneration"));
        assert!(
            !names.contains(&"image_generation"),
            "owned tool row is replaced"
        );
        let image = merged.iter().find(|r| r.name == "imageGeneration").unwrap();
        assert!(image.enabled && image.active && image.config.is_some());
        let gmail = merged.iter().find(|r| r.name == "gmail").unwrap();
        assert!(!gmail.enabled && !gmail.active);
        assert!(
            merged
                .iter()
                .find(|r| r.name == "grep")
                .unwrap()
                .config
                .is_none()
        );
    }

    #[test]
    fn mcp_rows_compare_the_file_with_the_servers_started_enabled() {
        let configured = configured_mcp_servers(CONFIG).unwrap();
        let running = HashSet::from(["files".to_string()]);

        let rows = mcp_server_setting_rows(&configured, &running);

        let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["files", "legacy", "search"]);
        assert!(rows[0].enabled && rows[0].active);
        assert!(rows[1].enabled && !rows[1].active, "added after startup");
        assert!(!rows[2].enabled && !rows[2].active);
        assert_eq!(rows[0].description, "MCP server (stdio)");
    }

    #[test]
    fn apply_edit_saves_the_change_and_backs_up_the_previous_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, CONFIG).unwrap();

        let outcome = apply_edit(&path, |text| with_tool_disabled(text, "grep", true)).unwrap();

        assert!(outcome.changed);
        let backup = outcome.backup.expect("a backup");
        assert_eq!(std::fs::read_to_string(backup).unwrap(), CONFIG);
        let saved = std::fs::read_to_string(&path).unwrap();
        assert_eq!(disabled_tool_names(&saved).unwrap(), vec!["shell", "grep"]);
    }

    #[test]
    fn apply_edit_writes_nothing_when_the_toggle_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, CONFIG).unwrap();

        let outcome = apply_edit(&path, |text| with_tool_disabled(text, "shell", true)).unwrap();

        assert_eq!(
            outcome,
            ApplyOutcome {
                changed: false,
                backup: None
            }
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), CONFIG);
    }

    #[test]
    fn apply_edit_reports_a_refused_edit_and_leaves_the_file_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, CONFIG).unwrap();

        let result = apply_edit(&path, |text| with_mcp_server_enabled(text, "ghost", false));

        assert_eq!(result, Err(ApplyError::Toggle(ToggleError::UnknownServer)));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), CONFIG);
    }

    #[test]
    fn apply_edit_starts_from_the_default_config_when_the_file_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");

        let outcome = apply_edit(&path, |text| with_tool_disabled(text, "grep", true)).unwrap();

        assert!(outcome.changed);
        assert_eq!(outcome.backup, None);
        let saved = std::fs::read_to_string(&path).unwrap();
        assert_eq!(disabled_tool_names(&saved).unwrap(), vec!["grep"]);
    }

    const CONFIG: &str = r#"{
  "agents": {"mode": "standard"},
  "tools": {
    "disabledTools": ["shell"],
    "mcpServers": {
      "files": {"command": "npx", "env": {"TOKEN": "${FILES_TOKEN}"}},
      "search": {"url": "http://localhost:8080/mcp", "enabled": false},
      "legacy": {"type": "sse", "url": "http://x/sse"}
    }
  }
}
"#;

    #[test]
    fn names_with_path_or_control_characters_are_invalid() {
        assert!(is_valid_name("read_file"));
        assert!(is_valid_name("my-server.v2"));
        assert!(!is_valid_name(""));
        assert!(!is_valid_name("../etc"));
        assert!(!is_valid_name("a\nb"));
        assert!(!is_valid_name(&"x".repeat(MAX_NAME_LENGTH + 1)));
    }

    #[test]
    fn disabled_tool_names_reads_the_list_in_file_order() {
        assert_eq!(disabled_tool_names(CONFIG).unwrap(), vec!["shell"]);
        assert!(disabled_tool_names("{}").unwrap().is_empty());
    }

    #[test]
    fn disabled_tool_names_accepts_the_snake_case_key() {
        let text = r#"{"tools": {"disabled_tools": ["grep"]}}"#;
        assert_eq!(disabled_tool_names(text).unwrap(), vec!["grep"]);
    }

    #[test]
    fn configured_mcp_servers_reports_enabled_flag_and_transport() {
        let servers = configured_mcp_servers(CONFIG).unwrap();

        assert_eq!(
            servers,
            vec![
                ConfiguredMcpServer {
                    name: "files".into(),
                    enabled: true,
                    transport: "stdio".into()
                },
                ConfiguredMcpServer {
                    name: "search".into(),
                    enabled: false,
                    transport: "streamableHttp".into()
                },
                ConfiguredMcpServer {
                    name: "legacy".into(),
                    enabled: true,
                    transport: "sse".into()
                },
            ]
        );
    }

    #[test]
    fn disabling_a_tool_appends_it_and_leaves_the_rest_untouched() {
        let updated = with_tool_disabled(CONFIG, "grep", true).unwrap().unwrap();

        assert_eq!(
            disabled_tool_names(&updated).unwrap(),
            vec!["shell", "grep"]
        );
        assert!(updated.contains("${FILES_TOKEN}"), "placeholders survive");
        assert!(updated.ends_with('\n'), "trailing newline is kept");
        let before: Value = serde_json::from_str(CONFIG).unwrap();
        let mut after: Value = serde_json::from_str(&updated).unwrap();
        after["tools"]["disabledTools"] = before["tools"]["disabledTools"].clone();
        assert_eq!(before, after);
    }

    #[test]
    fn enabling_a_tool_removes_it_from_the_list() {
        let updated = with_tool_disabled(CONFIG, "shell", false).unwrap().unwrap();
        assert!(disabled_tool_names(&updated).unwrap().is_empty());
    }

    #[test]
    fn toggling_a_tool_to_its_current_state_changes_nothing() {
        assert_eq!(with_tool_disabled(CONFIG, "shell", true).unwrap(), None);
        assert_eq!(with_tool_disabled(CONFIG, "grep", false).unwrap(), None);
    }

    #[test]
    fn disabling_creates_the_tools_section_when_missing() {
        let updated = with_tool_disabled("{}", "grep", true).unwrap().unwrap();
        assert_eq!(disabled_tool_names(&updated).unwrap(), vec!["grep"]);
    }

    #[test]
    fn disabling_writes_to_the_snake_case_key_when_that_is_the_one_in_use() {
        let text = r#"{"tools": {"disabled_tools": []}}"#;
        let updated = with_tool_disabled(text, "grep", true).unwrap().unwrap();
        assert!(updated.contains("disabled_tools"));
        assert!(!updated.contains("disabledTools"));
    }

    #[test]
    fn a_protected_tool_cannot_be_disabled_but_can_be_enabled() {
        assert_eq!(
            with_tool_disabled(CONFIG, "message", true),
            Err(ToggleError::ProtectedTool)
        );
        assert_eq!(with_tool_disabled(CONFIG, "message", false).unwrap(), None);
    }

    #[test]
    fn tool_toggles_reject_invalid_names_and_broken_files() {
        assert_eq!(
            with_tool_disabled(CONFIG, "../x", true),
            Err(ToggleError::InvalidName)
        );
        assert!(matches!(
            with_tool_disabled("not json", "grep", true),
            Err(ToggleError::InvalidConfig(_))
        ));
        assert!(matches!(
            with_tool_disabled("[1]", "grep", true),
            Err(ToggleError::InvalidConfig(_))
        ));
        assert!(matches!(
            with_tool_disabled(r#"{"tools": {"disabledTools": "grep"}}"#, "x", true),
            Err(ToggleError::InvalidConfig(_))
        ));
    }

    #[test]
    fn switching_an_mcp_server_off_sets_its_flag() {
        let updated = with_mcp_server_enabled(CONFIG, "files", false)
            .unwrap()
            .unwrap();

        let servers = configured_mcp_servers(&updated).unwrap();
        assert!(!servers.iter().find(|s| s.name == "files").unwrap().enabled);
        assert!(updated.contains("${FILES_TOKEN}"));
    }

    #[test]
    fn switching_an_mcp_server_on_sets_its_flag() {
        let updated = with_mcp_server_enabled(CONFIG, "search", true)
            .unwrap()
            .unwrap();
        let servers = configured_mcp_servers(&updated).unwrap();
        assert!(servers.iter().find(|s| s.name == "search").unwrap().enabled);
    }

    #[test]
    fn an_absent_flag_counts_as_enabled_so_enabling_changes_nothing() {
        assert_eq!(
            with_mcp_server_enabled(CONFIG, "files", true).unwrap(),
            None
        );
        assert_eq!(
            with_mcp_server_enabled(CONFIG, "search", false).unwrap(),
            None
        );
    }

    #[test]
    fn an_unknown_mcp_server_is_refused_and_never_created() {
        assert_eq!(
            with_mcp_server_enabled(CONFIG, "ghost", false),
            Err(ToggleError::UnknownServer)
        );
        assert_eq!(
            with_mcp_server_enabled("{}", "ghost", false),
            Err(ToggleError::UnknownServer)
        );
    }

    #[test]
    fn toggled_text_still_passes_the_config_validation() {
        let updated = with_mcp_server_enabled(CONFIG, "files", false)
            .unwrap()
            .unwrap();
        assert!(super::super::config_file::validate_config_text(&updated).is_ok());
    }
}
