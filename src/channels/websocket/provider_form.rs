//! The LLM providers of `config.json` (`providers.*`) for the WebUI's Settings →
//! Providers page: the rows of the list, the settings forms, and the pure edits of the
//! config text (add, change, delete, forget the stored key).
//!
//! Two kinds of provider exist:
//!
//! * **Dedicated** providers ([`DEDICATED_PROVIDERS`]): a built-in slot such as
//!   `providers.anthropic`. Deleting one resets the slot to `{"apiKey": ""}`.
//! * **Custom** providers: a named entry in `providers.customProviders` (any OpenAI
//!   compatible, Responses or Anthropic endpoint), and the legacy `providers.custom`
//!   slot, which is listed as a custom provider named `custom`.
//!
//! Like [`super::model_preset_form`], the edits work on the raw config text so `${VAR}`
//! placeholders, key spellings and key order survive. **API keys never leave this
//! module**: rows carry only whether a key is stored, a blank submitted key never
//! overwrites the stored one, and a form field for a secret has no value.

use std::collections::HashMap;

use serde::Serialize;
use serde_json::{Map, Value};

use super::config_file::validate_config_text;
use super::config_toggles::{ToggleError, child_object_mut, is_valid_name, parse_root};
use super::model_preset_form::{rendered_and_checked, typed_config};
use super::settings_form::{
    SettingSpellings, check_placeholders, form_fields, process_env_is_set, remove_value,
    schema_properties, store_value, stored_value,
};
use crate::agent::tools::tool_settings_api::{
    FieldKind, ToolFieldSummary, ToolSettingsSummary, check_value, field_kind,
};
use crate::config::loader::resolve_env_vars_in;
use crate::config::schema::{
    Config, PROVIDER_BACKEND_CHOICES, ProviderBackend, ProviderConfig, ProvidersConfig,
    is_reserved_provider_name,
};
use crate::providers::model_catalog::default_api_base_of;
use crate::providers::registry::find_by_name;

/// The providers the "Dedicated model provider" tab offers: the ones the factory can
/// build from a built-in slot, in the order the tab lists them.
pub const DEDICATED_PROVIDERS: [&str; 4] = ["openai", "anthropic", "openrouter", "together"];

/// The legacy single custom slot (`providers.custom`), listed as a custom provider.
const LEGACY_CUSTOM_SLOT: &str = "custom";

/// Spellings the configuration accepts for the top-level `providers` section.
const PROVIDERS_KEYS: &[&str] = &["providers"];
/// Spellings the configuration accepts for the map of named custom providers.
const CUSTOM_PROVIDERS_KEYS: &[&str] = &["customProviders", "custom_providers"];

/// Setting names (as the form calls them) with every spelling a provider entry accepts.
const SETTING_SPELLINGS: SettingSpellings = &[
    &["apiKey", "api_key"],
    &["apiBase", "api_base"],
    &["extraHeaders", "extra_headers"],
    &["providerBackEnd", "provider_back_end"],
];

const API_KEY: &str = "apiKey";
const API_BASE: &str = "apiBase";
const EXTRA_HEADERS: &str = "extraHeaders";
const PROVIDER_BACK_END: &str = "providerBackEnd";

/// What to do about a `${VAR}` placeholder whose variable is not set; shown after its name.
const UNSET_VARIABLE_EXPLANATION: &str = "which is not set in the environment of the process running rust-bot. Set the variable there before starting rust-bot, or write the value here directly.";

/// Which tab of the add panel a provider comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    /// A built-in slot (`providers.anthropic`, ...).
    Dedicated,
    /// A named entry of `providers.customProviders` (or the legacy `providers.custom`).
    Custom,
}

impl ProviderKind {
    /// The kind a client names on the wire (`dedicated` or `custom`).
    pub fn parse(wire_name: &str) -> Option<Self> {
        match wire_name {
            "dedicated" => Some(Self::Dedicated),
            "custom" => Some(Self::Custom),
            _ => None,
        }
    }
}

/// One row of the Providers page. Never holds an API key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProviderRow {
    /// The slot name (`anthropic`) or the key in `customProviders`.
    pub name: String,
    pub kind: ProviderKind,
    /// The name shown for the row: the registry's display name for a dedicated provider.
    pub label: String,
    /// The configured Base URL, when one is set.
    pub api_base: Option<String>,
    /// The Base URL used when none is set, when the provider has a known one.
    pub default_api_base: Option<String>,
    /// The configured `providerBackEnd` (`openai_responses`, ...); `None`: automatic.
    pub backend: Option<String>,
    /// The API used when `backend` is not set.
    pub backend_default: String,
    /// Whether an API key is stored (a `${VAR}` placeholder counts).
    pub api_key_configured: bool,
    /// The environment variable that supplies the key when none is stored.
    pub env_var: Option<String>,
    /// Whether that variable is set (and not blank).
    pub env_var_set: bool,
    /// What uses the provider: `agents.provider` and the model presets that name it.
    pub used_by: Vec<String>,
    /// Whether Delete is offered: nothing uses the provider.
    pub deletable: bool,
    /// Why Delete is not offered, empty when it is.
    pub delete_blocked_reason: String,
    /// The settings form, only for connections that may change the configuration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub form: Option<ToolSettingsSummary>,
}

/// A provider of the "Dedicated model provider" drop-down.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DedicatedProviderChoice {
    pub name: String,
    pub label: String,
    pub env_var: String,
    pub env_var_set: bool,
    pub default_api_base: Option<String>,
    /// Whether it already has a row (it cannot be added twice).
    pub configured: bool,
}

/// One entry of the API protocol drop-down.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BackendChoice {
    /// The `providerBackEnd` value; empty means automatic.
    pub value: String,
    pub label: String,
}

/// Everything the Providers page shows, read from the config text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProvidersOverview {
    /// The configured dedicated providers (sorted A–Z by name) followed by the custom
    /// ones (sorted A–Z ignoring case).
    pub rows: Vec<ProviderRow>,
    /// The providers the Dedicated tab offers.
    pub dedicated_choices: Vec<DedicatedProviderChoice>,
    /// The API protocols, automatic first.
    pub backend_choices: Vec<BackendChoice>,
    /// The blank form of the add panel; only for connections that may change the
    /// configuration. Both tabs pick their fields from it by name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_provider_form: Option<ToolSettingsSummary>,
}

