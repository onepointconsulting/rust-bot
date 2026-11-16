//! Per-channel configuration types.
//!
//! The field doc comments of [`EmailConfig`] are the help texts of the WebUI's
//! Settings → Channels form (generated from the struct's JSON Schema). `x-group` sorts
//! the fields under headings of that form.

use std::collections::HashMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::config::schema::Config;

/// Key of the email channel inside `channels` (`channels.email`).
pub const EMAIL_CHANNEL_KEY: &str = "email";

/// Form group of the switches that decide whether the channel runs at all.
const GROUP_GENERAL: &str = "General";
/// Form group of the inbound (IMAP) settings.
const GROUP_IMAP: &str = "Incoming mail (IMAP)";
/// Form group of the outbound (SMTP) settings.
const GROUP_SMTP: &str = "Outgoing mail (SMTP)";
/// Form group of how mail is polled, read and answered.
const GROUP_BEHAVIOUR: &str = "Behaviour";
/// Form group of who may write to the bot and how senders are verified.
const GROUP_SECURITY: &str = "Security";
/// Form group of the attachment limits.
const GROUP_ATTACHMENTS: &str = "Attachments";

/// Email channel configuration (IMAP inbound + SMTP outbound).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", default)]
pub struct EmailConfig {
    /// Start the email channel with the gateway.
    #[schemars(extend("x-group" = GROUP_GENERAL))]
    pub enabled: bool,

    /// The owner of the mailbox has explicitly agreed that the bot reads and sends
    /// mail with it. The channel does not start without it.
    #[schemars(extend("x-group" = GROUP_GENERAL, "x-label" = "Consent granted"))]
    pub consent_granted: bool,

    /// Host name of the IMAP server that receives the bot's mail, e.g.
    /// imap.example.com. Required.
    #[schemars(extend("x-group" = GROUP_IMAP, "x-label" = "IMAP host"))]
    pub imap_host: String,

    /// Port of the IMAP server (993 for SSL, 143 without).
    #[schemars(extend("x-group" = GROUP_IMAP, "x-label" = "IMAP port"))]
    pub imap_port: u16,

    /// User name for the IMAP login, usually the mailbox address. Required.
    #[schemars(extend("x-group" = GROUP_IMAP, "x-label" = "IMAP username"))]
    pub imap_username: String,

    /// Password for the IMAP login. Required. ${VAR} reads it from an environment
    /// variable.
    #[schemars(extend("x-group" = GROUP_IMAP, "x-label" = "IMAP password", "x-secret" = true))]
    pub imap_password: String,

    /// Mailbox (folder) polled for new mail. Blank means INBOX.
    #[schemars(extend("x-group" = GROUP_IMAP, "x-label" = "IMAP mailbox"))]
    pub imap_mailbox: String,

    /// Connect to the IMAP server over SSL/TLS.
    #[schemars(extend("x-group" = GROUP_IMAP, "x-label" = "Use SSL"))]
    pub imap_use_ssl: bool,

    /// Host name of the SMTP server that sends the bot's replies, e.g.
    /// smtp.example.com. Required.
    #[schemars(extend("x-group" = GROUP_SMTP, "x-label" = "SMTP host"))]
    pub smtp_host: String,

    /// Port of the SMTP server (587 for STARTTLS, 465 for SSL).
    #[schemars(extend("x-group" = GROUP_SMTP, "x-label" = "SMTP port"))]
    pub smtp_port: u16,

    /// User name for the SMTP login. Required.
    #[schemars(extend("x-group" = GROUP_SMTP, "x-label" = "SMTP username"))]
    pub smtp_username: String,

    /// Password for the SMTP login. Required. ${VAR} reads it from an environment
    /// variable.
    #[schemars(extend("x-group" = GROUP_SMTP, "x-label" = "SMTP password", "x-secret" = true))]
    pub smtp_password: String,

    /// Upgrade the SMTP connection with STARTTLS.
    #[schemars(extend("x-group" = GROUP_SMTP, "x-label" = "Use STARTTLS"))]
    pub smtp_use_tls: bool,

    /// Connect to the SMTP server over SSL/TLS from the start (port 465). Takes
    /// precedence over STARTTLS.
    #[schemars(extend("x-group" = GROUP_SMTP, "x-label" = "Use SSL"))]
    pub smtp_use_ssl: bool,

