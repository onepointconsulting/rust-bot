//! The Settings dialog's Dream, Skills and Consolidator pages: one settings form per
//! fixed object of `config.json`, and the pure edit of the config text behind each.
//!
//! | Page         | Object in `config.json`                        | Schema              |
//! |--------------|------------------------------------------------|---------------------|
//! | Dream        | `agents.dream`                                 | [`DreamConfig`]     |
//! | Skills       | `tools.skills`                                 | [`SkillsToolConfig`]|
//! | Consolidator | three settings directly under `agents`         | [`ConsolidatorSettings`] |
//!
//! Like the other forms ([`super::settings_form`]) the edits work on the raw config text,
//! so `${VAR}` placeholders, key order and every key a form does not edit survive, and a
//! setting keeps the spelling the file already uses (`max_batch_size` stays snake_case).
//! The forms show what the bot would run with: the stored value, else the default.
//! Clearing a text or choice removes the key, so the default applies again. Changes
//! apply after a restart: Dream, the skills and the Consolidator are set up at startup.

use std::collections::HashMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use super::config_toggles::{ToggleError, child_object_mut, parse_root};
use super::model_preset_form::{rendered_and_checked, typed_config};
use super::settings_form::{
    SettingSpellings, UNSET_VARIABLE_EXPLANATION, check_placeholders, is_blank, process_env_is_set,
    remove_value, schema_properties, store_value, stored_value,
};
use crate::agent::tools::tool_settings_api::{
    FieldKind, ToolSettingsSummary, check_value, field_kind, summarize_field,
};
use crate::config::schema::{AgentsConfig, DreamConfig, SkillsToolConfig};

/// Accepted spellings of the top-level `agents` object.
const AGENTS_KEYS: &[&str] = &["agents"];
/// Accepted spellings of the top-level `tools` object.
const TOOLS_KEYS: &[&str] = &["tools"];
/// Accepted spellings of `agents.dream`.
const DREAM_KEYS: &[&str] = &["dream"];
/// Accepted spellings of `tools.skills`.
const SKILLS_KEYS: &[&str] = &["skills"];

/// The Dream form's settings with every spelling `DreamConfig` accepts.
const DREAM_SPELLINGS: SettingSpellings = &[
    &["intervalH", "interval_h"],
    &["modelOverride", "model", "model_override"],
    &["dreamModelPreset", "dream_model_preset"],
    &["maxBatchSize", "max_batch_size"],
    &["maxIterations", "max_iterations"],
    &["learnSkills", "learn_skills"],
];

/// The Skills form's settings with every spelling `SkillsToolConfig` accepts.
const SKILLS_SPELLINGS: SettingSpellings = &[
    &["enabled", "enable"],
    &["requireApproval", "require_approval"],
    &["maxSkillChars", "max_skill_chars"],
    &["maxDescriptionChars", "max_description_chars"],
];

/// The Consolidator form's settings with every spelling `AgentsConfig` accepts.
const CONSOLIDATOR_SPELLINGS: SettingSpellings = &[
    &["contextWindowTokens", "context_window_tokens"],
    &[
        "sessionTtlMinutes",
        "idleCompactAfterMinutes",
        "session_ttl_minutes",
        "idle_compact_after_minutes",
    ],
    &[
        "idleCompactCheckIntervalSeconds",
        "idle_compact_check_interval_seconds",
    ],
];

/// Name of the Dream setting whose choices are the configured model presets.
const DREAM_MODEL_PRESET: &str = "dreamModelPreset";

/// The Consolidator's settings in `agents`, as a form: when sessions are summarised
/// into `memory/history.jsonl`, the history Dream learns from. Only describes the form;
/// the values live in [`AgentsConfig`], which also supplies the defaults.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", default)]
#[schemars(extend("x-title" = "Consolidator settings"))]
pub struct ConsolidatorSettings {
    /// The model's context window. When a session gets close to it, the Consolidator
    /// summarises the older messages into the history. Must be ≥ 1.
    #[schemars(range(min = 1), extend("x-label" = "Context window tokens"))]
    pub context_window_tokens: u64,

