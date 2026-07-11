use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
    time::Duration,
};

use tokio::{
    sync::{Mutex, mpsc::error::TryRecvError},
    task::JoinHandle,
    time::timeout,
};

use crate::{
    bus::{events::OutboundMessage, queue::MessageBus},
    channels::{base::BaseChannel, registry::discover_all},
    config::schema::Config,
    utils::{
        exit_codes::{self, CHANNEL_ALLOW_FROM_EMPTY},
        restart::{consume_restart_notice_from_env, format_restart_completed_message},
    },
};

/// Retry delays for message sending (exponential backoff: 1s, 2s, 4s).
/// Matches Python `_SEND_RETRY_DELAYS`; delays beyond the table stay at the last value.
const SEND_RETRY_DELAYS_SECS: &[u64] = &[1, 2, 4];

/// Shared channel handle. `Arc` lets the dispatcher and each channel's listen
/// task hold the same instance (Python shares `self.channels` across tasks).
type SharedChannel = Arc<dyn BaseChannel>;

/// Manages chat channels and coordinates message routing.
///
/// Responsibilities:
/// - Initialize enabled channels (Telegram, WhatsApp, etc.)
/// - Start/stop channels
/// - Route outbound messages
pub struct ChannelManager {
    config: Config,
    bus: Arc<MessageBus>,
    channels: HashMap<String, SharedChannel>,
    /// Python: `self._dispatch_task = asyncio.create_task(...)`
    ///
    /// Wrapped in a `tokio::sync::Mutex` (rather than stored by value) so
    /// [`Self::start_all`] and [`Self::stop_all`] can both take `&self`. That
    /// lets a caller hold the manager behind an `Arc` and call `stop_all`
    /// concurrently while `start_all` is still awaiting channel tasks —
    /// mirroring how Python can call `stop_all()` on the same object while
    /// `start_all()`'s `asyncio.gather(...)` is in flight.
    dispatch_task: Mutex<Option<JoinHandle<()>>>,
}

impl ChannelManager {
    /// Discover enabled built-in channels and attach them to the shared bus.
    pub fn new(config: Config, bus: Arc<MessageBus>) -> Self {
        let channels = Self::init_channels(&config, Arc::clone(&bus));
        if let Err(name) = Self::validate_allow_from(&channels) {
            log::error!("Channel {name} has no allow list configured");
            log::error!("Set [\"*\"] to allow everyone, or add specific user IDs.");
            exit_codes::exit(CHANNEL_ALLOW_FROM_EMPTY);
        }
        Self {
            config,
            bus,
            channels,
            dispatch_task: Mutex::new(None),
        }
    }

    fn resolve_transcription_key(config: &Config, provider: &str) -> String {
        match provider {
            "groq" => config.providers.groq.api_key.clone(),
            "openai" => config.providers.openai.api_key.clone(),
            _ => {
                log::error!("Unknown transcription provider: {provider}");
                String::new()
            }
        }
    }

    fn init_channels(config: &Config, bus: Arc<MessageBus>) -> HashMap<String, SharedChannel> {
        let transcription_key =
            Self::resolve_transcription_key(config, &config.channels.transcription_provider);
        let mut channels = HashMap::new();
        for (name, mut channel) in discover_all(config, Arc::clone(&bus)) {
            channel.set_transcription_api_key(transcription_key.clone());
            // Box → Arc after last `&mut` setup (transcription key).
            channels.insert(name.to_string(), Arc::from(channel));
            log::info!("Initialized channel: {name}");
        }
        channels
    }

    /// Returns `Err(channel_name)` when that channel has an empty `allowFrom` list.
    fn validate_allow_from(channels: &HashMap<String, SharedChannel>) -> Result<(), String> {
        for (name, channel) in channels {
            if channel.config().allow_from.is_empty() {
                return Err(name.clone());
            }
        }
        Ok(())
    }

