//! The model presets of `config.json` (`modelPresets.<name>`) for the WebUI's Settings
//! dialog (the Models page): the rows, the settings form of a preset, and the edits
//! behind "+ Add model preset", Save, Delete and Activate.
//!
//! The form is generated from the JSON Schema of [`ModelPresetConfig`] with the shared
//! helpers of [`super::settings_form`], so the browser renders it with the same component
//! as the tool and MCP server forms. Like [`super::mcp_server_form`], every edit works on
//! the raw JSON text: `${ENV_VAR}` placeholders, key spellings and every key outside the
//! edited preset (or `agents.modelPreset`, for Activate) stay exactly as written.
//!
//! Each edited text is checked with the same validation the bot runs at startup
//! ([`validate_config_text`]), so an edit can never produce a file the bot refuses: a
//! preset that is the active default or Dream's preset cannot be deleted, `default` is
//! reserved, and a preset needs a model.
//!
//! The implicit `default` preset (the flat `agents.model` / `agents.provider` / ...
//! settings) is listed first as a read-only row; activating it removes
//! `agents.modelPreset`. Everything here is a pure function of strings (the process
//! environment is passed in where a check needs it); saving is
//! [`super::config_toggles::apply_edit`]'s job and the live switch is the resolver's.

use std::collections::HashMap;

use serde::Serialize;
use serde_json::{Map, Value};

use super::config_file::validate_config_text;
use super::config_toggles::{
    ToggleError, child_object_mut, existing_or_default_key, is_valid_name, parse_root, render,
};
use super::settings_form::{
    SettingSpellings, check_placeholders, form_fields, process_env_is_set, schema_properties,
    store_value, stored_value,
};
use crate::agent::tools::tool_settings_api::{ToolSettingsSummary, check_value, field_kind};
use crate::config::loader::resolve_env_vars_in;
use crate::config::schema::{
    Config, MODEL_PRESET_PROVIDER_CHOICES, ModelPresetConfig, REASONING_EFFORT_NONE,
    RESERVED_MODEL_PRESET_NAME,
};
use crate::providers::factory::try_auto_provider_selection;

/// Spellings the configuration accepts for the top-level `modelPresets` map.
const MODEL_PRESETS_KEYS: &[&str] = &["modelPresets", "model_presets"];
/// Spellings the configuration accepts for the `agents` section.
const AGENTS_KEYS: &[&str] = &["agents"];
/// Spellings the configuration accepts for `agents.modelPreset` (the active preset).
const AGENTS_MODEL_PRESET_KEYS: &[&str] = &["modelPreset", "model_preset"];

/// Setting names (as the form calls them) with every spelling a preset entry accepts.
const SETTING_SPELLINGS: SettingSpellings = &[
    &["maxTokens", "max_tokens"],
    &["contextWindowTokens", "context_window_tokens"],
    &["reasoningEffort", "reasoning_effort"],
];

const LABEL: &str = "label";
const MODEL: &str = "model";
const REASONING_EFFORT: &str = "reasoningEffort";

/// Label of the read-only row that stands for the flat `agents.*` settings.
pub const DEFAULT_PRESET_LABEL: &str = "Default (agent settings)";

/// What to do about a `${VAR}` placeholder whose variable is not set; shown after its name.
const UNSET_VARIABLE_EXPLANATION: &str = "which is not set in the environment of the process running rust-bot. Set the variable there before starting rust-bot, or write the value here directly.";

/// Why the active preset cannot be deleted.
const ACTIVE_PRESET_IN_USE: &str = "it is the active preset. Activate another one first.";
/// Why Dream's preset cannot be deleted.
const DREAM_PRESET_IN_USE: &str =
    "Dream uses it (agents.dream.dreamModelPreset). Change that setting first.";

/// One row of the Models page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModelPresetRow {
    /// The preset's key in `modelPresets`, or `default` for the built-in row.
    pub name: String,
    /// The display label, when the preset has one.
    pub label: Option<String>,
    pub model: String,
    pub provider: String,
    /// Whether this is the configured default (`agents.modelPreset`).
    pub active: bool,
    /// Whether this is the implicit `default` row (the flat `agents.*` settings): it can
    /// be activated but not edited or deleted.
    pub built_in: bool,
    /// Whether Delete is offered: not for the built-in row, the active preset or the
    /// preset Dream uses.
    pub deletable: bool,
    /// Why Delete is not offered, empty when it is (shown as the button's tooltip).
    pub delete_blocked_reason: String,
    /// Whether the preset's provider has no API key in `providers.<name>` (for `auto`:
    /// no provider has one), so a turn with it would fail.
    pub missing_api_key: bool,
    /// The settings form. Only sent to connections that may change the configuration,
    /// and never for the built-in row.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub form: Option<ToolSettingsSummary>,
}