    /// Summarise a session left idle this long, in minutes. 0 turns idle compaction off.
    #[schemars(extend("x-label" = "Idle compaction after (minutes)"))]
    pub session_ttl_minutes: u32,

    /// How often to look for idle sessions to summarise, in seconds. Must be ≥ 1.
    #[schemars(range(min = 1), extend("x-label" = "Idle check interval (seconds)"))]
    pub idle_compact_check_interval_seconds: u32,
}

impl Default for ConsolidatorSettings {
    fn default() -> Self {
        let agents = AgentsConfig::default();
        Self {
            context_window_tokens: agents.context_window_tokens,
            session_ttl_minutes: agents.session_ttl_minutes,
            idle_compact_check_interval_seconds: agents.idle_compact_check_interval_seconds,
        }
    }
}

/// A page of the Settings dialog backed by one fixed object of `config.json`. The key is
/// the section's name on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemorySettingsSection {
    /// `agents.dream`.
    Dream,
    /// `tools.skills`.
    Skills,
    /// The compaction settings directly under `agents`.
    Consolidator,
}

impl MemorySettingsSection {
    /// Every section, in the order the dialog lists them.
    pub const ALL: [Self; 3] = [Self::Dream, Self::Skills, Self::Consolidator];

    /// The section's name on the wire.
    pub fn key(self) -> &'static str {
        match self {
            Self::Dream => "dream",
            Self::Skills => "skills",
            Self::Consolidator => "consolidator",
        }
    }

    /// The heading of the section's form.
    fn title(self) -> &'static str {
        match self {
            Self::Dream => "Dream settings",
            Self::Skills => "Skill settings",
            Self::Consolidator => "Consolidator settings",
        }
    }

    /// The section named `key` on the wire.
    pub fn parse(key: &str) -> Result<Self, ToggleError> {
        Self::ALL
            .into_iter()
            .find(|section| section.key() == key)
            .ok_or(ToggleError::UnknownSettingsSection)
    }

    /// The accepted spellings of each object on the way from the root to the section.
    fn path(self) -> &'static [&'static [&'static str]] {
        match self {
            Self::Dream => &[AGENTS_KEYS, DREAM_KEYS],
            Self::Skills => &[TOOLS_KEYS, SKILLS_KEYS],
            Self::Consolidator => &[AGENTS_KEYS],
        }
    }

    /// Every spelling of each setting of the form.
    fn spellings(self) -> SettingSpellings {
        match self {
            Self::Dream => DREAM_SPELLINGS,
            Self::Skills => SKILLS_SPELLINGS,
            Self::Consolidator => CONSOLIDATOR_SPELLINGS,
        }
    }

    /// The schema properties the form edits, in declaration order. Dream's model preset
    /// offers the configured `preset_names`, after an empty choice for "none".
    fn form_properties(self, preset_names: &[String]) -> Map<String, Value> {
        match self {
            Self::Dream => with_preset_choices(schema_properties::<DreamConfig>(), preset_names),
            Self::Skills => schema_properties::<SkillsToolConfig>(),
            Self::Consolidator => schema_properties::<ConsolidatorSettings>(),
        }
    }
}

/// `properties` with the Dream model preset turned into a choice of `preset_names`,
/// preceded by `""` (no preset).
fn with_preset_choices(
    mut properties: Map<String, Value>,
    preset_names: &[String],
) -> Map<String, Value> {
    if let Some(Value::Object(preset)) = properties.get_mut(DREAM_MODEL_PRESET) {
        let choices: Vec<&str> = std::iter::once("")
            .chain(preset_names.iter().map(String::as_str))
            .collect();
        preset.insert("x-choices".to_string(), json!(choices));
    }
    properties
}

/// The names of the model presets in the config `text`, sorted.
fn preset_names(text: &str) -> Result<Vec<String>, ToggleError> {
    let mut names: Vec<String> = typed_config(text)?.model_presets.into_keys().collect();
    names.sort();
    Ok(names)
}

/// The child of `object` under the first of `keys` it has, if that child is an object.
fn child_object<'a>(
    object: &'a Map<String, Value>,
    keys: &[&str],
) -> Option<&'a Map<String, Value>> {
    keys.iter().find_map(|key| object.get(*key))?.as_object()
}

