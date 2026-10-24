//! Settings forms for the built-in tools, generated from the JSON Schema of their
//! configuration structs (`config::schema`).
//!
//! A tool config struct opts in by deriving `JsonSchema` and carrying the
//! `x-editable` extension (plus `x-tools`, the registered tool names it owns), then
//! being listed in [`editable_sections`]. Field level extensions refine the form: `x-secret` (value is never sent out), `x-env-var`
//! (an exported variable satisfies the field) and `x-choices` (a fixed set of values).
//!
//! [`tool_settings_overview`] flattens the schema plus the current values into plain
//! summaries for the browser; [`apply_tool_settings`] validates a submitted change
//! against the same schema and writes it into a [`Config`]. Both are pure, so the
//! gateway only has to load and save the configuration around them.
//!
//! Every change only takes effect after the bot restarts. Collection and nested
//! fields (objects, maps) are not supported and are skipped; they stay in the raw
//! configuration editor.

use std::collections::HashMap;

use garde::Validate;
use schemars::{JsonSchema, schema_for};
use serde::Serialize;
use serde_json::{Map, Value};

use crate::config::schema::{
    AcpConfig, Config, DocxToolConfig, ExecToolConfig, GmailToolConfig, ImageGenerationToolConfig,
    OcrToolConfig, QuestionToolConfig, ToolsConfig, WebToolsConfig,
};

/// Names of the toggle property of a tool section; shown as the switch, not a field.
const ENABLE_PROPERTY_NAMES: [&str; 2] = ["enable", "enabled"];

/// Key of the section made of the settings that sit directly under `tools` (not inside a
/// tool's own object), such as `restrictToWorkspace`.
const GENERAL_SECTION_KEY: &str = "general";

/// An editable tool section: its key inside `tools` and its JSON Schema.
struct Section {
    key: &'static str,
    schema: Value,
}

/// Build the section for the config struct `T` stored under `tools.<key>`.
fn section_of<T: JsonSchema>(key: &'static str) -> Section {
    Section {
        key,
        schema: serde_json::to_value(schema_for!(T)).unwrap_or(Value::Null),
    }
}

/// Every tool section the UI may edit, in display order. Add a line here (and derive
/// `JsonSchema` + `x-editable` on the struct) to expose another tool.
fn editable_sections() -> Vec<Section> {
    vec![
        section_of::<ToolsConfig>(GENERAL_SECTION_KEY),
        section_of::<GmailToolConfig>("gmail"),
        section_of::<ImageGenerationToolConfig>("imageGeneration"),
        section_of::<OcrToolConfig>("ocr"),
        section_of::<DocxToolConfig>("docx"),
        section_of::<QuestionToolConfig>("question"),
        section_of::<ExecToolConfig>("exec"),
        section_of::<WebToolsConfig>("web"),
        section_of::<AcpConfig>("acp"),
    ]
    .into_iter()
    .filter(|section| section.schema["x-editable"] == true)
    .collect()
}

/// The kind of input a field needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FieldKind {
    Bool,
    Integer { min: Option<i64>, max: Option<i64> },
    Text,
    Choice { choices: Vec<String> },
    StringList,
}

/// One editable setting of a tool, ready to render.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ToolFieldSummary {
    /// Property name inside the tool section, e.g. `apiKey`.
    pub name: String,
    pub label: String,
    pub help: String,
    #[serde(flatten)]
    pub kind: FieldKind,
    pub secret: bool,
    /// Environment variable that satisfies the field, empty when there is none.
    pub env_var: String,
    /// Whether the field already has a usable value (stored, or via its env var).
    pub configured: bool,
    /// Current value; always `None` for secrets.
    pub value: Option<Value>,
    pub default: Option<Value>,
}

/// One tool: its switch state and its settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ToolSettingsSummary {
    /// Key inside `tools`, e.g. `imageGeneration`.
    pub name: String,
    pub title: String,
    pub description: String,
    pub enabled: Option<bool>,
    /// Registered tool names this section switches on and off as a group.
    pub tool_names: Vec<String>,
    pub fields: Vec<ToolFieldSummary>,
}

