pub mod agent;
pub mod api;
pub mod bus;
pub mod channels;
pub mod cli;
pub mod command;
pub mod config;
pub mod cron;
pub mod heartbeat;
pub mod pairing;
pub mod providers;
pub mod runtime_context;
pub mod security;
pub mod session;
pub mod utils;

// src/lib.rs (or a small version.rs)
pub const PKG_VERSION: &str = env!("CARGO_PKG_VERSION");
