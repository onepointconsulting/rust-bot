//! The settings form of one configured MCP server (`tools.mcpServers.<name>`) for the
//! WebUI's Settings dialog: transport, command, arguments, environment, URL, headers,
//! tool timeout, enabled tools, exposure and description.
//!
//! The form is generated from the JSON Schema of [`McpServerConfig`] with the same
//! helpers as the tool section forms ([`crate::agent::tools::tool_settings_api`]), so the
//! browser renders it with the same component. Values are read from and written to the
//! raw JSON text, like [`super::config_toggles`]: `${ENV_VAR}` placeholders are shown and
//! kept exactly as written, and nothing outside the edited server entry changes.
//! Everything here is a pure function of strings (the process environment is passed in
//! where a check needs it); saving is [`super::config_toggles::apply_edit`]'s job.
//!
//! A `${VAR}` placeholder in a submitted value must name a variable that is set on this
//! machine: an unset one is accepted by the file format but stops the bot from starting.

use std::collections::{HashMap, HashSet};

use schemars::schema_for;
use serde_json::{Map, Value};

use super::config_toggles::{
    MCP_SERVERS_KEYS, McpServerSettingRow, ToggleError, configured_mcp_servers,
    existing_or_default_key, is_valid_name, mcp_server_object_mut, mcp_server_setting_rows,
    parse_root, render, tools_object_mut,
};
use crate::agent::tools::tool_settings_api::{
    ToolSettingsSummary, check_value, field_kind, summarize_field,
};
use crate::config::loader::referenced_env_vars;
use crate::config::schema::McpServerConfig;

/// Setting names (as the form calls them) with every spelling the configuration accepts,
/// the form's own name first. Settings not listed have a single spelling.
const SETTING_SPELLINGS: &[&[&str]] = &[
    &["transportType", "type"],
    &["toolTimeout", "tool_timeout"],
    &["enabledTools", "enabled_tools"],
];

const TRANSPORT_TYPE: &str = "transportType";
const EXPOSURE: &str = "exposure";
const COMMAND: &str = "command";
const URL: &str = "url";
const ENABLED: &str = "enabled";

/// The `exposure` value that is the default, so it is stored by removing the key.
const MCP_EXPOSURE_DIRECT: &str = "direct";

/// Every spelling of the setting `name`, the form's own name first.
fn spellings(name: &str) -> Vec<String> {
    match SETTING_SPELLINGS
        .iter()
        .find(|spellings| spellings[0] == name)
    {
        Some(spellings) => spellings.iter().map(|s| s.to_string()).collect(),
        None => vec![name.to_string()],
    }
}

/// The schema properties the form edits, in declaration order (`enabled` is the row
/// switch and is not part of the schema).
fn form_properties() -> Map<String, Value> {
    let schema = serde_json::to_value(schema_for!(McpServerConfig)).unwrap_or(Value::Null);
    schema["properties"]
        .as_object()
        .cloned()
        .unwrap_or_default()
}

/// The value the server entry stores for the setting `name` under any of its spellings.
fn stored_value<'a>(server: &'a Map<String, Value>, name: &str) -> Option<&'a Value> {
    spellings(name).iter().find_map(|key| server.get(key))
}

/// The settings form of the MCP server `name` in the config `text`, showing what the
/// file says (or the default for a setting it leaves out). Like a tool section's form,
/// `enabled` is the state of the row's switch (the server's `enabled` flag, `true` when
/// absent); the flag is switched by its own request, not through the form.
pub fn mcp_server_form(text: &str, name: &str) -> Result<ToolSettingsSummary, ToggleError> {
    if !is_valid_name(name) {
        return Err(ToggleError::InvalidName);
    }
    let mut root = parse_root(text)?;
    let server = mcp_server_object_mut(&mut root, name)?;
    let enabled = server.get(ENABLED).and_then(Value::as_bool).unwrap_or(true);
    Ok(ToolSettingsSummary {
        name: name.to_string(),
        title: name.to_string(),
        description: String::new(),
        enabled: Some(enabled),
        tool_names: Vec::new(),
        fields: form_fields(Some(server)),
    })
}