    /// Sender address of outgoing mail. Blank uses the SMTP username, then the IMAP
    /// username.
    #[schemars(extend("x-group" = GROUP_SMTP, "x-label" = "From address"))]
    pub from_address: String,

    /// Answer incoming mail automatically. When off, the bot only sends mail it is
    /// asked to send.
    #[schemars(extend("x-group" = GROUP_BEHAVIOUR, "x-label" = "Automatic replies"))]
    pub auto_reply_enabled: bool,

    /// Seconds between two checks for new mail (at least 5).
    #[schemars(
        range(min = 5),
        extend("x-group" = GROUP_BEHAVIOUR, "x-label" = "Poll interval (seconds)")
    )]
    pub poll_interval_seconds: u32,

    /// Mark fetched mail as read on the server.
    #[schemars(extend("x-group" = GROUP_BEHAVIOUR, "x-label" = "Mark as read"))]
    pub mark_seen: bool,

    /// Longest mail body (in characters) passed to the agent; longer bodies are cut.
    #[schemars(extend("x-group" = GROUP_BEHAVIOUR, "x-label" = "Maximum body length"))]
    pub max_body_chars: u32,

    /// Prefix added to the subject of a reply, e.g. "Re: ".
    #[schemars(extend("x-group" = GROUP_BEHAVIOUR, "x-label" = "Reply subject prefix"))]
    pub subject_prefix: String,

    /// Sender addresses the bot accepts mail from, one per line (no commas); * accepts
    /// everyone. Mail from anyone else is ignored, and an empty list ignores all mail.
    #[schemars(extend("x-group" = GROUP_SECURITY, "x-label" = "Allowed senders"))]
    pub allow_from: Vec<String>,

    /// Only accept mail whose Authentication-Results header reports dkim=pass
    /// (anti-spoofing).
    #[schemars(extend("x-group" = GROUP_SECURITY, "x-label" = "Require DKIM"))]
    pub verify_dkim: bool,

    /// Only accept mail whose Authentication-Results header reports spf=pass
    /// (anti-spoofing).
    #[schemars(extend("x-group" = GROUP_SECURITY, "x-label" = "Require SPF"))]
    pub verify_spf: bool,

    /// MIME types of attachments passed to the agent, one per line, e.g. application/pdf
    /// or image/*; * allows all. Empty ignores every attachment.
    #[schemars(extend("x-group" = GROUP_ATTACHMENTS, "x-label" = "Allowed attachment types"))]
    pub allowed_attachment_types: Vec<String>,

    /// Largest attachment (in bytes) passed to the agent; larger ones are skipped.
    #[schemars(extend("x-group" = GROUP_ATTACHMENTS, "x-label" = "Maximum attachment size (bytes)"))]
    pub max_attachment_size: u32,

    /// Most attachments of one mail passed to the agent.
    #[schemars(extend("x-group" = GROUP_ATTACHMENTS, "x-label" = "Maximum attachments per mail"))]
    pub max_attachments_per_email: u32,
}

impl Default for EmailConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            consent_granted: false,

            imap_host: String::new(),
            imap_port: 993,
            imap_username: String::new(),
            imap_password: String::new(),
            imap_mailbox: String::new(),
            imap_use_ssl: true,

            smtp_host: String::new(),
            smtp_port: 587,
            smtp_username: String::new(),
            smtp_password: String::new(),
            smtp_use_tls: true,
            smtp_use_ssl: false,
            from_address: String::new(),

            auto_reply_enabled: false,
            poll_interval_seconds: 60,
            mark_seen: true,
            max_body_chars: 12000,

            subject_prefix: "Re: ".to_string(),
            allow_from: Vec::new(),

            // Email authentication verification (anti-spoofing)
            verify_dkim: true, // Require Authentication-Results with dkim=pass
            verify_spf: true,  // Require Authentication-Results with spf=pass

            allowed_attachment_types: Vec::new(),
            // 2 MB per attachment
            max_attachment_size: 2 * 1024 * 1024,
            max_attachments_per_email: 5,
        }
    }
}