/// Whether the environment variable `name` is set to something other than blanks.
pub fn process_env_has_value(name: &str) -> bool {
    std::env::var(name).is_ok_and(|value| !value.trim().is_empty())
}

/// The schema properties the form edits, in declaration order.
fn form_properties() -> Map<String, Value> {
    schema_properties::<ProviderConfig>()
}

/// The API protocol drop-down: automatic, then every implementation.
fn backend_choices() -> Vec<BackendChoice> {
    PROVIDER_BACKEND_CHOICES
        .iter()
        .map(|value| BackendChoice {
            value: value.to_string(),
            label: ProviderBackend::ALL
                .iter()
                .find(|backend| backend.config_name() == *value)
                .map_or("Automatic", |backend| backend.description())
                .to_string(),
        })
        .collect()
}

/// The slot `name` of `config.providers` when it is one a row can show: a dedicated
/// provider or the legacy custom slot.
fn built_in_slot<'a>(config: &'a Config, name: &str) -> Option<&'a ProviderConfig> {
    (DEDICATED_PROVIDERS.contains(&name) || name == LEGACY_CUSTOM_SLOT)
        .then(|| config.providers.get_by_name(name))
        .flatten()
}

/// Whether a built-in slot holds anything: a key, a Base URL, headers or a protocol.
fn slot_is_configured(slot: &ProviderConfig) -> bool {
    !slot.api_key.trim().is_empty()
        || slot
            .api_base
            .as_deref()
            .is_some_and(|base| !base.trim().is_empty())
        || slot
            .extra_headers
            .as_ref()
            .is_some_and(|headers| !headers.is_empty())
        || slot.provider_back_end.is_some()
}

/// What uses the provider `name`: `agents.provider` and each model preset that names it.
fn provider_users(config: &Config, name: &str) -> Vec<String> {
    let mut users = Vec::new();
    if config.agents.provider == name {
        users.push("agents.provider".to_string());
    }
    let mut presets: Vec<&String> = config
        .model_presets
        .iter()
        .filter(|(_, preset)| preset.provider == name)
        .map(|(preset_name, _)| preset_name)
        .collect();
    presets.sort_by_key(|preset_name| preset_name.to_lowercase());
    users.extend(
        presets
            .into_iter()
            .map(|preset| format!("model preset '{preset}'")),
    );
    users
}

/// The explanation shown when `users` stop the provider from being deleted.
fn in_use_reason(users: &[String]) -> String {
    format!(
        "it is used by {}. Switch those to another provider first.",
        users.join(", ")
    )
}

/// The settings form of a provider whose stored entry is `entry` (`None`: not added yet).
/// The key field is a secret (no value) that an exported `env_var` also satisfies;
/// the protocol shows `""` (automatic) when unset.
fn provider_form(
    name: &str,
    title: &str,
    entry: Option<&Map<String, Value>>,
    env_var: &str,
    env_is_set: &dyn Fn(&str) -> bool,
) -> ToolSettingsSummary {
    let mut fields: Vec<ToolFieldSummary> =
        form_fields(&form_properties(), SETTING_SPELLINGS, entry);
    for field in &mut fields {
        match field.name.as_str() {
            API_KEY => {
                field.env_var = env_var.to_string();
                let stored = entry
                    .and_then(|entry| stored_value(SETTING_SPELLINGS, entry, API_KEY))
                    .and_then(Value::as_str)
                    .is_some_and(|key| !key.trim().is_empty());
                field.configured = stored || (!env_var.is_empty() && env_is_set(env_var));
            }
            PROVIDER_BACK_END if field.value.as_ref().is_none_or(Value::is_null) => {
                field.value = Some(Value::String(String::new()));
            }
            _ => {}
        }
    }
    ToolSettingsSummary {
        name: name.to_string(),
        title: title.to_string(),
        description: String::new(),
        enabled: None,
        tool_names: Vec::new(),
        fields,
    }
}

/// The row of the provider `name` stored as `slot` (and `entry`, its raw object).
fn provider_row(
    config: &Config,
    name: &str,
    kind: ProviderKind,
    slot: &ProviderConfig,
    entry: Option<&Map<String, Value>>,
    with_forms: bool,
    env_is_set: &dyn Fn(&str) -> bool,
) -> ProviderRow {
    let spec = find_by_name(name);
    let env_var = spec
        .as_ref()
        .map(|spec| spec.env_key.clone())
        .filter(|env_key| !env_key.is_empty());
    let label = match (&spec, kind) {
        (Some(spec), _) => spec.display_name.clone(),
        (None, _) => name.to_string(),
    };
    let users = provider_users(config, name);
    ProviderRow {
        name: name.to_string(),
        kind,
        form: with_forms.then(|| {
            provider_form(
                name,
                &label,
                entry,
                env_var.as_deref().unwrap_or_default(),
                env_is_set,
            )
        }),
        label,
        api_base: slot.api_base.clone().filter(|base| !base.trim().is_empty()),
        default_api_base: spec.as_ref().and_then(default_api_base_of),
        backend: slot
            .provider_back_end
            .map(|backend| backend.config_name().to_string()),
        backend_default: spec.as_ref().map_or_else(
            || ProviderBackend::OpenAiCompat.config_name().to_string(),
            |spec| spec.backend.clone(),
        ),
        api_key_configured: !slot.api_key.trim().is_empty(),
        env_var_set: env_var.as_deref().is_some_and(env_is_set),
        env_var,
        deletable: users.is_empty(),
        delete_blocked_reason: if users.is_empty() {
            String::new()
        } else {
            in_use_reason(&users)
        },
        used_by: users,
    }
}

/// The `providers` object of a parsed config, if it has one.
fn providers_object(root: &Value) -> Option<&Map<String, Value>> {
    let root = root.as_object()?;
    root.get(PROVIDERS_KEYS[0])?.as_object()
}

