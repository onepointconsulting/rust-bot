use serde::{Deserialize, Deserializer, Serialize};

use crate::config::schema::JwtConfig;

/// Strip a trailing `/`, keeping root `"/"` unchanged.
fn strip_trailing_slash(path: &str) -> String {
    if path.len() > 1 && path.ends_with('/') {
        path.trim_end_matches('/').to_string()
    } else if path.is_empty() {
        "/".to_string()
    } else {
        path.to_string()
    }
}

/// Normalize a WebSocket config path for consistent routing.
fn normalize_config_path(path: &str) -> String {
    strip_trailing_slash(path)
}

/// Serde equivalent of a Pydantic `@field_validator("path")`:
/// require a leading `/`, then normalize trailing slashes.
fn deserialize_path<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    if !value.starts_with('/') {
        return Err(serde::de::Error::custom(r#"path must start with "/""#));
    }
    Ok(normalize_config_path(&value))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct WebSocketConfig {
    pub enabled: bool,
    pub host: String,
    pub port: u16,
    #[serde(deserialize_with = "deserialize_path")]
    pub path: String,
    pub jwt: JwtConfig,
    pub allow_from: Vec<String>,
    pub streaming: bool,
    pub max_message_bytes: usize,
    pub ping_interval_s: u64,
    pub ping_timeout_s: u64,
    pub ssl_certfile: String,
    pub ssl_keyfile: String,
}

impl Default for WebSocketConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            host: "127.0.0.1".to_string(),
            port: 8765,
            path: "/".to_string(),
            jwt: JwtConfig::default(),
            allow_from: vec![],
            streaming: false,
            max_message_bytes: 1024 * 1024 * 32,
            ping_interval_s: 30,
            ping_timeout_s: 30,
            ssl_certfile: "".to_string(),
            ssl_keyfile: "".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_config_path_strips_trailing_slash() {
        assert_eq!(normalize_config_path("/ws/"), "/ws");
        assert_eq!(normalize_config_path("/"), "/");
    }

    #[test]
    fn path_must_start_with_slash() {
        let err = serde_json::from_str::<WebSocketConfig>(r#"{"path":"bad"}"#)
            .expect_err("path without leading slash should fail");
        assert!(
            err.to_string().contains(r#"path must start with "/""#),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn path_is_normalized_on_deserialize() {
        let cfg: WebSocketConfig =
            serde_json::from_str(r#"{"path":"/ws/"}"#).expect("valid path should deserialize");
        assert_eq!(cfg.path, "/ws");
    }
}
