use async_trait::async_trait;
use whatsapp_rust::{
    TokioRuntime,
    bot::Bot,
    store::SqliteStore,
    types::events::Event,
};
use whatsapp_rust_tokio_transport::TokioWebSocketTransportFactory;
use whatsapp_rust_ureq_http_client::UreqHttpClient;
use std::{
    path::PathBuf,
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

/// How long to wait for QR scan / reconnect before giving up on login.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);

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
}

impl Default for WhatsAppConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            session_db_path: "~/rust-bot/data/whatsapp/whatsapp.db".to_string(),
            allow_from: vec![],
            media_download_dir: "~/rust-bot/data/whatsapp_media".to_string(),
            group_policy: WhatsAppGroupPolicy::Open,
        }
    }
}

pub struct WhatsAppChannel {
    base: BaseChannelCommon,
    channels_config: ChannelsConfig,
    config: WhatsAppConfig,
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
        if self.config.session_db_path.trim().is_empty() {
            log::error!(
                "Session database path is empty. Please set sessionDbPath in the configuration."
            );
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
        let store = match SqliteStore::new(session_db_path.as_path().to_str().unwrap_or_default()).await {
            Ok(store) => store,
            Err(e) => {
                log::error!("Failed to create store: {}", e);
                return false;
            }
        };

        let (paired_tx, paired_rx) = tokio::sync::oneshot::channel::<()>();
        let paired_tx = Arc::new(Mutex::new(Some(paired_tx)));

        let mut bot = match Bot::builder()
            .with_backend(Arc::new(store))
            .with_transport_factory(TokioWebSocketTransportFactory::new())
            .with_http_client(UreqHttpClient::new())
            .with_runtime(TokioRuntime)
            .on_event({
                let paired_tx = Arc::clone(&paired_tx);
                move |event, _client| {
                    let paired_tx = Arc::clone(&paired_tx);
                    async move {
                        match &*event {
                            Event::PairingQrCode { code, timeout } => {
                                println!(
                                    "\nScan this QR code with WhatsApp (Linked Devices).\n\
                                     Code expires in ~{}s:\n\n{}\n",
                                    timeout.as_secs(),
                                    code
                                );
                                log::info!(
                                    "WhatsApp pairing QR ready (timeout ~{}s)",
                                    timeout.as_secs()
                                );
                            }
                            Event::PairSuccess(_) => {
                                log::info!("WhatsApp pairing succeeded");
                                if let Ok(mut guard) = paired_tx.lock() {
                                    if let Some(tx) = guard.take() {
                                        let _ = tx.send(());
                                    }
                                }
                            }
                            Event::Connected(_) => {
                                log::info!("WhatsApp connected");
                                if let Ok(mut guard) = paired_tx.lock() {
                                    if let Some(tx) = guard.take() {
                                        let _ = tx.send(());
                                    }
                                }
                            }
                            Event::PairError(err) => {
                                log::error!("WhatsApp pairing error: {err:?}");
                            }
                            Event::LoggedOut(_) => {
                                log::error!("WhatsApp logged out during login");
                            }
                            _ => {}
                        }
                    }
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
        let wait_paired = async {
            if client.is_logged_in() {
                return true;
            }
            tokio::select! {
                result = paired_rx => result.is_ok(),
                _ = async {
                    loop {
                        if client.is_logged_in() {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(250)).await;
                    }
                } => true,
            }
        };

        let ok = match tokio::time::timeout(LOGIN_TIMEOUT, wait_paired).await {
            Ok(true) => {
                log::info!("WhatsApp login complete; session saved to {}", self.session_db_path().display());
                println!("WhatsApp login complete.");
                true
            }
            Ok(false) => {
                log::error!("WhatsApp login aborted before pairing completed");
                false
            }
            Err(_) => {
                log::error!(
                    "WhatsApp login timed out after {}s waiting for pairing",
                    LOGIN_TIMEOUT.as_secs()
                );
                false
            }
        };

        // Shut down the temporary login bot; credentials remain in SQLite for later start().
        handle.abort();
        client.disconnect().await;
        ok
    }

    async fn start(&self) {
        self.base.running.store(true, Ordering::Relaxed);
    }

    async fn stop(&self) {
        self.base.running.store(false, Ordering::Relaxed);
    }

    async fn send(&self, _msg: OutboundMessage) -> Result<(), String> {
        Err("WhatsApp channel send is not implemented".to_string())
    }
}