/// The object of `section` in the parsed config `root`, when every object on its path
/// exists.
fn section_entry(root: &Value, section: MemorySettingsSection) -> Option<&Map<String, Value>> {
    section
        .path()
        .iter()
        .try_fold(root.as_object()?, |object, keys| child_object(object, keys))
}

/// The object of `section` in the parsed config `root`, created (with the objects on its
/// path) when missing. Refused when something on the path is not an object.
fn section_entry_mut(
    root: &mut Value,
    section: MemorySettingsSection,
) -> Result<&mut Map<String, Value>, ToggleError> {
    let root_object = root.as_object_mut().expect("parse_root checked this");
    section
        .path()
        .iter()
        .try_fold(root_object, |object, keys| child_object_mut(object, keys))
}

/// What the form shows for `setting`: the stored value, else the schema default, so the
/// page reads as what the bot runs with.
fn effective_value(
    table: SettingSpellings,
    entry: Option<&Map<String, Value>>,
    setting: &str,
    property: &Value,
) -> Value {
    entry
        .and_then(|entry| stored_value(table, entry, setting))
        .or_else(|| property.get("default"))
        .cloned()
        .unwrap_or(Value::Null)
}

/// The settings form of `section` for the parsed config `root`.
fn section_form(
    root: &Value,
    section: MemorySettingsSection,
    preset_names: &[String],
) -> ToolSettingsSummary {
    let entry = section_entry(root, section);
    let fields = section
        .form_properties(preset_names)
        .iter()
        .filter_map(|(setting, property)| {
            let value = effective_value(section.spellings(), entry, setting, property);
            summarize_field(setting, property, &value)
        })
        .collect();
    ToolSettingsSummary {
        name: section.key().to_string(),
        title: section.title().to_string(),
        description: String::new(),
        enabled: None,
        tool_names: Vec::new(),
        fields,
    }
}

/// One form per section for the config `text`, in [`MemorySettingsSection::ALL`] order.
/// None of these settings is a secret.
pub fn memory_settings_overview(text: &str) -> Result<Vec<ToolSettingsSummary>, ToggleError> {
    let root = parse_root(text)?;
    let preset_names = preset_names(text)?;
    Ok(MemorySettingsSection::ALL
        .into_iter()
        .map(|section| section_form(&root, section, &preset_names))
        .collect())
}

/// `text` with the submitted `changes` (setting name -> value, as the form names them)
/// applied to the object of `section`, under the key spellings it already uses; keys the
/// form does not know are kept. A blank text or choice removes the setting, so its
/// default applies. Nothing is applied unless every change is valid and the bot could
/// start with the result. `None` when the file already says all of it.
pub fn with_memory_settings(
    text: &str,
    section: MemorySettingsSection,
    changes: &HashMap<String, Value>,
) -> Result<Option<String>, ToggleError> {
    with_memory_settings_in(text, section, changes, &process_env_is_set)
}

