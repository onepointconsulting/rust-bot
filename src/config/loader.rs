use std::path::PathBuf;
use std::sync::OnceLock;

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
