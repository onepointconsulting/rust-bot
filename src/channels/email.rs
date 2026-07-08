use async_trait::async_trait;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use serde::Serialize;

use crate::{
    bus::{events::OutboundMessage, queue::MessageBus},
    channels::base::{BaseChannel, BaseChannelCommon},
    config::schema::ChannelsConfig,
};

use async_imap::{Client, Session};
use native_tls::TlsConnector as NativeTlsConnector;
use tokio::net::TcpStream;
use tokio_native_tls::{TlsConnector, TlsStream};

type ImapTlsStream = TlsStream<TcpStream>;

/// Unauthenticated IMAP client over plain TCP or implicit TLS.
enum ImapClient {
    Plain(Client<TcpStream>),
    Tls(Client<ImapTlsStream>),
}

/// Authenticated IMAP session over plain TCP or implicit TLS.
enum ImapSession {
    Plain(Session<TcpStream>),
    Tls(Session<ImapTlsStream>),
}

impl ImapClient {
    async fn login(self, config: &EmailConfig) -> Result<ImapSession, String> {
        match self {
            ImapClient::Plain(client) => client
                .login(&config.imap_username, &config.imap_password)
                .await
                .map(ImapSession::Plain)
                .map_err(|(e, _)| e.to_string()),
            ImapClient::Tls(client) => client
                .login(&config.imap_username, &config.imap_password)
                .await
                .map(ImapSession::Tls)
                .map_err(|(e, _)| e.to_string()),
        }
    }
}

impl ImapSession {
    async fn select_mailbox(&mut self, mailbox: &str) -> Result<(), String> {
        match self {
            ImapSession::Plain(session) => session
                .select(mailbox)
                .await
                .map(|_| ())
                .map_err(|e| e.to_string()),
            ImapSession::Tls(session) => session
                .select(mailbox)
                .await
                .map(|_| ())
                .map_err(|e| e.to_string()),
        }
    }

    async fn search(
        &mut self,
        criteria: &[&str],
    ) -> Result<std::collections::HashSet<u32>, String> {
        let query = if criteria.is_empty() {
            "ALL".to_string()
        } else {
            criteria.join(" ")
        };
        let seqs = match self {
            ImapSession::Plain(session) => session
                .search(query.as_str())
                .await
                .map_err(|e| e.to_string())?,
            ImapSession::Tls(session) => session
                .search(query.as_str())
                .await
                .map_err(|e| e.to_string())?,
        };
        Ok(seqs)
    }
}

/// Email channel configuration (IMAP inbound + SMTP outbound).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct EmailConfig {
    enabled: bool,
    consent_granted: bool,

    imap_host: String,
    imap_port: u16,
    imap_username: String,
    imap_password: String,
    imap_mailbox: String,
    imap_use_ssl: bool,

    smtp_host: String,
    smtp_port: u16,
    smtp_username: String,
    smtp_password: String,
    smtp_use_tls: bool,
    smtp_use_ssl: bool,
    from_address: String,

    auto_reply_enabled: bool,
    poll_interval_seconds: u32,
    mark_seen: bool,
    max_body_chars: u32,
    subject_prefix: String,
    allow_from: Vec<String>,

    // Email authentication verification (anti-spoofing)
    /// Require Authentication-Results with dkim=pass
    verify_dkim: bool,

    /// Require Authentication-Results with spf=pass
    verify_spf: bool,

    /// Attachment handling — set allowed types to enable (e.g. ["application/pdf", "image/*"], or ["*"] for all)
    allowed_attachment_types: Vec<String>,
    max_attachment_size: u32,
    max_attachments_per_email: u32,
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
    fn to_config_map(&self) -> HashMap<String, serde_json::Value> {
        match serde_json::to_value(self).expect("EmailConfig should serialize") {
            serde_json::Value::Object(map) => map.into_iter().collect(),
            _ => HashMap::new(),
        }
    }
}

/// Email channel.

/// Inbound:
///- Poll IMAP mailbox for unread messages.
///- Convert each message into an inbound event.

/// Outbound:
///- Send responses via SMTP back to the sender address.
///- Send responses via SMTP back to the sender address.
struct EmailChannel {
    name: String,
    display_name: String,
    base: BaseChannelCommon,
    channels_config: ChannelsConfig,
    config: EmailConfig,
    last_subject_by_chat_id: HashMap<String, String>,
    last_message_id_by_chat: HashMap<String, String>,
    processed_uids: HashSet<String>, // Capped to prevent unbounded growth
    running: bool,
}

