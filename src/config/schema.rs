use std::collections::HashMap;

use garde::Validate;
use serde::{Deserialize, Serialize};

fn default_send_progress() -> bool {
    true
}
fn default_send_max_retries() -> u8 {
    3
}
fn default_transcription_provider() -> String {
    "groq".to_string()
}

/// Configuration for chat channels.
///
/// Built-in and plugin channel configs are stored in `extra`. Each channel
/// parses its own config independently. Per-channel `"streaming": true`
/// enables streaming output (requires a `send_delta` implementation).
#[derive(Deserialize, Serialize, Validate)]
#[serde(rename_all = "camelCase", default)]
pub struct ChannelsConfig {
    /// Stream agent's text progress to the channel.
    #[serde(alias = "send_progress")]
    #[garde(skip)]
    pub send_progress: bool,

    /// Stream tool-call hints (e.g. `read_file("…")`).
    #[serde(alias = "send_tool_hints")]
    #[garde(skip)]
    pub send_tool_hints: bool,

    /// Max delivery attempts, including the initial send. Range: 0–10.
    #[serde(alias = "send_max_retries", default = "default_send_max_retries")]
    #[garde(range(min = 0, max = 10))]
    pub send_max_retries: u8,

    /// Voice transcription backend: `"groq"` or `"openai"`.
    #[serde(alias = "transcription_provider", default = "default_transcription_provider")]
    #[garde(skip)]
    pub transcription_provider: String,

    /// Plugin-specific channel configs not covered by the typed fields above.
    #[serde(flatten)]
    #[garde(skip)]
    pub extra: HashMap<String, serde_json::Value>,
}

impl Default for ChannelsConfig {
    fn default() -> Self {
        Self {
            send_progress: default_send_progress(),
            send_tool_hints: false,
            send_max_retries: default_send_max_retries(),
            transcription_provider: default_transcription_provider(),
            extra: HashMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_defaults() {
        let cfg = ChannelsConfig::default();
        assert!(cfg.send_progress);
        assert!(!cfg.send_tool_hints);
        assert_eq!(cfg.send_max_retries, 3);
        assert_eq!(cfg.transcription_provider, "groq");
        assert!(cfg.extra.is_empty());
    }

    #[test]
    fn test_deserialize_camel_case() {
        let json = r#"{"sendProgress": false, "sendMaxRetries": 5}"#;
        let cfg: ChannelsConfig = serde_json::from_str(json).unwrap();
        assert!(!cfg.send_progress);
        assert_eq!(cfg.send_max_retries, 5);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_deserialize_snake_case_alias() {
        let json = r#"{"send_progress": false, "send_max_retries": 2}"#;
        let cfg: ChannelsConfig = serde_json::from_str(json).unwrap();
        assert!(!cfg.send_progress);
        assert_eq!(cfg.send_max_retries, 2);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_deserialize_extra_fields() {
        let json = r#"{"telegram": {"token": "abc123"}, "slack": {"webhook": "https://hooks.slack.com/x"}}"#;
        let cfg: ChannelsConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.extra.contains_key("telegram"));
        assert!(cfg.extra.contains_key("slack"));
    }

    #[test]
    fn test_retries_out_of_range_fails_validation() {
        let json = r#"{"sendMaxRetries": 11}"#;
        let cfg: ChannelsConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.validate().is_err());
    }
}