/// The blank form for a server that does not exist yet: every setting at its default.
/// It has no name (the browser asks for one) and no switch.
pub fn new_server_form() -> ToolSettingsSummary {
    ToolSettingsSummary {
        name: String::new(),
        title: String::new(),
        description: String::new(),
        enabled: None,
        tool_names: Vec::new(),
        fields: form_fields(None),
    }
}

/// One form field per setting: what `server` stores for it, else the default.
fn form_fields(
    server: Option<&Map<String, Value>>,
) -> Vec<crate::agent::tools::tool_settings_api::ToolFieldSummary> {
    form_properties()
        .iter()
        .filter_map(|(setting, property)| {
            let shown = server
                .and_then(|server| stored_value(server, setting))
                .or_else(|| property.get("default"))
                .cloned()
                .unwrap_or(Value::Null);
            summarize_field(setting, property, &shown)
        })
        .collect()
}

/// The MCP server rows of the config `text` (see [`mcp_server_setting_rows`]), each
/// with its settings form when `with_forms`. Forms show commands, URLs and credentials,
/// so only connections that may change the configuration get them.
pub fn mcp_server_rows(
    text: &str,
    running: &HashSet<String>,
    with_forms: bool,
) -> Result<Vec<McpServerSettingRow>, ToggleError> {
    let configured = configured_mcp_servers(text)?;
    Ok(mcp_server_setting_rows(&configured, running, |name| {
        with_forms
            .then(|| mcp_server_form(text, name).ok())
            .flatten()
    }))
}

/// Whether storing `value` for `setting` means removing the key: a blank transport is
/// detected from the command or URL, and `direct` is the default exposure.
fn is_stored_as_absent(setting: &str, value: &Value) -> bool {
    match setting {
        TRANSPORT_TYPE => value.as_str() == Some(""),
        EXPOSURE => value.as_str() == Some(MCP_EXPOSURE_DIRECT),
        _ => false,
    }
}

/// Write `value` for `setting` into the server entry, under the spelling the file already
/// uses (the form's own name otherwise), or remove it when the value means "default".
fn store_setting(server: &mut Map<String, Value>, setting: &str, value: Value) {
    let spellings = spellings(setting);
    if is_stored_as_absent(setting, &value) {
        for key in &spellings {
            server.remove(key);
        }
        return;
    }
    let key = spellings
        .iter()
        .find(|key| server.contains_key(key.as_str()))
        .unwrap_or(&spellings[0])
        .clone();
    server.insert(key, value);
}

/// The text the server entry stores for `setting`, empty when absent or not text.
fn stored_text<'a>(server: &'a Map<String, Value>, setting: &str) -> &'a str {
    stored_value(server, setting)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
}

/// Refuse an entry the bot could not connect with: a stdio server needs a command, an
/// HTTP one a URL, and an entry without a transport needs one of the two to detect it.
fn check_connection_target(server: &Map<String, Value>) -> Result<(), ToggleError> {
    let has_command = !stored_text(server, COMMAND).is_empty();
    let has_url = !stored_text(server, URL).is_empty();
    let missing = match stored_text(server, TRANSPORT_TYPE) {
        "stdio" if !has_command => Some((COMMAND, "a stdio server needs a command")),
        "sse" | "streamableHttp" if !has_url => Some((URL, "an HTTP server needs a URL")),
        "" if !has_command && !has_url => Some((COMMAND, "set a command or a URL")),
        _ => None,
    };
    match missing {
        Some((setting, reason)) => Err(ToggleError::InvalidValue(
            setting.to_string(),
            reason.to_string(),
        )),
        None => Ok(()),
    }
}

/// Whether the environment variable `name` is set on this machine.
fn process_env_is_set(name: &str) -> bool {
    std::env::var_os(name).is_some()
}