/// [`with_memory_settings`] with the environment lookup passed in.
fn with_memory_settings_in(
    text: &str,
    section: MemorySettingsSection,
    changes: &HashMap<String, Value>,
    env_is_set: &dyn Fn(&str) -> bool,
) -> Result<Option<String>, ToggleError> {
    let properties = section.form_properties(&preset_names(text)?);
    if let Some(unknown) = changes
        .keys()
        .find(|setting| !properties.contains_key(*setting))
    {
        return Err(ToggleError::UnknownField(unknown.clone()));
    }
    let table = section.spellings();
    let mut root = parse_root(text)?;
    let entry = section_entry_mut(&mut root, section)?;
    let before = entry.clone();
    for (setting, property) in &properties {
        let Some(value) = changes.get(setting) else {
            continue;
        };
        let kind =
            field_kind(property).ok_or_else(|| ToggleError::UnknownField(setting.clone()))?;
        if kind != FieldKind::Bool && is_blank(value) {
            remove_value(table, entry, setting);
            continue;
        }
        let checked = check_value(&kind, value)
            .map_err(|reason| ToggleError::InvalidValue(setting.clone(), reason))?;
        check_placeholders(setting, &checked, env_is_set, UNSET_VARIABLE_EXPLANATION)?;
        store_value(table, entry, setting, checked);
    }
    if *entry == before {
        return Ok(None);
    }
    rendered_and_checked(&root, text).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tools::tool_settings_api::ToolFieldSummary;

    /// A config with snake_case Dream keys, a placeholder elsewhere and one preset.
    fn config_text() -> String {
        r#"{
  "agents": {
    "model": "${MODEL_NAME}",
    "dream": {"interval_h": 4, "custom": true},
    "sessionTtlMinutes": 30
  },
  "modelPresets": {
    "cheap": {"model": "small-model", "provider": "openrouter"}
  }
}
"#
        .to_string()
    }

    fn change(pairs: &[(&str, Value)]) -> HashMap<String, Value> {
        pairs
            .iter()
            .map(|(name, value)| (name.to_string(), value.clone()))
            .collect()
    }

    fn all_set(_: &str) -> bool {
        true
    }

    fn edited(
        text: &str,
        section: MemorySettingsSection,
        changes: &HashMap<String, Value>,
    ) -> Result<Option<String>, ToggleError> {
        with_memory_settings_in(text, section, changes, &all_set)
    }

    fn json_of(text: &str) -> Value {
        serde_json::from_str(text).unwrap()
    }

    fn form(section: MemorySettingsSection) -> ToolSettingsSummary {
        memory_settings_overview(&config_text())
            .unwrap()
            .into_iter()
            .find(|form| form.name == section.key())
            .unwrap()
    }

    fn field<'a>(form: &'a ToolSettingsSummary, name: &str) -> &'a ToolFieldSummary {
        form.fields.iter().find(|field| field.name == name).unwrap()
    }

    #[test]
    fn sections_are_named_by_their_wire_key() {
        for section in MemorySettingsSection::ALL {
            assert_eq!(MemorySettingsSection::parse(section.key()), Ok(section));
        }
        assert_eq!(
            MemorySettingsSection::parse("memory"),
            Err(ToggleError::UnknownSettingsSection)
        );
    }

    #[test]
    fn the_overview_has_one_form_per_section_in_menu_order() {
        let names: Vec<String> = memory_settings_overview(&config_text())
            .unwrap()
            .into_iter()
            .map(|form| form.name)
            .collect();
        assert_eq!(names, ["dream", "skills", "consolidator"]);
    }

    #[test]
    fn forms_show_stored_values_under_any_spelling_and_defaults_otherwise() {
        let dream = form(MemorySettingsSection::Dream);
        assert_eq!(field(&dream, "intervalH").value, Some(json!(4)));
        assert_eq!(field(&dream, "maxBatchSize").value, Some(json!(20)));
        assert_eq!(field(&dream, "learnSkills").value, Some(json!(false)));
        assert_eq!(field(&dream, "intervalH").label, "Interval (hours)");

        let consolidator = form(MemorySettingsSection::Consolidator);
        assert_eq!(
            field(&consolidator, "sessionTtlMinutes").value,
            Some(json!(30))
        );
        assert_eq!(
            field(&consolidator, "idleCompactCheckIntervalSeconds").value,
            Some(json!(60))
        );

        let skills = form(MemorySettingsSection::Skills);
        assert_eq!(field(&skills, "enabled").value, Some(json!(false)));
        assert_eq!(field(&skills, "maxSkillChars").value, Some(json!(100_000)));
    }

    #[test]
    fn the_legacy_cron_schedule_is_not_on_the_dream_form() {
        let dream = form(MemorySettingsSection::Dream);
        assert!(dream.fields.iter().all(|field| field.name != "cron"));
    }

    #[test]
    fn the_dream_model_preset_offers_none_and_the_configured_presets() {
        let dream = form(MemorySettingsSection::Dream);
        assert_eq!(
            field(&dream, DREAM_MODEL_PRESET).kind,
            FieldKind::Choice {
                choices: vec![String::new(), "cheap".to_string()]
            }
        );
    }

    #[test]
    fn editing_keeps_the_spelling_in_use_unknown_keys_and_placeholders() {
        let text = config_text();
        let updated = edited(
            &text,
            MemorySettingsSection::Dream,
            &change(&[("intervalH", json!(6)), ("learnSkills", json!(true))]),
        )
        .unwrap()
        .unwrap();
        let root = json_of(&updated);
        assert_eq!(
            root["agents"]["dream"],
            json!({"interval_h": 6, "custom": true, "learnSkills": true})
        );
        assert_eq!(root["agents"]["model"], "${MODEL_NAME}");
    }

    #[test]
    fn a_missing_section_object_is_created() {
        let updated = edited(
            &config_text(),
            MemorySettingsSection::Skills,
            &change(&[("requireApproval", json!(true))]),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            json_of(&updated)["tools"]["skills"],
            json!({"requireApproval": true})
        );
    }

    #[test]
    fn consolidator_settings_are_written_directly_under_agents() {
        let updated = edited(
            &config_text(),
            MemorySettingsSection::Consolidator,
            &change(&[
                ("sessionTtlMinutes", json!(0)),
                ("contextWindowTokens", json!(64_000)),
            ]),
        )
        .unwrap()
        .unwrap();
        let agents = &json_of(&updated)["agents"];
        assert_eq!(agents["sessionTtlMinutes"], 0);
        assert_eq!(agents["contextWindowTokens"], 64_000);
    }

    #[test]
    fn choosing_a_preset_and_then_none_removes_the_key_again() {
        let with_preset = edited(
            &config_text(),
            MemorySettingsSection::Dream,
            &change(&[(DREAM_MODEL_PRESET, json!("cheap"))]),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            json_of(&with_preset)["agents"]["dream"][DREAM_MODEL_PRESET],
            "cheap"
        );

        let without = edited(
            &with_preset,
            MemorySettingsSection::Dream,
            &change(&[(DREAM_MODEL_PRESET, json!(""))]),
        )
        .unwrap()
        .unwrap();
        assert!(
            json_of(&without)["agents"]["dream"]
                .get(DREAM_MODEL_PRESET)
                .is_none()
        );
    }

    #[test]
    fn submitting_the_stored_values_changes_nothing() {
        let unchanged = edited(
            &config_text(),
            MemorySettingsSection::Dream,
            &change(&[("intervalH", json!(4))]),
        );
        assert_eq!(unchanged, Ok(None));
    }

    #[test]
    fn values_below_the_minimum_unknown_settings_and_unknown_presets_are_refused() {
        let text = config_text();
        let dream = MemorySettingsSection::Dream;
        assert!(matches!(
            edited(&text, dream, &change(&[("intervalH", json!(0))])),
            Err(ToggleError::InvalidValue(..))
        ));
        assert!(matches!(
            edited(
                &text,
                MemorySettingsSection::Skills,
                &change(&[("maxSkillChars", json!(999))])
            ),
            Err(ToggleError::InvalidValue(..))
        ));
        assert!(matches!(
            edited(&text, dream, &change(&[("cron", json!("0 * * * *"))])),
            Err(ToggleError::UnknownField(_))
        ));
        assert!(matches!(
            edited(
                &text,
                dream,
                &change(&[(DREAM_MODEL_PRESET, json!("missing"))])
            ),
            Err(ToggleError::InvalidValue(..))
        ));
    }

    #[test]
    fn a_placeholder_for_an_unset_variable_is_refused() {
        let result = with_memory_settings_in(
            &config_text(),
            MemorySettingsSection::Dream,
            &change(&[("modelOverride", json!("${DREAM_MODEL}"))]),
            &|_| false,
        );
        assert!(matches!(result, Err(ToggleError::InvalidValue(..))));
    }

    #[test]
    fn the_consolidator_form_defaults_come_from_the_agent_defaults() {
        let defaults = ConsolidatorSettings::default();
        let agents = AgentsConfig::default();
        assert_eq!(defaults.context_window_tokens, agents.context_window_tokens);
        assert_eq!(defaults.session_ttl_minutes, agents.session_ttl_minutes);
    }
}
