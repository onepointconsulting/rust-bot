use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

pub const RESTART_NOTIFY_CHANNEL_ENV: &str = "RUST_BOT_RESTART_NOTIFY_CHANNEL";
pub const RESTART_NOTIFY_CHAT_ID_ENV: &str = "RUST_BOT_RESTART_NOTIFY_CHAT_ID";
pub const RESTART_STARTED_AT_ENV: &str = "RUST_BOT_RESTART_STARTED_AT";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestartNotice {
    pub channel: String,
    pub chat_id: String,
    pub started_at_raw: String,
}

fn pop_env_var(key: &str) -> String {
    let value = std::env::var(key).unwrap_or_default();
    // SAFETY: restart notice env vars are consumed once at process startup.
    unsafe { std::env::remove_var(key) };
    value.trim().to_string()
}

/// Read and clear restart notice env values once for this process.
pub fn consume_restart_notice_from_env() -> Option<RestartNotice> {
    let channel = pop_env_var(RESTART_NOTIFY_CHANNEL_ENV);
    let chat_id = pop_env_var(RESTART_NOTIFY_CHAT_ID_ENV);
    let started_at_raw = pop_env_var(RESTART_STARTED_AT_ENV);
    if channel.is_empty() || chat_id.is_empty() {
        return None;
    }
    Some(RestartNotice {
        channel,
        chat_id,
        started_at_raw,
    })
}

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
        let _ = cmd.exec();
        return Err(std::io::Error::last_os_error());
    }

    #[cfg(not(unix))]
    {
        cmd.spawn()?;
        crate::utils::exit_codes::exit(crate::utils::exit_codes::SUCCESS);
    }
}

/// Return True when a restart notice should be shown in this CLI session.
pub fn should_show_cli_restart_notice(notice: RestartNotice, session_id: &str) -> bool {
    if notice.channel != "cli" {
        return false;
    }
    let cli_chat_id = session_id
        .split_once(':')
        .map(|(_, chat_id)| chat_id)
        .unwrap_or(session_id);
    notice.chat_id.is_empty() || notice.chat_id == cli_chat_id
}

/// Build restart completion text and include elapsed time when available.
pub fn format_restart_completed_message(started_at_raw: &str) -> String {
    let mut elapsed_suffix = String::new();
    if !started_at_raw.is_empty() {
        if let Ok(started_at) = started_at_raw.parse::<f64>() {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0);
            let elapsed_s = (now - started_at).max(0.0);
            elapsed_suffix = format!(" in {elapsed_s:.1}s");
        }
    }
    format!("Restart completed{elapsed_suffix}.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_restart_completed_message_includes_elapsed_seconds() {
        let started_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs_f64() - 2.0)
            .unwrap();
        let message = format_restart_completed_message(&started_at.to_string());
        assert!(message.starts_with("Restart completed in "));
        assert!(message.ends_with("s."));
    }

    #[test]
    fn format_restart_completed_message_without_timestamp() {
        assert_eq!(format_restart_completed_message(""), "Restart completed.");
    }
}
