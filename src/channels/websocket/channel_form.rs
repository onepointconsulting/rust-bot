//! The chat channels of `config.json` (`channels.<type>`) for the WebUI's Settings →
//! Channels page: one row per channel type, its settings form, and the pure edits of
//! the config text (create, change, delete).
//!
//! The page is keyed by channel type ([`SettingsChannelType`]); there is at most one
//! channel of each type. Today only email is offered; another type is one more variant.
//!
//! Like [`super::provider_form`], the edits work on the raw config text so `${VAR}`
//! placeholders, key order and every key the form does not know (e.g. a per-channel
//! `sendProgress`) survive. **Passwords never leave this module**: their form fields
//! are secrets without a value, rows never quote them, and a blank submitted password
//! keeps the stored one. Changes apply after a restart: channels are built once at
//! startup.

use std::collections::HashMap;

use serde::Serialize;
use serde_json::{Map, Value};

use super::config_toggles::{ToggleError, child_object_mut, parse_root};
use super::model_preset_form::rendered_and_checked;
use super::settings_form::{
    SettingSpellings, UNSET_VARIABLE_EXPLANATION, blank_value, check_placeholders, form_fields,
    is_blank, process_env_is_set, schema_properties, store_value,
};
use crate::agent::tools::tool_settings_api::{ToolSettingsSummary, check_value, field_kind};
use crate::config::channels::{EMAIL_CHANNEL_KEY, EmailConfig};

/// Spellings the configuration accepts for the top-level `channels` section.
const CHANNELS_KEYS: &[&str] = &["channels"];

/// Channel settings have a single spelling each (camelCase, as the form names them).
const SETTING_SPELLINGS: SettingSpellings = &[];

/// Why a channel that is switched on without consent is refused.
const CONSENT_REQUIRED_REASON: &str = "must be on before the channel is enabled: the owner of the mailbox has to agree that the bot reads and sends mail with it.";

/// Row note for a channel without consent.
const CONSENT_MISSING_NOTE: &str = "Consent not granted: the channel will not start.";

/// Row note for an email channel without allowed senders.
const NO_ALLOWED_SENDERS_NOTE: &str = "No allowed senders: all incoming mail is ignored.";

/// Row note for an entry that does not have the shape of the channel's settings.
const UNREADABLE_ENTRY_NOTE: &str =
    "The settings cannot be read: fix this channel in config.json or delete it.";

/// Separator between the parts of a row's summary line.
const SUMMARY_SEPARATOR: &str = " · ";

/// A channel type the Channels page can set up. The key is the entry's name under
/// `channels` and the type's name on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsChannelType {
    /// IMAP in, SMTP out (`channels.email`).
    Email,
}

impl SettingsChannelType {
    /// Every type, in the order the page lists them.
    pub const ALL: [Self; 1] = [Self::Email];

    /// The entry's key under `channels`, also the type's name on the wire.
    pub fn key(self) -> &'static str {
        match self {
            Self::Email => EMAIL_CHANNEL_KEY,
        }
    }

    /// The heading of the type's section on the page.
    pub fn title(self) -> &'static str {
        match self {
            Self::Email => "Email",
        }
    }

    /// The type named `key` on the wire, if the page knows it.
    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.key() == key)
    }

    /// [`Self::from_key`], refusing an unknown type.
    pub fn parse(key: &str) -> Result<Self, ToggleError> {
        Self::from_key(key).ok_or(ToggleError::UnknownChannelType)
    }

    /// The schema properties the type's form edits, in declaration order.
    fn form_properties(self) -> Map<String, Value> {
        match self {
            Self::Email => schema_properties::<EmailConfig>(),
        }
    }

    /// Refuse an entry the channel could not start with: missing connection settings,
    /// or switched on without consent.
    fn check_entry(self, entry: &Map<String, Value>) -> Result<(), ToggleError> {
        match self {
            Self::Email => check_email_entry(entry, &self.form_properties()),
        }
    }

    /// The row's one-line description of a configured entry; never a password.
    fn summary(self, entry: &Map<String, Value>) -> String {
        match self {
            Self::Email => email_config_of(entry)
                .map(|config| email_summary(&config))
                .unwrap_or_default(),
        }
    }

    /// Warnings about a configured entry, e.g. that it will not start.
    fn notes(self, entry: &Map<String, Value>) -> Vec<String> {
        match self {
            Self::Email => match email_config_of(entry) {
                Some(config) => email_notes(&config, &self.form_properties()),
                None => vec![UNREADABLE_ENTRY_NOTE.to_string()],
            },
        }
    }
}

