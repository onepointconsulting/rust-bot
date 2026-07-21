use async_trait::async_trait;
use qrcode::{QrCode, render::unicode, types::QrError};
use whatsapp_rust::{
    Client, Jid, Server, TokioRuntime,
    bot::{Bot, BotHandle},
    proto_helpers::MessageExt,
    store::SqliteStore,
    types::events::Event,
    waproto::whatsapp as wa,
};
use whatsapp_rust_tokio_transport::TokioWebSocketTransportFactory;
use whatsapp_rust_ureq_http_client::UreqHttpClient;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::PathBuf,
    str::FromStr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use serde::{Deserialize, Serialize};
use crate::{
    bus::{events::OutboundMessage, queue::MessageBus},
    channels::base::{BaseChannel, BaseChannelCommon},
    config::schema::ChannelsConfig,
    utils::helpers::expand_tilde_path,
};

/// Inbound WhatsApp message forwarded from the bot event handler to `start()`.
struct IncomingWaMessage {
    sender_id: String,
    /// Bare user part of the sender JID (phone number or LID, no device/server).
    sender_user: String,
    /// Bare user part of the alternate sender JID (e.g. the phone number when
    /// the primary sender is a LID), if the server provided one.
    sender_alt_user: Option<String>,
    chat_id: String,
    content: String,
    media: Option<Vec<String>>,
    metadata: Option<HashMap<String, serde_json::Value>>,
}

/// How long to wait for QR scan / reconnect before giving up on login.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);

/// Prefix prepended to bot replies in self-chat so echoes can be ignored
/// even if the outbound message id is missed.
const DEFAULT_REPLY_PREFIX: &str = "⚕ *Rust Bot*\n────────────\n";

/// Max outbound message ids retained for echo suppression.
const RECENTLY_SENT_CAP: usize = 512;

/// Ring of recently sent WhatsApp message ids (Hermes-style echo filter).
struct RecentlySentIds {
    order: VecDeque<String>,
    set: HashSet<String>,
}

impl RecentlySentIds {
    fn new() -> Self {
        Self {
            order: VecDeque::new(),
            set: HashSet::new(),
        }
    }

    fn remember(&mut self, id: impl Into<String>) {
        let id = id.into();
        if id.is_empty() || !self.set.insert(id.clone()) {
            return;
        }
        self.order.push_back(id);
        while self.order.len() > RECENTLY_SENT_CAP {
            if let Some(old) = self.order.pop_front() {
                self.set.remove(&old);
            }
        }
    }

    fn contains(&self, id: &str) -> bool {
        self.set.contains(id)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WhatsAppGroupPolicy {
    Open,
    Mention,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct WhatsAppConfig {
    pub enabled: bool,
    pub session_db_path: String,
    pub allow_from: Vec<String>,
    pub media_download_dir: String,
    pub group_policy: WhatsAppGroupPolicy,
    /// Prepended to outbound replies; inbound messages starting with this
    /// prefix are treated as agent echoes and ignored (self-chat loop guard).
    pub reply_prefix: String,
}

impl Default for WhatsAppConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            session_db_path: "~/rust-bot/data/whatsapp/whatsapp.db".to_string(),
            allow_from: vec![],
            media_download_dir: "~/rust-bot/data/whatsapp_media".to_string(),
            group_policy: WhatsAppGroupPolicy::Open,
            reply_prefix: DEFAULT_REPLY_PREFIX.to_string(),
        }
    }
}

pub struct WhatsAppChannel {
    base: BaseChannelCommon,
    channels_config: ChannelsConfig,
    config: WhatsAppConfig,
    /// Kept alive while the channel is running; dropping/aborting stops the bot.
    bot_handle: Mutex<Option<BotHandle>>,
    client: Mutex<Option<Arc<Client>>>,
    /// Bare user parts of our PN and LID (filled after connect).
    own_jid_users: Arc<Mutex<Vec<String>>>,
    /// Preferred self-chat destination for outbound replies, filled after connect.
    /// Prefer LID (`{lid}@lid`) when available: on LID-migrated accounts the
    /// library encrypts participants under LID, and a PN outer `to` produces a
    /// mixed-namespace stanza that WhatsApp silently rejects with ack 400
    /// (oxidezap/whatsapp-rust#730). Fall back to `{phone}@s.whatsapp.net`.
    own_reply_chat_id: Arc<Mutex<Option<String>>>,
    /// Outbound message ids we just sent — ignore when they echo back as from_me.
    recently_sent_ids: Arc<Mutex<RecentlySentIds>>,
}