    /// Start a channel by name. No-op (with error log) if the channel is not registered.
    ///
    /// Note: [`BaseChannel::start`] is typically a long-running loop; callers that need
    /// to start multiple channels should spawn each call onto its own task.
    pub async fn start_channel(&self, name: &str) {
        let Some(channel) = self.channels.get(name) else {
            log::error!("Channel {name} not found");
            return;
        };
        channel.start().await;
    }

    /// Start all channels and the outbound dispatcher.
    ///
    /// Mirrors Python:
    /// ```python
    /// self._dispatch_task = asyncio.create_task(self._dispatch_outbound())
    /// tasks = [asyncio.create_task(self._start_channel(n, ch)) for ...]
    /// self._notify_restart_done_if_needed()
    /// await asyncio.gather(*tasks, return_exceptions=True)
    /// ```
    pub async fn start_all(&self) {
        if self.channels.is_empty() {
            log::warn!("No channels enabled.");
            return;
        }

        // Spawn dispatcher in the background (does not block start_all).
        let bus = Arc::clone(&self.bus);
        let channels = self.channels.clone();
        let send_progress = self.config.channels.send_progress;
        let send_tool_hints = self.config.channels.send_tool_hints;
        let send_max_retries = self.config.channels.send_max_retries;
        let dispatch_task = tokio::spawn(async move {
            Self::dispatch_outbound(
                bus,
                channels,
                send_progress,
                send_tool_hints,
                send_max_retries,
            )
            .await;
        });
        *self.dispatch_task.lock().await = Some(dispatch_task);

        // Spawn each channel listen loop concurrently, then wait for them.
        let mut set = tokio::task::JoinSet::new();
        for (name, channel) in &self.channels {
            let name = name.clone();
            let channel = Arc::clone(channel);
            set.spawn(async move {
                log::info!("Starting {name} channel...");
                channel.start().await;
            });
        }

        self.notify_restart_done_if_needed();

        while let Some(result) = set.join_next().await {
            if let Err(e) = result {
                log::error!("Channel task failed: {e}");
            }
        }
    }

    /// Send restart completion message when runtime env markers are present.
    fn notify_restart_done_if_needed(&self) {
        let Some(notice) = consume_restart_notice_from_env() else {
            return;
        };
        let Some(target) = self.channels.get(&notice.channel) else {
            return;
        };
        let channel = Arc::clone(target);
        let max_attempts = std::cmp::max(self.config.channels.send_max_retries, 1);
        let msg = OutboundMessage {
            channel: notice.channel,
            chat_id: notice.chat_id,
            content: format_restart_completed_message(&notice.started_at_raw),
            reply_to: None,
            media: Vec::new(),
            metadata: HashMap::new(),
        };
        tokio::spawn(async move {
            Self::send_with_retry(channel.as_ref(), msg, max_attempts).await;
        });
    }

    /// Stop the outbound dispatcher and all channels.
    pub async fn stop_all(&self) {
        log::info!("Stopping all channels...");

        let task = self.dispatch_task.lock().await.take();
        if let Some(task) = task {
            task.abort();
            let _ = task.await;
        }

        for (name, channel) in &self.channels {
            channel.stop().await;
            log::info!("Stopped {name} channel");
        }
    }

