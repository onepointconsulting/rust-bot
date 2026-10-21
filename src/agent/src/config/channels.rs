//! Per-channel configuration types.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Email channel configuration (IMAP inbound + SMTP outbound).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct EmailConfig {
    pub enabled: bool,
    pub consent_granted: bool,

    pub imap_host: String,
    pub imap_port: u16,
    pub imap_username: String,
    pub imap_password: String,
    pub imap_mailbox: String,
    pub imap_use_ssl: bool,

    pub smtp_host: String,
    pub smtp_port: u16,
    pub smtp_username: String,
    pub smtp_password: String,
    pub smtp_use_tls: bool,
    pub smtp_use_ssl: bool,
    pub from_address: String,

    pub auto_reply_enabled: bool,
    pub poll_interval_seconds: u32,
    pub mark_seen: bool,
    pub max_body_chars: u32,
    pub subject_prefix: String,
    pub allow_from: Vec<String>,

    // Email authentication verification (anti-spoofing)
    /// Require Authentication-Results with dkim=pass
    pub verify_dkim: bool,

    /// Require Authentication-Results with spf=pass
    pub verify_spf: bool,

    /// Attachment handling — set allowed types to enable (e.g. ["application/pdf", "image/*"], or ["*"] for all)
    pub allowed_attachment_types: Vec<String>,
    pub max_attachment_size: u32,
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
            // 2MB per attachment
            max_attachment_size: 2 * 1024 * 1024, // 10MB
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
}