/// Why a submitted change was refused.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolSettingsError {
    UnknownTool(String),
    UnknownField(String, String),
    InvalidValue(String, String),
    /// The result failed the configuration's own validation rules.
    Invalid(String),
}

impl std::fmt::Display for ToolSettingsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownTool(tool) => write!(f, "Unknown tool '{tool}'."),
            Self::UnknownField(tool, field) => write!(f, "'{field}' is not a setting of '{tool}'."),
            Self::InvalidValue(field, reason) => write!(f, "Invalid value for '{field}': {reason}"),
            Self::Invalid(reason) => write!(f, "Invalid configuration: {reason}"),
        }
    }
}

impl std::error::Error for ToolSettingsError {}

/// Turn a camelCase property name into a label: `clientSecretPath` -> `Client secret path`.
pub fn label_from_property_name(name: &str) -> String {
    let mut label = String::new();
    for character in name.chars() {
        if character.is_uppercase() && !label.is_empty() {
            label.push(' ');
            label.extend(character.to_lowercase());
        } else if label.is_empty() {
            label.extend(character.to_uppercase());
        } else {
            label.push(character);
        }
    }
    label
}

/// The doc comment of a field without the trailing `Default: ...` sentence, which the
/// form shows separately.
pub fn help_text(description: &str) -> String {
    let without_default = match description.rfind("Default:") {
        Some(position) => &description[..position],
        None => description,
    };
    without_default
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// The JSON type of a property, ignoring `null` in a nullable union such as
/// `["string", "null"]`.
fn base_type(property: &Value) -> Option<&str> {
    match &property["type"] {
        Value::String(name) => Some(name),
        Value::Array(names) => names
            .iter()
            .filter_map(Value::as_str)
            .find(|n| *n != "null"),
        _ => None,
    }
}

/// Whether the property may be `null` (an `Option` in the config struct).
fn is_nullable(property: &Value) -> bool {
    property["type"]
        .as_array()
        .is_some_and(|names| names.iter().any(|name| name == "null"))
}

/// Every editable-looking property of a section schema, with nested (inlined) structs
/// flattened into dotted paths such as `search.provider`.
fn flatten_properties(schema: &Value, prefix: &str) -> Vec<(String, Value)> {
    let mut flattened = Vec::new();
    for (name, property) in schema["properties"].as_object().into_iter().flatten() {
        let path = format!("{prefix}{name}");
        if property["properties"].is_object() {
            flattened.extend(flatten_properties(property, &format!("{path}.")));
        } else {
            flattened.push((path, property.clone()));
        }
    }
    flattened
}

/// The JSON of the section `key` inside the serialized `tools` config: the root object
/// for the general section, the `tools.<key>` object otherwise.
fn section_value<'a>(tools_json: &'a Value, key: &str) -> &'a Value {
    if key == GENERAL_SECTION_KEY {
        tools_json
    } else {
        &tools_json[key]
    }
}

/// Mutable counterpart of [`section_value`].
fn section_value_mut<'a>(tools_json: &'a mut Value, key: &str) -> &'a mut Value {
    if key == GENERAL_SECTION_KEY {
        tools_json
    } else {
        &mut tools_json[key]
    }
}

/// The value at the dotted `path` inside `root`, or `Null`.
fn value_at<'a>(root: &'a Value, path: &str) -> &'a Value {
    path.split('.').fold(root, |value, key| &value[key])
}

/// The input kind a property needs, or `None` for properties the form cannot edit
/// (objects, maps, arrays of non-strings, ...).
fn field_kind(property: &Value) -> Option<FieldKind> {
    if let Some(choices) = property["x-choices"].as_array() {
        let choices = choices
            .iter()
            .filter_map(|choice| choice.as_str().map(str::to_string))
            .collect();
        return Some(FieldKind::Choice { choices });
    }
    match base_type(property)? {
        "boolean" => Some(FieldKind::Bool),
        "integer" => Some(FieldKind::Integer {
            min: property["minimum"].as_i64(),
            max: property["maximum"].as_i64(),
        }),
        "string" => Some(FieldKind::Text),
        "array" if property["items"]["type"] == "string" => Some(FieldKind::StringList),
        _ => None,
    }
}