/// The raw object of the provider `name` (a built-in slot or a custom entry), if present.
fn raw_entry<'a>(root: &'a Value, name: &str) -> Option<&'a Map<String, Value>> {
    let providers = providers_object(root)?;
    if DEDICATED_PROVIDERS.contains(&name) || name == LEGACY_CUSTOM_SLOT {
        return providers.get(name)?.as_object();
    }
    CUSTOM_PROVIDERS_KEYS
        .iter()
        .find_map(|key| providers.get(*key))?
        .as_object()?
        .get(name)?
        .as_object()
}

/// The rows of the Providers page for the config `text`. Forms (and the blank add form)
/// are only included `with_forms`, for connections that may change the configuration.
/// `env_is_set` tells whether an environment variable holds a value.
pub fn providers_overview(
    text: &str,
    with_forms: bool,
    env_is_set: &dyn Fn(&str) -> bool,
) -> Result<ProvidersOverview, ToggleError> {
    let root = parse_root(text)?;
    let config = typed_config(text)?;
    let row_of = |name: &str, kind: ProviderKind, slot: &ProviderConfig| {
        provider_row(
            &config,
            name,
            kind,
            slot,
            raw_entry(&root, name),
            with_forms,
            env_is_set,
        )
    };

    let mut rows: Vec<ProviderRow> = DEDICATED_PROVIDERS
        .iter()
        .filter_map(|name| built_in_slot(&config, name).map(|slot| (name, slot)))
        .filter(|(_, slot)| slot_is_configured(slot))
        .map(|(name, slot)| row_of(name, ProviderKind::Dedicated, slot))
        .collect();
    rows.sort_by(|a, b| a.name.cmp(&b.name));

    let mut custom_rows: Vec<ProviderRow> = Vec::new();
    if slot_is_configured(&config.providers.custom) {
        custom_rows.push(row_of(
            LEGACY_CUSTOM_SLOT,
            ProviderKind::Custom,
            &config.providers.custom,
        ));
    }
    custom_rows.extend(
        config
            .providers
            .custom_providers
            .iter()
            .map(|(name, slot)| row_of(name, ProviderKind::Custom, slot)),
    );
    custom_rows.sort_by_key(|row| row.name.to_lowercase());
    rows.extend(custom_rows);

    let dedicated_choices = DEDICATED_PROVIDERS
        .iter()
        .filter_map(|name| find_by_name(name).map(|spec| (name, spec)))
        .map(|(name, spec)| DedicatedProviderChoice {
            name: name.to_string(),
            configured: rows.iter().any(|row| row.name == *name),
            env_var_set: !spec.env_key.is_empty() && env_is_set(&spec.env_key),
            default_api_base: default_api_base_of(&spec),
            label: spec.display_name,
            env_var: spec.env_key,
        })
        .collect();

    Ok(ProvidersOverview {
        rows,
        dedicated_choices,
        backend_choices: backend_choices(),
        new_provider_form: with_forms.then(|| provider_form("", "", None, "", env_is_set)),
    })
}

/// [`providers_overview`] against the environment of this process.
pub fn providers_overview_for_process(
    text: &str,
    with_forms: bool,
) -> Result<ProvidersOverview, ToggleError> {
    providers_overview(text, with_forms, &process_env_has_value)
}

/// The provider settings a running process should switch to once a config text has been
/// saved: `providers` with every `${VAR}` placeholder resolved. Never panics (unlike
/// `load_config`); errs with a message for the user when `text` is not a configuration
/// the bot could start with or a provider names an unset variable.
pub fn live_providers(text: &str) -> Result<ProvidersConfig, String> {
    validate_config_text(text)?;
    let config = typed_config(text).map_err(|error| error.message())?;
    resolve_env_vars_in(&config.providers)
}

/// A blank stand-in for a `null` or missing submitted value, by field kind.
fn blank_value(kind: &FieldKind) -> Value {
    match kind {
        FieldKind::StringMap => Value::Object(Map::new()),
        FieldKind::StringList => Value::Array(Vec::new()),
        _ => Value::String(String::new()),
    }
}

/// Whether `value` (already checked) is what a form sends for "nothing here".
fn is_blank(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::String(text) => text.trim().is_empty(),
        Value::Object(entries) => entries.is_empty(),
        Value::Array(items) => items.is_empty(),
        _ => false,
    }
}

/// Check every submitted `changes` entry against the schema and the environment and
/// write it into `entry`. A blank API key is never written (the stored key is kept); a
/// blank Base URL, header map or protocol removes the setting (the default applies).
/// `entry` may be left half edited on an error: callers work on a parsed copy.
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
        let submitted = if value.is_null() {
            blank_value(&kind)
        } else {
            value.clone()
        };
        let checked = check_value(&kind, &submitted)
            .map_err(|reason| ToggleError::InvalidValue(setting.clone(), reason))?;
        check_placeholders(setting, &checked, env_is_set, UNSET_VARIABLE_EXPLANATION)?;
        if is_blank(&checked) {
            if setting != API_KEY {
                remove_value(SETTING_SPELLINGS, entry, setting);
            }
        } else {
            store_value(SETTING_SPELLINGS, entry, setting, checked);
        }
    }
    Ok(())
}

/// Refuse a named custom provider without a Base URL: there is no default to fall back to.
fn check_base_url_is_set(entry: &Map<String, Value>) -> Result<(), ToggleError> {
    let has_base = stored_value(SETTING_SPELLINGS, entry, API_BASE)
        .and_then(Value::as_str)
        .is_some_and(|base| !base.trim().is_empty());
    if has_base {
        Ok(())
    } else {
        Err(ToggleError::InvalidValue(
            API_BASE.to_string(),
            "a custom provider needs a Base URL".to_string(),
        ))
    }
}

/// Whether the raw provider `entry` holds a key, a Base URL, headers or a protocol: what
/// makes a slot count as configured (see [`slot_is_configured`]).
fn entry_is_configured(entry: &Map<String, Value>) -> bool {
    [API_KEY, API_BASE, EXTRA_HEADERS, PROVIDER_BACK_END]
        .iter()
        .filter_map(|setting| stored_value(SETTING_SPELLINGS, entry, setting))
        .any(|value| !is_blank(value))
}