/// Everything the Models page shows, read from the config text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModelPresetsOverview {
    /// The built-in `default` row first, then the presets sorted A–Z ignoring case.
    pub presets: Vec<ModelPresetRow>,
    /// The name of the active row (`default` when `agents.modelPreset` is not set).
    pub active: String,
    /// The blank form for "+ Add model preset"; only for connections that may change the
    /// configuration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_preset_form: Option<ToolSettingsSummary>,
    /// Providers offered by the form that have no API key configured.
    pub providers_without_key: Vec<String>,
}

/// The schema properties the form edits, in declaration order.
fn form_properties() -> Map<String, Value> {
    schema_properties::<ModelPresetConfig>()
}

/// Parse `text` into the typed configuration (no environment resolution), for reading
/// names and references.
pub(crate) fn typed_config(text: &str) -> Result<Config, ToggleError> {
    serde_json::from_str(text)
        .map_err(|error| ToggleError::InvalidConfig(format!("Invalid configuration: {error}")))
}

/// The name of the configured default preset: `agents.modelPreset`, or `default`.
pub fn active_preset_name(config: &Config) -> String {
    config
        .agents
        .model_preset
        .clone()
        .unwrap_or_else(|| RESERVED_MODEL_PRESET_NAME.to_string())
}

/// Whether `provider` has no API key in `config` (for `auto`: no provider it could pick
/// has one). Unknown provider names count as having none.
pub fn provider_lacks_api_key(config: &Config, provider: &str) -> bool {
    if provider == "auto" {
        return try_auto_provider_selection(config).is_err();
    }
    config
        .providers
        .get_by_name(provider)
        .is_none_or(|slot| slot.api_key.trim().is_empty())
}

/// Why the preset `name` cannot be deleted, if it cannot.
fn delete_blocked_reason(config: &Config, name: &str) -> Option<&'static str> {
    if config.agents.model_preset.as_deref() == Some(name) {
        return Some(ACTIVE_PRESET_IN_USE);
    }
    if config.agents.dream.dream_model_preset.as_deref() == Some(name) {
        return Some(DREAM_PRESET_IN_USE);
    }
    None
}

/// The `modelPresets` map of a parsed config, if it has one.
fn presets_object(root: &Value) -> Option<&Map<String, Value>> {
    let root = root.as_object()?;
    root.get(existing_or_default_key(root, MODEL_PRESETS_KEYS))?
        .as_object()
}

/// The `modelPresets` map of a parsed config as a mutable object, created when missing.
fn presets_object_mut(root: &mut Value) -> Result<&mut Map<String, Value>, ToggleError> {
    let root_object = root.as_object_mut().expect("parse_root checked this");
    child_object_mut(root_object, MODEL_PRESETS_KEYS)
}

/// The entry `modelPresets.<name>` of a parsed config as a mutable object.
fn preset_object_mut<'a>(
    root: &'a mut Value,
    name: &str,
) -> Result<&'a mut Map<String, Value>, ToggleError> {
    presets_object_mut(root)?
        .get_mut(name)
        .ok_or(ToggleError::UnknownPreset)?
        .as_object_mut()
        .ok_or_else(|| ToggleError::InvalidConfig("The model preset is not an object.".to_string()))
}

/// The settings form of a preset whose stored entry is `entry` (`None`: a preset not
/// added yet). A `reasoningEffort` of `null` is shown as the `none` choice.
fn preset_form(name: &str, entry: Option<&Map<String, Value>>) -> ToolSettingsSummary {
    let mut fields = form_fields(&form_properties(), SETTING_SPELLINGS, entry);
    for field in fields
        .iter_mut()
        .filter(|field| field.name == REASONING_EFFORT)
    {
        if field.value.as_ref().is_none_or(Value::is_null) {
            field.value = Some(Value::String(REASONING_EFFORT_NONE.to_string()));
        }
        field.default = Some(Value::String(REASONING_EFFORT_NONE.to_string()));
    }
    ToolSettingsSummary {
        name: name.to_string(),
        title: name.to_string(),
        description: String::new(),
        enabled: None,
        tool_names: Vec::new(),
        fields,
    }
}

/// The blank form for "+ Add model preset": numbers and text empty with the default as
/// placeholder, the provider `auto` and the reasoning effort `none`. It has no name (the
/// browser asks for one).
pub fn new_model_preset_form() -> ToolSettingsSummary {
    preset_form("", None)
}

/// The built-in row for the flat `agents.*` settings.
fn default_row(config: &Config) -> ModelPresetRow {
    ModelPresetRow {
        name: RESERVED_MODEL_PRESET_NAME.to_string(),
        label: Some(DEFAULT_PRESET_LABEL.to_string()),
        model: config.agents.model.clone(),
        provider: config.agents.provider.clone(),
        active: active_preset_name(config) == RESERVED_MODEL_PRESET_NAME,
        built_in: true,
        deletable: false,
        delete_blocked_reason: "The agent settings cannot be deleted.".to_string(),
        missing_api_key: provider_lacks_api_key(config, &config.agents.provider),
        form: None,
    }
}

