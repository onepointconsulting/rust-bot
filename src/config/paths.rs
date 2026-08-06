use std::path::PathBuf;

use crate::{config::loader::get_config_path, utils::helpers::{ensure_dir, expand_tilde_path}};

/// Return the instance-level runtime data directory.
pub fn get_data_dir() -> PathBuf {
    let config_path = get_config_path();
    let data_dir = config_path.parent().unwrap_or_else(|| {
        panic!(
            "Config path '{}' has no parent directory. \
             Ensure the config path points to a file, not a filesystem root.",
            config_path.display()
        )
    });
    ensure_dir(data_dir)
}

/// Return the directory for WebUI-only persisted display threads (JSON).
pub fn get_webui_dir() -> PathBuf {
    get_runtime_subdir("webui")
}

/// Return a named runtime subdirectory under the instance data dir.
pub fn get_runtime_subdir(name: &str) -> PathBuf {
    ensure_dir(get_data_dir().join(name))
}

/// Return the media directory, optionally namespaced per channel.
pub fn get_media_dir(channel_option: Option<&str>) -> PathBuf {
    let base = get_runtime_subdir("media");
    if let Some(channel) = channel_option {
        ensure_dir(base.join(channel))
    } else {
        base
    }
}

/// Return the cron storage directory.
pub fn get_cron_dir() -> PathBuf {
    get_runtime_subdir("cron")
}

/// Returns `<data_dir>/logs/`, creating it if it does not exist.
pub fn get_logs_dir() -> PathBuf {
    get_runtime_subdir("logs")
}

/// Resolve and ensure the agent workspace path.
pub fn get_workspace_path(workspace: Option<&str>) -> PathBuf {
    let path = if let Some(workspace) = workspace {
        PathBuf::from(expand_tilde_path(workspace).as_ref())
    } else {
        home::home_dir().unwrap_or_else(|| {
            panic!("Home directory not found. Please set the HOME environment variable.")
        }).join(".rust-bot/workspace")
    };
    ensure_dir(path)
}

/// Return whether a workspace path resolves to the default workspace.
pub fn is_default_workspace(workspace: Option<&str>) -> bool {
    let default = home::home_dir()
        .unwrap_or_else(|| panic!("Home directory not found. Please set the HOME environment variable."))
        .join(".rust-bot/workspace");

    let current = match workspace {
        Some(ws) => PathBuf::from(expand_tilde_path(ws).as_ref()),
        None => default.clone(),
    };

    // canonicalize requires the path to exist; use clean comparison as fallback.
    let resolve = |p: PathBuf| std::fs::canonicalize(&p).unwrap_or(p);
    resolve(current) == resolve(default)
}

fn rust_bot_home() -> PathBuf {
    home::home_dir()
        .unwrap_or_else(|| panic!("Home directory not found. Please set the HOME environment variable."))
        .join(".rust-bot")
}

/// Return the shared CLI history file path.
pub fn get_cli_history_path() -> PathBuf {
    rust_bot_home().join("history").join("cli_history")
}

/// Return the shared WhatsApp bridge installation directory.
pub fn get_bridge_install_dir() -> PathBuf {
    rust_bot_home().join("bridge")
}

