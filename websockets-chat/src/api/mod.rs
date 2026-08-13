//! Transport layer for talking to the gateway.
//!
//! `login()`/`ApiError` are reused unchanged from `chat_ui::api` — both apps
//! authenticate against the identical `POST /v1/login` shape, so there is
//! nothing app-specific to add here. This module's own contribution is
//! [`ws_client`], the WebSocket transport that is specific to this app (unlike
//! `web-chat`, which is REST-only end to end).

mod ws_client;

pub use ws_client::*;
