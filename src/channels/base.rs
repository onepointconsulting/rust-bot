use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::AtomicBool,
        Mutex as StdMutex,
    },
};

use async_trait::async_trait;
use chrono::Utc;
use tokio::sync::mpsc::error::SendError;

use crate::{
    bus::{events::{InboundMessage, OutboundMessage}, queue::MessageBus}, config::schema::ChannelsConfig, providers::transcription::{
        GROQ_DEFAULT_API_URL, GROQ_DEFAULT_MODEL, GroqTranscriptionProvider,
        OPENAI_DEFAULT_API_URL, OPENAI_DEFAULT_MODEL, OpenAITranscriptionProvider, PathLike,
        TranscriptionProvider,
    },
    security::workspace_requests::WorkspaceRequestHandler,
    session::manager::SessionManager,
};

pub async fn handle_message(
    sender_id: &str,
    chat_id: &str,
    content: &str,
    media: Option<Vec<String>>,
    metadata: Option<HashMap<String, serde_json::Value>>,
    session_key: Option<String>,
    is_allowed: bool,
    supports_streaming: bool,
    channel_name: &str,
    bus: &MessageBus,
) -> Result<(), SendError<String>> {
    if !is_allowed {
        let msg = format!("Sender {} is not allowed to send messages to channel {}", sender_id, channel_name);
        log::warn!("{}", msg);
        return Err(SendError(msg));
    }

    let mut meta = metadata.unwrap_or_default();
    if supports_streaming {
        meta.insert("_wants_stream".to_string(), serde_json::json!(true));
    }
    let message = InboundMessage {
        channel: channel_name.to_string(),
        sender_id: sender_id.to_string(),
        chat_id: chat_id.to_string(),
        content: content.to_string(),
        timestamp: Utc::now(),
        media: media.unwrap_or_default(),
        metadata: meta,
        session_key_override: session_key,
    };
    if let Err(e) = bus.publish_inbound(message) {
        let msg = format!("Failed to publish inbound message to bus: {}", e);
        log::error!("{}", msg);
        return Err(SendError(msg));
    }
    return Ok(());
}