impl EmailConfig {
    /// Serialize this config to a map using camelCase keys (Pydantic `model_dump(by_alias=True)`).
    pub fn to_config_map(&self) -> HashMap<String, serde_json::Value> {
        match serde_json::to_value(self).expect("EmailConfig should serialize") {
            serde_json::Value::Object(map) => map.into_iter().collect(),
            _ => HashMap::new(),
        }
    }

    /// The settings (camelCase, as `config.json` names them) the channel cannot start
    /// without that are blank: the IMAP and SMTP host, username and password. Empty when
    /// all of them are set.
    pub fn missing_required_settings(&self) -> Vec<&'static str> {
        [
            ("imapHost", &self.imap_host),
            ("imapUsername", &self.imap_username),
            ("imapPassword", &self.imap_password),
            ("smtpHost", &self.smtp_host),
            ("smtpUsername", &self.smtp_username),
            ("smtpPassword", &self.smtp_password),
        ]
        .into_iter()
        .filter(|(_, value)| value.trim().is_empty())
        .map(|(setting, _)| setting)
        .collect()
    }
}

/// Check the typed channel entries of `config` can be read: `channels.email`, when
/// present, must have the shape of an [`EmailConfig`]. Without this check a malformed
/// entry would be dropped silently and the channel would not start.
pub fn validate_channel_configs(config: &Config) -> Result<(), String> {
    if let Some(email) = config.channels.extra.get(EMAIL_CHANNEL_KEY) {
        serde_json::from_value::<EmailConfig>(email.clone())
            .map_err(|error| format!("channels.{EMAIL_CHANNEL_KEY}: {error}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn config_with_channels(channels: serde_json::Value) -> Config {
        serde_json::from_value(json!({ "channels": channels })).unwrap()
    }

    #[test]
    fn a_default_email_config_misses_every_required_setting() {
        assert_eq!(
            EmailConfig::default().missing_required_settings(),
            vec![
                "imapHost",
                "imapUsername",
                "imapPassword",
                "smtpHost",
                "smtpUsername",
                "smtpPassword"
            ]
        );
    }

    #[test]
    fn blank_text_counts_as_missing_and_filled_settings_do_not() {
        let config = EmailConfig {
            imap_host: "imap.example.com".into(),
            imap_username: "bot".into(),
            imap_password: "${IMAP_PASSWORD}".into(),
            smtp_host: "   ".into(),
            smtp_username: "bot".into(),
            smtp_password: "secret".into(),
            ..EmailConfig::default()
        };

        assert_eq!(config.missing_required_settings(), vec!["smtpHost"]);
    }

    #[test]
    fn a_config_without_an_email_channel_is_valid() {
        assert!(validate_channel_configs(&Config::default()).is_ok());
    }

    #[test]
    fn a_well_formed_email_channel_is_valid() {
        let config = config_with_channels(json!({
            "email": {"enabled": true, "imapHost": "imap.example.com", "imapPort": 993}
        }));

        assert!(validate_channel_configs(&config).is_ok());
    }

    #[test]
    fn a_malformed_email_channel_is_refused_naming_it() {
        let config = config_with_channels(json!({ "email": {"imapPort": "not a port"} }));

        let error = validate_channel_configs(&config).unwrap_err();

        assert!(error.starts_with("channels.email:"), "{error}");
    }

    #[test]
    fn the_schema_carries_groups_labels_and_secrets() {
        let schema = serde_json::to_value(schemars::schema_for!(EmailConfig)).unwrap();
        let properties = &schema["properties"];

        assert_eq!(properties["imapHost"]["x-label"], "IMAP host");
        assert_eq!(properties["imapHost"]["x-group"], GROUP_IMAP);
        assert_eq!(properties["imapPassword"]["x-secret"], true);
        assert_eq!(properties["smtpPassword"]["x-secret"], true);
        assert_eq!(properties["pollIntervalSeconds"]["minimum"], 5);
        assert_eq!(properties["maxAttachmentSize"]["default"], 2 * 1024 * 1024);
        for list_setting in ["allowFrom", "allowedAttachmentTypes"] {
            let help = properties[list_setting]["description"].as_str().unwrap();
            assert!(help.contains("one per line"), "{list_setting}: {help}");
        }
        assert!(
            properties
                .as_object()
                .unwrap()
                .values()
                .all(|property| property["x-group"].is_string()
                    && property["description"].is_string()),
            "every field has a group and help text"
        );
    }
}
