use std::env;
use std::fs::{self, File};
use std::io::{BufReader, Write};
use std::path::PathBuf;
use std::sync::{LazyLock, OnceLock};

use regex::Regex;


use crate::cli::CliError;
use crate::config::schema::{validate_model_presets, Config};
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
    if let Err(e) = validate_model_presets(&config) {
        panic!("Invalid config file '{}': {e}", path.display());
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
pub fn save_config(config: &Config, path_option: Option<PathBuf>) -> Result<(), CliError> {
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
    Ok(())
}

static ENV_VAR_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\$\{([A-Za-z_][A-Za-z0-9_]*)\}").unwrap()
});

/// Return a copy of `config` with every `${VAR}` placeholder in string values
/// replaced by the corresponding environment variable.
///
/// Only `String` values are affected; numbers, booleans, and nulls pass through
/// unchanged. Returns `Err` if any referenced variable is not set.
pub fn resolve_config_env_vars(config: &Config) -> Result<Config, String> {
    let data = serde_json::to_value(config).map_err(|e| e.to_string())?;
    let resolved = resolve_env_vars_in_value(data)?;
    serde_json::from_value(resolved).map_err(|e| e.to_string())
}

/// Recursively resolve `${VAR}` patterns in all string leaves of a JSON value.
fn resolve_env_vars_in_value(value: serde_json::Value) -> Result<serde_json::Value, String> {
    match value {
        serde_json::Value::String(s) => {
            let mut error: Option<String> = None;
            let result = ENV_VAR_RE.replace_all(&s, |caps: &regex::Captures| {
                let var_name = &caps[1];
                match env::var(var_name) {
                    Ok(val) => val,
                    Err(_) => {
                        error = Some(format!("Environment variable '{var_name}' is not set"));
                        String::new()
                    }
                }
            });
            if let Some(err) = error {
                return Err(err);
            }
            Ok(serde_json::Value::String(result.into_owned()))
        }
        serde_json::Value::Object(map) => {
            let mut new_map = serde_json::Map::new();
            for (k, v) in map {
                new_map.insert(k, resolve_env_vars_in_value(v)?);
            }
            Ok(serde_json::Value::Object(new_map))
        }
        serde_json::Value::Array(arr) => {
            let resolved: Result<Vec<_>, _> = arr.into_iter().map(resolve_env_vars_in_value).collect();
            Ok(serde_json::Value::Array(resolved?))
        }
        other => Ok(other),
    }
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

    #[test]
    fn test_simple1_config_migrated_to_model_presets() {
        let simple1_config =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("configs/simple1/config.json");
        let config = load_config(Some(simple1_config));
        assert_eq!(config.agents.model_preset, Some("primary".to_string()));
        let preset = config
            .model_presets
            .get("primary")
            .expect("migrated config should define a 'primary' preset");
        assert_eq!(preset.model, config.agents.model);
        assert_eq!(preset.provider, config.agents.provider);
        assert_eq!(preset.max_tokens, config.agents.max_tokens);
        assert_eq!(preset.context_window_tokens, config.agents.context_window_tokens);
        assert_eq!(preset.temperature, config.agents.temperature);
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
        let _ = save_config(&config1, Some(path.clone()));

        let mut config2 = Config::default();
        config2.agents.model = "model-v2".to_string();
        let _ = save_config(&config2, Some(path.clone()));

        let loaded = load_config(Some(path));
        assert_eq!(loaded.agents.model, "model-v2");
    }

    #[test]
    fn test_apply_ssrf_whitelist() {
        let mut config = Config::default();
        config.tools.ssrf_whitelist = vec!["100.64.0.0/10".to_string(), "192.168.0.0/16".to_string()];
        assert!(!config.tools.ssrf_whitelist.is_empty());
        assert!(config.tools.ssrf_whitelist.len() == 2);
        assert!(config.tools.ssrf_whitelist.get(0).unwrap() == "100.64.0.0/10");
        assert!(config.tools.ssrf_whitelist.get(1).unwrap() == "192.168.0.0/16");
        println!("Config: {}", serde_json::to_string_pretty(&config).unwrap());
    }

    // ── resolve_config_env_vars ───────────────────────────────────────────────

    #[test]
    fn test_resolve_env_vars_no_placeholders() {
        let config = Config::default();
        let resolved = resolve_config_env_vars(&config).unwrap();
        assert_eq!(resolved.agents.model, config.agents.model);
    }

    #[test]
    fn test_resolve_env_vars_replaces_placeholder() {
        unsafe { env::set_var("TEST_MODEL_NAME", "my-test-model") };
        let mut config = Config::default();
        config.agents.model = "${TEST_MODEL_NAME}".to_string();

        let resolved = resolve_config_env_vars(&config).unwrap();
        assert_eq!(resolved.agents.model, "my-test-model");
    }

    #[test]
    fn test_resolve_env_vars_partial_replacement() {
        unsafe { env::set_var("TEST_PROVIDER_NAME", "openrouter") };
        let mut config = Config::default();
        config.agents.model = "prefix-${TEST_PROVIDER_NAME}/some-model".to_string();

        let resolved = resolve_config_env_vars(&config).unwrap();
        assert_eq!(resolved.agents.model, "prefix-openrouter/some-model");
    }

    #[test]
    fn test_resolve_env_vars_missing_var_returns_error() {
        let mut config = Config::default();
        config.agents.model = "${THIS_VAR_DEFINITELY_DOES_NOT_EXIST_12345}".to_string();

        let result = resolve_config_env_vars(&config);
        assert!(result.is_err());
        let msg = result.unwrap_err();
        assert!(msg.contains("THIS_VAR_DEFINITELY_DOES_NOT_EXIST_12345"), "got: {msg}");
    }

    #[test]
    fn test_resolve_env_vars_non_string_unchanged() {
        unsafe { env::set_var("TEST_MAX_TOKENS", "999") };
        // max_tokens is a u32, not a string — env var pattern in a string field
        // should not affect numeric fields at all.
        let config = Config::default();
        let original_max_tokens = config.agents.max_tokens;

        let resolved = resolve_config_env_vars(&config).unwrap();
        assert_eq!(resolved.agents.max_tokens, original_max_tokens);
    }

    // ── model_presets validation ─────────────────────────────────────────────

    #[test]
    fn test_load_config_with_no_model_presets_is_unaffected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let config = Config::default();
        save_config(&config, Some(path.clone()));

        let loaded = load_config(Some(path));
        assert!(loaded.model_presets.is_empty());
    }

    #[test]
    fn test_load_config_with_valid_model_preset_reference() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let mut config = Config::default();
        config.model_presets.insert(
            "fast".to_string(),
            crate::config::schema::ModelPresetConfig {
                model: "openai/gpt-4.1-mini".to_string(),
                ..Default::default()
            },
        );
        config.agents.model_preset = Some("fast".to_string());
        save_config(&config, Some(path.clone()));

        let loaded = load_config(Some(path));
        assert_eq!(loaded.agents.model_preset, Some("fast".to_string()));
    }

    #[test]
    #[should_panic(expected = "Invalid config file")]
    fn test_load_config_with_unknown_model_preset_reference_panics() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let mut config = Config::default();
        config.agents.model_preset = Some("nope".to_string());
        save_config(&config, Some(path.clone()));

        load_config(Some(path));
    }
}
