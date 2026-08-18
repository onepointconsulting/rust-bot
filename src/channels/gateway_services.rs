use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::{
    channels::websocket::webui::transcript::WebUiTranscriptRecorder, config::paths::get_webui_dir,
    security::WebUIIngressPolicy, session::websocket_turns::WebsocketTurnRegistry,
};

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
        }
    }
}

impl Default for GatewayServices {
    fn default() -> Self {
        Self::new(get_webui_dir())
    }
}