/// Make a newly added dedicated provider show up in the list even when nothing was
/// entered (its API key then comes from the environment): a slot without any setting
/// counts as not configured, so the provider's default Base URL is written explicitly.
fn mark_as_added(slot: &mut Map<String, Value>, name: &str) {
    if entry_is_configured(slot) {
        return;
    }
    if let Some(default_base) = find_by_name(name).as_ref().and_then(default_api_base_of) {
        store_value(
            SETTING_SPELLINGS,
            slot,
            API_BASE,
            Value::String(default_base),
        );
    }
}

/// The `providers` section of a parsed config as a mutable object, created when missing.
fn providers_object_mut(root: &mut Value) -> Result<&mut Map<String, Value>, ToggleError> {
    let root_object = root.as_object_mut().expect("parse_root checked this");
    child_object_mut(root_object, PROVIDERS_KEYS)
}

/// The entry of a provider that has a row, as a mutable object. `UnknownProvider` when
/// `name` is not configured.
fn entry_mut<'a>(
    root: &'a mut Value,
    config: &Config,
    name: &str,
) -> Result<&'a mut Map<String, Value>, ToggleError> {
    let providers = providers_object_mut(root)?;
    let is_built_in = DEDICATED_PROVIDERS.contains(&name) || name == LEGACY_CUSTOM_SLOT;
    if is_built_in {
        let configured = built_in_slot(config, name).is_some_and(slot_is_configured);
        if !configured {
            return Err(ToggleError::UnknownProvider);
        }
        return providers
            .get_mut(name)
            .and_then(Value::as_object_mut)
            .ok_or(ToggleError::UnknownProvider);
    }
    let customs = child_object_mut(providers, CUSTOM_PROVIDERS_KEYS)?;
    customs
        .get_mut(name)
        .and_then(Value::as_object_mut)
        .ok_or(ToggleError::UnknownProvider)
}

/// Whether `name` is a named custom provider (not a built-in slot).
fn is_named_custom(name: &str) -> bool {
    !(DEDICATED_PROVIDERS.contains(&name) || name == LEGACY_CUSTOM_SLOT)
}

/// `text` with a new provider added. A dedicated provider is one of
/// [`DEDICATED_PROVIDERS`] that has no row yet (its slot gets the submitted settings); a
/// custom provider needs a valid, unreserved, unused `name` and a Base URL (it is added to
/// `providers.customProviders`). Adding never overwrites.
pub fn with_provider_added(
    text: &str,
    kind: ProviderKind,
    name: &str,
    changes: &HashMap<String, Value>,
) -> Result<String, ToggleError> {
    with_provider_added_in(text, kind, name, changes, &process_env_is_set)
}

/// [`with_provider_added`] with the environment lookup passed in.
fn with_provider_added_in(
    text: &str,
    kind: ProviderKind,
    name: &str,
    changes: &HashMap<String, Value>,
    env_is_set: &dyn Fn(&str) -> bool,
) -> Result<String, ToggleError> {
    let config = typed_config(text)?;
    let mut root = parse_root(text)?;
    match kind {
        ProviderKind::Dedicated => {
            if !DEDICATED_PROVIDERS.contains(&name) {
                return Err(ToggleError::UnknownProvider);
            }
            if built_in_slot(&config, name).is_some_and(slot_is_configured) {
                return Err(ToggleError::ProviderExists);
            }
            let providers = providers_object_mut(&mut root)?;
            let slot = providers
                .entry(name.to_string())
                .or_insert_with(|| Value::Object(Map::new()));
            let slot = slot.as_object_mut().ok_or_else(|| {
                ToggleError::InvalidConfig(format!("\"{name}\" is not an object."))
            })?;
            slot.clear();
            slot.insert(API_KEY.to_string(), Value::String(String::new()));
            apply_settings(slot, changes, env_is_set)?;
            mark_as_added(slot, name);
        }
        ProviderKind::Custom => {
            if !is_valid_name(name) {
                return Err(ToggleError::InvalidName);
            }
            if is_reserved_provider_name(name) {
                return Err(ToggleError::ReservedName);
            }
            if config.providers.custom_providers.contains_key(name) {
                return Err(ToggleError::ProviderExists);
            }
            let mut entry = Map::new();
            entry.insert(API_KEY.to_string(), Value::String(String::new()));
            apply_settings(&mut entry, changes, env_is_set)?;
            check_base_url_is_set(&entry)?;
            let providers = providers_object_mut(&mut root)?;
            child_object_mut(providers, CUSTOM_PROVIDERS_KEYS)?
                .insert(name.to_string(), Value::Object(entry));
        }
    }
    rendered_and_checked(&root, text)
}

/// `text` with the submitted `changes` (setting name -> value, as the form names them)
/// applied to the provider `name`, under the key spellings its entry already uses.
/// Nothing is applied unless every change is valid. `None` when the entry already says
/// all of it. The provider must have a row.
pub fn with_provider_settings(
    text: &str,
    name: &str,
    changes: &HashMap<String, Value>,
) -> Result<Option<String>, ToggleError> {
    with_provider_settings_in(text, name, changes, &process_env_is_set)
}

/// [`with_provider_settings`] with the environment lookup passed in.
fn with_provider_settings_in(
    text: &str,
    name: &str,
    changes: &HashMap<String, Value>,
    env_is_set: &dyn Fn(&str) -> bool,
) -> Result<Option<String>, ToggleError> {
    if !is_valid_name(name) {
        return Err(ToggleError::InvalidName);
    }
    let config = typed_config(text)?;
    let mut root = parse_root(text)?;
    let entry = entry_mut(&mut root, &config, name)?;
    let before = entry.clone();
    apply_settings(entry, changes, env_is_set)?;
    if is_named_custom(name) {
        check_base_url_is_set(entry)?;
    }
    if *entry == before {
        return Ok(None);
    }
    rendered_and_checked(&root, text).map(Some)
}