/// Whether the environment variable `name` is set to a non-empty value.
fn env_var_is_set(name: &str) -> bool {
    !name.is_empty() && std::env::var(name).is_ok_and(|value| !value.is_empty())
}

/// Whether a stored value counts as "has a value".
fn has_value(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        _ => true,
    }
}

/// Summarize one property of a section against its current value.
fn summarize_field(name: &str, property: &Value, current: &Value) -> Option<ToolFieldSummary> {
    let kind = field_kind(property)?;
    let secret = property["x-secret"] == true;
    let env_var = property["x-env-var"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let configured = has_value(current) || env_var_is_set(&env_var);
    Some(ToolFieldSummary {
        name: name.to_string(),
        label: property["x-label"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| label_from_property_name(&name.replace('.', " "))),
        help: help_text(property["description"].as_str().unwrap_or_default()),
        kind,
        secret,
        env_var,
        configured,
        value: (!secret).then(|| current.clone()),
        default: property.get("default").cloned(),
    })
}

/// Summarize a section given the current value of `tools.<key>`.
fn summarize_section(section: &Section, current: &Value) -> ToolSettingsSummary {
    let properties = section.schema["properties"].as_object();
    let enabled = properties.and_then(|properties| {
        ENABLE_PROPERTY_NAMES
            .iter()
            .find(|name| properties.contains_key(**name))
            .and_then(|name| current[*name].as_bool())
    });
    let fields = flatten_properties(&section.schema, "")
        .iter()
        .filter(|(name, _)| !ENABLE_PROPERTY_NAMES.contains(&name.as_str()))
        .filter_map(|(name, property)| summarize_field(name, property, value_at(current, name)))
        .collect();
    ToolSettingsSummary {
        name: section.key.to_string(),
        title: section.schema["x-title"]
            .as_str()
            .unwrap_or(section.key)
            .to_string(),
        description: help_text(section.schema["description"].as_str().unwrap_or_default()),
        enabled,
        tool_names: section.schema["x-tools"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|name| name.as_str().map(str::to_string))
            .collect(),
        fields,
    }
}

/// The editable tools with their current settings. Secret values are never included.
pub fn tool_settings_overview(tools: &ToolsConfig) -> Vec<ToolSettingsSummary> {
    let tools_json = serde_json::to_value(tools).unwrap_or(Value::Null);
    editable_sections()
        .iter()
        .map(|section| summarize_section(section, section_value(&tools_json, section.key)))
        .collect()
}

/// Check `value` fits `kind` and return it normalized (strings trimmed), or why not.
fn check_value(kind: &FieldKind, value: &Value) -> Result<Value, String> {
    match (kind, value) {
        (FieldKind::Bool, Value::Bool(_)) => Ok(value.clone()),
        (FieldKind::Integer { min, max }, Value::Number(number)) => {
            let integer = number.as_i64().ok_or("must be a whole number")?;
            if min.is_some_and(|min| integer < min) {
                return Err(format!("must be at least {}", min.unwrap_or_default()));
            }
            if max.is_some_and(|max| integer > max) {
                return Err(format!("must be at most {}", max.unwrap_or_default()));
            }
            Ok(value.clone())
        }
        (FieldKind::Text, Value::String(text)) => Ok(Value::String(text.trim().to_string())),
        (FieldKind::Choice { choices }, Value::String(text)) => {
            let text = text.trim();
            if choices.iter().any(|choice| choice == text) {
                Ok(Value::String(text.to_string()))
            } else {
                Err(format!("must be one of: {}", choices.join(", ")))
            }
        }
        (FieldKind::StringList, Value::Array(items)) if items.iter().all(Value::is_string) => {
            Ok(value.clone())
        }
        _ => Err("has the wrong type".to_string()),
    }
}

/// Validate and merge `changes` (property name -> value, plus `enable`/`enabled` for the
/// switch) into the section `tool` of `config.tools`.
///
/// A blank value for a secret field keeps the stored one. Nothing is changed unless
/// every change is valid and the resulting configuration passes its own validation.
/// Returns whether the configuration actually changed (i.e. needs saving and a restart).
pub fn apply_tool_settings(
    config: &mut Config,
    tool: &str,
    changes: &HashMap<String, Value>,
) -> Result<bool, ToolSettingsError> {
    let section = editable_sections()
        .into_iter()
        .find(|section| section.key == tool)
        .ok_or_else(|| ToolSettingsError::UnknownTool(tool.to_string()))?;
    let properties: HashMap<String, Value> = flatten_properties(&section.schema, "")
        .into_iter()
        .collect();

    let mut tools_json = serde_json::to_value(&config.tools)
        .map_err(|error| ToolSettingsError::Invalid(error.to_string()))?;
    let section_json = section_value_mut(&mut tools_json, tool)
        .as_object_mut()
        .ok_or_else(|| ToolSettingsError::UnknownTool(tool.to_string()))?;

    for (name, value) in changes {
        let property = properties
            .get(name)
            .ok_or_else(|| ToolSettingsError::UnknownField(tool.to_string(), name.clone()))?;
        let kind = if ENABLE_PROPERTY_NAMES.contains(&name.as_str()) {
            FieldKind::Bool
        } else {
            field_kind(property)
                .ok_or_else(|| ToolSettingsError::UnknownField(tool.to_string(), name.clone()))?
        };
        let checked = check_value(&kind, value)
            .map_err(|reason| ToolSettingsError::InvalidValue(name.clone(), reason))?;
        let blank = checked == Value::String(String::new());
        let keeps_stored_secret = property["x-secret"] == true && blank;
        if keeps_stored_secret {
            continue;
        }
        let stored = if blank && is_nullable(property) {
            Value::Null
        } else {
            checked
        };
        set_property(section_json, name, stored);
    }

    let updated: ToolsConfig = serde_json::from_value(tools_json)
        .map_err(|error| ToolSettingsError::Invalid(error.to_string()))?;
    if changes.contains_key("ssrfWhitelist") {
        check_cidr_list("ssrfWhitelist", &updated.ssrf_whitelist)?;
    }
    updated
        .validate()
        .map_err(|report| ToolSettingsError::Invalid(report.to_string()))?;
    let changed = serde_json::to_value(&updated).ok() != serde_json::to_value(&config.tools).ok();
    config.tools = updated;
    Ok(changed)
}

/// Refuse a list holding something that is not a CIDR range (the gateway would silently
/// ignore it at startup).
fn check_cidr_list(field: &str, entries: &[String]) -> Result<(), ToolSettingsError> {
    match entries
        .iter()
        .find(|entry| entry.parse::<ipnet::IpNet>().is_err())
    {
        Some(entry) => Err(ToolSettingsError::InvalidValue(
            field.to_string(),
            format!("'{entry}' is not a CIDR range such as 100.64.0.0/10"),
        )),
        None => Ok(()),
    }
}

/// Whether `name` is the key of an editable tool section.
pub fn is_section_name(name: &str) -> bool {
    editable_sections()
        .iter()
        .any(|section| section.key == name)
}

/// Switch the section `tool` on or off. Returns whether the configuration changed.
pub fn set_section_enabled(
    config: &mut Config,
    tool: &str,
    enabled: bool,
) -> Result<bool, ToolSettingsError> {
    let section = editable_sections()
        .into_iter()
        .find(|section| section.key == tool)
        .ok_or_else(|| ToolSettingsError::UnknownTool(tool.to_string()))?;
    let property = section.schema["properties"]
        .as_object()
        .and_then(|properties| {
            ENABLE_PROPERTY_NAMES
                .iter()
                .find(|name| properties.contains_key(**name))
        })
        .ok_or_else(|| ToolSettingsError::UnknownField(tool.to_string(), "enable".to_string()))?;
    let changes = HashMap::from([(property.to_string(), Value::Bool(enabled))]);
    apply_tool_settings(config, tool, &changes)
}

/// Insert `value` under the dotted path `name` in a section's JSON object, creating
/// intermediate objects as needed.
fn set_property(section_json: &mut Map<String, Value>, name: &str, value: Value) {
    match name.split_once('.') {
        None => {
            section_json.insert(name.to_string(), value);
        }
        Some((head, rest)) => {
            let child = section_json
                .entry(head.to_string())
                .or_insert_with(|| Value::Object(Map::new()));
            if let Some(child) = child.as_object_mut() {
                set_property(child, rest, value);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn image_summary(tools: &ToolsConfig) -> ToolSettingsSummary {
        tool_settings_overview(tools)
            .into_iter()
            .find(|summary| summary.name == "imageGeneration")
            .unwrap()
    }

    fn field<'a>(summary: &'a ToolSettingsSummary, name: &str) -> &'a ToolFieldSummary {
        summary
            .fields
            .iter()
            .find(|field| field.name == name)
            .unwrap()
    }

    fn change(pairs: &[(&str, Value)]) -> HashMap<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn labels_are_split_camel_case_words() {
        assert_eq!(
            label_from_property_name("clientSecretPath"),
            "Client secret path"
        );
        assert_eq!(label_from_property_name("model"), "Model");
    }

    #[test]
    fn help_text_drops_the_default_sentence_and_joins_lines() {
        assert_eq!(
            help_text("Empty means omit.\nSecond line. Default: `\"\"`."),
            "Empty means omit. Second line."
        );
    }

    #[test]
    fn overview_lists_the_editable_tools_with_the_switch_separate() {
        let overview = tool_settings_overview(&ToolsConfig::default());
        let names: Vec<_> = overview.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "general",
                "gmail",
                "imageGeneration",
                "ocr",
                "docx",
                "question",
                "exec",
                "web",
                "acp"
            ]
        );
        let image = image_summary(&ToolsConfig::default());
        assert_eq!(image.title, "Image generation");
        assert_eq!(image.enabled, Some(false));
        assert!(image.fields.iter().all(|field| field.name != "enable"));
    }

    #[test]
    fn overview_maps_property_types_to_field_kinds() {
        let overview = tool_settings_overview(&ToolsConfig::default());
        let gmail = section(&overview, "gmail");
        assert_eq!(
            field(gmail, "maxResults").kind,
            FieldKind::Integer {
                min: Some(1),
                max: None
            }
        );
        assert_eq!(field(gmail, "tokenCachePath").kind, FieldKind::Text);
        let image = image_summary(&ToolsConfig::default());
        assert!(matches!(
            field(&image, "quality").kind,
            FieldKind::Choice { .. }
        ));
    }

    #[test]
    fn overview_never_includes_secret_values() {
        let mut tools = ToolsConfig::default();
        tools.image_generation.api_key = "sk-very-secret".to_string();
        let summary = image_summary(&tools);
        let api_key = field(&summary, "apiKey");
        assert!(api_key.secret);
        assert!(api_key.configured);
        assert_eq!(api_key.value, None);
        assert!(
            !serde_json::to_string(&summary)
                .unwrap()
                .contains("sk-very-secret")
        );
    }

    #[test]
    fn an_empty_secret_is_not_configured_without_its_env_var() {
        if std::env::var("OPENROUTER_API_KEY").is_ok() {
            return;
        }
        let summary = image_summary(&ToolsConfig::default());
        assert!(!field(&summary, "apiKey").configured);
    }

    #[test]
    fn apply_updates_values_and_the_switch() {
        let mut config = Config::default();
        let changes = change(&[
            ("enable", json!(true)),
            ("model", json!("  my/model ")),
            ("quality", json!("high")),
        ]);
        assert_eq!(
            apply_tool_settings(&mut config, "imageGeneration", &changes),
            Ok(true)
        );
        assert!(config.tools.image_generation.enable);
        assert_eq!(config.tools.image_generation.model, "my/model");
        assert_eq!(config.tools.image_generation.quality, "high");
    }

    #[test]
    fn apply_keeps_the_stored_secret_when_the_submitted_one_is_blank() {
        let mut config = Config::default();
        config.tools.image_generation.api_key = "stored".to_string();
        apply_tool_settings(
            &mut config,
            "imageGeneration",
            &change(&[("apiKey", json!(" "))]),
        )
        .unwrap();
        assert_eq!(config.tools.image_generation.api_key, "stored");
        apply_tool_settings(
            &mut config,
            "imageGeneration",
            &change(&[("apiKey", json!("new"))]),
        )
        .unwrap();
        assert_eq!(config.tools.image_generation.api_key, "new");
    }

    #[test]
    fn apply_reports_no_change_when_the_values_are_already_set() {
        let mut config = Config::default();
        let same_model = ToolsConfig::default().image_generation.model;
        let changes = change(&[("model", json!(same_model))]);
        assert_eq!(
            apply_tool_settings(&mut config, "imageGeneration", &changes),
            Ok(false)
        );
    }

    #[test]
    fn sections_list_the_tool_names_they_own() {
        let overview = tool_settings_overview(&ToolsConfig::default());
        assert_eq!(
            section(&overview, "gmail").tool_names,
            ["gmail", "gmail_email_send", "gmail_email_download"]
        );
        assert_eq!(
            section(&overview, "imageGeneration").tool_names,
            ["image_generation"]
        );
    }

    #[test]
    fn set_section_enabled_flips_the_switch_and_reports_changes() {
        let mut config = Config::default();
        assert_eq!(set_section_enabled(&mut config, "gmail", true), Ok(true));
        assert!(config.tools.gmail.enable);
        assert_eq!(set_section_enabled(&mut config, "gmail", true), Ok(false));
        assert_eq!(
            set_section_enabled(&mut config, "nope", true),
            Err(ToolSettingsError::UnknownTool("nope".into()))
        );
    }

    fn section<'a>(overview: &'a [ToolSettingsSummary], name: &str) -> &'a ToolSettingsSummary {
        overview
            .iter()
            .find(|summary| summary.name == name)
            .unwrap()
    }

    #[test]
    fn nested_web_search_settings_are_flattened_into_dotted_names() {
        let overview = tool_settings_overview(&ToolsConfig::default());
        let web = section(&overview, "web");
        let names: Vec<&str> = web.fields.iter().map(|f| f.name.as_str()).collect();
        for expected in [
            "proxy",
            "timeout",
            "search.provider",
            "search.apiKey",
            "search.maxResults",
        ] {
            assert!(names.contains(&expected), "{expected} in {names:?}");
        }
        assert_eq!(field(web, "search.provider").label, "Search provider");
        assert!(field(web, "search.apiKey").secret);
        assert_eq!(web.enabled, Some(true));
    }

    #[test]
    fn nested_and_nullable_settings_can_be_applied_and_cleared() {
        let mut config = Config::default();
        let changes = change(&[
            ("search.provider", json!("brave")),
            ("search.maxResults", json!(7)),
            ("proxy", json!("http://127.0.0.1:7890")),
        ]);
        assert_eq!(apply_tool_settings(&mut config, "web", &changes), Ok(true));
        assert_eq!(config.tools.web.search.provider.as_str(), "brave");
        assert_eq!(config.tools.web.search.max_results, 7);
        assert_eq!(
            config.tools.web.proxy.as_deref(),
            Some("http://127.0.0.1:7890")
        );

        apply_tool_settings(&mut config, "web", &change(&[("proxy", json!(""))])).unwrap();
        assert_eq!(config.tools.web.proxy, None);
    }

    #[test]
    fn acp_choices_match_the_serde_names_and_agents_map_is_not_editable() {
        let overview = tool_settings_overview(&ToolsConfig::default());
        let acp = section(&overview, "acp");
        assert_eq!(acp.enabled, Some(false));
        assert!(acp.fields.iter().all(|f| f.name != "launchPresets"));
        let mut config = Config::default();
        for (name, value) in [
            ("permissionPolicy", "escalate"),
            ("permissionPolicy", "allow-all"),
            ("permissionPolicy", "auto-approve-read"),
            ("sessionScope", "shared"),
            ("sessionScope", "perParentSession"),
        ] {
            let result = apply_tool_settings(&mut config, "acp", &change(&[(name, json!(value))]));
            assert!(result.is_ok(), "{name}={value}: {result:?}");
        }
    }

    #[test]
    fn exec_sandbox_and_timeout_are_validated() {
        let mut config = Config::default();
        assert!(
            apply_tool_settings(&mut config, "exec", &change(&[("sandbox", json!("bwrap"))]))
                .is_ok()
        );
        assert!(
            apply_tool_settings(
                &mut config,
                "exec",
                &change(&[("sandbox", json!("docker"))])
            )
            .is_err()
        );
        assert!(
            apply_tool_settings(&mut config, "exec", &change(&[("timeout", json!(0))])).is_err()
        );
        assert_eq!(
            set_section_enabled(&mut config, "question", false),
            Ok(true)
        );
        assert!(!config.tools.question.enabled);
    }

    #[test]
    fn general_section_holds_only_the_settings_not_owned_by_a_tool() {
        let overview = tool_settings_overview(&ToolsConfig::default());
        let general = section(&overview, "general");
        let names: Vec<&str> = general.fields.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "restrictToWorkspace",
                "confirmBeforeExecute",
                "ssrfWhitelist"
            ]
        );
        assert_eq!(general.enabled, None);
        assert!(general.tool_names.is_empty());
        assert_eq!(field(general, "ssrfWhitelist").kind, FieldKind::StringList);
    }

    #[test]
    fn a_field_can_override_its_generated_label() {
        let overview = tool_settings_overview(&ToolsConfig::default());
        let general = section(&overview, "general");
        assert_eq!(field(general, "ssrfWhitelist").label, "SSRF whitelist");
        assert_eq!(
            field(general, "restrictToWorkspace").label,
            "Restrict to workspace"
        );
    }

    #[test]
    fn general_settings_can_be_applied() {
        let mut config = Config::default();
        let changes = change(&[
            ("restrictToWorkspace", json!(true)),
            ("confirmBeforeExecute", json!(true)),
            ("ssrfWhitelist", json!(["100.64.0.0/10"])),
        ]);
        assert_eq!(
            apply_tool_settings(&mut config, "general", &changes),
            Ok(true)
        );
        assert!(config.tools.restrict_to_workspace);
        assert!(config.tools.confirm_before_execute);
        assert_eq!(config.tools.ssrf_whitelist, ["100.64.0.0/10"]);
        assert_eq!(config.tools.web.timeout, ToolsConfig::default().web.timeout);
    }

    #[test]
    fn general_section_rejects_other_fields_and_bad_cidr_ranges() {
        let mut config = Config::default();
        let other_field = change(&[("disabledTools", json!(["x"]))]);
        assert!(matches!(
            apply_tool_settings(&mut config, "general", &other_field),
            Err(ToolSettingsError::UnknownField(..))
        ));
        let bad_range = change(&[("ssrfWhitelist", json!(["nope"]))]);
        assert!(matches!(
            apply_tool_settings(&mut config, "general", &bad_range),
            Err(ToolSettingsError::InvalidValue(..))
        ));
        assert!(config.tools.ssrf_whitelist.is_empty());
    }

    #[test]
    fn apply_rejects_unknown_tools_and_fields() {
        let mut config = Config::default();
        assert_eq!(
            apply_tool_settings(&mut config, "nope", &HashMap::new()),
            Err(ToolSettingsError::UnknownTool("nope".into()))
        );
        assert!(matches!(
            apply_tool_settings(&mut config, "gmail", &change(&[("bogus", json!(1))])),
            Err(ToolSettingsError::UnknownField(..))
        ));
    }

    #[test]
    fn apply_rejects_wrong_types_bad_choices_and_out_of_range_numbers() {
        let mut config = Config::default();
        for (tool, name, value) in [
            ("gmail", "maxResults", json!("ten")),
            ("gmail", "maxResults", json!(0)),
            ("imageGeneration", "quality", json!("ultra")),
            ("imageGeneration", "enable", json!("yes")),
        ] {
            let result = apply_tool_settings(&mut config, tool, &change(&[(name, value)]));
            assert!(
                matches!(result, Err(ToolSettingsError::InvalidValue(..))),
                "{name}"
            );
        }
    }

    #[test]
    fn apply_changes_nothing_when_any_change_is_invalid() {
        let mut config = Config::default();
        let changes = change(&[("model", json!("changed")), ("quality", json!("ultra"))]);
        assert!(apply_tool_settings(&mut config, "imageGeneration", &changes).is_err());
        assert_eq!(
            config.tools.image_generation.model,
            ToolsConfig::default().image_generation.model
        );
    }
}