    /// Dispatch outbound messages to the appropriate channel.
    ///
    /// Takes owned/cloned shared state so it can run inside `tokio::spawn`
    /// without borrowing `&mut ChannelManager`.
    async fn dispatch_outbound(
        bus: Arc<MessageBus>,
        channels: HashMap<String, SharedChannel>,
        send_progress: bool,
        send_tool_hints: bool,
        send_max_retries: u8,
    ) {
        log::info!("Outbound dispatcher started.");
        let mut pending: VecDeque<OutboundMessage> = VecDeque::new();
        loop {
            // First check pending buffer before waiting on queue
            let mut msg = if let Some(msg) = pending.pop_front() {
                msg
            } else {
                match timeout(Duration::from_secs(1), bus.consume_outbound()).await {
                    Ok(Some(msg)) => msg,
                    Ok(None) => {
                        // outbound channel closed
                        break;
                    }
                    Err(_elapsed) => {
                        // 1s timeout — same as asyncio.TimeoutError
                        continue;
                    }
                }
            };

            let metadata = msg.metadata.clone();
            if metadata.get("_progress").is_some() {
                if metadata.get("_tool_hint").is_some() && !send_tool_hints {
                    continue;
                }
                if metadata.get("_tool_hint").is_none() && !send_progress {
                    continue;
                }
            }

            // Coalesce consecutive _stream_delta messages for the same (channel, chat_id)
            // to reduce API calls and improve streaming latency
            if metadata.get("_stream_delta").is_some() && metadata.get("_stream_end").is_none() {
                let (message, extra_pending) = Self::coalesce_stream_deltas(&bus, &msg);
                pending.extend(extra_pending);
                msg = message;
            }

            let max_attempts = std::cmp::max(send_max_retries, 1);
            if let Some(channel) = channels.get(&msg.channel) {
                Self::send_with_retry(channel.as_ref(), msg, max_attempts).await;
            } else {
                log::warn!("Unknown channel: {}", msg.channel);
            }
        }
    }

    /// Merge consecutive `_stream_delta` messages for the same `(channel, chat_id)`.
    fn coalesce_stream_deltas(
        bus: &MessageBus,
        first_msg: &OutboundMessage,
    ) -> (OutboundMessage, Vec<OutboundMessage>) {
        let target_key = (first_msg.channel.clone(), first_msg.chat_id.clone());
        let mut combined_content = first_msg.content.clone();
        let mut final_metadata = first_msg.metadata.clone();
        let mut non_matching = Vec::new();

        // Only merge consecutive deltas. As soon as we hit any other message,
        // stop and hand that boundary back to the dispatcher via `pending`.
        loop {
            let next_msg = match bus.outbound.try_recv() {
                Ok(next_msg) => next_msg,
                Err(TryRecvError::Empty) => break, // QueueEmpty
                Err(TryRecvError::Disconnected) => break, // senders gone
            };
            let same_target = (next_msg.channel.clone(), next_msg.chat_id.clone()) == target_key;
            let metadata = next_msg.metadata.clone();
            let is_delta = metadata.get("_stream_delta").is_some();
            let is_end = metadata.get("_stream_end").is_some();

            if same_target && is_delta && final_metadata.get("_stream_end").is_none() {
                // Accumulate content
                combined_content.push_str(&next_msg.content);
                // If we see _stream_end, remember it and stop coalescing this stream
                if is_end {
                    final_metadata.insert("_stream_end".to_string(), serde_json::Value::Bool(true));
                    // Stream ended - stop coalescing this stream
                    break;
                }
            } else {
                non_matching.push(next_msg);
                break;
            }
        }

        let merged = OutboundMessage {
            channel: first_msg.channel.clone(),
            chat_id: first_msg.chat_id.clone(),
            content: combined_content,
            metadata: final_metadata,
            media: vec![],
            reply_to: None,
        };
        (merged, non_matching)
    }

    /// Send a message with retry on failure using exponential backoff.
    ///
    /// Python maps two exception classes here:
    /// - `asyncio.CancelledError` → re-raise (graceful shutdown)
    /// - `Exception` → retry / log-and-give-up
    ///
    /// In Tokio there is no `CancelledError`. Aborting/dropping this future cancels
    /// any `.await` (including `send_once` and the backoff sleep) automatically, so
    /// cancellation already "propagates" without an explicit branch. Delivery failures
    /// are the `Err` arm of `Result` — the analogue of `except Exception`.
    async fn send_with_retry(channel: &dyn BaseChannel, msg: OutboundMessage, max_attempts: u8) {
        for attempt in 0..max_attempts {
            match Self::send_once(channel, msg.clone()).await {
                Ok(()) => return,
                Err(e) => {
                    if attempt + 1 >= max_attempts {
                        log::error!(
                            "Failed to send to {} after {} attempts: {}",
                            msg.channel,
                            max_attempts,
                            e
                        );
                        return;
                    }
                    let delay_secs = SEND_RETRY_DELAYS_SECS
                        [usize::from(attempt).min(SEND_RETRY_DELAYS_SECS.len() - 1)];
                    log::warn!(
                        "Send to {} failed (attempt {}/{}): {}, retrying in {}s",
                        msg.channel,
                        attempt + 1,
                        max_attempts,
                        e,
                        delay_secs
                    );
                    // Cancelled if the dispatch task is aborted — same as re-raising
                    // CancelledError around asyncio.sleep in Python.
                    tokio::time::sleep(Duration::from_secs(delay_secs)).await;
                }
            }
        }
    }

