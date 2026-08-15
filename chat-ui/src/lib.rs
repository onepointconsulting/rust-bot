//! Shared Leptos component library for rust-bot's chat frontends.
//!
//! Holds the pieces identical across `web-chat` (REST) and `websockets-chat`
//! (WebSocket streaming): the login form, message composer, markdown
//! rendering, a generalized message bubble, the core domain models, and the
//! shared `login()` REST call.

pub mod api;
pub mod components;
pub mod markdown;
pub mod models;