/// Why an unset `${VAR}` is refused, and what to do instead. Shown after the variable's
/// name. People often expect the server's Env field to define variables for `${NAME}`;
/// it does not (it only feeds the launched command), so the message says so.
const UNSET_VARIABLE_EXPLANATION: &str = "which is not set in the environment of the process running rust-bot. ${NAME} is read from that environment, not from this server's Env field: set the variable there before starting rust-bot, or write the value here directly.";

/// Refuse a submitted value naming a `${VAR}` placeholder whose variable is not set.
/// The message names the variable, never the value.
fn check_placeholders(
    setting: &str,
    value: &Value,
    env_is_set: &dyn Fn(&str) -> bool,
) -> Result<(), ToggleError> {
    match referenced_env_vars(value)
        .into_iter()
        .find(|variable| !env_is_set(variable))
    {
        Some(variable) => Err(ToggleError::InvalidValue(
            setting.to_string(),
            format!("uses ${{{variable}}}, {UNSET_VARIABLE_EXPLANATION}"),
        )),
        None => Ok(()),
    }
}

/// Check every submitted `changes` entry against the schema and the environment, write
/// them into `server`, then check the result can connect. `server` may be left half
/// edited on an error: callers work on a parsed copy of the config and drop it.
fn apply_settings(
    server: &mut Map<String, Value>,
    changes: &HashMap<String, Value>,
    env_is_set: &dyn Fn(&str) -> bool,
) -> Result<(), ToggleError> {
    let properties = form_properties();
    for (setting, value) in changes {
        let kind = properties
            .get(setting)
            .and_then(field_kind)
            .ok_or_else(|| ToggleError::UnknownField(setting.clone()))?;
        let checked = check_value(&kind, value)
            .map_err(|reason| ToggleError::InvalidValue(setting.clone(), reason))?;
        check_placeholders(setting, &checked, env_is_set)?;
        store_setting(server, setting, checked);
    }
    check_connection_target(server)
}

/// `text` with the submitted `changes` (setting name -> value, as the form names them)
/// applied to `tools.mcpServers.<name>`. Every change is checked against the schema
/// first; nothing is applied unless all of them are valid. `None` when the entry already
/// says all of it, so the caller can skip the write. The server must already exist.
pub fn with_mcp_server_settings(
    text: &str,
    name: &str,
    changes: &HashMap<String, Value>,
) -> Result<Option<String>, ToggleError> {
    with_mcp_server_settings_in(text, name, changes, &process_env_is_set)
}

/// [`with_mcp_server_settings`] with the environment lookup passed in.
fn with_mcp_server_settings_in(
    text: &str,
    name: &str,
    changes: &HashMap<String, Value>,
    env_is_set: &dyn Fn(&str) -> bool,
) -> Result<Option<String>, ToggleError> {
    if !is_valid_name(name) {
        return Err(ToggleError::InvalidName);
    }
    let mut root = parse_root(text)?;
    let server = mcp_server_object_mut(&mut root, name)?;
    let before = server.clone();
    apply_settings(server, changes, env_is_set)?;
    if *server == before {
        return Ok(None);
    }
    Ok(Some(render(&root, text)))
}

/// `text` with a new server `name` added to `tools.mcpServers` (created when missing,
/// under the key spelling the file uses) holding the submitted `values`, checked like an
/// edit. It starts enabled, as an entry without an `enabled` flag is. Refuses a name that
/// is already configured: adding never overwrites.
pub fn with_mcp_server_added(
    text: &str,
    name: &str,
    values: &HashMap<String, Value>,
) -> Result<String, ToggleError> {
    with_mcp_server_added_in(text, name, values, &process_env_is_set)
}

