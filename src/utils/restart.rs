use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

pub const RESTART_NOTIFY_CHANNEL_ENV: &str = "RUST_BOT_RESTART_NOTIFY_CHANNEL";
pub const RESTART_NOTIFY_CHAT_ID_ENV: &str = "RUST_BOT_RESTART_NOTIFY_CHAT_ID";
pub const RESTART_STARTED_AT_ENV: &str = "RUST_BOT_RESTART_STARTED_AT";

/// Restart the process, passing notice env vars to the new instance.
pub fn restart_with_notice(channel: &str, chat_id: &str) -> std::io::Result<()> {
    let started_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
        .to_string();
    let exe = std::env::current_exe()?;
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut cmd = Command::new(exe);
    cmd.args(args)
        .env(RESTART_NOTIFY_CHANNEL_ENV, channel)
        .env(RESTART_NOTIFY_CHAT_ID_ENV, chat_id)
        .env(RESTART_STARTED_AT_ENV, started_at);

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.exec();
        return Err(std::io::Error::last_os_error());
    }

    #[cfg(not(unix))]
    {
        cmd.spawn()?;
        std::process::exit(0);
    }
}