impl WhatsAppChannel {
    pub fn new(
        config: WhatsAppConfig,
        bus: Arc<MessageBus>,
        channels_config: ChannelsConfig,
    ) -> Self {
        Self {
            base: BaseChannelCommon {
                bus,
                running: AtomicBool::new(false),
                transcription_api_key: String::new(),
            },
            channels_config,
            config,
            bot_handle: Mutex::new(None),
            client: Mutex::new(None),
            own_jid_users: Arc::new(Mutex::new(Vec::new())),
            own_reply_chat_id: Arc::new(Mutex::new(None)),
            recently_sent_ids: Arc::new(Mutex::new(RecentlySentIds::new())),
        }
    }

    fn session_db_path(&self) -> PathBuf {
        PathBuf::from(expand_tilde_path(&self.config.session_db_path).as_ref())
    }

    fn media_download_dir(&self) -> PathBuf {
        PathBuf::from(expand_tilde_path(&self.config.media_download_dir).as_ref())
    }

    fn clear_session_store(&self) -> Result<(), String> {
        let path = self.session_db_path();
        for suffix in ["", "-wal", "-shm"] {
            let candidate = if suffix.is_empty() {
                path.clone()
            } else {
                PathBuf::from(format!("{}{suffix}", path.display()))
            };
            if candidate.exists() {
                std::fs::remove_file(&candidate).map_err(|e| {
                    format!(
                        "Failed to remove WhatsApp session file '{}': {e}",
                        candidate.display()
                    )
                })?;
                log::info!("Removed WhatsApp session file: {}", candidate.display());
            }
        }
        Ok(())
    }

    fn convert_qr_code_to_image(qr_code: &str) -> Result<(), QrError> {
        let code = QrCode::new(qr_code.as_bytes())?;
        let ascii = code.render::<unicode::Dense1x2>().build();
        println!("{ascii}");
        Ok(())
    }

    /// Reduce an allow-list entry or JID to its bare user part:
    /// strips the `@server` suffix and any `:device` suffix, so
    /// "49171234567:12@s.whatsapp.net", "49171234567@s.whatsapp.net" and
    /// "49171234567" all normalize to "49171234567".
    fn jid_user_part(value: &str) -> &str {
        let value = value.split('@').next().unwrap_or(value);
        value.split(':').next().unwrap_or(value)
    }

    /// `sender_users` holds the bare user parts of the sender JID and its
    /// alternate (LID <-> phone number), so entries match regardless of
    /// device suffix or addressing mode.
    fn sender_allowed(&self, sender_id: &str, sender_users: &[&str]) -> bool {
        let list = &self.config.allow_from;
        if !list.is_empty() {
            return list.iter().any(|entry| {
                if entry == "*" || entry == sender_id {
                    return true;
                }
                let entry_user = Self::jid_user_part(entry);
                !entry_user.is_empty() && sender_users.contains(&entry_user)
            });
        }
        self.is_allowed(sender_id)
    }

    fn is_status_chat(chat: &Jid) -> bool {
        chat.server == Server::Broadcast && chat.user.as_str() == "status"
    }

    /// Self-chat only: chat JID user must be our own PN or LID.
    fn is_self_chat(chat: &Jid, own_users: &[String]) -> bool {
        let chat_user = chat.user.as_str();
        !chat_user.is_empty() && own_users.iter().any(|u| u == chat_user)
    }

