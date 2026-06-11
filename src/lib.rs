pub mod agent;
pub mod bus;
pub mod config;
pub mod providers;
pub mod utils;
pub mod security;
pub mod session;
pub mod cron;
pub mod command;
pub mod cli;

// src/lib.rs (or a small version.rs)
pub const PKG_VERSION: &str = env!("CARGO_PKG_VERSION");