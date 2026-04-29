use std::fs::File;
use std::io::BufReader;
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

fn apply_ssrf_whitelist(config: &Config) {
    let ssrf_whitelist = config.tools.ssrf_whitelist.clone();
    configure_ssrf_whitelist(ssrf_whitelist);
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
}