/// The row of the configured preset `name`.
fn preset_row(
    config: &Config,
    name: &str,
    preset: &ModelPresetConfig,
    entry: Option<&Map<String, Value>>,
    with_forms: bool,
) -> ModelPresetRow {
    let blocked = delete_blocked_reason(config, name);
    ModelPresetRow {
        name: name.to_string(),
        label: preset
            .label
            .clone()
            .filter(|label| !label.trim().is_empty()),
        model: preset.model.clone(),
        provider: preset.provider.clone(),
        active: active_preset_name(config) == name,
        built_in: false,
        deletable: blocked.is_none(),
        delete_blocked_reason: blocked.unwrap_or_default().to_string(),
        missing_api_key: provider_lacks_api_key(config, &preset.provider),
        form: with_forms.then(|| preset_form(name, entry)),
    }
}

/// The rows of the Models page for the config `text`: the built-in `default` row first,
/// then every preset sorted A–Z ignoring case. Forms (and the blank add form) are only
/// included `with_forms`, for connections that may change the configuration.
pub fn model_presets_overview(
    text: &str,
    with_forms: bool,
) -> Result<ModelPresetsOverview, ToggleError> {
    let root = parse_root(text)?;
    let config = typed_config(text)?;
    let raw_presets = presets_object(&root);
    let mut names: Vec<&String> = config.model_presets.keys().collect();
    names.sort_by_key(|name| name.to_lowercase());

    let mut presets = vec![default_row(&config)];
    presets.extend(names.into_iter().map(|name| {
        let entry = raw_presets
            .and_then(|presets| presets.get(name))
            .and_then(Value::as_object);
        preset_row(
            &config,
            name,
            &config.model_presets[name],
            entry,
            with_forms,
        )
    }));
    Ok(ModelPresetsOverview {
        presets,
        active: active_preset_name(&config),
        new_preset_form: with_forms.then(new_model_preset_form),
        providers_without_key: MODEL_PRESET_PROVIDER_CHOICES
            .iter()
            .filter(|provider| provider_lacks_api_key(&config, provider))
            .map(|provider| provider.to_string())
            .collect(),
    })
}

/// The preset catalog and default a running process should switch to once a config text
/// has been saved (see [`live_model_presets`]).
#[derive(Debug, Clone)]
pub struct LiveModelPresets {
    /// `modelPresets` with every `${VAR}` placeholder resolved.
    pub presets: HashMap<String, ModelPresetConfig>,
    /// `agents.modelPreset` (`None`: the flat `agents.*` settings).
    pub default_name: Option<String>,
}

/// What the running process should use after `text` was saved: the presets with their
/// `${VAR}` placeholders resolved from the environment and the configured default.
/// Never panics (unlike `load_config`); errs with a message for the user when `text`
/// is not a configuration the bot could start with or a preset names an unset variable.
pub fn live_model_presets(text: &str) -> Result<LiveModelPresets, String> {
    validate_config_text(text)?;
    let config = typed_config(text).map_err(|error| error.message())?;
    let presets = resolve_env_vars_in(&config.model_presets)?;
    Ok(LiveModelPresets {
        presets,
        default_name: config.agents.model_preset,
    })
}

/// The value stored for a checked form value: a blank label and the `none` reasoning
/// effort are `null`, everything else as given.
fn stored_form_value(setting: &str, checked: Value) -> Value {
    let means_null = match setting {
        LABEL => checked.as_str() == Some(""),
        REASONING_EFFORT => checked.as_str() == Some(REASONING_EFFORT_NONE),
        _ => false,
    };
    if means_null { Value::Null } else { checked }
}

/// Check every submitted `changes` entry against the schema and the environment and
/// write it into `entry`. `entry` may be left half edited on an error: callers work on a
/// parsed copy of the config and drop it.
fn apply_settings(
    entry: &mut Map<String, Value>,
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
        check_placeholders(setting, &checked, env_is_set, UNSET_VARIABLE_EXPLANATION)?;
        store_value(
            SETTING_SPELLINGS,
            entry,
            setting,
            stored_form_value(setting, checked),
        );
    }
    check_model_is_set(entry)
}

/// Refuse an entry without a model: the bot would refuse to start with it.
fn check_model_is_set(entry: &Map<String, Value>) -> Result<(), ToggleError> {
    let model = stored_value(SETTING_SPELLINGS, entry, MODEL)
        .and_then(Value::as_str)
        .unwrap_or_default();
    if model.trim().is_empty() {
        return Err(ToggleError::InvalidValue(
            MODEL.to_string(),
            "a preset needs a model".to_string(),
        ));
    }
    Ok(())
}

/// The full entry of a new preset: every setting in declaration order, the submitted
/// value where there is one and the schema default otherwise (so the file shows the whole
/// preset, as the setup wizard writes it).
fn complete_entry(submitted: &Map<String, Value>) -> Map<String, Value> {
    form_properties()
        .iter()
        .map(|(setting, property)| {
            let value = submitted
                .get(setting)
                .cloned()
                .unwrap_or_else(|| property.get("default").cloned().unwrap_or(Value::Null));
            (setting.clone(), value)
        })
        .collect()
}

