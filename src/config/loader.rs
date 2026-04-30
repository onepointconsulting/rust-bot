use std::fs::{self, File};
use std::io::{BufReader, Write};
use std::path::PathBuf;
use std::sync::OnceLock;


use crate::config::schema::Config;
use crate::security::network::configure_ssrf_whitelist;
use crate::utils::helpers::expand_tilde_path;

static CURRENT_CONFIG_PATH: OnceLock<PathBuf> = OnceLock::new();

/// Set the current config path. Can only be called once; subsequent calls are ignored.
pub fn set_config_path(path: PathBuf) {
    let _ = CURRENT_CONFIG_PATH.set(path);
}

/// Get the current config path, if it has been set.
pub fn get_config_path() -> PathBuf {
    if let Some(current_config_path) = CURRENT_CONFIG_PATH.get() {
        return current_config_path.clone();
    }
    return PathBuf::from(expand_tilde_path("~/.rust-bot/config.json").as_ref());
}

/// Load configuration from a file, or create a default configuration if
/// the file does not exist.
///
/// # Arguments
///
/// * `config_path` - Optional path to the config file. Uses the default
///   path if not provided.
///
/// # Returns
///
/// The loaded (or default) configuration object.
pub fn load_config(path_option: Option<PathBuf>) -> Config {
    let path = path_option.unwrap_or_else(|| get_config_path());
    let mut config = Config::default();
    if path.exists() && path.is_file() {
        let file = File::open(&path).unwrap_or_else(|e| {
            panic!("Failed to open config file '{}': {e}", path.display());
        });
        let reader = BufReader::new(file);
        config = serde_json::from_reader(reader).unwrap_or_else(|e| {
            panic!("Failed to parse config file '{}': {e}", path.display());
        });
    }
    // Apply ssrf whitelist
    apply_ssrf_whitelist(&config);
    config
}

/// Save a [`Config`] to a JSON file.
///
/// Creates any missing parent directories. Uses the default config path when
/// `path_option` is `None`.
///
/// # Panics
///
/// Panics if the parent directory cannot be created, the file cannot be
/// opened for writing, or the config cannot be serialised.
pub fn save_config(config: &Config, path_option: Option<PathBuf>) {
    let path = path_option.unwrap_or_else(get_config_path);

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap_or_else(|e| {
            panic!("Failed to create config directory '{}': {e}", parent.display());
        });
    }

    let json = serde_json::to_string_pretty(config).unwrap_or_else(|e| {
        panic!("Failed to serialise config: {e}");
    });

    let mut file = File::create(&path).unwrap_or_else(|e| {
        panic!("Failed to open config file '{}' for writing: {e}", path.display());
    });

    file.write_all(json.as_bytes()).unwrap_or_else(|e| {
        panic!("Failed to write config file '{}': {e}", path.display());
    });
}

fn apply_ssrf_whitelist(config: &Config) {
    let ssrf_whitelist = config.tools.ssrf_whitelist.clone();
    if !ssrf_whitelist.is_empty() {
        configure_ssrf_whitelist(ssrf_whitelist);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_config_path() {
        let simple1_config =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("configs/simple1/config.json");
        set_config_path(simple1_config.clone());
        let config_path = get_config_path();
        println!("config_path: {:?}", config_path);
        assert_eq!(config_path, simple1_config);
        assert!(config_path.is_file());
    }

    // ── save_config ───────────────────────────────────────────────────────────

    #[test]
    fn test_save_config_writes_valid_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let config = Config::default();

        save_config(&config, Some(path.clone()));

        assert!(path.is_file());
        let content = std::fs::read_to_string(&path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert!(parsed.is_object());
    }

    #[test]
    fn test_save_config_creates_parent_directories() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("deep").join("config.json");
        let config = Config::default();

        save_config(&config, Some(path.clone()));

        assert!(path.is_file());
    }

    #[test]
    fn test_save_and_reload_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let mut config = Config::default();
        config.agents.model = "test-model".to_string();

        save_config(&config, Some(path.clone()));
        let loaded = load_config(Some(path));

        assert_eq!(loaded.agents.model, "test-model");
    }

    #[test]
    fn test_save_config_overwrites_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");

        let mut config1 = Config::default();
        config1.agents.model = "model-v1".to_string();
        save_config(&config1, Some(path.clone()));

        let mut config2 = Config::default();
        config2.agents.model = "model-v2".to_string();
        save_config(&config2, Some(path.clone()));

        let loaded = load_config(Some(path));
        assert_eq!(loaded.agents.model, "model-v2");
    }
}