impl EmailChannel {
    const IMAP_MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    const IMAP_RECONNECT_MARKERS: [&str; 6] = [
        "disconnected for inactivity",
        "eof occurred in violation of protocol",
        "socket error",
        "connection reset",
        "broken pipe",
        "bye",
    ];
    const IMAP_MISSING_MAILBOX_MARKERS: [&str; 5] = [
        "mailbox doesn't exist",
        "select failed",
        "no such mailbox",
        "can't open mailbox",
        "does not exist",
    ];

    const MAX_PROCESSED_UIDS: usize = 100000;

    fn new(config: EmailConfig, bus: Arc<MessageBus>, channels_config: ChannelsConfig) -> Self {
        Self {
            name: "email".to_string(),
            display_name: "Email".to_string(),
            base: BaseChannelCommon {
                bus: bus,
                running: false,
            },
            channels_config,
            config: config,
            last_subject_by_chat_id: HashMap::new(),
            last_message_id_by_chat: HashMap::new(),
            processed_uids: HashSet::new(),
            running: false,
        }
    }

    fn validate_config(&self) -> bool {
        let mut missing = Vec::new();
        if self.config.imap_host.trim().is_empty() {
            missing.push("imap_host".to_string());
        }
        if self.config.imap_username.trim().is_empty() {
            missing.push("imap_username".to_string());
        }
        if self.config.imap_password.trim().is_empty() {
            missing.push("imap_password".to_string());
        }
        if self.config.smtp_host.trim().is_empty() {
            missing.push("smtp_host".to_string());
        }
        if self.config.smtp_username.trim().is_empty() {
            missing.push("smtp_username".to_string());
        }
        if self.config.smtp_password.trim().is_empty() {
            missing.push("smtp_password".to_string());
        }
        if !missing.is_empty() {
            log::error!(
                "Email channel config missing required fields: {}",
                missing.join(", ")
            );
            return false;
        }
        true
    }

    /// Poll IMAP and return parsed unread messages.
    async fn fetch_new_messages(&self) -> Vec<HashMap<String, serde_json::Value>> {
        self.fetch_messages(vec!["UNSEEN"], self.config.mark_seen, true, 0)
            .await
    }

    async fn fetch_messages(
        &self,
        search_criteria: Vec<&str>,
        mark_seen: bool,
        dedupe: bool,
        limit: usize,
    ) -> Vec<HashMap<String, serde_json::Value>> {
        let mut messages = Vec::new();
        let mut cycle_uids: HashSet<String> = HashSet::new();

        let attempt_limit = 2;
        for attempt in 0..attempt_limit {
            match self
                .fetch_messages_once(
                    search_criteria.clone(),
                    mark_seen,
                    dedupe,
                    limit,
                    &mut messages,
                    &mut cycle_uids,
                )
                .await
            {
                Ok(messages) => return messages,
                Err(error) => {
                    if attempt == attempt_limit - 1 {
                        log::error!("Email channel: Failed to fetch messages: {}", error);
                        return messages;
                    }
                    log::warn!("Email channel: Failed to fetch messages: {}", error);
                }
            }
        }
        messages
    }

    /// Fetch messages by arbitrary IMAP search criteria.
    async fn fetch_messages_once(
        &self,
        search_criteria: Vec<&str>,
        mark_seen: bool,
        dedupe: bool,
        limit: usize,
        messages: &mut Vec<HashMap<String, serde_json::Value>>,
        cycle_uids: &mut HashSet<String>,
    ) -> Result<Vec<HashMap<String, serde_json::Value>>, String> {
        let mailbox = if self.config.imap_mailbox.trim().is_empty() {
            "INBOX"
        } else {
            &self.config.imap_mailbox
        };

        let client = Self::connect_imap_client(&self.config).await?;
        let mut session = client.login(&self.config).await?;
        if let Err(e) = session.select_mailbox(mailbox).await {
            log::warn!("Email channel: Failed to select mailbox: {}", e);
            return Ok(messages.clone());
        }
        let seqs = session.search(&search_criteria).await;
        let Ok(mut ids) = seqs else {
            log::warn!("Email channel: Failed to search: {}", seqs.unwrap_err());
            return Ok(messages.clone());
        };
        if limit > 0 && ids.len() > limit {
            let mut sorted: Vec<u32> = ids.into_iter().collect();
            sorted.sort_unstable();
            let start = sorted.len() - limit;
            ids = sorted.into_iter().skip(start).collect();
        }
        for imap_id in ids {
        }

        Ok(messages.clone()) // TODO: Implement
    }

