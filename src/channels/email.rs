use async_trait::async_trait;
use chrono::{DateTime, Utc};
use glob::Pattern;
use rand::seq::SliceRandom;
use regex::Regex;
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, LazyLock, Mutex,
    },
};

use futures::TryStreamExt;

use crate::{
    bus::{events::OutboundMessage, queue::MessageBus},
    channels::{
        base::{BaseChannel, BaseChannelCommon},
        types::MessageBytes,
    },
    config::{
        channels::EmailConfig, paths::get_media_dir, schema::ChannelsConfig,
    },
    utils::helpers::safe_filename,
};

use async_imap::{Client, Session};
use lettre::{
    message::{Mailbox, MessageBuilder},
    transport::smtp::authentication::Credentials,
    AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor,
};
use mailparse::{DispositionType, MailHeaderMap, ParsedMail, addrparse_header, parse_mail};
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

/// Email channel.

/// Inbound:
///- Poll IMAP mailbox for unread messages.
///- Convert each message into an inbound event.

/// Outbound:
///- Send responses via SMTP back to the sender address.
///- Send responses via SMTP back to the sender address.
pub struct EmailChannel {
    base: BaseChannelCommon,
    channels_config: ChannelsConfig,
    config: EmailConfig,
    last_subject_by_chat_id: Mutex<HashMap<String, String>>,
    last_message_id_by_chat: Mutex<HashMap<String, String>>,
    processed_uids: Mutex<HashSet<String>>, // Capped to prevent unbounded growth
    running: AtomicBool,
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

