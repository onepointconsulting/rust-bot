//! Reading, checking and saving the raw text of `config.json` for the WebUI's Settings
//! dialog.
//!
//! The dialog edits the file as text, so the user's own layout and key order survive a
//! save. Everything here works on a path and plain strings (no websocket types), which
//! keeps it unit-testable on its own; the envelope handlers in `runtime.rs` add the
//! access checks and the replies.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use crate::config::schema::{Config, validate_model_presets};
use crate::utils::fs::write_atomic;

/// Largest configuration text the dialog will save. A real `config.json` is a few
/// kilobytes; the limit only stops a runaway paste from being written to disk.
pub const MAX_CONFIG_TEXT_BYTES: usize = 1024 * 1024;

/// The text of the configuration file at `path`. When the file does not exist yet, the
/// default configuration as pretty-printed JSON, so the dialog always has something to
/// show and edit.
pub fn read_config_text(path: &Path) -> io::Result<String> {
    if path.is_file() {
        return fs::read_to_string(path);
    }
    serde_json::to_string_pretty(&Config::default()).map_err(io::Error::other)
}

/// Check that `text` is a configuration the bot can start with: valid JSON, matching the
/// configuration schema, and passing the model preset checks `load_config` applies.
/// Returns a message for the user when it is not. A file that fails these checks would
/// make the bot panic on its next start, so it must never be saved.
pub fn validate_config_text(text: &str) -> Result<(), String> {
    if text.len() > MAX_CONFIG_TEXT_BYTES {
        return Err(format!(
            "The configuration is larger than {} KB.",
            MAX_CONFIG_TEXT_BYTES / 1024
        ));
    }
    let json: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("Invalid JSON: {e}"))?;
    let config: Config =
        serde_json::from_value(json).map_err(|e| format!("Invalid configuration: {e}"))?;
    validate_model_presets(&config).map_err(|e| format!("Invalid configuration: {e}"))
}

/// Path of the backup for the file at `path`, taken at `now`: the same folder, named
/// `config_<iso_timestamp>.json.bck`. The timestamp is ISO 8601 in UTC with the colons
/// left out (`2026-10-17T14:03:22Z` becomes `2026-10-17T140322Z`), because Windows
/// does not allow `:` in file names.
pub fn backup_path_for(path: &Path, now: DateTime<Utc>) -> PathBuf {
    let folder = path.parent().unwrap_or_else(|| Path::new("."));
    folder.join(format!("config_{}.json.bck", now.format("%Y-%m-%dT%H%M%SZ")))
}

/// Copy the file at `path` to its backup path. Returns the backup's path, or `None`
/// when there is nothing to back up: the file does not exist yet, or already holds
/// exactly `new_text` (a save that changes nothing needs no backup).
fn back_up_existing(
    path: &Path,
    new_text: &str,
    now: DateTime<Utc>,
) -> Result<Option<PathBuf>, String> {
    if !path.is_file() {
        return Ok(None);
    }
    let existing = fs::read(path).map_err(|e| format!("Could not read the current file: {e}"))?;
    if existing == new_text.as_bytes() {
        return Ok(None);
    }
    let backup = backup_path_for(path, now);
    write_atomic(&backup, &existing).map_err(|e| format!("Could not write the backup: {e}"))?;
    Ok(Some(backup))
}

/// Validate `text` and, only if it passes, replace the file at `path` with it, first
/// saving the current file as a backup (see [`backup_path_for`]) so the change can be
/// reverted by hand. Returns the backup's path, if one was made. On any failure,
/// including a failed backup, the existing file is left untouched.
pub fn save_config_text(path: &Path, text: &str) -> Result<Option<PathBuf>, String> {
    validate_config_text(text)?;
    let backup = back_up_existing(path, text, Utc::now())?;
    write_atomic(path, text.as_bytes()).map_err(|e| format!("Could not write the file: {e}"))?;
    Ok(backup)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_returns_the_file_text_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        fs::write(&path, "{ \"agents\":   {} }").unwrap();

        assert_eq!(read_config_text(&path).unwrap(), "{ \"agents\":   {} }");
    }

    #[test]
    fn read_returns_the_default_configuration_when_the_file_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let text = read_config_text(&dir.path().join("missing.json")).unwrap();

        assert!(validate_config_text(&text).is_ok());
    }

    #[test]
    fn validate_accepts_an_empty_object() {
        assert!(validate_config_text("{}").is_ok());
    }

    #[test]
    fn validate_rejects_malformed_json() {
        let error = validate_config_text("{ \"agents\": ").unwrap_err();
        assert!(error.starts_with("Invalid JSON"), "{error}");
    }

    #[test]
    fn validate_rejects_a_value_of_the_wrong_shape() {
        let error = validate_config_text("[1, 2]").unwrap_err();
        assert!(error.starts_with("Invalid configuration"), "{error}");
    }

    #[test]
    fn validate_rejects_oversized_text() {
        let text = " ".repeat(MAX_CONFIG_TEXT_BYTES + 1);
        assert!(validate_config_text(&text).unwrap_err().contains("larger"));
    }

    #[test]
    fn save_writes_valid_text_exactly_as_given() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");

        save_config_text(&path, "{\n  \"agents\": {}\n}").unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), "{\n  \"agents\": {}\n}");
    }

    #[test]
    fn save_keeps_the_existing_file_when_the_text_is_invalid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        fs::write(&path, "{}").unwrap();

        assert!(save_config_text(&path, "not json").is_err());

        assert_eq!(fs::read_to_string(&path).unwrap(), "{}");
    }

    fn backups_in(dir: &Path) -> Vec<PathBuf> {
        fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "bck"))
            .collect()
    }

    #[test]
    fn backup_path_uses_an_iso_timestamp_without_colons() {
        let now = "2026-10-17T14:03:22Z".parse::<DateTime<Utc>>().unwrap();
        let path = Path::new("/home/u/.rust-bot/config.json");

        assert_eq!(
            backup_path_for(path, now),
            Path::new("/home/u/.rust-bot").join("config_2026-10-17T140322Z.json.bck")
        );
    }

    #[test]
    fn save_backs_up_the_previous_file_next_to_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        fs::write(&path, "{ \"agents\": {} }").unwrap();

        let backup = save_config_text(&path, "{}").unwrap().expect("a backup");

        assert_eq!(backup.parent(), Some(dir.path()));
        assert_eq!(fs::read_to_string(&backup).unwrap(), "{ \"agents\": {} }");
        assert_eq!(fs::read_to_string(&path).unwrap(), "{}");
    }

    #[test]
    fn save_makes_no_backup_when_the_file_does_not_exist_yet() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");

        assert_eq!(save_config_text(&path, "{}").unwrap(), None);
        assert!(backups_in(dir.path()).is_empty());
    }

    #[test]
    fn save_makes_no_backup_when_nothing_changed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        fs::write(&path, "{}").unwrap();

        assert_eq!(save_config_text(&path, "{}").unwrap(), None);
        assert!(backups_in(dir.path()).is_empty());
    }

    #[test]
    fn save_makes_no_backup_when_the_text_is_invalid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        fs::write(&path, "{}").unwrap();

        assert!(save_config_text(&path, "not json").is_err());
        assert!(backups_in(dir.path()).is_empty());
    }
}