    async fn connect_imap_client(config: &EmailConfig) -> Result<ImapClient, String> {
        let addr = (config.imap_host.as_str(), config.imap_port);
        let tls = TlsConnector::from(
            NativeTlsConnector::new().map_err(|e| e.to_string())?,
        );

        if config.imap_use_ssl {
            // IMAP4_SSL equivalent
            let tcp = TcpStream::connect(addr).await.map_err(|e| e.to_string())?;
            let tls_stream = tls
                .connect(config.imap_host.as_str(), tcp)
                .await
                .map_err(|e| e.to_string())?;
            let mut client = Client::new(tls_stream);
            client
                .read_response()
                .await
                .map_err(|e| e.to_string())?
                .ok_or_else(|| "IMAP server closed before greeting".to_string())?;
            Ok(ImapClient::Tls(client))
        } else {
            // IMAP4 equivalent (plain TCP)
            let tcp = TcpStream::connect(addr).await.map_err(|e| e.to_string())?;
            let mut client = Client::new(tcp);
            client
                .read_response()
                .await
                .map_err(|e| e.to_string())?
                .ok_or_else(|| "IMAP server closed before greeting".to_string())?;
            Ok(ImapClient::Plain(client))
        }
    }
}

#[async_trait]
impl BaseChannel for EmailChannel {
    fn name(&self) -> &'static str {
        "email"
    }

    fn running(&self) -> bool {
        self.running
    }

    fn bus(&self) -> &MessageBus {
        self.base.bus.as_ref()
    }

    fn config(&self) -> &ChannelsConfig {
        &self.channels_config
    }

    fn default_config(&self) -> HashMap<String, serde_json::Value> {
        EmailConfig::default().to_config_map()
    }

    async fn stop(&self) {}

    async fn send(&self, _msg: OutboundMessage) {}

    /// Start polling IMAP for inbound emails.
    async fn start(&mut self) {
        if !self.config.consent_granted {
            log::error!(
                "Email channel disabled: consent_granted is false. 
            Set channels.email.consentGranted=true after explicit user permission."
            );
            return;
        }
        if !self.validate_config() {
            return;
        }
        self.running = true;

        if !self.config.verify_dkim && !self.config.verify_spf {
            log::warn!(
                "Email channel: DKIM and SPF verification are both DISABLED.
                Emails with spoofed From headers will be accepted.
                Set verify_dkim=true and verify_spf=true for anti-spoofing protection."
            )
        }
        log::info!("Starting Email channel (IMAP polling mode)...");
        let _poll_seconds = std::cmp::max(5, self.config.poll_interval_seconds);
        while self.running {}
    }
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn test_default_config() {
        let config = EmailConfig {
            enabled: true,
            consent_granted: true,
            imap_host: "imap.example.com".to_string(),
            ..EmailConfig::default()
        };

        assert!(config.enabled);
        assert!(config.consent_granted);
        assert_eq!(config.imap_host, "imap.example.com".to_string());
    }

    #[test]
    fn default_config_map_matches_email_config_default() {
        let map = EmailConfig::default().to_config_map();

        assert_eq!(map.get("enabled").and_then(|v| v.as_bool()), Some(false));
        assert_eq!(
            map.get("consentGranted").and_then(|v| v.as_bool()),
            Some(false)
        );
        assert_eq!(map.get("imapHost").and_then(|v| v.as_str()), Some(""));
        assert_eq!(map.get("imapPort").and_then(|v| v.as_u64()), Some(993));
        assert_eq!(map.get("smtpPort").and_then(|v| v.as_u64()), Some(587));
        assert_eq!(
            map.get("pollIntervalSeconds").and_then(|v| v.as_u64()),
            Some(60)
        );
        assert_eq!(
            map.get("maxAttachmentSize").and_then(|v| v.as_u64()),
            Some(2 * 1024 * 1024)
        );
        assert_eq!(
            map.get("maxAttachmentsPerEmail").and_then(|v| v.as_u64()),
            Some(5)
        );
    }
}