/// Check `name` can be the key of a new preset: a valid name, not the reserved `default`.
fn check_new_name(name: &str) -> Result<(), ToggleError> {
    if !is_valid_name(name) {
        return Err(ToggleError::InvalidName);
    }
    if name == RESERVED_MODEL_PRESET_NAME {
        return Err(ToggleError::ReservedName);
    }
    Ok(())
}

/// Render `root` like `original` and check the bot could start with the result.
fn rendered_and_checked(root: &Value, original: &str) -> Result<String, ToggleError> {
    let text = render(root, original);
    validate_config_text(&text).map_err(ToggleError::InvalidConfig)?;
    Ok(text)
}

/// `text` with a new preset `name` added to `modelPresets` (created when missing, under
/// the key spelling the file uses) holding the submitted `values` and the defaults of
/// everything not submitted. Refuses a name that is invalid, reserved or already used:
/// adding never overwrites.
pub fn with_model_preset_added(
    text: &str,
    name: &str,
    values: &HashMap<String, Value>,
) -> Result<String, ToggleError> {
    with_model_preset_added_in(text, name, values, &process_env_is_set)
}

/// [`with_model_preset_added`] with the environment lookup passed in.
fn with_model_preset_added_in(
    text: &str,
    name: &str,
    values: &HashMap<String, Value>,
    env_is_set: &dyn Fn(&str) -> bool,
) -> Result<String, ToggleError> {
    check_new_name(name)?;
    let mut root = parse_root(text)?;
    let presets = presets_object_mut(&mut root)?;
    if presets.contains_key(name) {
        return Err(ToggleError::PresetExists);
    }
    let mut submitted = Map::new();
    apply_settings(&mut submitted, values, env_is_set)?;
    presets.insert(name.to_string(), Value::Object(complete_entry(&submitted)));
    rendered_and_checked(&root, text)
}

/// `text` with the submitted `changes` (setting name -> value, as the form names them)
/// applied to `modelPresets.<name>`, under the key spellings the entry already uses.
/// Nothing is applied unless every change is valid. `None` when the entry already says
/// all of it, so the caller can skip the write. The preset must already exist.
pub fn with_model_preset_settings(
    text: &str,
    name: &str,
    changes: &HashMap<String, Value>,
) -> Result<Option<String>, ToggleError> {
    with_model_preset_settings_in(text, name, changes, &process_env_is_set)
}

/// [`with_model_preset_settings`] with the environment lookup passed in.
fn with_model_preset_settings_in(
    text: &str,
    name: &str,
    changes: &HashMap<String, Value>,
    env_is_set: &dyn Fn(&str) -> bool,
) -> Result<Option<String>, ToggleError> {
    if !is_valid_name(name) {
        return Err(ToggleError::InvalidName);
    }
    let mut root = parse_root(text)?;
    let entry = preset_object_mut(&mut root, name)?;
    let before = entry.clone();
    apply_settings(entry, changes, env_is_set)?;
    if *entry == before {
        return Ok(None);
    }
    rendered_and_checked(&root, text).map(Some)
}

/// `text` without the preset `name`. Refused for the active preset and for Dream's
/// preset (the bot would not start with a dangling reference). The preset must exist.
pub fn without_model_preset(text: &str, name: &str) -> Result<String, ToggleError> {
    if !is_valid_name(name) {
        return Err(ToggleError::InvalidName);
    }
    if name == RESERVED_MODEL_PRESET_NAME {
        return Err(ToggleError::ReservedName);
    }
    let config = typed_config(text)?;
    if !config.model_presets.contains_key(name) {
        return Err(ToggleError::UnknownPreset);
    }
    if let Some(why) = delete_blocked_reason(&config, name) {
        return Err(ToggleError::PresetInUse(why.to_string()));
    }
    let mut root = parse_root(text)?;
    presets_object_mut(&mut root)?.shift_remove(name);
    rendered_and_checked(&root, text)
}