/// [`with_mcp_server_added`] with the environment lookup passed in.
fn with_mcp_server_added_in(
    text: &str,
    name: &str,
    values: &HashMap<String, Value>,
    env_is_set: &dyn Fn(&str) -> bool,
) -> Result<String, ToggleError> {
    if !is_valid_name(name) {
        return Err(ToggleError::InvalidName);
    }
    let mut root = parse_root(text)?;
    let tools = tools_object_mut(&mut root)?;
    let key = existing_or_default_key(tools, MCP_SERVERS_KEYS).to_string();
    let servers = tools
        .entry(key)
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| {
            ToggleError::InvalidConfig("\"mcpServers\" is not an object.".to_string())
        })?;
    if servers.contains_key(name) {
        return Err(ToggleError::ServerExists);
    }
    let mut server = Map::new();
    apply_settings(&mut server, values, env_is_set)?;
    servers.insert(name.to_string(), Value::Object(server));
    Ok(render(&root, text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tools::tool_settings_api::{FieldKind, ToolFieldSummary};
    use serde_json::json;

    const CONFIG: &str = r#"{
  "agents": {"mode": "standard"},
  "tools": {
    "mcpServers": {
      "files": {
        "command": "npx",
        "args": ["-y", "files-mcp"],
        "env": {"TOKEN": "${FILES_TOKEN}"},
        "tool_timeout": 45
      },
      "cms": {
        "type": "streamableHttp",
        "url": "https://cms/mcp",
        "headers": {"Authorization": "Bearer ${CMS_TOKEN}"},
        "exposure": "deferred",
        "description": "Strapi CMS"
      }
    }
  }
}
"#;

    /// The edit with every `${VAR}` treated as set, so tests do not depend on the real
    /// environment. Shadows the public function this module imports.
    fn with_mcp_server_settings(
        text: &str,
        name: &str,
        changes: &HashMap<String, Value>,
    ) -> Result<Option<String>, ToggleError> {
        with_mcp_server_settings_in(text, name, changes, &|_| true)
    }

    /// The add with every `${VAR}` treated as set.
    fn with_mcp_server_added(
        text: &str,
        name: &str,
        values: &HashMap<String, Value>,
    ) -> Result<String, ToggleError> {
        with_mcp_server_added_in(text, name, values, &|_| true)
    }

    fn field<'a>(form: &'a ToolSettingsSummary, name: &str) -> &'a ToolFieldSummary {
        form.fields
            .iter()
            .find(|field| field.name == name)
            .unwrap_or_else(|| panic!("no field {name}"))
    }

    fn change(pairs: &[(&str, Value)]) -> HashMap<String, Value> {
        pairs
            .iter()
            .map(|(name, value)| (name.to_string(), value.clone()))
            .collect()
    }

    fn server(text: &str, name: &str) -> Value {
        let root: Value = serde_json::from_str(text).unwrap();
        root["tools"]["mcpServers"][name].clone()
    }

    #[test]
    fn the_form_lists_every_server_setting_in_declaration_order() {
        let form = mcp_server_form(CONFIG, "files").unwrap();

        let names: Vec<&str> = form.fields.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "transportType",
                "command",
                "args",
                "env",
                "url",
                "headers",
                "toolTimeout",
                "enabledTools",
                "exposure",
                "description",
            ]
        );
        assert_eq!(form.name, "files");
        assert_eq!(form.enabled, Some(true), "the row switch: absent means on");
        let mut switched_off: Value = serde_json::from_str(CONFIG).unwrap();
        switched_off["tools"]["mcpServers"]["files"]["enabled"] = json!(false);
        let off_form = mcp_server_form(&switched_off.to_string(), "files").unwrap();
        assert_eq!(off_form.enabled, Some(false));
    }

    #[test]
    fn the_form_maps_each_setting_to_its_input_kind() {
        let form = mcp_server_form(CONFIG, "files").unwrap();

        assert!(matches!(
            field(&form, "transportType").kind,
            FieldKind::Choice { .. }
        ));
        assert_eq!(field(&form, "command").kind, FieldKind::Text);
        assert_eq!(field(&form, "args").kind, FieldKind::StringList);
        assert_eq!(field(&form, "env").kind, FieldKind::StringMap);
        assert_eq!(field(&form, "headers").kind, FieldKind::StringMap);
        assert_eq!(
            field(&form, "toolTimeout").kind,
            FieldKind::Integer {
                min: Some(1),
                max: None
            }
        );
        assert_eq!(field(&form, "enabledTools").kind, FieldKind::StringList);
        assert_eq!(
            field(&form, "exposure").kind,
            FieldKind::Choice {
                choices: vec!["direct".into(), "deferred".into()]
            }
        );
    }

    #[test]
    fn the_env_and_headers_help_explain_where_placeholders_come_from() {
        let form = new_server_form();

        let env_help = &field(&form, "env").help;
        assert!(env_help.contains("Stdio only") && env_help.contains("cannot refer to them"));
        let headers_help = &field(&form, "headers").help;
        assert!(headers_help.contains("not by an entry of the Env field"));
    }

    #[test]
    fn the_form_shows_values_as_written_including_placeholders_and_aliases() {
        let form = mcp_server_form(CONFIG, "files").unwrap();

        assert_eq!(field(&form, "command").value, Some(json!("npx")));
        assert_eq!(field(&form, "args").value, Some(json!(["-y", "files-mcp"])));
        assert_eq!(
            field(&form, "env").value,
            Some(json!({"TOKEN": "${FILES_TOKEN}"}))
        );
        assert_eq!(field(&form, "toolTimeout").value, Some(json!(45)));

        let cms = mcp_server_form(CONFIG, "cms").unwrap();
        assert_eq!(
            field(&cms, "transportType").value,
            Some(json!("streamableHttp"))
        );
        assert_eq!(
            field(&cms, "headers").value,
            Some(json!({"Authorization": "Bearer ${CMS_TOKEN}"}))
        );
        assert_eq!(field(&cms, "exposure").value, Some(json!("deferred")));
    }

    #[test]
    fn the_form_shows_defaults_for_settings_the_file_leaves_out() {
        let form = mcp_server_form(CONFIG, "files").unwrap();

        assert_eq!(field(&form, "enabledTools").value, Some(json!(["*"])));
        assert_eq!(field(&form, "exposure").value, Some(json!("direct")));
        assert_eq!(field(&form, "url").value, Some(json!("")));
        assert_eq!(field(&form, "transportType").value, Some(Value::Null));
    }

    #[test]
    fn the_form_refuses_unknown_servers_and_invalid_names() {
        assert_eq!(
            mcp_server_form(CONFIG, "ghost"),
            Err(ToggleError::UnknownServer)
        );
        assert_eq!(
            mcp_server_form(CONFIG, "../x"),
            Err(ToggleError::InvalidName)
        );
    }

    #[test]
    fn rows_carry_forms_only_when_asked_to() {
        let running = HashSet::from(["files".to_string()]);

        let with_forms = mcp_server_rows(CONFIG, &running, true).unwrap();
        let without_forms = mcp_server_rows(CONFIG, &running, false).unwrap();

        let names: Vec<&str> = with_forms.iter().map(|row| row.name.as_str()).collect();
        assert_eq!(names, vec!["cms", "files"]);
        assert!(with_forms.iter().all(|row| row.config.is_some()));
        assert!(without_forms.iter().all(|row| row.config.is_none()));
        assert!(with_forms[1].active && !with_forms[0].active);
    }

    #[test]
    fn every_kind_of_setting_can_be_changed() {
        let changes = change(&[
            ("transportType", json!("stdio")),
            ("command", json!("  uvx  ")),
            ("args", json!(["server", "--verbose"])),
            ("env", json!({"TOKEN": "${NEW_TOKEN}", "MODE": "ro"})),
            ("toolTimeout", json!(90)),
            ("enabledTools", json!(["search", "read"])),
            ("exposure", json!("deferred")),
            ("description", json!("Files: read and search")),
        ]);

        let updated = with_mcp_server_settings(CONFIG, "files", &changes)
            .unwrap()
            .unwrap();

        let files = server(&updated, "files");
        assert_eq!(files["transportType"], "stdio");
        assert_eq!(files["command"], "uvx", "text is trimmed");
        assert_eq!(files["args"], json!(["server", "--verbose"]));
        assert_eq!(files["env"], json!({"TOKEN": "${NEW_TOKEN}", "MODE": "ro"}));
        assert_eq!(files["tool_timeout"], 90, "the spelling in use is kept");
        assert!(files.get("toolTimeout").is_none());
        assert_eq!(files["enabledTools"], json!(["search", "read"]));
        assert_eq!(files["exposure"], "deferred");
        assert_eq!(files["description"], "Files: read and search");
    }

    #[test]
    fn http_settings_can_be_changed_and_the_type_alias_is_kept() {
        let changes = change(&[
            ("transportType", json!("sse")),
            ("url", json!("https://cms/sse")),
            ("headers", json!({"X-Api-Key": "${CMS_KEY}"})),
        ]);

        let updated = with_mcp_server_settings(CONFIG, "cms", &changes)
            .unwrap()
            .unwrap();

        let cms = server(&updated, "cms");
        assert_eq!(cms["type"], "sse");
        assert!(cms.get("transportType").is_none());
        assert_eq!(cms["url"], "https://cms/sse");
        assert_eq!(cms["headers"], json!({"X-Api-Key": "${CMS_KEY}"}));
    }

    #[test]
    fn a_blank_transport_and_direct_exposure_remove_their_keys() {
        let changes = change(&[("transportType", json!("")), ("exposure", json!("direct"))]);

        let updated = with_mcp_server_settings(CONFIG, "cms", &changes)
            .unwrap()
            .unwrap();

        let cms = server(&updated, "cms");
        assert!(cms.get("type").is_none() && cms.get("transportType").is_none());
        assert!(cms.get("exposure").is_none());
        assert_eq!(cms["url"], "https://cms/mcp");
    }

    #[test]
    fn the_rest_of_the_file_is_left_exactly_as_it_was() {
        let changes = change(&[("description", json!("Files"))]);

        let updated = with_mcp_server_settings(CONFIG, "files", &changes)
            .unwrap()
            .unwrap();

        assert!(updated.contains("${FILES_TOKEN}") && updated.contains("${CMS_TOKEN}"));
        assert!(updated.ends_with('\n'));
        let mut after: Value = serde_json::from_str(&updated).unwrap();
        let before: Value = serde_json::from_str(CONFIG).unwrap();
        after["tools"]["mcpServers"]["files"]
            .as_object_mut()
            .unwrap()
            .remove("description");
        assert_eq!(before, after);
    }

    #[test]
    fn submitting_the_stored_values_changes_nothing() {
        let changes = change(&[
            ("command", json!("npx")),
            ("toolTimeout", json!(45)),
            ("exposure", json!("direct")),
        ]);

        assert_eq!(
            with_mcp_server_settings(CONFIG, "files", &changes).unwrap(),
            None
        );
    }

    #[test]
    fn unknown_settings_and_bad_values_are_refused() {
        let refuse = |pairs: &[(&str, Value)]| {
            with_mcp_server_settings(CONFIG, "files", &change(pairs)).unwrap_err()
        };

        assert_eq!(
            refuse(&[("enabled", json!(false))]),
            ToggleError::UnknownField("enabled".into())
        );
        assert_eq!(
            refuse(&[("cwd", json!("/tmp"))]),
            ToggleError::UnknownField("cwd".into())
        );
        for (setting, value) in [
            ("toolTimeout", json!(0)),
            ("toolTimeout", json!("ten")),
            ("transportType", json!("websocket")),
            ("exposure", json!("lazy")),
            ("args", json!("-y")),
            ("env", json!({"TOKEN": 1})),
            ("headers", json!({" ": "x"})),
        ] {
            assert!(
                matches!(refuse(&[(setting, value)]), ToggleError::InvalidValue(ref s, _) if s == setting),
                "{setting}"
            );
        }
    }

    #[test]
    fn a_server_the_bot_could_not_connect_to_is_refused() {
        let no_command = change(&[("command", json!(""))]);
        let stdio_without_command = change(&[("transportType", json!("stdio"))]);
        let http_without_url = change(&[("url", json!(""))]);

        assert!(matches!(
            with_mcp_server_settings(CONFIG, "files", &no_command),
            Err(ToggleError::InvalidValue(ref s, _)) if s == "command"
        ));
        assert!(matches!(
            with_mcp_server_settings(CONFIG, "cms", &stdio_without_command),
            Err(ToggleError::InvalidValue(ref s, _)) if s == "command"
        ));
        assert!(matches!(
            with_mcp_server_settings(CONFIG, "cms", &http_without_url),
            Err(ToggleError::InvalidValue(ref s, _)) if s == "url"
        ));
    }

    #[test]
    fn changes_to_unknown_servers_and_invalid_names_are_refused() {
        let changes = change(&[("description", json!("x"))]);
        assert_eq!(
            with_mcp_server_settings(CONFIG, "ghost", &changes),
            Err(ToggleError::UnknownServer)
        );
        assert_eq!(
            with_mcp_server_settings(CONFIG, "a/b", &changes),
            Err(ToggleError::InvalidName)
        );
    }

    #[test]
    fn edited_text_still_passes_the_config_validation() {
        let changes = change(&[
            ("transportType", json!("stdio")),
            ("env", json!({"A": "1"})),
            ("toolTimeout", json!(5)),
        ]);

        let updated = with_mcp_server_settings(CONFIG, "files", &changes)
            .unwrap()
            .unwrap();

        assert!(super::super::config_file::validate_config_text(&updated).is_ok());
    }

    #[test]
    fn the_blank_form_lists_every_setting_at_its_default() {
        let form = new_server_form();

        let names: Vec<&str> = form.fields.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names.len(), 10);
        assert!(names.contains(&"command") && names.contains(&"headers"));
        assert!(form.name.is_empty() && form.enabled.is_none());
        assert_eq!(field(&form, "toolTimeout").value, Some(json!(30)));
        assert_eq!(field(&form, "enabledTools").value, Some(json!(["*"])));
        assert_eq!(field(&form, "exposure").value, Some(json!("direct")));
        assert_eq!(field(&form, "command").value, Some(json!("")));
    }

    #[test]
    fn adding_a_server_appends_an_entry_and_leaves_the_rest_alone() {
        let values = change(&[
            ("command", json!("uvx")),
            ("args", json!(["server"])),
            ("env", json!({"TOKEN": "${NEW_TOKEN}"})),
            ("description", json!("New one")),
        ]);

        let updated = with_mcp_server_added(CONFIG, "fresh", &values).unwrap();

        let added = server(&updated, "fresh");
        assert_eq!(added["command"], "uvx");
        assert_eq!(added["env"], json!({"TOKEN": "${NEW_TOKEN}"}));
        assert!(added.get("enabled").is_none(), "starts enabled by default");
        assert_eq!(server(&updated, "files"), server(CONFIG, "files"));
        assert!(updated.contains("${FILES_TOKEN}") && updated.ends_with('\n'));
        let names: Vec<String> =
            serde_json::from_str::<Value>(&updated).unwrap()["tools"]["mcpServers"]
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect();
        assert_eq!(names, vec!["files", "cms", "fresh"], "appended last");
        assert!(super::super::config_file::validate_config_text(&updated).is_ok());
    }

    #[test]
    fn adding_creates_the_missing_sections_and_follows_the_key_spelling_in_use() {
        let values = change(&[("url", json!("https://x/mcp"))]);

        let from_empty = with_mcp_server_added("{}", "first", &values).unwrap();
        assert_eq!(server(&from_empty, "first")["url"], "https://x/mcp");

        let snake = r#"{"tools": {"mcp_servers": {}}}"#;
        let updated = with_mcp_server_added(snake, "first", &values).unwrap();
        assert!(updated.contains("mcp_servers") && !updated.contains("mcpServers"));
    }

    #[test]
    fn adding_never_overwrites_and_checks_the_name() {
        let values = change(&[("command", json!("x"))]);

        assert_eq!(
            with_mcp_server_added(CONFIG, "files", &values),
            Err(ToggleError::ServerExists)
        );
        assert_eq!(
            with_mcp_server_added(CONFIG, "a/b", &values),
            Err(ToggleError::InvalidName)
        );
        assert_eq!(
            with_mcp_server_added(CONFIG, "", &values),
            Err(ToggleError::InvalidName)
        );
    }

    #[test]
    fn a_new_server_needs_a_command_or_a_url_and_valid_values() {
        let refuse = |pairs: &[(&str, Value)]| {
            with_mcp_server_added(CONFIG, "fresh", &change(pairs)).unwrap_err()
        };

        assert!(matches!(
            refuse(&[("description", json!("nothing to connect to"))]),
            ToggleError::InvalidValue(ref s, _) if s == "command"
        ));
        assert!(matches!(
            refuse(&[("transportType", json!("sse"))]),
            ToggleError::InvalidValue(ref s, _) if s == "url"
        ));
        assert!(matches!(
            refuse(&[("command", json!("x")), ("toolTimeout", json!(0))]),
            ToggleError::InvalidValue(ref s, _) if s == "toolTimeout"
        ));
        assert_eq!(
            refuse(&[("command", json!("x")), ("cwd", json!("/"))]),
            ToggleError::UnknownField("cwd".into())
        );
    }

    #[test]
    fn a_placeholder_for_an_unset_variable_is_refused_naming_it_but_not_the_value() {
        let env_without_token = |name: &str| name != "MISSING_TOKEN";
        let values = change(&[
            ("url", json!("https://x/mcp")),
            (
                "headers",
                json!({"Authorization": "Bearer ${MISSING_TOKEN}"}),
            ),
        ]);

        let refused = with_mcp_server_added_in(CONFIG, "fresh", &values, &env_without_token);

        let Err(ToggleError::InvalidValue(setting, reason)) = refused else {
            panic!("expected a refusal");
        };
        assert_eq!(setting, "headers");
        assert!(reason.contains("${MISSING_TOKEN}") && !reason.contains("Bearer"));
        assert!(reason.contains("Env field") && reason.contains("environment"));
    }

    #[test]
    fn a_set_placeholder_is_accepted_and_kept_as_written() {
        let values = change(&[
            ("env", json!({"A": "${PRESENT}:${ALSO_PRESENT}"})),
            ("command", json!("x")),
        ]);

        let updated = with_mcp_server_added_in(CONFIG, "fresh", &values, &|_| true).unwrap();

        assert_eq!(
            server(&updated, "fresh")["env"]["A"],
            "${PRESENT}:${ALSO_PRESENT}"
        );
    }

    #[test]
    fn editing_checks_only_the_submitted_values_for_unset_variables() {
        let none_set = |_: &str| false;

        // The stored `${FILES_TOKEN}` is not re-checked when something else changes.
        let untouched = change(&[("description", json!("still fine"))]);
        assert!(with_mcp_server_settings_in(CONFIG, "files", &untouched, &none_set).is_ok());

        let submitted = change(&[("args", json!(["--token", "${NOT_SET_HERE}"]))]);
        assert!(matches!(
            with_mcp_server_settings_in(CONFIG, "files", &submitted, &none_set),
            Err(ToggleError::InvalidValue(ref s, _)) if s == "args"
        ));
    }

    #[test]
    fn the_public_edit_and_add_read_the_real_environment() {
        let values = change(&[
            ("command", json!("x")),
            (
                "env",
                json!({"A": "${RUST_BOT_SURELY_UNSET_VARIABLE_4711}"}),
            ),
        ]);

        assert!(matches!(
            super::with_mcp_server_added(CONFIG, "fresh", &values),
            Err(ToggleError::InvalidValue(..))
        ));
        assert!(matches!(
            super::with_mcp_server_settings(CONFIG, "files", &values),
            Err(ToggleError::InvalidValue(..))
        ));
    }
}
