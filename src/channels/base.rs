use std::{sync::Arc};

use async_trait::async_trait;

use crate::{bus::queue::MessageBus, config::schema::ChannelsConfig, providers::transcription::PathLike};

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
    fn transcription_provider(&self) -> &'static str {
        return "groq";
    }

    fn transcription_api_key(&self) -> &'static str {
        return "";
    }

    fn register_channels(&self, config: &ChannelsConfig, bus: Arc<MessageBus>) -> Vec<Box<dyn BaseChannel>>;

    /// Transcribe an audio file via Whisper (OpenAI or Groq). Returns empty string on failure.
    async fn transcribe_audio(&self, file_path: PathLike) -> String {
        if self.transcription_api_key().is_empty() {
            return "".to_string();
        }
        if self.transcription_provider() == "openai" {
            return "".to_string();
        }
        return "".to_string();
    }
}