/// `text` with `agents.modelPreset` set to `name` (created under the spelling the file
/// uses), or removed for `default`, which means the flat `agents.*` settings. `None` when
/// that preset is already the active one. A named preset must exist.
pub fn with_default_model_preset(text: &str, name: &str) -> Result<Option<String>, ToggleError> {
    if !is_valid_name(name) {
        return Err(ToggleError::InvalidName);
    }
    let config = typed_config(text)?;
    if active_preset_name(&config) == name {
        return Ok(None);
    }
    let is_default = name == RESERVED_MODEL_PRESET_NAME;
    if !is_default && !config.model_presets.contains_key(name) {
        return Err(ToggleError::UnknownPreset);
    }
    let mut root = parse_root(text)?;
    let root_object = root.as_object_mut().expect("parse_root checked this");
    let agents = child_object_mut(root_object, AGENTS_KEYS)?;
    if is_default {
        for key in AGENTS_MODEL_PRESET_KEYS {
            agents.shift_remove(*key);
        }
    } else {
        let key = existing_or_default_key(agents, AGENTS_MODEL_PRESET_KEYS).to_string();
        agents.insert(key, Value::String(name.to_string()));
    }
    rendered_and_checked(&root, text).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tools::tool_settings_api::{FieldKind, ToolFieldSummary};
    use serde_json::json;

    const CONFIG: &str = r#"{
  "agents": {
    "model": "anthropic/claude-opus-4-6",
    "provider": "anthropic",
    "modelPreset": "fast",
    "dream": {"dreamModelPreset": "dreamy"}
  },
  "providers": {"anthropic": {"apiKey": "${ANTHROPIC_KEY}"}},
  "modelPresets": {
    "fast": {"model": "claude-haiku", "provider": "anthropic", "max_tokens": 1024},
    "dreamy": {"model": "claude-sonnet", "provider": "anthropic", "label": "Dream model"},
    "Spare": {"model": "${SPARE_MODEL}", "provider": "openai", "reasoningEffort": "low"}
  }
}
"#;

    /// The add with every `${VAR}` treated as set. Shadows the public function.
    fn with_model_preset_added(
        text: &str,
        name: &str,
        values: &HashMap<String, Value>,
    ) -> Result<String, ToggleError> {
        with_model_preset_added_in(text, name, values, &|_| true)
    }

    /// The edit with every `${VAR}` treated as set. Shadows the public function.
    fn with_model_preset_settings(
        text: &str,
        name: &str,
        changes: &HashMap<String, Value>,
    ) -> Result<Option<String>, ToggleError> {
        with_model_preset_settings_in(text, name, changes, &|_| true)
    }

    fn change(pairs: &[(&str, Value)]) -> HashMap<String, Value> {
        pairs
            .iter()
            .map(|(name, value)| (name.to_string(), value.clone()))
            .collect()
    }

    fn preset(text: &str, name: &str) -> Value {
        serde_json::from_str::<Value>(text).unwrap()["modelPresets"][name].clone()
    }

    fn field<'a>(form: &'a ToolSettingsSummary, name: &str) -> &'a ToolFieldSummary {
        form.fields
            .iter()
            .find(|field| field.name == name)
            .unwrap_or_else(|| panic!("no field {name}"))
    }

    fn terra_values() -> HashMap<String, Value> {
        change(&[
            ("model", json!("gpt-6.1-terra")),
            ("provider", json!("openai")),
            ("maxTokens", json!(32768)),
            ("contextWindowTokens", json!(131072)),
            ("temperature", json!(1.0)),
            ("reasoningEffort", json!("medium")),
        ])
    }

    #[test]
    fn the_form_lists_the_seven_preset_settings_with_their_input_kinds() {
        let form = new_model_preset_form();

        let names: Vec<&str> = form.fields.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "label",
                "model",
                "provider",
                "maxTokens",
                "contextWindowTokens",
                "temperature",
                "reasoningEffort"
            ]
        );
        assert_eq!(field(&form, "model").kind, FieldKind::Text);
        assert!(matches!(
            field(&form, "provider").kind,
            FieldKind::Choice { ref choices } if choices.len() == 6
        ));
        assert!(matches!(
            field(&form, "temperature").kind,
            FieldKind::Number { .. }
        ));
        assert!(matches!(
            field(&form, "maxTokens").kind,
            FieldKind::Integer { min: Some(1), .. }
        ));
        assert_eq!(field(&form, "provider").value, Some(json!("auto")));
        assert_eq!(field(&form, "reasoningEffort").value, Some(json!("none")));
        assert_eq!(field(&form, "temperature").value, Some(Value::Null));
        assert_eq!(field(&form, "temperature").default, Some(json!(0.1)));
    }

    #[test]
    fn a_preset_form_shows_stored_values_including_aliases_and_placeholders() {
        let overview = model_presets_overview(CONFIG, true).unwrap();
        let fast = overview.presets.iter().find(|r| r.name == "fast").unwrap();
        let form = fast.form.as_ref().unwrap();
        assert_eq!(field(form, "maxTokens").value, Some(json!(1024)));
        assert_eq!(field(form, "reasoningEffort").value, Some(json!("none")));

        let spare = overview.presets.iter().find(|r| r.name == "Spare").unwrap();
        let spare_form = spare.form.as_ref().unwrap();
        assert_eq!(
            field(spare_form, "model").value,
            Some(json!("${SPARE_MODEL}"))
        );
        assert_eq!(
            field(spare_form, "reasoningEffort").value,
            Some(json!("low"))
        );
    }

    #[test]
    fn rows_put_default_first_then_sort_and_mark_exactly_one_active() {
        let overview = model_presets_overview(CONFIG, false).unwrap();

        let names: Vec<&str> = overview.presets.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["default", "dreamy", "fast", "Spare"]);
        assert_eq!(overview.active, "fast");
        let active: Vec<&str> = overview
            .presets
            .iter()
            .filter(|r| r.active)
            .map(|r| r.name.as_str())
            .collect();
        assert_eq!(active, ["fast"]);
        let default = &overview.presets[0];
        assert!(default.built_in && !default.deletable);
        assert_eq!(default.label.as_deref(), Some(DEFAULT_PRESET_LABEL));
        assert_eq!(default.model, "anthropic/claude-opus-4-6");
        assert_eq!(overview.presets[1].label.as_deref(), Some("Dream model"));
    }

    #[test]
    fn rows_say_which_presets_cannot_be_deleted_and_why() {
        let overview = model_presets_overview(CONFIG, false).unwrap();
        let row = |name: &str| overview.presets.iter().find(|r| r.name == name).unwrap();

        assert!(!row("fast").deletable);
        assert!(row("fast").delete_blocked_reason.contains("active"));
        assert!(!row("dreamy").deletable);
        assert!(row("dreamy").delete_blocked_reason.contains("Dream"));
        assert!(row("Spare").deletable && row("Spare").delete_blocked_reason.is_empty());
    }

    #[test]
    fn forms_are_only_included_when_asked_for() {
        let without = model_presets_overview(CONFIG, false).unwrap();
        assert!(without.presets.iter().all(|row| row.form.is_none()));
        assert!(without.new_preset_form.is_none());

        let with = model_presets_overview(CONFIG, true).unwrap();
        assert!(
            with.presets[0].form.is_none(),
            "the built-in row has no form"
        );
        assert!(with.presets[1..].iter().all(|row| row.form.is_some()));
        assert!(with.new_preset_form.is_some());
    }

    #[test]
    fn rows_flag_providers_without_an_api_key() {
        let overview = model_presets_overview(CONFIG, false).unwrap();
        let row = |name: &str| overview.presets.iter().find(|r| r.name == name).unwrap();

        assert!(
            !row("fast").missing_api_key,
            "a placeholder counts as a key"
        );
        assert!(row("Spare").missing_api_key, "openai has no key");
        assert!(
            overview
                .providers_without_key
                .contains(&"openai".to_string())
        );
        assert!(
            !overview
                .providers_without_key
                .contains(&"anthropic".to_string())
        );
        assert!(!overview.providers_without_key.contains(&"auto".to_string()));
    }

    #[test]
    fn a_file_without_presets_has_only_the_active_default_row() {
        let overview = model_presets_overview("{}", true).unwrap();
        assert_eq!(overview.presets.len(), 1);
        assert!(overview.presets[0].active && overview.presets[0].built_in);
        assert_eq!(overview.active, "default");
    }

    #[test]
    fn adding_writes_the_whole_preset_and_leaves_the_rest_alone() {
        let updated =
            with_model_preset_added(CONFIG, "openai-gpt-6.1-terra", &terra_values()).unwrap();

        assert_eq!(
            preset(&updated, "openai-gpt-6.1-terra"),
            json!({
                "label": null,
                "model": "gpt-6.1-terra",
                "provider": "openai",
                "maxTokens": 32768,
                "contextWindowTokens": 131072,
                "temperature": 1.0,
                "reasoningEffort": "medium"
            })
        );
        let added_keys: Vec<String> = preset(&updated, "openai-gpt-6.1-terra")
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        assert_eq!(added_keys[0], "label", "declaration order");
        let mut after: Value = serde_json::from_str(&updated).unwrap();
        after["modelPresets"]
            .as_object_mut()
            .unwrap()
            .shift_remove("openai-gpt-6.1-terra");
        assert_eq!(after, serde_json::from_str::<Value>(CONFIG).unwrap());
        assert!(updated.contains("${ANTHROPIC_KEY}") && updated.ends_with('\n'));
    }

    #[test]
    fn adding_fills_in_defaults_for_settings_not_submitted() {
        let updated =
            with_model_preset_added(CONFIG, "plain", &change(&[("model", json!("m"))])).unwrap();

        let plain = preset(&updated, "plain");
        assert_eq!(plain["provider"], "auto");
        assert_eq!(plain["maxTokens"], 32768);
        assert_eq!(plain["contextWindowTokens"], 131072);
        assert_eq!(plain["temperature"], 0.1);
        assert_eq!(plain["reasoningEffort"], Value::Null);
        assert_eq!(plain["label"], Value::Null);
    }

    #[test]
    fn adding_creates_the_map_and_follows_the_key_spelling_in_use() {
        let values = change(&[("model", json!("m"))]);

        let from_empty = with_model_preset_added("{}", "first", &values).unwrap();
        assert_eq!(preset(&from_empty, "first")["model"], "m");

        let snake = r#"{"model_presets": {}}"#;
        let updated = with_model_preset_added(snake, "first", &values).unwrap();
        assert!(updated.contains("model_presets") && !updated.contains("modelPresets"));
    }

    #[test]
    fn adding_refuses_duplicates_reserved_and_invalid_names() {
        let values = change(&[("model", json!("m"))]);

        assert_eq!(
            with_model_preset_added(CONFIG, "fast", &values),
            Err(ToggleError::PresetExists)
        );
        assert_eq!(
            with_model_preset_added(CONFIG, "default", &values),
            Err(ToggleError::ReservedName)
        );
        assert_eq!(
            with_model_preset_added(CONFIG, "a/b", &values),
            Err(ToggleError::InvalidName)
        );
    }

    #[test]
    fn adding_refuses_a_missing_model_and_bad_values() {
        let refuse = |pairs: &[(&str, Value)]| {
            with_model_preset_added(CONFIG, "fresh", &change(pairs)).unwrap_err()
        };

        assert!(matches!(
            refuse(&[("label", json!("No model"))]),
            ToggleError::InvalidValue(ref s, _) if s == "model"
        ));
        assert!(matches!(
            refuse(&[("model", json!("   "))]),
            ToggleError::InvalidValue(ref s, _) if s == "model"
        ));
        for (setting, value) in [
            ("temperature", json!(1.5)),
            ("temperature", json!(-0.1)),
            ("maxTokens", json!(0)),
            ("contextWindowTokens", json!(0)),
            ("provider", json!("gemini")),
            ("reasoningEffort", json!("extreme")),
        ] {
            assert!(
                matches!(
                    refuse(&[("model", json!("m")), (setting, value)]),
                    ToggleError::InvalidValue(ref s, _) if s == setting
                ),
                "{setting}"
            );
        }
        assert_eq!(
            refuse(&[("model", json!("m")), ("apiKey", json!("x"))]),
            ToggleError::UnknownField("apiKey".into())
        );
    }

    #[test]
    fn editing_changes_only_the_named_preset_and_keeps_the_spelling_in_use() {
        let changes = change(&[("maxTokens", json!(2048)), ("temperature", json!(0.7))]);

        let updated = with_model_preset_settings(CONFIG, "fast", &changes)
            .unwrap()
            .unwrap();

        let fast = preset(&updated, "fast");
        assert_eq!(fast["max_tokens"], 2048);
        assert!(fast.get("maxTokens").is_none());
        assert_eq!(fast["temperature"], 0.7);
        assert_eq!(preset(&updated, "dreamy"), preset(CONFIG, "dreamy"));
        assert_eq!(preset(&updated, "Spare"), preset(CONFIG, "Spare"));
    }

    #[test]
    fn none_and_a_blank_label_are_stored_as_null() {
        let changes = change(&[("reasoningEffort", json!("none")), ("label", json!(" "))]);

        let updated = with_model_preset_settings(CONFIG, "Spare", &changes)
            .unwrap()
            .unwrap();

        assert_eq!(preset(&updated, "Spare")["reasoningEffort"], Value::Null);
        assert_eq!(preset(&updated, "Spare")["label"], Value::Null);
    }

    #[test]
    fn editing_to_the_stored_values_changes_nothing() {
        let same = change(&[("model", json!("claude-haiku")), ("maxTokens", json!(1024))]);
        assert_eq!(with_model_preset_settings(CONFIG, "fast", &same), Ok(None));
    }

    #[test]
    fn editing_refuses_unknown_presets_an_empty_model_and_bad_values() {
        let changes = change(&[("model", json!("x"))]);
        assert_eq!(
            with_model_preset_settings(CONFIG, "ghost", &changes),
            Err(ToggleError::UnknownPreset)
        );
        assert!(matches!(
            with_model_preset_settings(CONFIG, "fast", &change(&[("model", json!(""))])),
            Err(ToggleError::InvalidValue(ref s, _)) if s == "model"
        ));
        assert!(matches!(
            with_model_preset_settings(CONFIG, "fast", &change(&[("temperature", json!(2))])),
            Err(ToggleError::InvalidValue(ref s, _)) if s == "temperature"
        ));
    }

    #[test]
    fn placeholders_survive_an_edit_and_unset_ones_are_refused() {
        let untouched = change(&[("temperature", json!(0.2))]);
        let updated = with_model_preset_settings_in(CONFIG, "Spare", &untouched, &|_| false)
            .unwrap()
            .unwrap();
        assert_eq!(preset(&updated, "Spare")["model"], "${SPARE_MODEL}");
        assert!(updated.contains("${ANTHROPIC_KEY}"));

        let submitted = change(&[("model", json!("${NOT_SET_HERE}"))]);
        let refused = with_model_preset_settings_in(CONFIG, "Spare", &submitted, &|_| false);
        let Err(ToggleError::InvalidValue(setting, reason)) = refused else {
            panic!("expected a refusal");
        };
        assert_eq!(setting, "model");
        assert!(reason.contains("${NOT_SET_HERE}"));
    }

    #[test]
    fn deleting_removes_only_that_preset() {
        let updated = without_model_preset(CONFIG, "Spare").unwrap();

        assert_eq!(preset(&updated, "Spare"), Value::Null);
        assert_eq!(preset(&updated, "fast"), preset(CONFIG, "fast"));
        assert!(validate_config_text(&updated).is_ok());
    }

    #[test]
    fn deleting_refuses_the_active_dream_unknown_and_default_presets() {
        assert!(matches!(
            without_model_preset(CONFIG, "fast"),
            Err(ToggleError::PresetInUse(ref why)) if why.contains("active")
        ));
        assert!(matches!(
            without_model_preset(CONFIG, "dreamy"),
            Err(ToggleError::PresetInUse(ref why)) if why.contains("Dream")
        ));
        assert_eq!(
            without_model_preset(CONFIG, "ghost"),
            Err(ToggleError::UnknownPreset)
        );
        assert_eq!(
            without_model_preset(CONFIG, "default"),
            Err(ToggleError::ReservedName)
        );
    }

    #[test]
    fn activating_a_preset_sets_agents_model_preset() {
        let updated = with_default_model_preset(CONFIG, "Spare").unwrap().unwrap();

        let root: Value = serde_json::from_str(&updated).unwrap();
        assert_eq!(root["agents"]["modelPreset"], "Spare");
        assert_eq!(root["agents"]["model"], "anthropic/claude-opus-4-6");
        assert_eq!(
            model_presets_overview(&updated, false).unwrap().active,
            "Spare"
        );
    }

    #[test]
    fn activating_default_removes_agents_model_preset() {
        let updated = with_default_model_preset(CONFIG, "default")
            .unwrap()
            .unwrap();

        let root: Value = serde_json::from_str(&updated).unwrap();
        assert!(root["agents"].get("modelPreset").is_none());
        assert_eq!(
            root["agents"]["dream"],
            json!({"dreamModelPreset": "dreamy"})
        );
        assert!(model_presets_overview(&updated, false).unwrap().presets[0].active);
    }

    #[test]
    fn activating_follows_the_spelling_in_use_and_creates_agents_when_missing() {
        let snake = r#"{"agents": {"model_preset": "a"}, "modelPresets": {"a": {"model": "m"}, "b": {"model": "n"}}}"#;
        let updated = with_default_model_preset(snake, "b").unwrap().unwrap();
        let root: Value = serde_json::from_str(&updated).unwrap();
        assert_eq!(root["agents"]["model_preset"], "b");
        assert!(root["agents"].get("modelPreset").is_none());

        let no_agents = r#"{"modelPresets": {"a": {"model": "m"}}}"#;
        let updated = with_default_model_preset(no_agents, "a").unwrap().unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&updated).unwrap()["agents"]["modelPreset"],
            "a"
        );
    }

    #[test]
    fn activating_the_active_preset_changes_nothing_and_unknown_ones_are_refused() {
        assert_eq!(with_default_model_preset(CONFIG, "fast"), Ok(None));
        assert_eq!(with_default_model_preset("{}", "default"), Ok(None));
        assert_eq!(
            with_default_model_preset(CONFIG, "ghost"),
            Err(ToggleError::UnknownPreset)
        );
    }

    #[test]
    fn an_added_preset_round_trips_through_the_startup_validation() {
        let updated =
            with_model_preset_added(CONFIG, "openai-gpt-6.1-terra", &terra_values()).unwrap();
        let activated = with_default_model_preset(&updated, "openai-gpt-6.1-terra")
            .unwrap()
            .unwrap();

        assert!(validate_config_text(&activated).is_ok());
        let config: Config = serde_json::from_str(&activated).unwrap();
        let terra = &config.model_presets["openai-gpt-6.1-terra"];
        assert_eq!(terra.model, "gpt-6.1-terra");
        assert_eq!(terra.temperature, 1.0);
        assert_eq!(terra.reasoning_effort.as_deref(), Some("medium"));
        assert_eq!(
            config.agents.model_preset.as_deref(),
            Some("openai-gpt-6.1-terra")
        );
    }

    #[test]
    fn the_live_catalog_resolves_placeholders_and_names_the_default() {
        // PATH is set wherever the tests run.
        let text =
            r#"{"agents": {"modelPreset": "p"}, "modelPresets": {"p": {"model": "${PATH}"}}}"#;

        let live = live_model_presets(text).unwrap();

        assert_eq!(live.default_name.as_deref(), Some("p"));
        let model = &live.presets["p"].model;
        assert!(!model.is_empty() && !model.contains("${"));
    }

    #[test]
    fn the_live_catalog_refuses_unset_variables_and_invalid_files() {
        let unset =
            r#"{"modelPresets": {"p": {"model": "${RUST_BOT_SURELY_UNSET_VARIABLE_4711}"}}}"#;
        let error = live_model_presets(unset).unwrap_err();
        assert!(error.contains("RUST_BOT_SURELY_UNSET_VARIABLE_4711"));

        let dangling = r#"{"agents": {"modelPreset": "ghost"}}"#;
        assert!(live_model_presets(dangling).is_err());
        assert!(live_model_presets("not json").is_err());
    }

    #[test]
    fn the_public_add_reads_the_real_environment() {
        let values = change(&[("model", json!("${RUST_BOT_SURELY_UNSET_VARIABLE_4711}"))]);
        assert!(matches!(
            super::with_model_preset_added(CONFIG, "fresh", &values),
            Err(ToggleError::InvalidValue(..))
        ));
    }
}
