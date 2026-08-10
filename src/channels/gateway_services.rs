use std::sync::{Arc, Mutex};

use crate::{security::WebUIIngressPolicy, session::websocket_turns::WebsocketTurnRegistry};

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
}

impl Default for GatewayServices {
    fn default() -> Self {
        Self {
            ingress: WebUIIngressPolicy::default(),
            turn_registry: Arc::new(Mutex::new(WebsocketTurnRegistry::default())),
        }
    }
}