    async fn refresh_own_identity(
        client: &Client,
        own_jid_users: &Mutex<Vec<String>>,
        own_reply_chat_id: &Mutex<Option<String>>,
    ) {
        let mut users = Vec::new();
        let mut pn_chat = None;
        let mut lid_chat = None;
        if let Some(pn) = client.get_pn().await {
            let user = pn.user.to_string();
            if !user.is_empty() {
                pn_chat = Some(format!("{user}@s.whatsapp.net"));
                users.push(user);
            }
        }
        if let Some(lid) = client.get_lid().await {
            let user = lid.user.to_string();
            if !user.is_empty() {
                lid_chat = Some(format!("{user}@lid"));
                if !users.iter().any(|u| u == &user) {
                    users.push(user);
                }
            }
        }
        // Prefer LID for outbound self-chat: PN `to` + LID participants → silent 400.
        *own_jid_users.lock().unwrap() = users;
        *own_reply_chat_id.lock().unwrap() = lid_chat.or(pn_chat);
    }

    /// Self-chat replies must use a single addressing namespace end-to-end.
    /// Prefer the LID chat id when known; remapping to PN causes the library to
    /// encrypt LID participants under a PN outer `to`, which WhatsApp rejects.
    fn resolve_outbound_chat_id(&self, chat_id: &str) -> String {
        let Ok(to) = Jid::from_str(chat_id) else {
            return chat_id.to_string();
        };
        let own = self.own_jid_users.lock().unwrap().clone();
        if !Self::is_self_chat(&to, &own) {
            return chat_id.to_string();
        }
        self.own_reply_chat_id
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| chat_id.to_string())
    }

    /// Best-effort: fetch own/recipient device list and establish Signal sessions
    /// before send, so self-chat fan-out does not skip the phone.
    async fn warm_send_sessions(client: &Client, to: &Jid) {
        let signal = client.signal();
        match signal.get_user_devices(std::slice::from_ref(to)).await {
            Ok(devices) => {
                if devices.is_empty() {
                    return;
                }
                if let Err(e) = signal.assert_sessions(&devices).await {
                    log::warn!(
                        "WhatsApp session warm-up for {} failed: {e}",
                        to
                    );
                }
            }
            Err(e) => {
                log::warn!("WhatsApp device lookup for {} failed: {e}", to);
            }
        }
    }

    async fn shutdown_bot(&self) {
        if let Some(handle) = self.bot_handle.lock().unwrap().take() {
            handle.abort();
        }
        let client = self.client.lock().unwrap().take();
        if let Some(client) = client {
            client.disconnect().await;
        }
        self.own_jid_users.lock().unwrap().clear();
        *self.own_reply_chat_id.lock().unwrap() = None;
        *self.recently_sent_ids.lock().unwrap() = RecentlySentIds::new();
    }

    fn check_session_path(&self) -> bool {
        if self.config.session_db_path.trim().is_empty() {
            log::error!(
                "Session database path is empty. Please set sessionDbPath in the configuration."
            );
            return false;
        }
        true
    }

    async fn create_store(&self) -> Result<SqliteStore, String> {
        let session_db_path = self.session_db_path();
        let store = match SqliteStore::new(session_db_path.as_path().to_str().unwrap_or_default()).await {
            Ok(store) => store,
            Err(e) => {
                log::error!("Failed to create store: {}", e);
                return Err(e.to_string());
            }
        };
        Ok(store)
    }
}