    /// Send one outbound message without retry policy.
    ///
    /// `_streamed` marks the final "full content" message of a turn that has
    /// already been delivered incrementally via `_stream_delta`/`_stream_end`.
    /// It's only safe to skip that final send when the channel actually
    /// implements delta delivery — otherwise the recipient would never see
    /// the content at all (e.g. if streaming was requested but the channel
    /// silently doesn't support it, or config/state drifts).
    async fn send_once(channel: &dyn BaseChannel, msg: OutboundMessage) -> Result<(), String> {
        if msg.metadata.get("_stream_delta").is_some()
            || msg.metadata.get("_stream_end").is_some()
        {
            channel
                .send_delta(
                    msg.chat_id.as_str(),
                    msg.content.as_str(),
                    Some(msg.metadata.clone()),
                )
                .await
        } else if msg.metadata.get("_streamed").is_none() || !channel.implements_send_delta() {
            channel.send(msg).await
        } else {
            Ok(())
        }
    }

    /// Gets a single channel by name.
    pub fn get_channel(&self, name: &str) -> Option<Arc<dyn BaseChannel>> {
        self.channels.get(name).map(Arc::clone)
    }

    /// Get status of all channels: `{name: {"enabled": true, "running": bool}}`.
    pub fn get_status(&self) -> HashMap<String, serde_json::Value> {
        let mut output = HashMap::new();
        for (name, channel) in &self.channels {
            output.insert(
                name.clone(),
                serde_json::json!({
                    "enabled": true,
                    "running": channel.is_running(),
                }),
            );
        }
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::ChannelsConfig;
    use serde_json::json;

    fn config_with_email(enabled: bool) -> Config {
        let mut config = Config::default();
        config.channels.extra.insert(
            "email".to_string(),
            json!({
                "enabled": enabled,
                "consentGranted": true,
            }),
        );
        config.providers.groq.api_key = "test-groq-key".to_string();
        config.providers.openai.api_key = "test-openai-key".to_string();
        config
    }

    #[test]
    fn resolve_transcription_key_groq_and_openai() {
        let config = config_with_email(false);
        assert_eq!(
            ChannelManager::resolve_transcription_key(&config, "groq"),
            "test-groq-key"
        );
        assert_eq!(
            ChannelManager::resolve_transcription_key(&config, "openai"),
            "test-openai-key"
        );
    }

    #[test]
    fn resolve_transcription_key_unknown_returns_empty() {
        let config = config_with_email(false);
        assert_eq!(
            ChannelManager::resolve_transcription_key(&config, "whisper-local"),
            ""
        );
    }

    #[test]
    fn init_channels_skips_disabled_email() {
        let config = config_with_email(false);
        let bus = Arc::new(MessageBus::new());
        let channels = ChannelManager::init_channels(&config, bus);
        assert!(channels.is_empty());
    }

    #[test]
    fn init_channels_registers_enabled_email_with_transcription_key() {
        let config = config_with_email(true);
        let bus = Arc::new(MessageBus::new());
        let channels = ChannelManager::init_channels(&config, Arc::clone(&bus));
        assert!(channels.contains_key("email"));
        assert_eq!(
            channels.get("email").unwrap().transcription_api_key(),
            "test-groq-key"
        );
    }

    #[test]
    fn new_attaches_shared_bus() {
        let config = config_with_email(true);
        let bus = Arc::new(MessageBus::new());
        let manager = ChannelManager::new(config, Arc::clone(&bus));
        // Same Arc allocation (pointer equality).
        assert!(Arc::ptr_eq(&manager.bus, &bus));
        assert_eq!(
            manager.channels.get("email").unwrap().bus() as *const _,
            bus.as_ref() as *const _
        );
    }

    #[test]
    fn validate_allow_from_ok_with_default_star() {
        let config = config_with_email(true);
        let bus = Arc::new(MessageBus::new());
        let channels = ChannelManager::init_channels(&config, bus);
        assert!(ChannelManager::validate_allow_from(&channels).is_ok());
    }

    #[test]
    fn validate_allow_from_errs_when_empty() {
        let mut config = config_with_email(true);
        config.channels.allow_from.clear();
        let bus = Arc::new(MessageBus::new());
        let channels = ChannelManager::init_channels(&config, bus);
        assert_eq!(
            ChannelManager::validate_allow_from(&channels),
            Err("email".to_string())
        );
    }

    #[test]
    fn validate_allow_from_ok_when_no_channels() {
        let channels = HashMap::new();
        assert!(ChannelManager::validate_allow_from(&channels).is_ok());
    }

    #[tokio::test]
    async fn start_channel_missing_is_noop() {
        let config = config_with_email(false);
        let bus = Arc::new(MessageBus::new());
        let manager = ChannelManager::new(config, bus);
        manager.start_channel("does-not-exist").await;
        assert!(manager.channels.is_empty());
    }

    #[test]
    fn new_with_disabled_channels_succeeds() {
        let config = config_with_email(false);
        let bus = Arc::new(MessageBus::new());
        let manager = ChannelManager::new(config, bus);
        assert!(manager.channels.is_empty());
        assert_eq!(manager.config.channels.transcription_provider, "groq");
    }

    #[test]
    fn openai_transcription_provider_sets_openai_key() {
        let mut config = config_with_email(true);
        config.channels.transcription_provider = "openai".to_string();
        let bus = Arc::new(MessageBus::new());
        let channels = ChannelManager::init_channels(&config, bus);
        assert_eq!(
            channels.get("email").unwrap().transcription_api_key(),
            "test-openai-key"
        );
    }

    #[test]
    fn channels_config_default_allow_from_is_star() {
        // Documents the assumption validate_allow_from relies on.
        let cfg = ChannelsConfig::default();
        assert_eq!(cfg.allow_from, vec!["*".to_string()]);
    }

    // --- coalesce_stream_deltas / send_once ---
    // Mirrors upstream Python `test_channel_manager_delta_coalescing.py`.

    fn outbound_msg(
        channel: &str,
        chat_id: &str,
        content: &str,
        metadata: HashMap<String, serde_json::Value>,
    ) -> OutboundMessage {
        OutboundMessage {
            channel: channel.to_string(),
            chat_id: chat_id.to_string(),
            content: content.to_string(),
            reply_to: None,
            media: Vec::new(),
            metadata,
        }
    }

    fn delta_meta() -> HashMap<String, serde_json::Value> {
        let mut meta = HashMap::new();
        meta.insert("_stream_delta".to_string(), json!(true));
        meta
    }

    #[test]
    fn coalesce_stream_deltas_merges_multiple_deltas() {
        let bus = MessageBus::new();
        for text in ["Hello", " ", "world", "!"] {
            bus.publish_outbound(outbound_msg("mock", "chat1", text, delta_meta()))
                .unwrap();
        }

        let first = bus.outbound.try_recv().unwrap();
        let (merged, pending) = ChannelManager::coalesce_stream_deltas(&bus, &first);

        assert_eq!(merged.content, "Hello world!");
        assert!(merged.metadata.get("_stream_delta").is_some());
        assert!(pending.is_empty());
    }

    #[test]
    fn coalesce_stream_deltas_stops_at_different_chat_id() {
        let bus = MessageBus::new();
        bus.publish_outbound(outbound_msg("mock", "chat1", "Hello", delta_meta()))
            .unwrap();
        bus.publish_outbound(outbound_msg("mock", "chat2", "World", delta_meta()))
            .unwrap();

        let first = bus.outbound.try_recv().unwrap();
        let (merged, pending) = ChannelManager::coalesce_stream_deltas(&bus, &first);

        assert_eq!(merged.content, "Hello");
        assert_eq!(merged.chat_id, "chat1");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].chat_id, "chat2");
        assert_eq!(pending[0].content, "World");
    }

    #[test]
    fn coalesce_stream_deltas_terminates_on_stream_end_flag() {
        let bus = MessageBus::new();
        bus.publish_outbound(outbound_msg("mock", "chat1", "Hello", delta_meta()))
            .unwrap();
        let mut end_meta = delta_meta();
        end_meta.insert("_stream_end".to_string(), json!(true));
        bus.publish_outbound(outbound_msg("mock", "chat1", " world", end_meta))
            .unwrap();

        let first = bus.outbound.try_recv().unwrap();
        let (merged, pending) = ChannelManager::coalesce_stream_deltas(&bus, &first);

        assert_eq!(merged.content, "Hello world");
        assert_eq!(merged.metadata.get("_stream_end"), Some(&json!(true)));
        assert!(pending.is_empty());
    }

    #[test]
    fn coalesce_stream_deltas_stops_at_stream_end_without_delta_flag() {
        // A `_stream_end`-only message (no `_stream_delta`) is a boundary, not
        // mergeable content — it must be handed back via `pending` untouched,
        // and coalescing must not look further into the queue.
        let bus = MessageBus::new();
        bus.publish_outbound(outbound_msg("mock", "chat1", "Hello", delta_meta()))
            .unwrap();
        let mut end_only_meta = HashMap::new();
        end_only_meta.insert("_stream_end".to_string(), json!(true));
        bus.publish_outbound(outbound_msg("mock", "chat1", "", end_only_meta))
            .unwrap();
        bus.publish_outbound(outbound_msg("mock", "chat1", "world", delta_meta()))
            .unwrap();

        let first = bus.outbound.try_recv().unwrap();
        let (merged, pending) = ChannelManager::coalesce_stream_deltas(&bus, &first);

        assert_eq!(merged.content, "Hello");
        assert!(merged.metadata.get("_stream_end").is_none());
        assert_eq!(pending.len(), 1);
        assert!(pending[0].metadata.get("_stream_end").is_some());
    }

    /// Minimal channel double for exercising `send_once` branching without a
    /// real transport.
    struct MockChannel {
        bus: Arc<MessageBus>,
        config: ChannelsConfig,
        implements_delta: bool,
        send_calls: std::sync::Mutex<Vec<OutboundMessage>>,
        send_delta_calls: std::sync::Mutex<Vec<(String, String)>>,
    }

    impl MockChannel {
        fn new(bus: Arc<MessageBus>, implements_delta: bool) -> Self {
            Self {
                bus,
                config: ChannelsConfig::default(),
                implements_delta,
                send_calls: std::sync::Mutex::new(Vec::new()),
                send_delta_calls: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl BaseChannel for MockChannel {
        fn running(&self) -> bool {
            false
        }

        fn bus(&self) -> &MessageBus {
            &self.bus
        }

        fn config(&self) -> &ChannelsConfig {
            &self.config
        }

        fn set_transcription_api_key(&mut self, _key: String) {}

        async fn start(&self) {}

        async fn stop(&self) {}

        async fn send(&self, msg: OutboundMessage) -> Result<(), String> {
            self.send_calls.lock().unwrap().push(msg);
            Ok(())
        }

        async fn send_delta(
            &self,
            chat_id: &str,
            delta: &str,
            _metadata: Option<HashMap<String, serde_json::Value>>,
        ) -> Result<(), String> {
            self.send_delta_calls
                .lock()
                .unwrap()
                .push((chat_id.to_string(), delta.to_string()));
            Ok(())
        }

        fn implements_send_delta(&self) -> bool {
            self.implements_delta
        }
    }

    #[tokio::test]
    async fn send_once_routes_delta_metadata_to_send_delta() {
        let bus = Arc::new(MessageBus::new());
        let channel = MockChannel::new(Arc::clone(&bus), true);
        let msg = outbound_msg("mock", "chat1", "chunk", delta_meta());

        let result = ChannelManager::send_once(&channel, msg).await;

        assert!(result.is_ok());
        assert_eq!(channel.send_delta_calls.lock().unwrap().len(), 1);
        assert!(channel.send_calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn send_once_skips_streamed_message_when_channel_supports_delta() {
        let bus = Arc::new(MessageBus::new());
        let channel = MockChannel::new(Arc::clone(&bus), true);
        let mut meta = HashMap::new();
        meta.insert("_streamed".to_string(), json!(true));
        let msg = outbound_msg("mock", "chat1", "final content", meta);

        let result = ChannelManager::send_once(&channel, msg).await;

        assert!(result.is_ok());
        assert!(channel.send_calls.lock().unwrap().is_empty());
        assert!(channel.send_delta_calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn send_once_falls_back_to_send_when_channel_does_not_support_delta() {
        // Guards against silently dropping content: a `_streamed` message
        // must still go through `send` if the channel can't actually deliver
        // deltas, even though streaming was nominally requested upstream.
        let bus = Arc::new(MessageBus::new());
        let channel = MockChannel::new(Arc::clone(&bus), false);
        let mut meta = HashMap::new();
        meta.insert("_streamed".to_string(), json!(true));
        let msg = outbound_msg("mock", "chat1", "final content", meta);

        let result = ChannelManager::send_once(&channel, msg).await;

        assert!(result.is_ok());
        assert_eq!(channel.send_calls.lock().unwrap().len(), 1);
        assert!(channel.send_delta_calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn send_once_sends_plain_message_normally() {
        let bus = Arc::new(MessageBus::new());
        let channel = MockChannel::new(Arc::clone(&bus), true);
        let msg = outbound_msg("mock", "chat1", "hello", HashMap::new());

        let result = ChannelManager::send_once(&channel, msg).await;

        assert!(result.is_ok());
        assert_eq!(channel.send_calls.lock().unwrap().len(), 1);
        assert!(channel.send_delta_calls.lock().unwrap().is_empty());
    }

    #[test]
    fn get_status_reports_all_channels() {
        let config = config_with_email(true);
        let bus = Arc::new(MessageBus::new());
        let manager = ChannelManager::new(config, bus);
        let status = manager.get_status();
        assert_eq!(
            status.get("email"),
            Some(&json!({"enabled": true, "running": false}))
        );
    }

    #[test]
    fn get_channel_returns_registered_channel() {
        let config = config_with_email(true);
        let bus = Arc::new(MessageBus::new());
        let manager = ChannelManager::new(config, bus);
        assert!(manager.get_channel("email").is_some());
        assert!(manager.get_channel("missing").is_none());
    }

    #[tokio::test]
    async fn start_all_then_stop_all_do_not_require_exclusive_borrow() {
        // Regression guard for the lifecycle fix: both methods must be
        // callable through a shared reference so a caller can hold the
        // manager behind an `Arc` and stop it while start_all is running.
        // Email is enabled (so the dispatcher task actually gets spawned and
        // `stop_all` has a real `dispatch_task` to abort), but IMAP/SMTP are
        // unconfigured so `EmailChannel::start` returns almost immediately
        // instead of trying real network I/O.
        let config = config_with_email(true);
        let bus = Arc::new(MessageBus::new());
        let manager = Arc::new(ChannelManager::new(config, bus));

        let manager_for_start = Arc::clone(&manager);
        let start_handle = tokio::spawn(async move {
            manager_for_start.start_all().await;
        });

        manager.stop_all().await;
        start_handle.await.unwrap();
    }
}