/// `text` without the provider `name`: a custom entry is removed, a dedicated slot (and
/// the legacy custom slot) is reset to `{"apiKey": ""}`. Refused with
/// [`ToggleError::ProviderInUse`] while `agents.provider` or a model preset names it.
/// The provider must have a row.
pub fn without_provider(text: &str, name: &str) -> Result<String, ToggleError> {
    if !is_valid_name(name) {
        return Err(ToggleError::InvalidName);
    }
    let config = typed_config(text)?;
    let users = provider_users(&config, name);
    let mut root = parse_root(text)?;
    // Checks the provider exists before anything else is said about it.
    entry_mut(&mut root, &config, name)?;
    if !users.is_empty() {
        return Err(ToggleError::ProviderInUse(in_use_reason(&users)));
    }
    let providers = providers_object_mut(&mut root)?;
    if is_named_custom(name) {
        let key = CUSTOM_PROVIDERS_KEYS
            .iter()
            .find(|key| providers.contains_key(**key))
            .copied()
            .unwrap_or(CUSTOM_PROVIDERS_KEYS[0]);
        let customs = child_object_mut(providers, CUSTOM_PROVIDERS_KEYS)?;
        customs.shift_remove(name);
        if customs.is_empty() {
            providers.shift_remove(key);
        }
    } else {
        let mut reset = Map::new();
        reset.insert(API_KEY.to_string(), Value::String(String::new()));
        providers.insert(name.to_string(), Value::Object(reset));
    }
    rendered_and_checked(&root, text)
}