#[async_trait]
impl BaseChannel for WhatsAppChannel {
    fn name(&self) -> &'static str {
        "whatsapp"
    }

    fn display_name(&self) -> &'static str {
        "WhatsApp"
    }

    fn running(&self) -> bool {
        self.base.running.load(Ordering::Relaxed)
    }

    fn bus(&self) -> &MessageBus {
        self.base.bus.as_ref()
    }

    fn config(&self) -> &ChannelsConfig {
        &self.channels_config
    }

    fn set_transcription_api_key(&mut self, key: String) {
        self.base.transcription_api_key = key;
    }

    async fn login(&self, force: bool) -> bool {
        if !self.check_session_path() {
            return false;
        }
        let session_db_path = self.session_db_path();
        if force {
            if let Err(e) = self.clear_session_store() {
                log::error!("{e}");
                return false;
            }
        }
        if let Some(parent) = session_db_path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                log::error!(
                    "Failed to create WhatsApp session directory '{}': {e}",
                    parent.display()
                );
                return false;
            }
        }
        let media_dir = self.media_download_dir();
        if !self.config.media_download_dir.trim().is_empty() {
            if let Err(e) = std::fs::create_dir_all(&media_dir) {
                log::error!(
                    "Failed to create WhatsApp media directory '{}': {e}",
                    media_dir.display()
                );
                return false;
            }
        }
        let Ok(store) = self.create_store().await else {
            return false;
        };

        let mut bot = match Bot::builder()
            .with_backend(Arc::new(store))
            .with_transport_factory(TokioWebSocketTransportFactory::new())
            .with_http_client(UreqHttpClient::new())
            .with_runtime(TokioRuntime)
            .on_event(move |event, _client| async move {
                match &*event {
                    Event::PairingQrCode { code, timeout } => {
                        println!(
                            "\nScan this QR code with WhatsApp (Linked Devices).\n\
                             Code expires in ~{}s:\n",
                            timeout.as_secs()
                        );
                        log::info!(
                            "WhatsApp pairing QR ready (timeout ~{}s)",
                            timeout.as_secs()
                        );
                        if let Err(err) = WhatsAppChannel::convert_qr_code_to_image(code) {
                            log::error!("Failed to render QR code: {err}");
                        }
                    }
                    Event::PairSuccess(info) => {
                        // Pairing crypto succeeded; the client then reconnects and
                        // finishes critical sync before Event::Connected.
                        log::info!("WhatsApp pairing succeeded as {}", info.id);
                        println!("Pairing succeeded; finishing connection sync...");
                    }
                    Event::Connected(_) => {
                        log::info!("WhatsApp connected and ready");
                    }
                    Event::PairError(err) => {
                        log::error!("WhatsApp pairing error: {err:?}");
                    }
                    Event::LoggedOut(_) => {
                        log::error!("WhatsApp logged out during login");
                    }
                    _ => {}
                }
            })
            .build()
            .await
        {
            Ok(bot) => bot,
            Err(e) => {
                log::error!("Failed to build WhatsApp bot: {}", e);
                return false;
            }
        };

        let handle = match bot.run().await {
            Ok(handle) => handle,
            Err(e) => {
                log::error!("Failed to start WhatsApp bot: {}", e);
                return false;
            }
        };

        let client = bot.client();
        // Wait until connected + logged in + critical app-state sync — not just
        // PairSuccess / is_logged_in(). Aborting earlier leaves the phone on
        // "Logging In" because the post-pair reconnect never finishes.
        let ok = match client.wait_for_connected(LOGIN_TIMEOUT).await {
            Ok(()) => {
                log::info!(
                    "WhatsApp login complete; session saved to {}",
                    self.session_db_path().display()
                );
                println!("WhatsApp login complete.");
                // Brief grace so the phone UI can leave "Logging In" before we drop.
                tokio::time::sleep(Duration::from_secs(10)).await;
                true
            }
            Err(e) => {
                log::error!("WhatsApp login failed waiting for connected state: {e}");
                false
            }
        };

        // Shut down the temporary login bot; credentials remain in SQLite for later start().
        handle.abort();
        client.disconnect().await;
        ok
    }

    async fn start(&self) {
        if !self.check_session_path() {
            return;
        }

        let Ok(store) = self.create_store().await else {
            return;
        };

        // Bridge bot events (static handler) into this start() loop so we can
        // call handle_message on &self.
        let (inbound_tx, mut inbound_rx) =
            tokio::sync::mpsc::unbounded_channel::<IncomingWaMessage>();

        let own_jid_users = Arc::clone(&self.own_jid_users);
        let own_reply_chat_id = Arc::clone(&self.own_reply_chat_id);
        let recently_sent_ids = Arc::clone(&self.recently_sent_ids);
        let reply_prefix = self.config.reply_prefix.clone();

        let mut bot = match Bot::builder()
            .with_backend(Arc::new(store))
            .with_transport_factory(TokioWebSocketTransportFactory::new())
            .with_http_client(UreqHttpClient::new())
            .with_runtime(TokioRuntime)
            .skip_history_sync()
            .on_event(move |event, _client| {
                let inbound_tx = inbound_tx.clone();
                let own_jid_users = Arc::clone(&own_jid_users);
                let own_reply_chat_id = Arc::clone(&own_reply_chat_id);
                let recently_sent_ids = Arc::clone(&recently_sent_ids);
                let reply_prefix = reply_prefix.clone();
                async move {
                    match &*event {
                        Event::Message(msg, info) => {
                            // Self-chat mode: only process our own messages.
                            log::info!("WhatsApp message received ...");
                            if !info.source.is_from_me {
                                return;
                            }
                            // Ignore groups and status broadcasts.
                            if info.source.is_group
                                || Self::is_status_chat(&info.source.chat)
                            {
                                return;
                            }
                            // Require "Message yourself" chat (chat == own PN/LID).
                            {
                                let own = own_jid_users.lock().unwrap();
                                if own.is_empty()
                                    || !Self::is_self_chat(&info.source.chat, &own)
                                {
                                    return;
                                }
                            }

                            let Some(content) = msg.text_content() else {
                                return;
                            };
                            if content.trim().is_empty() {
                                return;
                            }
                            log::info!("WhatsApp message received: {content}");

                            // Suppress echoes of our own outbound replies.
                            let message_id = info.id.to_string();
                            if recently_sent_ids.lock().unwrap().contains(&message_id) {
                                log::debug!(
                                    "Ignoring WhatsApp echo (recently sent id {message_id})"
                                );
                                return;
                            }
                            if !reply_prefix.is_empty() && content.starts_with(&reply_prefix) {
                                log::debug!("Ignoring WhatsApp echo (reply prefix)");
                                return;
                            }

                            let sender_id = info.source.sender.to_string();
                            let sender_user = info.source.sender.user.to_string();
                            let sender_alt_user = info
                                .source
                                .sender_alt
                                .as_ref()
                                .map(|jid| jid.user.to_string());
                            // Prefer LID chat id so replies stay in one wire namespace.
                            let chat_id = own_reply_chat_id
                                .lock()
                                .unwrap()
                                .clone()
                                .unwrap_or_else(|| info.source.chat.to_string());
                            let mut metadata = HashMap::new();
                            metadata.insert(
                                "message_id".to_string(),
                                serde_json::json!(message_id),
                            );
                            metadata.insert(
                                "push_name".to_string(),
                                serde_json::json!(info.push_name),
                            );
                            metadata.insert(
                                "is_group".to_string(),
                                serde_json::json!(info.source.is_group),
                            );

                            let _ = inbound_tx.send(IncomingWaMessage {
                                sender_id,
                                sender_user,
                                sender_alt_user,
                                chat_id,
                                content: content.to_string(),
                                media: None,
                                metadata: Some(metadata),
                            });
                        }
                        Event::Connected(_) => {
                            log::info!("WhatsApp channel connected");
                        }
                        Event::LoggedOut(_) => {
                            log::error!("WhatsApp session logged out");
                        }
                        _ => {}
                    }
                }
            })
            .build()
            .await
        {
            Ok(bot) => bot,
            Err(e) => {
                log::error!("Failed to build WhatsApp bot: {e}");
                return;
            }
        };

        let handle = match bot.run().await {
            Ok(handle) => handle,
            Err(e) => {
                log::error!("Failed to start WhatsApp bot: {e}");
                return;
            }
        };
        let client = bot.client();

        if let Err(e) = client.wait_for_connected(LOGIN_TIMEOUT).await {
            log::error!("WhatsApp failed to reach connected state: {e}");
            handle.abort();
            client.disconnect().await;
            return;
        }

        Self::refresh_own_identity(&client, &self.own_jid_users, &self.own_reply_chat_id).await;
        let own_users_summary = self.own_jid_users.lock().unwrap().join(", ");
        let own_reply_chat = self.own_reply_chat_id.lock().unwrap().clone();
        if own_users_summary.is_empty() || own_reply_chat.is_none() {
            log::error!(
                "WhatsApp connected but own PN/LID unavailable; self-chat filtering cannot work"
            );
            handle.abort();
            client.disconnect().await;
            return;
        }
        let own_reply_chat = own_reply_chat.expect("checked above");
        log::info!(
            "WhatsApp self-chat mode — only messages to yourself are processed (own users: {own_users_summary}; reply chat: {own_reply_chat})"
        );
        // Warm Signal sessions for our own devices so the first self-chat reply
        // can encrypt to the phone instead of skipping it.
        if let Ok(reply_jid) = Jid::from_str(&own_reply_chat) {
            Self::warm_send_sessions(&client, &reply_jid).await;
        }

        *self.bot_handle.lock().unwrap() = Some(handle);
        *self.client.lock().unwrap() = Some(client);
        self.base.running.store(true, Ordering::Relaxed);
        log::info!("WhatsApp channel listening for messages...");

        while self.base.running.load(Ordering::Relaxed) {
            tokio::select! {
                maybe = inbound_rx.recv() => {
                    match maybe {
                        Some(msg) => {
                            let mut sender_users = vec![msg.sender_user.as_str()];
                            if let Some(alt) = msg.sender_alt_user.as_deref() {
                                sender_users.push(alt);
                            }
                            if !self.sender_allowed(&msg.sender_id, &sender_users) {
                                log::warn!(
                                    "Ignoring WhatsApp message from disallowed sender {}",
                                    msg.sender_id
                                );
                                continue;
                            }
                            self.handle_message(
                                &msg.sender_id,
                                &msg.chat_id,
                                &msg.content,
                                msg.media,
                                msg.metadata,
                                None,
                            )
                            .await;
                        }
                        None => {
                            // Event handler dropped — stop listening.
                            break;
                        }
                    }
                }
                _ = tokio::time::sleep(Duration::from_millis(250)) => {}
            }
        }

        self.shutdown_bot().await;
        self.base.running.store(false, Ordering::Relaxed);
        log::info!("WhatsApp channel stopped");
    }

    async fn stop(&self) {
        self.base.running.store(false, Ordering::Relaxed);
        self.shutdown_bot().await;
    }

    async fn send(&self, msg: OutboundMessage) -> Result<(), String> {
        if !self.base.running.load(Ordering::Relaxed) {
            return Err("WhatsApp channel is not running".to_string());
        }

        // Clone the Arc out of the mutex — never hold the guard across await.
        let client = self
            .client
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| "WhatsApp client is not connected".to_string())?;

        let chat_id = msg.chat_id.trim();
        if chat_id.is_empty() {
            return Err("WhatsApp send missing chat_id".to_string());
        }

        let resolved_chat_id = self.resolve_outbound_chat_id(chat_id);
        let to = Jid::from_str(&resolved_chat_id).map_err(|e| {
            format!("Invalid WhatsApp chat_id '{resolved_chat_id}': {e}")
        })?;
        if resolved_chat_id != chat_id {
            log::info!(
                "WhatsApp self-chat send remapped {chat_id} -> {resolved_chat_id}"
            );
        }

        let content = msg.content.trim();
        if !msg.media.is_empty() {
            log::warn!(
                "WhatsApp send ignoring {} media attachment(s); media send not implemented",
                msg.media.len()
            );
        }
        if content.is_empty() {
            return Err("WhatsApp send has empty content".to_string());
        }

        // Tag replies so self-chat echo-back can be ignored even if the id
        // tracker misses a race.
        let outbound_text = if self.config.reply_prefix.is_empty() {
            content.to_string()
        } else {
            format!("{}{content}", self.config.reply_prefix)
        };

        let message = wa::Message {
            conversation: Some(outbound_text),
            ..Default::default()
        };

        Self::warm_send_sessions(&client, &to).await;

        log::info!("WhatsApp sending reply to {resolved_chat_id}");
        let result = client
            .send_message(to, message)
            .await
            .map_err(|e| format!("Failed to send WhatsApp message: {e}"))?;

        log::info!(
            "WhatsApp reply sent to {resolved_chat_id} (id {})",
            result.message_id
        );
        self.recently_sent_ids
            .lock()
            .unwrap()
            .remember(result.message_id);

        Ok(())
    }
}