    pub fn new(config: EmailConfig, bus: Arc<MessageBus>, channels_config: ChannelsConfig) -> Self {
        Self {
            base: BaseChannelCommon {
                bus,
                running: false,
                transcription_api_key: String::new(),
            },
            channels_config,
            config: config,
            last_subject_by_chat_id: Mutex::new(HashMap::new()),
            last_message_id_by_chat: Mutex::new(HashMap::new()),
            processed_uids: Mutex::new(HashSet::new()),
            running: AtomicBool::new(false),
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
                        log::error!("Email channel: user: {:?}", self.config.imap_username);
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
        &self,
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
            if dedupe && self.processed_uids.lock().unwrap().contains(&uid_key) {
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
            if sender.trim().is_empty() {
                continue;
            }
            let allow_from = self.config.allow_from.clone();
            if !allow_from.contains(&sender.to_string()) && !allow_from.contains(&"*".to_string()) {
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
                let mut processed = self.processed_uids.lock().unwrap();
                processed.insert(uid_key);
                // mark_seen is the primary dedup; this set is a safety net
                if processed.len() >= Self::MAX_PROCESSED_UIDS {
                    // Evict a random half to cap memory; mark_seen is the primary dedup
                    let to_remove = processed.len() / 2;
                    let mut keys: Vec<String> = processed.iter().cloned().collect();
                    keys.shuffle(&mut rand::rng());
                    for key in keys.iter().take(to_remove) {
                        processed.remove(key);
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
        media_dir: &PathBuf,
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
                log::debug!("Email attachment skipped (type {content_type}): not in allowed list");
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

    fn reply_subject(&self, base_subject: &str) -> String {
        let subject = {
            let trimmed = base_subject.trim();
            if trimmed.is_empty() {
                "rust-bot reply".to_string()
            } else {
                trimmed.to_string()
            }
        };
        // Already a reply thread — don't stack another Re:
        if subject.to_lowercase().starts_with("re:") {
            return subject;
        }
        let prefix = {
            let trimmed = self.config.subject_prefix.trim();
            if trimmed.is_empty() {
                "Re:"
            } else {
                trimmed
            }
        };
        format!("{prefix} {subject}")
    }

    /// Build an SMTP transport from config (implicit TLS, STARTTLS, or plain).
    fn build_smtp_transport(
        &self,
    ) -> Result<AsyncSmtpTransport<Tokio1Executor>, lettre::transport::smtp::Error> {
        let creds = Credentials::new(
            self.config.smtp_username.clone(),
            self.config.smtp_password.clone(),
        );
        let host = self.config.smtp_host.as_str();
        let port = self.config.smtp_port;

        let builder = if self.config.smtp_use_ssl {
            // Implicit TLS (SMTPS), typically port 465
            AsyncSmtpTransport::<Tokio1Executor>::relay(host)?.port(port)
        } else if self.config.smtp_use_tls {
            // STARTTLS upgrade, typically port 587
            AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(host)?.port(port)
        } else {
            AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(host).port(port)
        };

        Ok(builder.credentials(creds).build())
    }
}

#[async_trait]
impl BaseChannel for EmailChannel {
    fn name(&self) -> &'static str {
        "email"
    }

    fn display_name(&self) -> &'static str {
        "Email"
    }

    fn running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    fn bus(&self) -> &MessageBus {
        self.base.bus.as_ref()
    }

    fn config(&self) -> &ChannelsConfig {
        &self.channels_config
    }

    fn transcription_api_key(&self) -> &str {
        &self.base.transcription_api_key
    }

    fn set_transcription_api_key(&mut self, key: String) {
        self.base.transcription_api_key = key;
    }

    fn default_config(&self) -> HashMap<String, serde_json::Value> {
        EmailConfig::default().to_config_map()
    }

    async fn stop(&self) {
        self.running.store(false, Ordering::Relaxed);
    }

    /// Send email via SMTP.
    async fn send(&self, msg: OutboundMessage) -> Result<(), String> {
        if !self.config.consent_granted {
            log::warn!("Skip email send: consent_granted is false.");
            return Ok(());
        }

        if self.config.smtp_host.trim().is_empty() {
            log::warn!("Email channel SMTP host not configured.");
            return Err("Email channel SMTP host not configured".to_string());
        }

        let to_addr = msg.chat_id.to_string();
        if to_addr.trim().is_empty() {
            log::warn!("Email channel missing recipient address.");
            return Err("Email channel missing recipient address".to_string());
        }

        // Determine if this is a reply (recipient has sent us an email before)
        let is_reply = self
            .last_message_id_by_chat
            .lock()
            .unwrap()
            .contains_key(&to_addr);
        let force_send = msg
            .metadata
            .get("force_send")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        // autoReplyEnabled only controls automatic replies, not proactive sends
        if is_reply && !self.config.auto_reply_enabled && !force_send {
            log::info!(
                "Skip automatic email reply to {}: auto_reply_enabled is false",
                to_addr
            );
            return Ok(());
        }

        let base_subject = self
            .last_subject_by_chat_id
            .lock()
            .unwrap()
            .get(&to_addr)
            .cloned()
            .unwrap_or_else(|| "rust-bot reply".to_string());
        let mut subject = self.reply_subject(&base_subject);

        if !msg.metadata.is_empty() {
            let subject_option = msg.metadata.get("subject");
            if subject_option
                .and_then(|v| Some(v.is_string()))
                .unwrap_or(false)
            {
                let override_subject = subject_option.and_then(|v| v.as_str()).unwrap_or("");
                if !override_subject.is_empty() {
                    subject = override_subject.to_string();
                };
            }
        }

        let from_addr = {
            let candidates = [
                self.config.from_address.as_str(),
                self.config.smtp_username.as_str(),
                self.config.imap_username.as_str(),
            ];
            candidates
                .into_iter()
                .find(|s| !s.trim().is_empty())
                .unwrap_or("")
                .to_string()
        };
        if from_addr.is_empty() {
            log::warn!("Email channel missing From address.");
            return Err("Email channel missing From address".to_string());
        }

        let from_mailbox: Mailbox = match from_addr.parse() {
            Ok(m) => m,
            Err(e) => {
                let err = format!("Email channel invalid From address {from_addr}: {e}");
                log::warn!("{err}");
                return Err(err);
            }
        };
        let to_mailbox: Mailbox = match to_addr.parse() {
            Ok(m) => m,
            Err(e) => {
                let err = format!("Email channel invalid To address {to_addr}: {e}");
                log::warn!("{err}");
                return Err(err);
            }
        };

        let mut builder: MessageBuilder = Message::builder()
            .from(from_mailbox)
            .to(to_mailbox)
            .subject(subject);

        if let Some(in_reply_to) = self
            .last_message_id_by_chat
            .lock()
            .unwrap()
            .get(&to_addr)
            .cloned()
        {
            if !in_reply_to.trim().is_empty() {
                builder = builder
                    .in_reply_to(in_reply_to.clone())
                    .references(in_reply_to);
            }
        }

        let email_msg = match builder.body(msg.content.clone()) {
            Ok(m) => m,
            Err(e) => {
                let err = format!("Failed to build email message: {e}");
                log::error!("{err}");
                return Err(err);
            }
        };

        let mailer = match self.build_smtp_transport() {
            Ok(m) => m,
            Err(e) => {
                let err = format!("Failed to create SMTP transport: {e}");
                log::error!("{err}");
                return Err(err);
            }
        };

        match mailer.send(email_msg).await {
            Ok(_) => {
                log::info!("Email sent to {to_addr}");
                Ok(())
            }
            Err(e) => {
                let err = format!("Failed to send email to {to_addr}: {e}");
                log::error!("{err}");
                Err(err)
            }
        }
    }

    /// Start polling IMAP for inbound emails.
    async fn start(&self) {
        if !self.config.consent_granted {
            log::error!(
                "Email channel disabled: consent_granted is false. Set channels.email.consentGranted=true after explicit user permission."
            );
            return;
        }
        if !self.validate_config() {
            return;
        }
        self.running.store(true, Ordering::Relaxed);

        if !self.config.verify_dkim && !self.config.verify_spf {
            log::warn!(
                "Email channel: DKIM and SPF verification are both DISABLED.
Emails with spoofed From headers will be accepted.
Set verify_dkim=true and verify_spf=true for anti-spoofing protection."
            )
        }
        log::info!("Starting Email channel (IMAP polling mode)...");
        let poll_seconds = std::cmp::max(5, self.config.poll_interval_seconds) as u64;
        while self.running.load(Ordering::Relaxed) {
            let inbound_items = self.fetch_new_messages().await;
            for item in inbound_items {
                let sender = item.get("sender").and_then(|v| v.as_str()).unwrap_or("");
                if sender.trim().is_empty() {
                    continue;
                }
                let subject = item.get("subject").and_then(|v| v.as_str()).unwrap_or("");
                let message_id = item
                    .get("message_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if !subject.trim().is_empty() {
                    self.last_subject_by_chat_id
                        .lock()
                        .unwrap()
                        .insert(sender.to_string(), subject.to_string());
                }
                if !message_id.trim().is_empty() {
                    self.last_message_id_by_chat
                        .lock()
                        .unwrap()
                        .insert(sender.to_string(), message_id.to_string());
                }
                let content = item.get("content").and_then(|v| v.as_str()).unwrap_or("");
                let media = item
                    .get("media")
                    .and_then(|v| serde_json::from_value::<Vec<String>>(v.clone()).ok());
                let metadata = item.get("metadata").and_then(|v| {
                    serde_json::from_value::<HashMap<String, serde_json::Value>>(v.clone()).ok()
                });
                self.handle_message(sender, sender, content, media, metadata, None)
                    .await;
            }
            tokio::time::sleep(std::time::Duration::from_secs(poll_seconds)).await;
        }
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
        EmailChannel::extract_attachments(
            &parsed,
            uid,
            allowed,
            max_count,
            max_size,
            &media_dir.to_path_buf(),
        )
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

    fn channel_with_prefix(prefix: &str) -> EmailChannel {
        EmailChannel::new(
            EmailConfig {
                subject_prefix: prefix.to_string(),
                ..EmailConfig::default()
            },
            Arc::new(MessageBus::new()),
            ChannelsConfig::default(),
        )
    }

    #[test]
    fn reply_subject_prefixes_plain_subject() {
        let channel = channel_with_prefix("Re:");
        assert_eq!(channel.reply_subject("Hello"), "Re: Hello");
    }

    #[test]
    fn reply_subject_default_prefix_has_single_space() {
        // Default config uses "Re: " (trailing space); join must not double-space.
        let channel = channel_with_prefix("Re: ");
        assert_eq!(channel.reply_subject("Hello"), "Re: Hello");
    }

    #[test]
    fn reply_subject_empty_prefix_falls_back_to_re() {
        let channel = channel_with_prefix("");
        assert_eq!(channel.reply_subject("Hello"), "Re: Hello");
        let channel = channel_with_prefix("   ");
        assert_eq!(channel.reply_subject("Hello"), "Re: Hello");
    }

    #[test]
    fn reply_subject_custom_prefix() {
        let channel = channel_with_prefix("[Bot]");
        assert_eq!(channel.reply_subject("Hello"), "[Bot] Hello");
    }

    #[test]
    fn reply_subject_skips_prefix_when_already_re() {
        let channel = channel_with_prefix("Re:");
        assert_eq!(channel.reply_subject("Re: Hello"), "Re: Hello");
        assert_eq!(channel.reply_subject("RE: Hello"), "RE: Hello");
        assert_eq!(channel.reply_subject("re:Hello"), "re:Hello");
    }

    #[test]
    fn reply_subject_empty_base_uses_default() {
        let channel = channel_with_prefix("Re:");
        assert_eq!(channel.reply_subject(""), "Re: rust-bot reply");
        assert_eq!(channel.reply_subject("   "), "Re: rust-bot reply");
    }

    #[test]
    fn reply_subject_trims_base_subject() {
        let channel = channel_with_prefix("Re:");
        assert_eq!(channel.reply_subject("  Hello  "), "Re: Hello");
    }
}