/// One section of the Channels page: a channel type and, when set up, its channel.
/// Never holds a password.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChannelRow {
    /// The type's key (`email`).
    pub channel_type: String,
    /// The section heading (`Email`).
    pub title: String,
    /// Whether `channels.<type>` exists.
    pub configured: bool,
    /// Whether the channel is switched on (`false` when not configured).
    pub enabled: bool,
    /// One line about the configured channel, e.g. `bot@example.com · imap.example.com`.
    pub summary: String,
    /// Warnings, e.g. that the channel will not start.
    pub notes: Vec<String>,
    /// The edit form of a configured channel; only for connections that may change the
    /// configuration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub form: Option<ToolSettingsSummary>,
    /// The blank form of a type that is not set up yet; only for connections that may
    /// change the configuration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_form: Option<ToolSettingsSummary>,
}

/// The entry `channels.<type>` of a parsed config, when it exists and is an object.
fn channel_entry(root: &Value, channel_type: SettingsChannelType) -> Option<&Map<String, Value>> {
    root.get(CHANNELS_KEYS[0])?
        .get(channel_type.key())?
        .as_object()
}

/// The `channels` section of a parsed config as a mutable object, created when missing.
fn channels_object_mut(root: &mut Value) -> Result<&mut Map<String, Value>, ToggleError> {
    let root_object = root.as_object_mut().expect("parse_root checked this");
    child_object_mut(root_object, CHANNELS_KEYS)
}

/// The settings form of `channel_type`, showing `entry` (`None`: not set up yet, every
/// setting at its default). Password fields are secrets: they carry no value.
fn channel_form(
    channel_type: SettingsChannelType,
    entry: Option<&Map<String, Value>>,
) -> ToolSettingsSummary {
    ToolSettingsSummary {
        name: channel_type.key().to_string(),
        title: channel_type.title().to_string(),
        description: String::new(),
        enabled: None,
        tool_names: Vec::new(),
        fields: form_fields(&channel_type.form_properties(), SETTING_SPELLINGS, entry),
    }
}

