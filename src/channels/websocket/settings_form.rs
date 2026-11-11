//! Helpers shared by the Settings dialog forms that edit one JSON object of `config.json`
//! in place: an MCP server entry ([`super::mcp_server_form`]) and a model preset
//! ([`super::model_preset_form`]).
//!
//! A form is generated from the JSON Schema of the entry's config struct, with the same
//! field helpers as the tool section forms
//! ([`crate::agent::tools::tool_settings_api`]). Values are read from and written to the
//! parsed raw text, so `${ENV_VAR}` placeholders and every key the form does not edit stay
//! exactly as written. A setting may have several accepted spellings (`maxTokens` and
//! `max_tokens`); a [`SettingSpellings`] table lists them, the form's own name first, and
//! an edit keeps the spelling the file already uses.

use schemars::{JsonSchema, schema_for};
use serde_json::{Map, Value};

use super::config_toggles::ToggleError;
use crate::agent::tools::tool_settings_api::{
    FieldKind, ToolFieldSummary, field_kind, summarize_field,
};
use crate::config::loader::referenced_env_vars;

/// Setting names (as the form calls them) with every spelling the configuration accepts,
/// the form's own name first. Settings not listed have a single spelling.
pub(super) type SettingSpellings = &'static [&'static [&'static str]];

/// Every spelling of the setting `name` according to `table`, the form's own name first.
pub(super) fn spellings(table: SettingSpellings, name: &str) -> Vec<String> {
    match table.iter().find(|spellings| spellings[0] == name) {
        Some(spellings) => spellings.iter().map(|s| s.to_string()).collect(),
        None => vec![name.to_string()],
    }
}

/// The value `entry` stores for the setting `name` under any of its spellings.
pub(super) fn stored_value<'a>(
    table: SettingSpellings,
    entry: &'a Map<String, Value>,
    name: &str,
) -> Option<&'a Value> {
    spellings(table, name).iter().find_map(|key| entry.get(key))
}

/// Write `value` for the setting `name` into `entry`, under the spelling the entry already
/// uses (the form's own name otherwise), so a user's snake_case key is not duplicated.
pub(super) fn store_value(
    table: SettingSpellings,
    entry: &mut Map<String, Value>,
    name: &str,
    value: Value,
) {
    let spellings = spellings(table, name);
    let key = spellings
        .iter()
        .find(|key| entry.contains_key(key.as_str()))
        .unwrap_or(&spellings[0])
        .clone();
    entry.insert(key, value);
}

/// Remove the setting `name` from `entry` under every spelling.
pub(super) fn remove_value(table: SettingSpellings, entry: &mut Map<String, Value>, name: &str) {
    for key in spellings(table, name) {
        entry.remove(&key);
    }
}

/// The top-level schema properties of the config struct `T`, in declaration order.
pub(super) fn schema_properties<T: JsonSchema>() -> Map<String, Value> {
    let schema = serde_json::to_value(schema_for!(T)).unwrap_or(Value::Null);
    schema["properties"]
        .as_object()
        .cloned()
        .unwrap_or_default()
}

/// The value a form shows for `setting`: what `entry` stores for it, else (for an entry
/// that leaves it out, or an entry not added yet) the schema default.
///
/// A number or text the file leaves out is shown empty instead, so the browser displays
/// the default as a placeholder: shown as a value, typing the default itself would look
/// like "no change" and never be written. Lists, maps and choices keep their default
/// (an empty `enabledTools` would read as "no tools"; choices label their default).
pub(super) fn shown_value(
    table: SettingSpellings,
    entry: Option<&Map<String, Value>>,
    setting: &str,
    property: &Value,
) -> Value {
    if let Some(stored) = entry.and_then(|entry| stored_value(table, entry, setting)) {
        return stored.clone();
    }
    match field_kind(property) {
        Some(FieldKind::Integer { .. } | FieldKind::Number { .. } | FieldKind::Text) => Value::Null,
        _ => property.get("default").cloned().unwrap_or(Value::Null),
    }
}