/// `text` with the stored API key of the provider `name` cleared (`""`, so the provider's
/// environment variable applies again). `None` when no key is stored. The provider must
/// have a row.
pub fn with_provider_key_forgotten(text: &str, name: &str) -> Result<Option<String>, ToggleError> {
    if !is_valid_name(name) {
        return Err(ToggleError::InvalidName);
    }
    let config = typed_config(text)?;
    let mut root = parse_root(text)?;
    let entry = entry_mut(&mut root, &config, name)?;
    let has_key = stored_value(SETTING_SPELLINGS, entry, API_KEY)
        .and_then(Value::as_str)
        .is_some_and(|key| !key.is_empty());
    if !has_key {
        return Ok(None);
    }
    store_value(
        SETTING_SPELLINGS,
        entry,
        API_KEY,
        Value::String(String::new()),
    );
    rendered_and_checked(&root, text).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const CONFIG: &str = r#"{
  "agents": {"model": "m", "provider": "anthropic", "modelPreset": "relayed"},
  "providers": {
    "anthropic": {"apiKey": "sk-ant-SECRET", "extraHeaders": {"anthropic-beta": "x"}},
    "openai": {"apiKey": ""},
    "together": {"api_key": "${TOGETHER_KEY}", "api_base": "https://together.example/v1"},
    "customProviders": {
      "my-relay": {"apiKey": "relay-SECRET", "apiBase": "https://gateway.example/v1", "providerBackEnd": "openai_responses"},
      "Alpha": {"apiBase": "http://localhost:11434/v1"}
    }
  },
  "modelPresets": {
    "relayed": {"model": "gpt", "provider": "my-relay"}
  }
}
"#;

    const BLANK_CONFIG: &str = "{}\n";

    fn change(pairs: &[(&str, Value)]) -> HashMap<String, Value> {
        pairs
            .iter()
            .map(|(name, value)| (name.to_string(), value.clone()))
            .collect()
    }

    fn all_set(_: &str) -> bool {
        true
    }

    fn none_set(_: &str) -> bool {
        false
    }

    fn added(
        text: &str,
        kind: ProviderKind,
        name: &str,
        changes: &HashMap<String, Value>,
    ) -> Result<String, ToggleError> {
        with_provider_added_in(text, kind, name, changes, &all_set)
    }

    fn edited(
        text: &str,
        name: &str,
        changes: &HashMap<String, Value>,
    ) -> Result<Option<String>, ToggleError> {
        with_provider_settings_in(text, name, changes, &all_set)
    }

    fn providers(text: &str) -> Value {
        serde_json::from_str::<Value>(text).unwrap()["providers"].clone()
    }

    fn row<'a>(overview: &'a ProvidersOverview, name: &str) -> &'a ProviderRow {
        overview
            .rows
            .iter()
            .find(|row| row.name == name)
            .unwrap_or_else(|| panic!("no row {name}"))
    }

    // ── overview ──────────────────────────────────────────────────────────────

    #[test]
    fn rows_list_configured_dedicated_providers_then_custom_ones_sorted() {
        let overview = providers_overview(CONFIG, false, &none_set).unwrap();
        let names: Vec<&str> = overview.rows.iter().map(|row| row.name.as_str()).collect();
        // openai's slot holds only an empty key: not configured.
        assert_eq!(names, ["anthropic", "together", "Alpha", "my-relay"]);
        assert_eq!(row(&overview, "anthropic").kind, ProviderKind::Dedicated);
        assert_eq!(row(&overview, "my-relay").kind, ProviderKind::Custom);
    }

    #[test]
    fn a_row_never_contains_an_api_key() {
        for with_forms in [false, true] {
            let overview = providers_overview(CONFIG, with_forms, &none_set).unwrap();
            let json = serde_json::to_string(&overview).unwrap();
            assert!(!json.contains("SECRET"), "{json}");
            assert!(!json.contains("TOGETHER_KEY"), "{json}");
        }
    }

    #[test]
    fn rows_say_whether_a_key_and_the_environment_variable_are_available() {
        let overview = providers_overview(CONFIG, false, &none_set).unwrap();
        let anthropic = row(&overview, "anthropic");
        assert!(anthropic.api_key_configured);
        assert_eq!(anthropic.env_var.as_deref(), Some("ANTHROPIC_API_KEY"));
        assert!(!anthropic.env_var_set);
        let alpha = row(&overview, "Alpha");
        assert!(!alpha.api_key_configured);
        assert_eq!(alpha.env_var, None);

        let with_env = providers_overview(CONFIG, false, &all_set).unwrap();
        assert!(row(&with_env, "anthropic").env_var_set);
    }

    #[test]
    fn rows_describe_the_endpoint_and_protocol() {
        let overview = providers_overview(CONFIG, false, &none_set).unwrap();
        let relay = row(&overview, "my-relay");
        assert_eq!(
            relay.api_base.as_deref(),
            Some("https://gateway.example/v1")
        );
        assert_eq!(relay.backend.as_deref(), Some("openai_responses"));
        assert_eq!(relay.backend_default, "openai_compat");
        let anthropic = row(&overview, "anthropic");
        assert_eq!(anthropic.api_base, None);
        assert_eq!(anthropic.backend, None);
        assert_eq!(anthropic.backend_default, "anthropic");
        assert!(anthropic.default_api_base.is_some());
        assert_eq!(anthropic.label, "Anthropic");
    }

    #[test]
    fn the_legacy_custom_slot_is_a_custom_row_named_custom() {
        let text = r#"{"providers": {"custom": {"apiKey": "k", "apiBase": "http://x/v1"}}}"#;
        let overview = providers_overview(text, false, &none_set).unwrap();
        assert_eq!(overview.rows.len(), 1);
        assert_eq!(overview.rows[0].name, "custom");
        assert_eq!(overview.rows[0].kind, ProviderKind::Custom);
    }

    #[test]
    fn rows_name_what_uses_the_provider_and_block_its_deletion() {
        let overview = providers_overview(CONFIG, false, &none_set).unwrap();
        let relay = row(&overview, "my-relay");
        assert_eq!(relay.used_by, ["model preset 'relayed'"]);
        assert!(!relay.deletable);
        assert!(relay.delete_blocked_reason.contains("relayed"));
        let anthropic = row(&overview, "anthropic");
        assert_eq!(anthropic.used_by, ["agents.provider"]);
        assert!(row(&overview, "Alpha").deletable);
        assert!(row(&overview, "Alpha").delete_blocked_reason.is_empty());
    }

    #[test]
    fn forms_are_only_included_for_connections_that_may_write() {
        let read_only = providers_overview(CONFIG, false, &none_set).unwrap();
        assert!(read_only.new_provider_form.is_none());
        assert!(read_only.rows.iter().all(|row| row.form.is_none()));
        let json = serde_json::to_string(&read_only).unwrap();
        assert!(!json.contains("anthropic-beta"), "no header values: {json}");

        let writer = providers_overview(CONFIG, true, &none_set).unwrap();
        assert!(writer.new_provider_form.is_some());
        assert!(writer.rows.iter().all(|row| row.form.is_some()));
    }

    #[test]
    fn the_form_has_the_four_settings_and_a_secret_key_without_a_value() {
        let overview = providers_overview(CONFIG, true, &none_set).unwrap();
        let form = row(&overview, "anthropic").form.clone().unwrap();
        let names: Vec<&str> = form.fields.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(
            names,
            ["apiKey", "apiBase", "extraHeaders", "providerBackEnd"]
        );
        let key = &form.fields[0];
        assert!(key.secret);
        assert!(key.value.is_none());
        assert!(key.configured);
        assert_eq!(key.env_var, "ANTHROPIC_API_KEY");
        assert_eq!(form.fields[2].kind, FieldKind::StringMap);
        assert_eq!(form.fields[2].value, Some(json!({"anthropic-beta": "x"})));
        assert_eq!(form.fields[3].value, Some(json!("")));
    }

    #[test]
    fn the_key_field_counts_the_environment_variable_as_configured() {
        let overview = providers_overview(
            r#"{"providers": {"anthropic": {"apiBase": "http://x"}}}"#,
            true,
            &all_set,
        )
        .unwrap();
        let form = row(&overview, "anthropic").form.clone().unwrap();
        assert!(form.fields[0].configured);
        let overview = providers_overview(
            r#"{"providers": {"anthropic": {"apiBase": "http://x"}}}"#,
            true,
            &none_set,
        )
        .unwrap();
        assert!(!row(&overview, "anthropic").form.clone().unwrap().fields[0].configured);
    }

    #[test]
    fn the_dedicated_choices_are_the_four_buildable_providers() {
        let overview = providers_overview(CONFIG, false, &none_set).unwrap();
        let names: Vec<&str> = overview
            .dedicated_choices
            .iter()
            .map(|choice| choice.name.as_str())
            .collect();
        assert_eq!(names, DEDICATED_PROVIDERS);
        let anthropic = &overview.dedicated_choices[1];
        assert!(anthropic.configured);
        assert_eq!(anthropic.env_var, "ANTHROPIC_API_KEY");
        assert!(!overview.dedicated_choices[0].configured);
        assert!(overview.dedicated_choices[0].default_api_base.is_some());
    }

    #[test]
    fn the_protocol_choices_start_with_automatic() {
        let overview = providers_overview(BLANK_CONFIG, false, &none_set).unwrap();
        let values: Vec<&str> = overview
            .backend_choices
            .iter()
            .map(|choice| choice.value.as_str())
            .collect();
        assert_eq!(values, PROVIDER_BACKEND_CHOICES);
        assert_eq!(overview.backend_choices[0].label, "Automatic");
        assert_eq!(overview.backend_choices[2].label, "OpenAI Responses API");
    }

    // ── adding ────────────────────────────────────────────────────────────────

    #[test]
    fn a_dedicated_provider_is_added_with_the_submitted_settings() {
        let values = change(&[
            ("apiKey", json!("sk-new")),
            ("apiBase", json!("https://proxy.example/v1")),
            ("extraHeaders", json!({"X-Team": "a"})),
            ("providerBackEnd", json!("openai_compat")),
        ]);
        let saved = added(BLANK_CONFIG, ProviderKind::Dedicated, "openai", &values).unwrap();
        assert_eq!(
            providers(&saved)["openai"],
            json!({
                "apiKey": "sk-new",
                "apiBase": "https://proxy.example/v1",
                "extraHeaders": {"X-Team": "a"},
                "providerBackEnd": "openai_compat"
            })
        );
    }

    #[test]
    fn a_dedicated_provider_with_a_blank_key_keeps_the_key_empty() {
        let values = change(&[("apiKey", json!("")), ("apiBase", json!(""))]);
        let saved = added(BLANK_CONFIG, ProviderKind::Dedicated, "openrouter", &values).unwrap();
        // Nothing was entered: the default Base URL is written so the provider has a row.
        let openrouter = &providers(&saved)["openrouter"];
        assert_eq!(openrouter["apiKey"], "");
        assert!(
            openrouter["apiBase"]
                .as_str()
                .unwrap()
                .starts_with("https://")
        );
    }

    #[test]
    fn a_dedicated_provider_added_with_nothing_still_gets_a_row() {
        let saved = added(
            BLANK_CONFIG,
            ProviderKind::Dedicated,
            "anthropic",
            &HashMap::new(),
        )
        .unwrap();
        let overview = providers_overview(&saved, false, &none_set).unwrap();
        let anthropic = row(&overview, "anthropic");
        assert!(!anthropic.api_key_configured);
        assert_eq!(anthropic.api_base, anthropic.default_api_base);
        assert_eq!(providers(&saved)["anthropic"]["apiKey"], "");
    }

    #[test]
    fn only_the_four_dedicated_providers_can_be_added_once() {
        let values = HashMap::new();
        assert_eq!(
            added(BLANK_CONFIG, ProviderKind::Dedicated, "gemini", &values),
            Err(ToggleError::UnknownProvider)
        );
        assert_eq!(
            added(CONFIG, ProviderKind::Dedicated, "anthropic", &values),
            Err(ToggleError::ProviderExists)
        );
        // An empty slot is not "configured": it can be added.
        assert!(added(CONFIG, ProviderKind::Dedicated, "openai", &values).is_ok());
    }

    #[test]
    fn a_custom_provider_is_added_under_custom_providers() {
        let values = change(&[
            ("apiKey", json!("${RELAY_KEY}")),
            ("apiBase", json!("https://gateway.example/v1")),
            ("providerBackEnd", json!("anthropic")),
        ]);
        let saved = added(BLANK_CONFIG, ProviderKind::Custom, "edenai", &values).unwrap();
        assert_eq!(
            providers(&saved)["customProviders"]["edenai"],
            json!({
                "apiKey": "${RELAY_KEY}",
                "apiBase": "https://gateway.example/v1",
                "providerBackEnd": "anthropic"
            })
        );
        // The typed config reads it back.
        let config: Config = serde_json::from_str(&saved).unwrap();
        assert!(config.providers.custom_providers.contains_key("edenai"));
    }

    #[test]
    fn a_custom_provider_needs_a_valid_free_name_and_a_base_url() {
        let values = change(&[("apiBase", json!("https://g/v1"))]);
        for reserved in ["auto", "default", "openai", "custom"] {
            assert_eq!(
                added(BLANK_CONFIG, ProviderKind::Custom, reserved, &values),
                Err(ToggleError::ReservedName),
                "{reserved}"
            );
        }
        assert_eq!(
            added(BLANK_CONFIG, ProviderKind::Custom, "../x", &values),
            Err(ToggleError::InvalidName)
        );
        assert_eq!(
            added(CONFIG, ProviderKind::Custom, "my-relay", &values),
            Err(ToggleError::ProviderExists)
        );
        assert!(matches!(
            added(BLANK_CONFIG, ProviderKind::Custom, "fresh", &HashMap::new()),
            Err(ToggleError::InvalidValue(setting, _)) if setting == "apiBase"
        ));
    }

    #[test]
    fn an_added_custom_provider_keeps_the_snake_case_spelling_of_the_map() {
        let text = r#"{"providers": {"custom_providers": {"a": {"apiBase": "http://a"}}}}"#;
        let values = change(&[("apiBase", json!("http://b"))]);
        let saved = added(text, ProviderKind::Custom, "b", &values).unwrap();
        let customs = &providers(&saved)["custom_providers"];
        assert!(customs.get("a").is_some() && customs.get("b").is_some());
        assert!(providers(&saved).get("customProviders").is_none());
    }

    #[test]
    fn an_unset_placeholder_is_refused_with_the_variable_named() {
        let values = change(&[("apiKey", json!("${NOT_SET_ANYWHERE}"))]);
        let error = with_provider_added_in(
            BLANK_CONFIG,
            ProviderKind::Dedicated,
            "openai",
            &values,
            &none_set,
        )
        .unwrap_err();
        let ToggleError::InvalidValue(setting, reason) = error else {
            panic!("expected an invalid value");
        };
        assert_eq!(setting, "apiKey");
        assert!(reason.contains("NOT_SET_ANYWHERE"));
    }

    #[test]
    fn unknown_settings_and_bad_protocols_are_refused() {
        let unknown = change(&[("bogus", json!("x"))]);
        assert_eq!(
            added(BLANK_CONFIG, ProviderKind::Dedicated, "openai", &unknown),
            Err(ToggleError::UnknownField("bogus".to_string()))
        );
        let bad_protocol = change(&[("providerBackEnd", json!("smtp"))]);
        assert!(matches!(
            added(
                BLANK_CONFIG,
                ProviderKind::Dedicated,
                "openai",
                &bad_protocol
            ),
            Err(ToggleError::InvalidValue(..))
        ));
    }

    // ── changing ──────────────────────────────────────────────────────────────

    #[test]
    fn a_blank_or_missing_key_keeps_the_stored_key() {
        for values in [
            change(&[
                ("apiKey", json!("")),
                ("apiBase", json!("https://new.example")),
            ]),
            change(&[
                ("apiKey", Value::Null),
                ("apiBase", json!("https://new.example")),
            ]),
            change(&[("apiBase", json!("https://new.example"))]),
        ] {
            let saved = edited(CONFIG, "anthropic", &values).unwrap().unwrap();
            assert_eq!(providers(&saved)["anthropic"]["apiKey"], "sk-ant-SECRET");
            assert_eq!(
                providers(&saved)["anthropic"]["apiBase"],
                "https://new.example"
            );
        }
    }

    #[test]
    fn a_new_key_replaces_the_stored_one_under_its_spelling() {
        let values = change(&[("apiKey", json!("sk-fresh"))]);
        let saved = edited(CONFIG, "together", &values).unwrap().unwrap();
        let together = &providers(&saved)["together"];
        assert_eq!(together["api_key"], "sk-fresh");
        assert!(together.get("apiKey").is_none());
        assert_eq!(together["api_base"], "https://together.example/v1");
    }

    #[test]
    fn an_empty_protocol_removes_the_setting() {
        let values = change(&[("providerBackEnd", json!(""))]);
        let saved = edited(CONFIG, "my-relay", &values).unwrap().unwrap();
        let relay = &providers(&saved)["customProviders"]["my-relay"];
        assert!(relay.get("providerBackEnd").is_none());
        assert_eq!(relay["apiKey"], "relay-SECRET");
    }

    #[test]
    fn empty_headers_and_a_blank_base_url_remove_their_settings() {
        let values = change(&[("extraHeaders", json!({})), ("apiBase", json!("  "))]);
        let saved = edited(CONFIG, "anthropic", &values).unwrap().unwrap();
        let anthropic = &providers(&saved)["anthropic"];
        assert!(anthropic.get("extraHeaders").is_none());
        assert!(anthropic.get("apiBase").is_none());
    }

    #[test]
    fn a_custom_provider_cannot_lose_its_base_url() {
        let values = change(&[("apiBase", json!(""))]);
        assert!(matches!(
            edited(CONFIG, "my-relay", &values),
            Err(ToggleError::InvalidValue(setting, _)) if setting == "apiBase"
        ));
    }

    #[test]
    fn saying_what_the_entry_already_says_changes_nothing() {
        let values = change(&[
            ("apiKey", json!("")),
            ("apiBase", json!("https://gateway.example/v1")),
            ("providerBackEnd", json!("openai_responses")),
        ]);
        assert_eq!(edited(CONFIG, "my-relay", &values), Ok(None));
    }

    #[test]
    fn only_providers_with_a_row_can_be_changed() {
        let values = change(&[("apiBase", json!("http://x"))]);
        assert_eq!(
            edited(CONFIG, "openai", &values),
            Err(ToggleError::UnknownProvider)
        );
        assert_eq!(
            edited(CONFIG, "nonesuch", &values),
            Err(ToggleError::UnknownProvider)
        );
        assert_eq!(
            edited(CONFIG, "a/b", &values),
            Err(ToggleError::InvalidName)
        );
    }

    // ── deleting and forgetting ───────────────────────────────────────────────

    #[test]
    fn a_provider_in_use_cannot_be_deleted() {
        let error = without_provider(CONFIG, "my-relay").unwrap_err();
        let ToggleError::ProviderInUse(why) = error else {
            panic!("expected in use");
        };
        assert!(why.contains("relayed"), "{why}");
        let error = without_provider(CONFIG, "anthropic").unwrap_err();
        assert!(
            matches!(&error, ToggleError::ProviderInUse(why) if why.contains("agents.provider"))
        );
    }

    #[test]
    fn a_custom_provider_is_removed_and_the_empty_map_goes_with_it() {
        let text = r#"{"providers": {"customProviders": {"a": {"apiBase": "http://a"}}}}"#;
        let saved = without_provider(text, "a").unwrap();
        assert!(providers(&saved).get("customProviders").is_none());

        let saved = without_provider(CONFIG, "Alpha").unwrap();
        let customs = &providers(&saved)["customProviders"];
        assert!(customs.get("Alpha").is_none());
        assert!(customs.get("my-relay").is_some());
    }

    #[test]
    fn a_dedicated_slot_is_reset_not_removed() {
        let text = r#"{"providers": {"openrouter": {"apiKey": "k", "apiBase": "http://x"}}}"#;
        let saved = without_provider(text, "openrouter").unwrap();
        assert_eq!(providers(&saved)["openrouter"], json!({"apiKey": ""}));
        let overview = providers_overview(&saved, false, &none_set).unwrap();
        assert!(overview.rows.is_empty());
    }

    #[test]
    fn deleting_an_unknown_provider_is_refused() {
        assert_eq!(
            without_provider(CONFIG, "nonesuch"),
            Err(ToggleError::UnknownProvider)
        );
        assert_eq!(
            without_provider(CONFIG, "openai"),
            Err(ToggleError::UnknownProvider)
        );
    }

    #[test]
    fn forgetting_the_key_clears_only_the_key() {
        let saved = with_provider_key_forgotten(CONFIG, "my-relay")
            .unwrap()
            .unwrap();
        let relay = &providers(&saved)["customProviders"]["my-relay"];
        assert_eq!(relay["apiKey"], "");
        assert_eq!(relay["apiBase"], "https://gateway.example/v1");
        assert!(!saved.contains("relay-SECRET"));
        assert_eq!(with_provider_key_forgotten(CONFIG, "Alpha"), Ok(None));
        assert_eq!(
            with_provider_key_forgotten(CONFIG, "nonesuch"),
            Err(ToggleError::UnknownProvider)
        );
    }

    #[test]
    fn edits_keep_key_order_and_untouched_placeholders() {
        let values = change(&[("apiBase", json!("https://new.example"))]);
        let saved = edited(CONFIG, "anthropic", &values).unwrap().unwrap();
        assert!(saved.contains("${TOGETHER_KEY}"));
        assert!(saved.find("\"agents\"").unwrap() < saved.find("\"providers\"").unwrap());
    }

    // ── live settings ─────────────────────────────────────────────────────────

    #[test]
    fn live_providers_resolve_placeholders() {
        let text = r#"{"providers": {"customProviders": {"r": {"apiKey": "${PATH}", "apiBase": "http://r"}}}}"#;
        let live = live_providers(text).unwrap();
        assert_ne!(live.custom_providers["r"].api_key, "${PATH}");
        let unset = r#"{"providers": {"openai": {"apiKey": "${RUST_BOT_SURELY_UNSET_VAR}"}}}"#;
        assert!(live_providers(unset).is_err());
        assert!(live_providers("not json").is_err());
    }

    #[test]
    fn kinds_parse_from_their_wire_names() {
        assert_eq!(
            ProviderKind::parse("dedicated"),
            Some(ProviderKind::Dedicated)
        );
        assert_eq!(ProviderKind::parse("custom"), Some(ProviderKind::Custom));
        assert_eq!(ProviderKind::parse("other"), None);
    }
}
