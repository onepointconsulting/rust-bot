//! Process exit codes for the rust-bot CLI and runtime.
//!
//! Keep this module and the **Exit codes** table in `README.md` in sync.

/// Success. Also used after spawning a restarted process on non-Unix platforms.
pub const SUCCESS: i32 = 0;

/// Config or general CLI error (missing config, load failure, API server start failure, etc.).
pub const GENERAL_ERROR: i32 = 1;

/// Invalid / unknown value in `agents.provider`.
pub const INVALID_PROVIDER: i32 = 3;

/// Gmail tool credentials missing (OAuth client secret or token cache path).
pub const GMAIL_CONFIG_ERROR: i32 = 4;

/// A channel has an empty `allowFrom` list (must be `["*"]` or specific user IDs).
pub const CHANNEL_ALLOW_FROM_EMPTY: i32 = 5;

/// Terminate the current process with `code`.
pub fn exit(code: i32) -> ! {
    std::process::exit(code);
}
