use glob::Pattern;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use regex::Regex;
use rand::seq::SliceRandom;
use std::{
    collections::{HashMap, HashSet}, path::PathBuf, sync::{Arc, LazyLock},
};

use futures::TryStreamExt;
use serde::Serialize;

use crate::{
    bus::{events::OutboundMessage, queue::MessageBus}, channels::{
        base::{BaseChannel, BaseChannelCommon},
        types::MessageBytes,
    }, config::{paths::get_media_dir, schema::ChannelsConfig}, utils::helpers::safe_filename,
};

use async_imap::{Client, Session};
use mailparse::{
    addrparse_header, parse_mail, DispositionType, MailHeaderMap, ParsedMail,
};
use native_tls::TlsConnector as NativeTlsConnector;
use tokio::net::TcpStream;
use tokio_native_tls::{TlsConnector, TlsStream};

static BSPF_MATCH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bspf\s*=\s*(pass|neutral)\b").unwrap());
static BDKIM_MATCH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bdkim\s*=\s*(pass|neutral)\b").unwrap());

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

    async fn fetch(&mut self, imap_id: u32, query: &str) -> Result<MessageBytes, String> {
        let imap_id_str = imap_id.to_string();
        let fetches = match self {
            ImapSession::Plain(session) => {
                let stream = session
                    .fetch(&imap_id_str, query)
                    .await
                    .map_err(|e| e.to_string())?;
                stream
                    .try_collect::<Vec<_>>()
                    .await
                    .map_err(|e| e.to_string())?
            }
            ImapSession::Tls(session) => {
                let stream = session
                    .fetch(&imap_id_str, query)
                    .await
                    .map_err(|e| e.to_string())?;
                stream
                    .try_collect::<Vec<_>>()
                    .await
                    .map_err(|e| e.to_string())?
            }
        };

        let fetch = fetches
            .into_iter()
            .next()
            .ok_or_else(|| format!("IMAP FETCH returned no message for id {imap_id}"))?;

        let uid = fetch
            .uid
            .ok_or_else(|| format!("IMAP FETCH missing UID for id {imap_id}"))?;
        let bytes = fetch
            .body()
            .ok_or_else(|| format!("IMAP FETCH missing body for id {imap_id}"))?
            .to_vec();

        Ok(MessageBytes::new(uid, bytes))
    }

    /// Mark message(s) with IMAP flags via `UID STORE` (e.g. `+FLAGS (\\Seen)`).
    async fn uid_store(&mut self, uid: u32, query: &str) -> Result<(), String> {
        let uid_str = uid.to_string();
        match self {
            ImapSession::Plain(session) => {
                let stream = session
                    .uid_store(&uid_str, query)
                    .await
                    .map_err(|e| e.to_string())?;
                stream
                    .try_collect::<Vec<_>>()
                    .await
                    .map_err(|e| e.to_string())?;
            }
            ImapSession::Tls(session) => {
                let stream = session
                    .uid_store(&uid_str, query)
                    .await
                    .map_err(|e| e.to_string())?;
                stream
                    .try_collect::<Vec<_>>()
                    .await
                    .map_err(|e| e.to_string())?;
            }
        }
        Ok(())
    }

    async fn logout(&mut self) -> Result<(), String> {
        match self {
            ImapSession::Plain(session) => session.logout().await.map_err(|e| e.to_string()),
            ImapSession::Tls(session) => session.logout().await.map_err(|e| e.to_string()),
        }
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
    async fn fetch_new_messages(&mut self) -> Vec<HashMap<String, serde_json::Value>> {
        self.fetch_messages(vec!["UNSEEN"], self.config.mark_seen, true, 0)
            .await
    }

    async fn fetch_messages(
        &mut self,
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
        &mut self,
        search_criteria: Vec<&str>,
        mark_seen: bool,
        dedupe: bool,
        limit: usize,
        messages: &mut Vec<HashMap<String, serde_json::Value>>,
        cycle_uids: &mut HashSet<String>,
    ) -> Result<Vec<HashMap<String, serde_json::Value>>, String> {
        let mailbox = if self.config.imap_mailbox.trim().is_empty() {
            "INBOX".to_string()
        } else {
            self.config.imap_mailbox.clone()
        };

        let client = Self::connect_imap_client(&self.config).await?;
        let mut session = client.login(&self.config).await?;

        let result = self
            .fetch_messages_once_with_session(
                &mut session,
                &mailbox,
                search_criteria,
                mark_seen,
                dedupe,
                limit,
                messages,
                cycle_uids,
            )
            .await;

        if let Err(e) = session.logout().await {
            log::warn!("Email channel: IMAP logout failed: {e}");
        }

        result.map(|_| messages.clone())
    }

    async fn fetch_messages_once_with_session(
        &mut self,
        session: &mut ImapSession,
        mailbox: &str,
        search_criteria: Vec<&str>,
        mark_seen: bool,
        dedupe: bool,
        limit: usize,
        messages: &mut Vec<HashMap<String, serde_json::Value>>,
        cycle_uids: &mut HashSet<String>,
    ) -> Result<(), String> {
        if let Err(e) = session.select_mailbox(mailbox).await {
            log::warn!("Email channel: Failed to select mailbox: {e}");
            return Ok(());
        }

        let mut ids = match session.search(&search_criteria).await {
            Ok(ids) => ids,
            Err(e) => {
                log::warn!("Email channel: Failed to search: {e}");
                return Ok(());
            }
        };
        if limit > 0 && ids.len() > limit {
            let mut sorted: Vec<u32> = ids.into_iter().collect();
            sorted.sort_unstable();
            let start = sorted.len() - limit;
            ids = sorted.into_iter().skip(start).collect();
        }

        let allowed_attachment_types: Vec<&str> = self
            .config
            .allowed_attachment_types
            .iter()
            .map(String::as_str)
            .collect();

        for imap_id in ids {
            let message = match session.fetch(imap_id, "(BODY.PEEK[] UID)").await {
                Ok(message) => message,
                Err(e) => {
                    log::warn!("Email channel: Failed to fetch seq={imap_id}: {e}");
                    continue;
                }
            };
            let raw_bytes = message.bytes;
            if raw_bytes.is_empty() {
                continue;
            }
            let uid = message.uid;
            let uid_key = uid.to_string();
            if cycle_uids.contains(&uid_key) {
                continue;
            }
            if dedupe && self.processed_uids.contains(&uid_key) {
                continue;
            }

            let parsed = match parse_mail(&raw_bytes) {
                Ok(mail) => mail,
                Err(e) => {
                    log::warn!("Email channel: Failed to parse message uid={uid_key}: {e}");
                    continue;
                }
            };
            let sender = parsed
                .headers
                .get_first_header("From")
                .and_then(|h| addrparse_header(h).ok())
                .and_then(|addrs| addrs.extract_single_info())
                .map(|info| info.addr)
                .unwrap_or_default();
            if sender.is_empty() {
                continue;
            }
            let (spf_pass, dkim_pass) = Self::check_authentication_results(&parsed);
            if self.config.verify_spf && !spf_pass {
                log::warn!(
                    "Email from {sender} rejected: SPF verification failed. (no 'spf=pass' in Authentication-Results header)",
                );
                continue;
            }
            if self.config.verify_dkim && !dkim_pass {
                log::warn!(
                    "Email from {sender} rejected: DKIM verification failed (no 'dkim=pass' in Authentication-Results header)",
                );
                continue;
            }
            let subject = parsed
                .headers
                .get_first_value("Subject")
                .unwrap_or_default();
            let date = parsed
                .headers
                .get_first_value("Date")
                .and_then(|v| DateTime::parse_from_rfc2822(&v).ok())
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or_else(Utc::now);
            let message_id = parsed
                .headers
                .get_first_value("Message-ID")
                .unwrap_or_default()
                .trim()
                .to_string();
            let mut body = Self::extract_text_body(&parsed);

            if body.is_empty() {
                body = "(empty email body)".to_string();
            }
            let max = self.config.max_body_chars as usize;
            if body.chars().count() > max {
                body = body.chars().take(max).collect();
            }
            let mut content = format!(
                "[EMAIL-CONTEXT] Email received.\nFrom: {sender}\nSubject: {subject}\nDate: {date}\nMessage-ID: {message_id}\nBody: {body}"
            );

            let mut attachment_paths = Vec::new();
            if !allowed_attachment_types.is_empty() {
                let media_dir = get_media_dir(Some("email"));
                let saved = Self::extract_attachments(
                    &parsed,
                    uid,
                    &allowed_attachment_types,
                    self.config.max_attachments_per_email,
                    self.config.max_attachment_size,
                    &media_dir,
                );
                for path in saved {
                    let path_str = path.to_string_lossy().to_string();
                    content.push_str(&format!("\nattachment: {path_str}"));
                    attachment_paths.push(path_str);
                }
            }

            let mut metadata: HashMap<String, serde_json::Value> = HashMap::new();
            metadata.insert("message_id".to_string(), message_id.clone().into());
            metadata.insert("subject".to_string(), subject.clone().into());
            metadata.insert("date".to_string(), date.to_string().into());
            metadata.insert("sender_email".to_string(), sender.clone().into());
            metadata.insert("uid".to_string(), uid.into());

            messages.push(HashMap::from([
                ("sender".to_string(), sender.into()),
                ("content".to_string(), content.into()),
                ("subject".to_string(), subject.into()),
                ("date".to_string(), date.to_string().into()),
                ("message_id".to_string(), message_id.into()),
                ("uid".to_string(), uid.into()),
                (
                    "media".to_string(),
                    serde_json::to_value(&attachment_paths).unwrap_or_default(),
                ),
                (
                    "metadata".to_string(),
                    serde_json::to_value(metadata).unwrap_or_default(),
                ),
            ]));

            cycle_uids.insert(uid_key.clone());
            if dedupe {
                self.processed_uids.insert(uid_key);
                // mark_seen is the primary dedup; this set is a safety net
                if self.processed_uids.len() >= Self::MAX_PROCESSED_UIDS {
                    // Evict a random half to cap memory; mark_seen is the primary dedup
                    let to_remove = self.processed_uids.len() / 2;
                    let mut keys: Vec<String> =
                        self.processed_uids.iter().cloned().collect();
                    keys.shuffle(&mut rand::rng());
                    for key in keys.iter().take(to_remove) {
                        self.processed_uids.remove(key);
                    }
                }
            }
            if mark_seen {
                if let Err(e) = session.uid_store(uid, "+FLAGS (\\Seen)").await {
                    log::warn!("Email channel: Failed to mark message uid={uid} as seen: {e}");
                }
            }
        }

        Ok(())
    }

    async fn connect_imap_client(config: &EmailConfig) -> Result<ImapClient, String> {
        let addr = (config.imap_host.as_str(), config.imap_port);
        let tls = TlsConnector::from(NativeTlsConnector::new().map_err(|e| e.to_string())?);

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

    /// Inspect `Authentication-Results` headers for SPF/DKIM pass (or neutral).
    ///
    /// Returns `(spf_pass, dkim_pass)`. Results are OR'd across multiple headers.
    fn check_authentication_results(parsed: &ParsedMail<'_>) -> (bool, bool) {
        let mut spf_pass = false;
        let mut dkim_pass = false;
        for header in &parsed.headers {
            if header
                .get_key()
                .eq_ignore_ascii_case("authentication-results")
            {
                let ar_lower = header.get_value().to_lowercase();
                if BSPF_MATCH.is_match(&ar_lower) {
                    spf_pass = true;
                }
                if BDKIM_MATCH.is_match(&ar_lower) {
                    dkim_pass = true;
                }
            }
        }
        (spf_pass, dkim_pass)
    }

    /// Best-effort extraction of readable body text.
    fn extract_text_body(parsed: &ParsedMail<'_>) -> String {
        if Self::is_multipart(parsed) {
            let mut plain_parts: Vec<String> = Vec::new();
            let mut html_parts: Vec<String> = Vec::new();

            for part in parsed.parts() {
                if part.get_content_disposition().disposition == DispositionType::Attachment {
                    continue;
                }

                let content_type = part.ctype.mimetype.as_str();
                let Ok(payload) = part.get_body() else {
                    continue;
                };

                match content_type {
                    "text/plain" => plain_parts.push(payload),
                    "text/html" => html_parts.push(payload),
                    _ => {}
                }
            }

            if !plain_parts.is_empty() {
                return plain_parts.join("\n\n").trim().to_string();
            }
            if !html_parts.is_empty() {
                return Self::html_to_text(&html_parts.join("\n\n"))
                    .trim()
                    .to_string();
            }
            return String::new();
        }

        let Ok(payload) = parsed.get_body() else {
            return String::new();
        };

        if parsed.ctype.mimetype == "text/html" {
            Self::html_to_text(&payload).trim().to_string()
        } else {
            payload.trim().to_string()
        }
    }

    fn html_to_text(raw_html: &str) -> String {
        html2md::parse_html(raw_html)
    }

    /// Extract and save email attachments to `media_dir`.
    /// Returns list of saved file paths.
    fn extract_attachments(
        parsed: &ParsedMail<'_>,
        uid: u32,
        allowed_types: &[&str],
        max_count: u32,
        max_size: u32,
        media_dir: &PathBuf
    ) -> Vec<PathBuf> {
        if !Self::is_multipart(parsed) {
            return vec![];
        }
        if allowed_types.is_empty() || max_count == 0 {
            return vec![];
        }

        if let Err(e) = std::fs::create_dir_all(media_dir) {
            log::warn!("Email attachments skipped (media dir create failed): {e}");
            return vec![];
        }

        let mut saved = Vec::new();
        for part in parsed.parts() {
            if saved.len() >= max_count as usize {
                break;
            }
            // `parts()` yields the multipart root first; skip non-attachments.
            if part.get_content_disposition().disposition != DispositionType::Attachment {
                continue;
            }

            let content_type = part.ctype.mimetype.as_str();
            if !Self::is_allowed_content_type(content_type, allowed_types) {
                log::debug!(
                    "Email attachment skipped (type {content_type}): not in allowed list"
                );
                continue;
            }

            let Ok(payload) = part.get_body_raw() else {
                log::warn!("Email attachment skipped (no body): type={content_type}");
                continue;
            };
            if payload.is_empty() {
                continue;
            }
            if payload.len() > max_size as usize {
                log::warn!(
                    "Email attachment skipped: size {} exceeds limit {}",
                    payload.len(),
                    max_size,
                );
                continue;
            }

            let disposition = part.get_content_disposition();
            let raw_name = disposition
                .params
                .get("filename")
                .or_else(|| part.ctype.params.get("name"))
                .map(|s| s.as_str())
                .unwrap_or("attachment");
            let mut sanitized = safe_filename(raw_name);
            if sanitized.is_empty() {
                sanitized = "attachment".to_string();
            }
            // Prefix with UID to avoid collisions across messages / same filenames.
            let dest = media_dir.join(format!("{uid}_{sanitized}"));

            if let Err(e) = std::fs::write(&dest, &payload) {
                log::warn!("Email attachment skipped (write error): {e}");
                continue;
            }
            saved.push(dest);
        }
        saved
    }

    fn is_multipart(parsed: &ParsedMail<'_>) -> bool {
        parsed.ctype.mimetype.starts_with("multipart/")
    }

    fn is_allowed_content_type(content_type: &str, allowed_types: &[&str]) -> bool {
        allowed_types.iter().any(|pat| {
            Pattern::new(pat)
                .map(|p| p.matches(content_type))
                .unwrap_or(false)
        })
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

    fn check_auth(auth_results: &[&str]) -> (bool, bool) {
        let mut raw = String::from("From: alice@example.com\r\nSubject: test\r\n");
        for ar in auth_results {
            raw.push_str("Authentication-Results: ");
            raw.push_str(ar);
            raw.push_str("\r\n");
        }
        raw.push_str("\r\nbody\r\n");
        let parsed = parse_mail(raw.as_bytes()).expect("test mail should parse");
        EmailChannel::check_authentication_results(&parsed)
    }

    fn extract_body(raw: &str) -> String {
        let parsed = parse_mail(raw.as_bytes()).expect("test mail should parse");
        EmailChannel::extract_text_body(&parsed)
    }

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

    #[test]
    fn auth_results_both_pass() {
        assert_eq!(
            check_auth(&[
                "mx.google.com; spf=pass smtp.mailfrom=example.com; dkim=pass header.d=example.com",
            ]),
            (true, true)
        );
    }

    #[test]
    fn auth_results_only_spf() {
        assert_eq!(
            check_auth(&["mx.example.com; spf=pass smtp.mailfrom=example.com"]),
            (true, false)
        );
    }

    #[test]
    fn auth_results_only_dkim() {
        assert_eq!(
            check_auth(&["mx.example.com; dkim=pass header.d=example.com"]),
            (false, true)
        );
    }

    #[test]
    fn auth_results_neutral_counts_as_pass() {
        assert_eq!(
            check_auth(&[
                "mx.example.com; spf=neutral smtp.mailfrom=example.com; dkim=neutral header.d=example.com",
            ]),
            (true, true)
        );
    }

    #[test]
    fn auth_results_fail_and_softfail_do_not_pass() {
        assert_eq!(
            check_auth(&[
                "mx.example.com; spf=fail smtp.mailfrom=example.com; dkim=softfail header.d=example.com",
            ]),
            (false, false)
        );
    }

    #[test]
    fn auth_results_case_insensitive() {
        assert_eq!(
            check_auth(&[
                "mx.example.com; SPF=Pass smtp.mailfrom=example.com; DKIM=PASS header.d=example.com",
            ]),
            (true, true)
        );
    }

    #[test]
    fn auth_results_or_across_multiple_headers() {
        assert_eq!(
            check_auth(&[
                "mx1.example.com; spf=pass smtp.mailfrom=example.com",
                "mx2.example.com; dkim=pass header.d=example.com",
            ]),
            (true, true)
        );
    }

    #[test]
    fn auth_results_missing_header() {
        assert_eq!(check_auth(&[]), (false, false));
    }

    #[test]
    fn auth_results_allows_spaces_around_equals() {
        assert_eq!(
            check_auth(&[
                "mx.example.com; spf = pass smtp.mailfrom=example.com; dkim = pass header.d=example.com",
            ]),
            (true, true)
        );
    }

    #[test]
    fn extract_text_body_plain() {
        let body = extract_body(
            "From: alice@example.com\r\n\
             Content-Type: text/plain; charset=utf-8\r\n\
             \r\n\
             Hello world\r\n",
        );
        assert_eq!(body, "Hello world");
    }

    #[test]
    fn extract_text_body_html_only() {
        let body = extract_body(
            "From: alice@example.com\r\n\
             Content-Type: text/html; charset=utf-8\r\n\
             \r\n\
             <p>Hello <b>world</b></p>\r\n",
        );
        assert!(body.to_lowercase().contains("hello"));
        assert!(body.to_lowercase().contains("world"));
        assert!(!body.contains("<p>"));
    }

    #[test]
    fn extract_text_body_multipart_prefers_plain() {
        let body = extract_body(
            "From: alice@example.com\r\n\
             Content-Type: multipart/alternative; boundary=bound\r\n\
             \r\n\
             --bound\r\n\
             Content-Type: text/plain; charset=utf-8\r\n\
             \r\n\
             Plain body\r\n\
             --bound\r\n\
             Content-Type: text/html; charset=utf-8\r\n\
             \r\n\
             <p>HTML body</p>\r\n\
             --bound--\r\n",
        );
        assert_eq!(body, "Plain body");
    }

    #[test]
    fn extract_text_body_multipart_html_fallback() {
        let body = extract_body(
            "From: alice@example.com\r\n\
             Content-Type: multipart/alternative; boundary=bound\r\n\
             \r\n\
             --bound\r\n\
             Content-Type: text/html; charset=utf-8\r\n\
             \r\n\
             <p>Only HTML</p>\r\n\
             --bound--\r\n",
        );
        assert!(body.to_lowercase().contains("only html"));
        assert!(!body.contains("<p>"));
    }

    #[test]
    fn extract_text_body_skips_attachments() {
        let body = extract_body(
            "From: alice@example.com\r\n\
             Content-Type: multipart/mixed; boundary=bound\r\n\
             \r\n\
             --bound\r\n\
             Content-Type: text/plain; charset=utf-8\r\n\
             \r\n\
             Visible body\r\n\
             --bound\r\n\
             Content-Type: application/pdf\r\n\
             Content-Disposition: attachment; filename=\"doc.pdf\"\r\n\
             \r\n\
             %PDF-fake-bytes\r\n\
             --bound--\r\n",
        );
        assert_eq!(body, "Visible body");
        assert!(!body.contains("%PDF"));
    }

    #[test]
    fn extract_text_body_empty_when_only_attachment() {
        let body = extract_body(
            "From: alice@example.com\r\n\
             Content-Type: multipart/mixed; boundary=bound\r\n\
             \r\n\
             --bound\r\n\
             Content-Type: application/pdf\r\n\
             Content-Disposition: attachment; filename=\"doc.pdf\"\r\n\
             \r\n\
             %PDF-fake-bytes\r\n\
             --bound--\r\n",
        );
        assert_eq!(body, "");
    }

    fn extract_atts(
        raw: &str,
        uid: u32,
        allowed: &[&str],
        max_count: u32,
        max_size: u32,
        media_dir: &std::path::Path,
    ) -> Vec<PathBuf> {
        let parsed = parse_mail(raw.as_bytes()).expect("test mail should parse");
        EmailChannel::extract_attachments(&parsed, uid, allowed, max_count, max_size, &media_dir.to_path_buf())
    }

    #[test]
    fn extract_attachments_saves_allowed_pdf() {
        let dir = tempfile::tempdir().unwrap();
        let raw = "From: alice@example.com\r\n\
             Content-Type: multipart/mixed; boundary=bound\r\n\
             \r\n\
             --bound\r\n\
             Content-Type: text/plain; charset=utf-8\r\n\
             \r\n\
             Hello\r\n\
             --bound\r\n\
             Content-Type: application/pdf\r\n\
             Content-Disposition: attachment; filename=\"report.pdf\"\r\n\
             \r\n\
             %PDF-1.4 fake\r\n\
             --bound--\r\n";

        let saved = extract_atts(raw, 42, &["application/pdf"], 5, 10_000, dir.path());
        assert_eq!(saved.len(), 1);
        assert_eq!(
            saved[0].file_name().and_then(|n| n.to_str()),
            Some("42_report.pdf")
        );
        assert_eq!(std::fs::read(&saved[0]).unwrap(), b"%PDF-1.4 fake");
    }

    #[test]
    fn extract_attachments_skips_disallowed_type() {
        let dir = tempfile::tempdir().unwrap();
        let raw = "From: alice@example.com\r\n\
             Content-Type: multipart/mixed; boundary=bound\r\n\
             \r\n\
             --bound\r\n\
             Content-Type: application/pdf\r\n\
             Content-Disposition: attachment; filename=\"report.pdf\"\r\n\
             \r\n\
             %PDF-fake\r\n\
             --bound--\r\n";

        let saved = extract_atts(raw, 1, &["image/*"], 5, 10_000, dir.path());
        assert!(saved.is_empty());
    }

    #[test]
    fn extract_attachments_respects_max_size() {
        let dir = tempfile::tempdir().unwrap();
        let raw = "From: alice@example.com\r\n\
             Content-Type: multipart/mixed; boundary=bound\r\n\
             \r\n\
             --bound\r\n\
             Content-Type: application/pdf\r\n\
             Content-Disposition: attachment; filename=\"big.pdf\"\r\n\
             \r\n\
             0123456789\r\n\
             --bound--\r\n";

        let saved = extract_atts(raw, 1, &["application/pdf"], 5, 5, dir.path());
        assert!(saved.is_empty());
    }

    #[test]
    fn extract_attachments_respects_max_count() {
        let dir = tempfile::tempdir().unwrap();
        let raw = "From: alice@example.com\r\n\
             Content-Type: multipart/mixed; boundary=bound\r\n\
             \r\n\
             --bound\r\n\
             Content-Type: application/pdf\r\n\
             Content-Disposition: attachment; filename=\"a.pdf\"\r\n\
             \r\n\
             AAA\r\n\
             --bound\r\n\
             Content-Type: application/pdf\r\n\
             Content-Disposition: attachment; filename=\"b.pdf\"\r\n\
             \r\n\
             BBB\r\n\
             --bound--\r\n";

        let saved = extract_atts(raw, 7, &["application/pdf"], 1, 10_000, dir.path());
        assert_eq!(saved.len(), 1);
        assert!(saved[0].ends_with("7_a.pdf"));
    }

    #[test]
    fn extract_attachments_empty_allowed_types_saves_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let raw = "From: alice@example.com\r\n\
             Content-Type: multipart/mixed; boundary=bound\r\n\
             \r\n\
             --bound\r\n\
             Content-Type: application/pdf\r\n\
             Content-Disposition: attachment; filename=\"a.pdf\"\r\n\
             \r\n\
             AAA\r\n\
             --bound--\r\n";

        let saved = extract_atts(raw, 1, &[], 5, 10_000, dir.path());
        assert!(saved.is_empty());
    }

    #[test]
    fn extract_attachments_non_multipart_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let raw = "From: alice@example.com\r\n\
             Content-Type: text/plain; charset=utf-8\r\n\
             \r\n\
             just text\r\n";

        let saved = extract_atts(raw, 1, &["*"], 5, 10_000, dir.path());
        assert!(saved.is_empty());
    }

    #[test]
    fn extract_attachments_uses_content_type_name_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let raw = "From: alice@example.com\r\n\
             Content-Type: multipart/mixed; boundary=bound\r\n\
             \r\n\
             --bound\r\n\
             Content-Type: application/pdf; name=\"via-ctype.pdf\"\r\n\
             Content-Disposition: attachment\r\n\
             \r\n\
             PDFDATA\r\n\
             --bound--\r\n";

        let saved = extract_atts(raw, 9, &["application/pdf"], 5, 10_000, dir.path());
        assert_eq!(saved.len(), 1);
        assert!(saved[0].ends_with("9_via-ctype.pdf"));
    }

    #[test]
    fn is_allowed_content_type_glob_star() {
        assert!(EmailChannel::is_allowed_content_type(
            "application/pdf",
            &["application/pdf"]
        ));
        assert!(EmailChannel::is_allowed_content_type(
            "image/png",
            &["image/*"]
        ));
        assert!(EmailChannel::is_allowed_content_type("text/plain", &["*"]));
        assert!(!EmailChannel::is_allowed_content_type(
            "application/pdf",
            &["image/*"]
        ));
    }
}