/// Return the legacy global session directory used for migration fallback.
pub fn get_legacy_sessions_dir() -> PathBuf {
    rust_bot_home().join("sessions")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::loader::set_config_path;
    use tempfile::tempdir;

    /// Set up a temporary config file and register it as the current config path.
    /// Returns the tempdir so it lives for the duration of the test.
    fn setup_temp_config() -> tempfile::TempDir {
        let dir = tempdir().unwrap();
        let config_file = dir.path().join("config.json");
        std::fs::write(&config_file, "{}").unwrap();
        set_config_path(config_file);
        dir
    }

    // ── get_data_dir / get_runtime_subdir ─────────────────────────────────────

    #[test]
    fn test_get_data_dir_returns_parent_of_config() {
        let _dir = setup_temp_config();
        let data_dir = get_data_dir();
        assert!(data_dir.is_dir());
        // data_dir should be the directory that contains config.json
        assert!(data_dir.join("config.json").exists());
    }

    #[test]
    fn test_get_runtime_subdir_creates_directory() {
        let _dir = setup_temp_config();
        let sub = get_runtime_subdir("test_sub");
        assert!(sub.is_dir());
        assert_eq!(sub.file_name().unwrap(), "test_sub");
    }

    // ── get_media_dir ─────────────────────────────────────────────────────────

    #[test]
    fn test_get_media_dir_without_channel() {
        let _dir = setup_temp_config();
        let media = get_media_dir(None);
        assert!(media.is_dir());
        assert_eq!(media.file_name().unwrap(), "media");
    }

    #[test]
    fn test_get_media_dir_with_channel() {
        let _dir = setup_temp_config();
        let media = get_media_dir(Some("telegram"));
        assert!(media.is_dir());
        assert_eq!(media.file_name().unwrap(), "telegram");
        assert_eq!(media.parent().unwrap().file_name().unwrap(), "media");
    }

    // ── get_webui_dir ─────────────────────────────────────────────────────────

    #[test]
    fn test_get_webui_dir_creates_directory() {
        let _dir = setup_temp_config();
        let webui = get_webui_dir();
        assert!(webui.is_dir());
        assert_eq!(webui.file_name().unwrap(), "webui");
    }

    // ── get_cron_dir / get_logs_dir ───────────────────────────────────────────

    #[test]
    fn test_get_cron_dir_creates_directory() {
        let _dir = setup_temp_config();
        let cron = get_cron_dir();
        assert!(cron.is_dir());
        assert_eq!(cron.file_name().unwrap(), "cron");
    }

    #[test]
    fn test_get_logs_dir_creates_directory() {
        let _dir = setup_temp_config();
        let logs = get_logs_dir();
        assert!(logs.is_dir());
        assert_eq!(logs.file_name().unwrap(), "logs");
    }

    // ── get_workspace_path ────────────────────────────────────────────────────

    #[test]
    fn test_get_workspace_path_explicit() {
        let dir = tempdir().unwrap();
        let ws = dir.path().to_str().unwrap();
        let result = get_workspace_path(Some(ws));
        assert_eq!(result, dir.path());
    }

    #[test]
    fn test_get_workspace_path_tilde() {
        let result = get_workspace_path(Some("~/.rust-bot/workspace"));
        let expected = home::home_dir().unwrap().join(".rust-bot/workspace");
        assert_eq!(result, expected);
    }

    #[test]
    fn test_get_workspace_path_none_uses_default() {
        let result = get_workspace_path(None);
        let expected = home::home_dir().unwrap().join(".rust-bot/workspace");
        assert_eq!(result, expected);
    }

    // ── is_default_workspace ─────────────────────────────────────────────────

    #[test]
    fn test_is_default_workspace_none_is_default() {
        assert!(is_default_workspace(None));
    }

    #[test]
    fn test_is_default_workspace_tilde_path_is_default() {
        assert!(is_default_workspace(Some("~/.rust-bot/workspace")));
    }

    #[test]
    fn test_is_default_workspace_absolute_default_path() {
        let default = home::home_dir().unwrap().join(".rust-bot/workspace");
        let path = default.to_str().unwrap().to_string();
        assert!(is_default_workspace(Some(&path)));
    }

    #[test]
    fn test_is_default_workspace_different_path_is_not_default() {
        assert!(!is_default_workspace(Some("/tmp/my_workspace")));
    }

    // ── static home-relative paths ────────────────────────────────────────────

    #[test]
    fn test_get_cli_history_path() {
        let path = get_cli_history_path();
        let home = home::home_dir().unwrap();
        assert_eq!(path, home.join(".rust-bot/history/cli_history"));
    }

    #[test]
    fn test_get_bridge_install_dir() {
        let path = get_bridge_install_dir();
        let home = home::home_dir().unwrap();
        assert_eq!(path, home.join(".rust-bot/bridge"));
    }

    #[test]
    fn test_get_legacy_sessions_dir() {
        let path = get_legacy_sessions_dir();
        let home = home::home_dir().unwrap();
        assert_eq!(path, home.join(".rust-bot/sessions"));
    }
}