/// Abstract base class for chat channel implementations.
/// Each channel (Telegram, Discord, etc.) should implement this interface
/// to integrate with the nanobot message bus.
#[async_trait]
pub trait BaseChannel: std::any::Any + Send + Sync {
    /// Channel name
    fn name(&self) -> &'static str {
        return "base";
    }
    /// Channel display name
    fn display_name(&self) -> &'static str {
        return "Base";
    }

    /// Transcription provider name, if configured.
    fn transcription_provider(&self) -> Option<&str> {
        self.config().transcription_provider.as_deref()
    }

    fn transcription_api_key(&self) -> &str {
        ""
    }

    fn set_transcription_api_key(&mut self, key: String);

    fn running(&self) -> bool;

    fn bus(&self) -> &MessageBus;

    fn config(&self) -> &ChannelsConfig;

    /// Transcribe an audio file via Whisper (OpenAI or Groq). Returns empty string on failure.
    async fn transcribe_audio(&self, file_path: PathLike) -> String {
        let Some(provider_name) = self.transcription_provider() else {
            log::error!(
                "No transcription provider configured for channel {}",
                self.name()
            );
            return "".to_string();
        };
        if self.transcription_api_key().is_empty() {
            log::error!("No transcription API key configured for channel {}", self.name());
            return "".to_string();
        }
        let provider: Box<dyn TranscriptionProvider> = match provider_name {
            "openai" => Box::new(OpenAITranscriptionProvider::new(
                OPENAI_DEFAULT_API_URL,
                Some(self.transcription_api_key().to_string()),
                Some(OPENAI_DEFAULT_MODEL.to_string()),
            )),
            "groq" => Box::new(GroqTranscriptionProvider::new(
                GROQ_DEFAULT_API_URL,
                Some(self.transcription_api_key().to_string()),
                Some(GROQ_DEFAULT_MODEL.to_string()),
            )),
            _ => Box::new(GroqTranscriptionProvider::new(
                GROQ_DEFAULT_API_URL,
                Some(self.transcription_api_key().to_string()),
                Some(GROQ_DEFAULT_MODEL.to_string()),
            )),
        };
        return provider.transcribe(file_path).await;
    }

    /// Perform channel-specific interactive login (e.g. QR code scan).
    ///
    /// # Arguments
    ///
    /// * `force` — If `true`, ignore existing credentials and force re-authentication.
    ///
    /// Returns `true` if already authenticated or login succeeds.
    ///
    /// Override in channel implementations that support interactive login.
    async fn login(&self, force: bool) -> bool {
        let _ = force;
        true
    }

    /// Start the channel and begin listening for messages.
    ///
    /// This should be a long-running async task that:
    /// 1. Connects to the chat platform
    /// 2. Listens for incoming messages
    /// 3. Forwards messages to the bus via [`Self::handle_message`]
    ///
    /// Takes `&self` so the channel manager can share the channel via [`Arc`]
    /// and run outbound `send` concurrently with the listen loop (Python does
    /// this naturally; Rust needs interior mutability for mutable state).
    async fn start(&self);

    /// Stop the channel and clean up resources.
    async fn stop(&self);

    /// Send a message through this channel.
    ///
    /// # Arguments
    ///
    /// * `msg` — The message to send.
    ///
    /// Returns `Err` on delivery failure so the channel manager can retry
    /// or mark as failed. Intentional skips (e.g. consent/policy) should return `Ok(())`.
    async fn send(&self, msg: OutboundMessage) -> Result<(), String>;

    /// Deliver a streaming text chunk.
    ///
    /// Override in channel implementations to enable streaming. Implementations should
    /// return `Err` on delivery failure so the channel manager can retry.
    ///
    /// Streaming contract: `send_delta` is a chunk, `send_stream_end` ends the current
    /// segment, and stateful implementations must key buffers by stream id rather than
    /// only by `chat_id`.
    async fn send_delta(
        &self,
        _chat_id: &str,
        _delta: &str,
        _metadata: Option<HashMap<String, serde_json::Value>>,
    ) -> Result<(), String> {
        Ok(())
    }

    /// Whether this channel overrides [`Self::send_delta`].
    ///
    /// Rust has no equivalent to Python's
    /// `type(self).send_delta is not BaseChannel.send_delta`.
    /// Override to return `true` in channels that implement streaming delivery.
    fn implements_send_delta(&self) -> bool {
        false
    }

    /// True when config enables streaming AND this channel implements [`send_delta`].
    fn supports_streaming(&self) -> bool {
        self.config().streaming && self.implements_send_delta()
    }

    /// Check if *sender_id* is permitted.  Empty list → deny all; ``"*"`` → allow all.
    fn is_allowed(&self, sender_id: &str) -> bool {
        let allow_list = self.config().allow_from.clone();
        if allow_list.is_empty() {
            log::warn!("No allow list configured for channel {}", self.name());
            return false;
        }
        if allow_list.contains(&"*".to_string()) {
            return true;
        }
        return allow_list.contains(&sender_id.to_string());
    }

    /// Handle an incoming message from the chat platform.
    ///
    /// Checks permissions and forwards the message to the bus.
    ///
    /// # Arguments
    ///
    /// * `sender_id` — The sender's identifier.
    /// * `chat_id` — The chat or channel identifier.
    /// * `content` — Message text content.
    /// * `media` — Optional list of media URLs.
    /// * `metadata` — Optional channel-specific metadata.
    /// * `session_key` — Optional session key override (e.g. thread-scoped sessions).
    /// * `is_dm` — Whether this message arrived as a direct message. When the
    ///   sender is rejected and this is `true`, a pairing code is generated
    ///   and sent back instead of a silent log warning.
    /// * `authorization_id` — Identity to authorize instead of `sender_id`
    ///   (e.g. a group/room), without changing the sender's recorded
    ///   identity. `None` authorizes by sender, as before.
    ///
    /// Mirrors nanobot's `_handle_message` (`nanobot/channels/base.py:230-286`).
    async fn handle_message(
        &self,
        sender_id: &str,
        chat_id: &str,
        content: &str,
        media: Option<Vec<String>>,
        metadata: Option<HashMap<String, serde_json::Value>>,
        session_key: Option<String>,
        is_dm: bool,
        authorization_id: Option<&str>,
    ) -> Result<(), SendError<String>> {
        let permission_id = authorization_id.unwrap_or(sender_id);
        if !self.is_allowed(permission_id) {
            if is_dm {
                let code = crate::pairing::generate_code(self.name(), sender_id);
                let reply = OutboundMessage {
                    channel: self.name().to_string(),
                    chat_id: chat_id.to_string(),
                    content: crate::pairing::format_pairing_reply(&code),
                    reply_to: None,
                    media: Vec::new(),
                    metadata: HashMap::from([(
                        crate::pairing::PAIRING_CODE_META_KEY.to_string(),
                        serde_json::json!(code),
                    )]),
                    event: None,
                };
                match self.send(reply).await {
                    Ok(()) => log::info!(
                        "Sent pairing code {code} to sender {sender_id} in chat {chat_id}"
                    ),
                    Err(e) => log::error!("Failed to send pairing reply: {e}"),
                }
            } else {
                log::warn!(
                    "Access denied for sender {sender_id}. Add them to allowFrom list in config to grant access."
                );
            }
            return Err(SendError(format!("Access denied for sender {sender_id}. Add them to allowFrom list in config to grant access.")));
        }

        handle_message(
            sender_id,
            chat_id,
            content,
            media,
            metadata,
            session_key,
            true, // already confirmed allowed above
            self.supports_streaming(),
            self.name(),
            self.bus(),
        )
        .await
    }

    fn default_config(&self) -> HashMap<String, serde_json::Value> {
        let mut map = HashMap::new();
        map.insert("enabled".to_string(), serde_json::json!(false));
        return map;
    }

    fn is_running(&self) -> bool {
        return self.running();
    }

}

pub struct BaseChannelCommon {
    pub bus: Arc<MessageBus>,
    pub running: AtomicBool,
    pub transcription_api_key: String,
    pub session_manager: Arc<StdMutex<SessionManager>>,
    pub workspace_request_handler: WorkspaceRequestHandler,
}

impl BaseChannelCommon {
    pub fn new(
        bus: Arc<MessageBus>,
        session_manager: Arc<StdMutex<SessionManager>>,
        workspace_request_handler: WorkspaceRequestHandler,
    ) -> Self {
        Self {
            bus,
            running: AtomicBool::new(false),
            transcription_api_key: String::new(),
            session_manager,
            workspace_request_handler,
        }
    }
}
