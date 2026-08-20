use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::{
    agent::agent_loop::AgentLoop, bus::events::InboundMessage,
    channels::websocket::webui::transcript::WebUiTranscriptRecorder, config::paths::get_webui_dir,
    security::WebUIIngressPolicy, session::websocket_turns::WebsocketTurnRegistry,
};

/// Cancellable handle to a live [`AgentLoop`]'s in-flight work, wired into
/// [`GatewayServices`] by the process that actually owns an `AgentLoop`
/// (`cli::commands::run_gateway`) — `GatewayServices::default()`/`::new`
/// leave it unset, which is exactly what unit tests around the WebSocket
/// channel want: `delete_chat` still tombstones and unlinks the session
/// (see `SessionManager::delete_session`), it just has nothing to abort.
#[derive(Clone)]
pub struct SessionWorkCanceller {
    agent_loop: Arc<AgentLoop>,
}

impl SessionWorkCanceller {
    pub fn new(agent_loop: Arc<AgentLoop>) -> Self {
        Self { agent_loop }
    }

    /// Abort in-flight agent tasks and subagents for `channel:chat_id`,
    /// mirroring `/stop`'s cancellation body (see
    /// [`AgentLoop::abort_session`]). `msg` is a minimal, synthetic
    /// [`InboundMessage`] — there is no real inbound turn behind a
    /// `delete_chat` envelope, just a `channel`/`chat_id` to cancel.
    pub async fn abort(&self, channel: &str, chat_id: &str) {
        let msg = InboundMessage {
            channel: channel.to_string(),
            sender_id: String::new(),
            chat_id: chat_id.to_string(),
            content: String::new(),
            timestamp: chrono::Utc::now(),
            media: Vec::new(),
            metadata: std::collections::HashMap::new(),
            session_key_override: None,
        };
        self.agent_loop.abort_session(&msg).await;
    }
}

#[derive(Clone)]
pub struct GatewayServices {
    pub ingress: WebUIIngressPolicy,
    /// `Arc<Mutex<_>>`, not a bare value: `GatewayServices` itself is
    /// `Arc`-wrapped and shared (via `Arc::clone`) across every connection's
    /// `WsShared` snapshot, so only `&GatewayServices` is ever reachable
    /// through it. Without its own interior mutability, `turn_registry`'s
    /// `&mut self` methods (`start_turn`, `clear_turn_if_current`, ...) could
    /// never be called through that shared reference at all.
    pub turn_registry: Arc<Mutex<WebsocketTurnRegistry>>,
    /// Same `Arc<Mutex<_>>` reasoning as `turn_registry`: `WebUiTranscriptRecorder`'s
    /// `_turn_sequences` bookkeeping needs `&mut self` and must be reachable
    /// through every connection's shared clone.
    pub transcripts: Arc<Mutex<WebUiTranscriptRecorder>>,
    /// `None` until `run_gateway` injects it via [`Self::set_work_canceller`]
    /// — see [`SessionWorkCanceller`]'s doc comment.
    work_canceller: Arc<Mutex<Option<SessionWorkCanceller>>>,
}

impl GatewayServices {
    /// Construct with an explicit transcripts directory. Tests should use
    /// this (with a tempdir) rather than `Default`, which resolves the real
    /// `get_webui_dir()`.
    pub fn new(webui_dir: PathBuf) -> Self {
        Self {
            ingress: WebUIIngressPolicy::default(),
            turn_registry: Arc::new(Mutex::new(WebsocketTurnRegistry::default())),
            transcripts: Arc::new(Mutex::new(WebUiTranscriptRecorder::new(webui_dir))),
            work_canceller: Arc::new(Mutex::new(None)),
        }
    }

    /// Inject the live [`AgentLoop`] this gateway's `delete_chat` handler
    /// should abort in-flight work through. Must run before
    /// `WebSocketChannel::router()` is called for the first time, since
    /// every connection's `WsShared` is a snapshot of this `Arc`-shared
    /// struct's *current* contents at upgrade time — see
    /// [`WebSocketChannel::shared`](crate::channels::websocket::runtime::WebSocketChannel).
    pub fn set_work_canceller(&self, canceller: SessionWorkCanceller) {
        *self
            .work_canceller
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(canceller);
    }

    /// The injected canceller, if any. `None` in every test fixture that
    /// builds `GatewayServices` directly (no real `AgentLoop` around).
    pub fn work_canceller(&self) -> Option<SessionWorkCanceller> {
        self.work_canceller
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

impl Default for GatewayServices {
    fn default() -> Self {
        Self::new(get_webui_dir())
    }
}