/// One form field per property the form can edit, showing [`shown_value`].
pub(super) fn form_fields(
    properties: &Map<String, Value>,
    table: SettingSpellings,
    entry: Option<&Map<String, Value>>,
) -> Vec<ToolFieldSummary> {
    properties
        .iter()
        .filter_map(|(setting, property)| {
            summarize_field(
                setting,
                property,
                &shown_value(table, entry, setting, property),
            )
        })
        .collect()
}

/// Whether the environment variable `name` is set on this machine.
pub(super) fn process_env_is_set(name: &str) -> bool {
    std::env::var_os(name).is_some()
}

/// Refuse a submitted value naming a `${VAR}` placeholder whose variable is not set: the
/// file format accepts it, but the bot would not start. The message names the variable
/// (never the value), followed by `explanation`, which tells the user what to do.
pub(super) fn check_placeholders(
    setting: &str,
    value: &Value,
    env_is_set: &dyn Fn(&str) -> bool,
    explanation: &str,
) -> Result<(), ToggleError> {
    match referenced_env_vars(value)
        .into_iter()
        .find(|variable| !env_is_set(variable))
    {
        Some(variable) => Err(ToggleError::InvalidValue(
            setting.to_string(),
            format!("uses ${{{variable}}}, {explanation}"),
        )),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const TABLE: SettingSpellings = &[&["maxTokens", "max_tokens"]];

    fn entry(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    #[test]
    fn spellings_list_the_form_name_first_and_default_to_the_name_itself() {
        assert_eq!(spellings(TABLE, "maxTokens"), ["maxTokens", "max_tokens"]);
        assert_eq!(spellings(TABLE, "model"), ["model"]);
    }

    #[test]
    fn values_are_read_and_written_under_the_spelling_in_use() {
        let mut snake = entry(json!({"max_tokens": 5}));
        assert_eq!(stored_value(TABLE, &snake, "maxTokens"), Some(&json!(5)));

        store_value(TABLE, &mut snake, "maxTokens", json!(7));
        assert_eq!(snake, entry(json!({"max_tokens": 7})));

        let mut empty = Map::new();
        store_value(TABLE, &mut empty, "maxTokens", json!(7));
        assert_eq!(empty, entry(json!({"maxTokens": 7})));
    }

    #[test]
    fn removing_a_value_removes_every_spelling() {
        let mut both = entry(json!({"max_tokens": 5, "maxTokens": 6, "model": "m"}));
        remove_value(TABLE, &mut both, "maxTokens");
        assert_eq!(both, entry(json!({"model": "m"})));
    }

    #[test]
    fn absent_numbers_and_text_are_shown_empty_and_choices_show_their_default() {
        let number = json!({"type": "number", "default": 0.1});
        let text = json!({"type": "string", "default": "x"});
        let choice = json!({"type": "string", "x-choices": ["a", "b"], "default": "b"});

        assert_eq!(shown_value(TABLE, None, "t", &number), Value::Null);
        assert_eq!(shown_value(TABLE, None, "t", &text), Value::Null);
        assert_eq!(shown_value(TABLE, None, "c", &choice), json!("b"));
        let stored = entry(json!({"t": 0.5}));
        assert_eq!(shown_value(TABLE, Some(&stored), "t", &number), json!(0.5));
    }

    #[test]
    fn a_placeholder_for_an_unset_variable_is_refused_with_the_explanation() {
        let value = json!("Bearer ${MISSING}");
        let error = check_placeholders("headers", &value, &|_| false, "set it.").unwrap_err();
        assert_eq!(
            error,
            ToggleError::InvalidValue("headers".into(), "uses ${MISSING}, set it.".into())
        );
        assert!(check_placeholders("headers", &value, &|_| true, "set it.").is_ok());
    }
}