/// The section of `channel_type` for the parsed config `root`.
fn channel_row(root: &Value, channel_type: SettingsChannelType, with_forms: bool) -> ChannelRow {
    let entry = channel_entry(root, channel_type);
    ChannelRow {
        channel_type: channel_type.key().to_string(),
        title: channel_type.title().to_string(),
        configured: entry.is_some(),
        enabled: entry
            .and_then(|entry| entry.get("enabled"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
        summary: entry
            .map(|entry| channel_type.summary(entry))
            .unwrap_or_default(),
        notes: entry
            .map(|entry| channel_type.notes(entry))
            .unwrap_or_default(),
        form: entry
            .filter(|_| with_forms)
            .map(|entry| channel_form(channel_type, Some(entry))),
        new_form: (with_forms && entry.is_none()).then(|| channel_form(channel_type, None)),
    }
}

/// One row per channel type for the config `text`, in [`SettingsChannelType::ALL`]
/// order. Forms are only included `with_forms`, for connections that may change the
/// configuration: they show host names and user names.
pub fn channels_overview(text: &str, with_forms: bool) -> Result<Vec<ChannelRow>, ToggleError> {
    let root = parse_root(text)?;
    Ok(SettingsChannelType::ALL
        .into_iter()
        .map(|channel_type| channel_row(&root, channel_type, with_forms))
        .collect())
}

/// Check every submitted `changes` entry against the type's schema and the environment
/// and write it into `entry`, in the form's order (so a new entry lists its settings as
/// the form does, whatever order they were submitted in). A blank password is never
/// written (the stored one is kept); a `null` stands for an emptied input. `entry` may be
/// left half edited on an error: callers work on a parsed copy.
fn apply_settings(
    channel_type: SettingsChannelType,
    entry: &mut Map<String, Value>,
    changes: &HashMap<String, Value>,
    env_is_set: &dyn Fn(&str) -> bool,
) -> Result<(), ToggleError> {
    let properties = channel_type.form_properties();
    if let Some(unknown) = changes
        .keys()
        .find(|setting| !properties.contains_key(*setting))
    {
        return Err(ToggleError::UnknownField(unknown.clone()));
    }
    for (setting, property) in &properties {
        let Some(value) = changes.get(setting) else {
            continue;
        };
        let kind =
            field_kind(property).ok_or_else(|| ToggleError::UnknownField(setting.clone()))?;
        let submitted = if value.is_null() {
            blank_value(&kind)
        } else {
            value.clone()
        };
        let checked = check_value(&kind, &submitted)
            .map_err(|reason| ToggleError::InvalidValue(setting.clone(), reason))?;
        check_placeholders(setting, &checked, env_is_set, UNSET_VARIABLE_EXPLANATION)?;
        let is_kept_secret = property["x-secret"] == true && is_blank(&checked);
        if !is_kept_secret {
            store_value(SETTING_SPELLINGS, entry, setting, checked);
        }
    }
    Ok(())
}

/// `text` with a new channel of `channel_type` (`channels.<type>`, `channels` created
/// when missing) holding the submitted `values`; settings not submitted keep their
/// defaults. Refused when a channel of that type exists (adding never overwrites) or
/// the channel could not start (see [`SettingsChannelType::check_entry`]).
pub fn with_channel_added(
    text: &str,
    channel_type: SettingsChannelType,
    values: &HashMap<String, Value>,
) -> Result<String, ToggleError> {
    with_channel_added_in(text, channel_type, values, &process_env_is_set)
}

/// [`with_channel_added`] with the environment lookup passed in.
fn with_channel_added_in(
    text: &str,
    channel_type: SettingsChannelType,
    values: &HashMap<String, Value>,
    env_is_set: &dyn Fn(&str) -> bool,
) -> Result<String, ToggleError> {
    let mut root = parse_root(text)?;
    let channels = channels_object_mut(&mut root)?;
    if channels.contains_key(channel_type.key()) {
        return Err(ToggleError::ChannelExists);
    }
    let mut entry = Map::new();
    apply_settings(channel_type, &mut entry, values, env_is_set)?;
    channel_type.check_entry(&entry)?;
    channels.insert(channel_type.key().to_string(), Value::Object(entry));
    rendered_and_checked(&root, text)
}

/// `text` with the submitted `changes` (setting name -> value, as the form names them)
/// applied to `channels.<type>`; keys the form does not know are kept. Nothing is applied
/// unless every change is valid and the channel could still start. `None` when the entry
/// already says all of it. The channel must be set up.
pub fn with_channel_settings(
    text: &str,
    channel_type: SettingsChannelType,
    changes: &HashMap<String, Value>,
) -> Result<Option<String>, ToggleError> {
    with_channel_settings_in(text, channel_type, changes, &process_env_is_set)
}

/// [`with_channel_settings`] with the environment lookup passed in.
fn with_channel_settings_in(
    text: &str,
    channel_type: SettingsChannelType,
    changes: &HashMap<String, Value>,
    env_is_set: &dyn Fn(&str) -> bool,
) -> Result<Option<String>, ToggleError> {
    let mut root = parse_root(text)?;
    let entry = channels_object_mut(&mut root)?
        .get_mut(channel_type.key())
        .and_then(Value::as_object_mut)
        .ok_or(ToggleError::ChannelNotConfigured)?;
    let before = entry.clone();
    apply_settings(channel_type, entry, changes, env_is_set)?;
    if *entry == before {
        return Ok(None);
    }
    channel_type.check_entry(entry)?;
    rendered_and_checked(&root, text).map(Some)
}

/// `text` without the whole `channels.<type>` entry, including any per-channel
/// overrides. The channel must be set up.
pub fn without_channel(
    text: &str,
    channel_type: SettingsChannelType,
) -> Result<String, ToggleError> {
    let mut root = parse_root(text)?;
    let channels = channels_object_mut(&mut root)?;
    channels
        .shift_remove(channel_type.key())
        .ok_or(ToggleError::ChannelNotConfigured)?;
    rendered_and_checked(&root, text)
}

// ── email ─────────────────────────────────────────────────────────────────────

/// The typed email settings of a raw entry, `None` when it does not have their shape.
fn email_config_of(entry: &Map<String, Value>) -> Option<EmailConfig> {
    serde_json::from_value(Value::Object(entry.clone())).ok()
}

/// The form label of `setting` (its `x-label`), else the setting name itself.
fn setting_label(properties: &Map<String, Value>, setting: &str) -> String {
    properties
        .get(setting)
        .and_then(|property| property["x-label"].as_str())
        .unwrap_or(setting)
        .to_string()
}

/// The labels of the required email settings `config` leaves blank.
fn missing_email_labels(config: &EmailConfig, properties: &Map<String, Value>) -> Vec<String> {
    config
        .missing_required_settings()
        .into_iter()
        .map(|setting| setting_label(properties, setting))
        .collect()
}

/// Refuse an email entry without its IMAP and SMTP connection settings, or switched on
/// without consent. The error names the first blank setting and lists the labels of all
/// of them.
fn check_email_entry(
    entry: &Map<String, Value>,
    properties: &Map<String, Value>,
) -> Result<(), ToggleError> {
    let config = email_config_of(entry).ok_or_else(|| {
        ToggleError::InvalidConfig(format!(
            "channels.{EMAIL_CHANNEL_KEY} is not a valid email channel configuration."
        ))
    })?;
    if let Some(first_missing) = config.missing_required_settings().first() {
        return Err(ToggleError::InvalidValue(
            first_missing.to_string(),
            format!(
                "the channel cannot start without it. Still blank: {}.",
                missing_email_labels(&config, properties).join(", ")
            ),
        ));
    }
    if config.enabled && !config.consent_granted {
        return Err(ToggleError::InvalidValue(
            "consentGranted".to_string(),
            CONSENT_REQUIRED_REASON.to_string(),
        ));
    }
    Ok(())
}

/// The sender address of outgoing mail (as the channel picks it) and the IMAP host,
/// joined; blank parts are left out.
fn email_summary(config: &EmailConfig) -> String {
    let sender = [
        &config.from_address,
        &config.smtp_username,
        &config.imap_username,
    ]
    .into_iter()
    .map(|text| text.trim())
    .find(|text| !text.is_empty())
    .unwrap_or_default();
    [sender, config.imap_host.trim()]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(SUMMARY_SEPARATOR)
}

/// Why a configured email channel will not start or will ignore mail.
fn email_notes(config: &EmailConfig, properties: &Map<String, Value>) -> Vec<String> {
    let mut notes = Vec::new();
    let missing = missing_email_labels(config, properties);
    if !missing.is_empty() {
        notes.push(format!(
            "Missing settings: {}. The channel will not start.",
            missing.join(", ")
        ));
    }
    if !config.consent_granted {
        notes.push(CONSENT_MISSING_NOTE.to_string());
    }
    if config.allow_from.is_empty() {
        notes.push(NO_ALLOWED_SENDERS_NOTE.to_string());
    }
    notes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tools::tool_settings_api::ToolFieldSummary;
    use serde_json::json;

    const IMAP_SECRET: &str = "imap-SECRET-pw";
    const SMTP_SECRET: &str = "smtp-SECRET-pw";

    /// A config with a working email channel, an unknown per-channel key and a
    /// placeholder elsewhere.
    fn config_text() -> String {
        format!(
            r#"{{
  "providers": {{"openai": {{"apiKey": "${{OPENAI_API_KEY}}"}}}},
  "channels": {{
    "sendProgress": true,
    "email": {{
      "enabled": true,
      "consentGranted": true,
      "imapHost": "imap.example.com",
      "imapUsername": "bot@example.com",
      "imapPassword": "{IMAP_SECRET}",
      "smtpHost": "smtp.example.com",
      "smtpUsername": "bot@example.com",
      "smtpPassword": "{SMTP_SECRET}",
      "allowFrom": ["gil@example.com"],
      "sendProgress": false
    }}
  }}
}}
"#
        )
    }

    /// Every setting a new channel needs, with the passwords as placeholders.
    fn complete_values() -> HashMap<String, Value> {
        change(&[
            ("imapHost", json!("imap.example.com")),
            ("imapUsername", json!("bot@example.com")),
            ("imapPassword", json!("${IMAP_PASSWORD}")),
            ("smtpHost", json!("smtp.example.com")),
            ("smtpUsername", json!("bot@example.com")),
            ("smtpPassword", json!("${SMTP_PASSWORD}")),
        ])
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

    fn added(text: &str, values: &HashMap<String, Value>) -> Result<String, ToggleError> {
        with_channel_added_in(text, SettingsChannelType::Email, values, &all_set)
    }

    fn edited(text: &str, changes: &HashMap<String, Value>) -> Result<Option<String>, ToggleError> {
        with_channel_settings_in(text, SettingsChannelType::Email, changes, &all_set)
    }

    fn email(text: &str) -> Value {
        serde_json::from_str::<Value>(text).unwrap()["channels"]["email"].clone()
    }

    fn email_row(text: &str, with_forms: bool) -> ChannelRow {
        let rows = channels_overview(text, with_forms).unwrap();
        assert_eq!(rows.len(), 1);
        rows.into_iter().next().unwrap()
    }

    fn field<'a>(form: &'a ToolSettingsSummary, name: &str) -> &'a ToolFieldSummary {
        form.fields
            .iter()
            .find(|field| field.name == name)
            .unwrap_or_else(|| panic!("no field {name}"))
    }

    // ── channel types ─────────────────────────────────────────────────────────

    #[test]
    fn channel_types_are_named_by_their_config_key() {
        assert_eq!(SettingsChannelType::Email.key(), "email");
        assert_eq!(SettingsChannelType::Email.title(), "Email");
        assert_eq!(
            SettingsChannelType::from_key("email"),
            Some(SettingsChannelType::Email)
        );
        assert_eq!(SettingsChannelType::from_key("fax"), None);
        assert_eq!(
            SettingsChannelType::parse("fax"),
            Err(ToggleError::UnknownChannelType)
        );
    }

    // ── overview ──────────────────────────────────────────────────────────────

    #[test]
    fn a_missing_channel_is_a_row_with_the_blank_form() {
        let row = email_row("{}", true);

        assert_eq!(row.channel_type, "email");
        assert_eq!(row.title, "Email");
        assert!(!row.configured && !row.enabled);
        assert!(row.summary.is_empty() && row.notes.is_empty());
        assert!(row.form.is_none());
        let blank = row.new_form.expect("a blank form to create the channel");
        assert_eq!(field(&blank, "imapPort").value, Some(Value::Null));
        assert_eq!(field(&blank, "imapPort").default, Some(json!(993)));
        assert_eq!(field(&blank, "verifyDkim").value, Some(json!(true)));
    }

    #[test]
    fn a_configured_channel_is_summarised_with_its_edit_form() {
        let row = email_row(&config_text(), true);

        assert!(row.configured && row.enabled);
        assert_eq!(row.summary, "bot@example.com · imap.example.com");
        assert!(row.notes.is_empty(), "{:?}", row.notes);
        assert!(row.new_form.is_none());
        let form = row.form.expect("the edit form");
        assert_eq!(form.name, "email");
        assert_eq!(
            field(&form, "imapHost").value,
            Some(json!("imap.example.com"))
        );
        assert_eq!(field(&form, "imapHost").group, "Incoming mail (IMAP)");
        let password = field(&form, "imapPassword");
        assert!(password.secret && password.configured && password.value.is_none());
    }

    #[test]
    fn the_form_lists_the_email_settings_in_declaration_order() {
        let form = email_row("{}", true).new_form.unwrap();

        let names: Vec<&str> = form.fields.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names[..3], ["enabled", "consentGranted", "imapHost"]);
        assert_eq!(names.len(), 26);
        assert!(
            !names.contains(&"sendProgress"),
            "only EmailConfig settings"
        );
    }

    #[test]
    fn rows_carry_forms_only_when_asked_to() {
        assert!(email_row(&config_text(), false).form.is_none());
        assert!(email_row("{}", false).new_form.is_none());
        let wire = serde_json::to_value(email_row("{}", false)).unwrap();
        assert!(wire.get("form").is_none() && wire.get("new_form").is_none());
    }

    #[test]
    fn rows_never_contain_password_text() {
        for with_forms in [true, false] {
            let wire = serde_json::to_string(&email_row(&config_text(), with_forms)).unwrap();
            assert!(
                !wire.contains(IMAP_SECRET) && !wire.contains(SMTP_SECRET),
                "{wire}"
            );
        }
    }

    #[test]
    fn notes_warn_when_the_channel_will_not_start_or_ignores_mail() {
        let text = r#"{"channels": {"email": {"enabled": true, "imapHost": "imap.x"}}}"#;

        let row = email_row(text, false);

        assert_eq!(row.summary, "imap.x");
        assert_eq!(row.notes.len(), 3, "{:?}", row.notes);
        assert!(row.notes[0].starts_with("Missing settings: IMAP username, IMAP password"));
        assert_eq!(row.notes[1], CONSENT_MISSING_NOTE);
        assert_eq!(row.notes[2], NO_ALLOWED_SENDERS_NOTE);
    }

    #[test]
    fn an_unreadable_entry_is_flagged_instead_of_failing_the_page() {
        let text = r#"{"channels": {"email": {"imapPort": "x"}}}"#;

        let row = email_row(text, true);

        assert!(row.configured);
        assert_eq!(row.notes, vec![UNREADABLE_ENTRY_NOTE.to_string()]);
        assert!(row.form.is_some(), "the form still lets it be fixed");
    }

    #[test]
    fn the_summary_prefers_the_from_address() {
        let config = EmailConfig {
            from_address: "help@example.com".into(),
            smtp_username: "smtp-user".into(),
            imap_host: "imap.example.com".into(),
            ..EmailConfig::default()
        };
        assert_eq!(
            email_summary(&config),
            "help@example.com · imap.example.com"
        );
        assert_eq!(email_summary(&EmailConfig::default()), "");
    }

    // ── add ───────────────────────────────────────────────────────────────────

    #[test]
    fn adding_creates_the_channels_section_and_keeps_placeholders() {
        let updated = added("{}\n", &complete_values()).unwrap();

        let entry = email(&updated);
        assert_eq!(entry["imapHost"], "imap.example.com");
        assert_eq!(entry["imapPassword"], "${IMAP_PASSWORD}");
        assert!(entry.get("enabled").is_none(), "only submitted settings");
        assert!(updated.ends_with('\n'));
        assert!(super::super::config_file::validate_config_text(&updated).is_ok());
    }

    #[test]
    fn a_new_entry_lists_its_settings_in_the_form_order() {
        let mut values = complete_values();
        values.insert("allowFrom".into(), json!(["gil@example.com"]));
        values.insert("consentGranted".into(), json!(true));
        values.insert("enabled".into(), json!(true));

        let updated = added("{}", &values).unwrap();

        let keys: Vec<String> = email(&updated)
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        assert_eq!(
            keys,
            vec![
                "enabled",
                "consentGranted",
                "imapHost",
                "imapUsername",
                "imapPassword",
                "smtpHost",
                "smtpUsername",
                "smtpPassword",
                "allowFrom"
            ]
        );
    }

    #[test]
    fn adding_leaves_the_rest_of_the_file_alone() {
        let text = r#"{"channels": {"sendProgress": true}, "providers": {"openai": {"apiKey": "${OPENAI_API_KEY}"}}}"#;

        let updated = added(text, &complete_values()).unwrap();

        let mut after: Value = serde_json::from_str(&updated).unwrap();
        after["channels"].as_object_mut().unwrap().remove("email");
        assert_eq!(after, serde_json::from_str::<Value>(text).unwrap());
    }

    #[test]
    fn adding_twice_is_refused() {
        assert_eq!(
            added(&config_text(), &complete_values()),
            Err(ToggleError::ChannelExists)
        );
    }

    #[test]
    fn a_new_channel_without_its_connection_settings_is_refused_naming_them() {
        let mut values = complete_values();
        values.remove("imapPassword");
        values.insert("smtpHost".into(), json!("  "));

        let Err(ToggleError::InvalidValue(setting, reason)) = added("{}", &values) else {
            panic!("expected a refusal");
        };

        assert_eq!(setting, "imapPassword");
        assert!(reason.contains("IMAP password, SMTP host"), "{reason}");
    }

    #[test]
    fn a_blank_password_does_not_count_as_set_on_a_new_channel() {
        let mut values = complete_values();
        values.insert("smtpPassword".into(), json!(""));

        assert!(matches!(
            added("{}", &values),
            Err(ToggleError::InvalidValue(ref s, _)) if s == "smtpPassword"
        ));
    }

    #[test]
    fn enabling_without_consent_is_refused() {
        let mut values = complete_values();
        values.insert("enabled".into(), json!(true));

        assert!(matches!(
            added("{}", &values),
            Err(ToggleError::InvalidValue(ref s, _)) if s == "consentGranted"
        ));
        values.insert("consentGranted".into(), json!(true));
        assert!(added("{}", &values).is_ok());
    }

    #[test]
    fn a_placeholder_for_an_unset_variable_is_refused_naming_it_but_not_the_value() {
        let only_imap = |name: &str| name == "IMAP_PASSWORD";

        let refused = with_channel_added_in(
            "{}",
            SettingsChannelType::Email,
            &complete_values(),
            &only_imap,
        );

        let Err(ToggleError::InvalidValue(setting, reason)) = refused else {
            panic!("expected a refusal");
        };
        assert_eq!(setting, "smtpPassword");
        assert!(reason.contains("${SMTP_PASSWORD}") && reason.contains("environment"));
    }

    #[test]
    fn unknown_settings_and_bad_values_are_refused() {
        let refuse = |pairs: &[(&str, Value)]| edited(&config_text(), &change(pairs)).unwrap_err();

        assert_eq!(
            refuse(&[("sendProgress", json!(true))]),
            ToggleError::UnknownField("sendProgress".into())
        );
        for (setting, value) in [
            ("pollIntervalSeconds", json!(1)),
            ("imapPort", json!(70000)),
            ("imapPort", json!("993")),
            ("allowFrom", json!("a@b")),
            ("verifyDkim", json!("yes")),
        ] {
            assert!(
                matches!(refuse(&[(setting, value.clone())]), ToggleError::InvalidValue(ref s, _) if s == setting),
                "{setting} = {value}"
            );
        }
    }

    // ── edit ──────────────────────────────────────────────────────────────────

    #[test]
    fn editing_writes_the_change_and_keeps_unknown_keys() {
        let changes = change(&[
            ("pollIntervalSeconds", json!(120)),
            ("allowFrom", json!(["gil@example.com", "ana@example.com"])),
        ]);

        let updated = edited(&config_text(), &changes).unwrap().unwrap();

        let entry = email(&updated);
        assert_eq!(entry["pollIntervalSeconds"], 120);
        assert_eq!(entry["allowFrom"][1], "ana@example.com");
        assert_eq!(entry["sendProgress"], false, "unknown keys survive");
        assert!(updated.contains("${OPENAI_API_KEY}"));
    }

    #[test]
    fn a_blank_password_on_edit_keeps_the_stored_one() {
        let changes = change(&[
            ("imapPassword", json!("")),
            ("smtpPassword", Value::Null),
            ("imapHost", json!("mail.example.com")),
        ]);

        let updated = edited(&config_text(), &changes).unwrap().unwrap();

        let entry = email(&updated);
        assert_eq!(entry["imapPassword"], IMAP_SECRET);
        assert_eq!(entry["smtpPassword"], SMTP_SECRET);
        assert_eq!(entry["imapHost"], "mail.example.com");
    }

    #[test]
    fn a_new_password_replaces_the_stored_one() {
        let changes = change(&[("smtpPassword", json!("${SMTP_PASSWORD}"))]);

        let updated = edited(&config_text(), &changes).unwrap().unwrap();

        assert_eq!(email(&updated)["smtpPassword"], "${SMTP_PASSWORD}");
    }

    #[test]
    fn submitting_the_stored_values_changes_nothing() {
        let changes = change(&[
            ("imapHost", json!("imap.example.com")),
            ("imapPassword", json!("")),
            ("enabled", json!(true)),
        ]);

        assert_eq!(edited(&config_text(), &changes), Ok(None));
    }

    #[test]
    fn an_edit_that_would_stop_the_channel_is_refused() {
        let blank_host = change(&[("smtpHost", json!(""))]);
        let consent_off = change(&[("consentGranted", json!(false))]);

        assert!(matches!(
            edited(&config_text(), &blank_host),
            Err(ToggleError::InvalidValue(ref s, _)) if s == "smtpHost"
        ));
        assert!(matches!(
            edited(&config_text(), &consent_off),
            Err(ToggleError::InvalidValue(ref s, _)) if s == "consentGranted"
        ));
        let off_and_no_consent =
            change(&[("enabled", json!(false)), ("consentGranted", json!(false))]);
        assert!(edited(&config_text(), &off_and_no_consent).is_ok());
    }

    #[test]
    fn editing_a_channel_that_is_not_set_up_is_refused() {
        let changes = change(&[("imapHost", json!("x"))]);

        assert_eq!(
            edited("{}", &changes),
            Err(ToggleError::ChannelNotConfigured)
        );
    }

    #[test]
    fn the_public_edit_and_add_read_the_real_environment() {
        let mut values = complete_values();
        values.insert(
            "imapPassword".into(),
            json!("${RUST_BOT_SURELY_UNSET_VARIABLE_4711}"),
        );

        assert!(matches!(
            with_channel_added("{}", SettingsChannelType::Email, &values),
            Err(ToggleError::InvalidValue(..))
        ));
        assert!(matches!(
            with_channel_settings(&config_text(), SettingsChannelType::Email, &values),
            Err(ToggleError::InvalidValue(..))
        ));
    }

    // ── delete ────────────────────────────────────────────────────────────────

    #[test]
    fn deleting_removes_the_whole_entry_and_nothing_else() {
        let updated = without_channel(&config_text(), SettingsChannelType::Email).unwrap();

        let root: Value = serde_json::from_str(&updated).unwrap();
        assert!(root["channels"].get("email").is_none());
        assert_eq!(root["channels"]["sendProgress"], true);
        assert!(updated.contains("${OPENAI_API_KEY}"));
        assert!(!updated.contains(IMAP_SECRET));
    }

    #[test]
    fn deleting_a_channel_that_is_not_set_up_is_refused() {
        assert_eq!(
            without_channel("{}", SettingsChannelType::Email),
            Err(ToggleError::ChannelNotConfigured)
        );
    }

    #[test]
    fn add_edit_delete_round_trip() {
        let created = added("{}\n", &complete_values()).unwrap();
        let changes = change(&[("subjectPrefix", json!("AW: "))]);
        let edited_text = edited(&created, &changes).unwrap().unwrap();
        let deleted = without_channel(&edited_text, SettingsChannelType::Email).unwrap();

        assert_eq!(
            email(&edited_text)["subjectPrefix"],
            "AW:",
            "text is trimmed"
        );
        assert!(email_row(&deleted, true).new_form.is_some());
        assert_eq!(
            serde_json::from_str::<Value>(&deleted).unwrap(),
            json!({"channels": {}})
        );
    }
}
