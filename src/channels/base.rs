use std::{collections::HashMap};

use async_trait::async_trait;
use chrono::Utc;

use crate::{
    bus::{events::{InboundMessage, OutboundMessage}, queue::MessageBus}, config::schema::ChannelsConfig, providers::transcription::{
        GROQ_DEFAULT_API_URL, GROQ_DEFAULT_MODEL, GroqTranscriptionProvider,
        OPENAI_DEFAULT_API_URL, OPENAI_DEFAULT_MODEL, OpenAITranscriptionProvider, PathLike,
        TranscriptionProvider,
    },
};

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

    /// Transcription provider
    fn transcription_provider(&self) -> &str {
        return self.config().transcription_provider.as_str();
    }

    fn transcription_api_key(&self) -> &'static str {
        return "";
    }

    fn running(&self) -> bool;

    fn bus(&self) -> &MessageBus;

    fn config(&self) -> &ChannelsConfig;

    /// Transcribe an audio file via Whisper (OpenAI or Groq). Returns empty string on failure.
    async fn transcribe_audio(&self, file_path: PathLike) -> String {
        if self.transcription_api_key().is_empty() {
            log::error!("No transcription API key configured for channel {}", self.name());
            return "".to_string();
        }
        let provider: Box<dyn TranscriptionProvider> = match self.transcription_provider() {
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
    fn login(&self, force: bool) -> bool {
        let _ = force;
        true
    }

    /// Start the channel and begin listening for messages.
    /// 
    /// This should be a long-running async task that:
    /// 1. Connects to the chat platform
    /// 2. Listens for incoming messages
    /// 3. Forwards messages to the bus via _handle_message()
    async fn start(&self);

    /// Stop the channel and clean up resources.
    async fn stop(&self);

    /// Send a message through this channel.
    /// 
    /// # Arguments
    /// 
    /// * `msg` — The message to send.
    /// 
    /// Implementations should raise on delivery failure so the channel manager
    /// can retry or mark as failed.
    async fn send(&self, msg: OutboundMessage);

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
    ) {
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
    async fn handle_message(
        &self,
        sender_id: &str,
        chat_id: &str,
        content: &str,
        media: Option<Vec<String>>,
        metadata: Option<HashMap<String, serde_json::Value>>,
        session_key: Option<String>,
    ) {
        if !self.is_allowed(sender_id) {
            log::warn!("Sender {} is not allowed to send messages to channel {}", sender_id, self.name());
            return;
        }

        let mut meta = metadata.unwrap_or_default();
        if self.supports_streaming() {
            meta.insert("_wants_stream".to_string(), serde_json::json!(true));
        }
        let message = InboundMessage {
            channel: self.name().to_string(),
            sender_id: sender_id.to_string(),
            chat_id: chat_id.to_string(),
            content: content.to_string(),
            timestamp: Utc::now(),
            media: media.unwrap_or_default(),
            metadata: meta,
            session_key_override: session_key,
        };
        if let Err(e) = self.bus().publish_inbound(message) {
            log::error!("Failed to publish inbound message to bus: {}", e);
        }
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
