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

fn default_dream_interval_h() -> u32 {
    2
}
fn default_dream_max_batch_size() -> u32 {
    20
}
fn default_dream_max_iterations() -> u32 {
    10
}

fn default_agent_workspace() -> String {
    "~/.rust-bot/workspace".to_string()
}
fn default_agent_model() -> String {
    "anthropic/claude-opus-4-6".to_string()
}
fn default_agent_provider() -> String {
    "auto".to_string()
}
fn default_agent_max_tokens() -> u32 {
    8192
}
fn default_agent_context_window_tokens() -> u32 {
    65_536
}
fn default_agent_temperature() -> f32 {
    0.1
}
fn default_agent_max_tool_iterations() -> u32 {
    100
}
fn default_agent_max_tool_result_chars() -> u32 {
    16_000
}
fn default_agent_provider_retry_mode() -> ProviderRetryMode {
    ProviderRetryMode::Standard
}
fn default_agent_reasoning_effort() -> Option<String> {
    None
}
fn default_agent_timezone() -> String {
    "UTC".to_string()
}
fn default_agent_dream_config() -> DreamConfig {
    DreamConfig::default()
}

/// LLM provider configuration.
#[derive(Deserialize, Serialize, Validate)]
#[serde(rename_all = "camelCase", default)]
pub struct ProviderConfig {
    /// API key for the provider. Defaults to an empty string so it can be
    /// supplied via an environment variable at runtime instead.
    #[serde(alias = "api_key")]
    #[garde(skip)]
    pub api_key: String,

    /// Base URL override for the provider API endpoint.
    #[serde(alias = "api_base")]
    #[garde(skip)]
    pub api_base: Option<String>,

    /// Custom HTTP headers forwarded with every request (e.g. `APP-Code` for AiHubMix).
    #[serde(alias = "extra_headers")]
    #[garde(skip)]
    pub extra_headers: Option<HashMap<String, String>>,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            api_base: None,
            extra_headers: None,
        }
    }
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



/// Represents a cron-based or interval-based schedule used by Dream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum CronSchedule {
    /// A standard cron expression schedule.
    Cron { expr: String, tz: String },
    /// A fixed millisecond interval schedule.
    Every { every_ms: u64 },
}

// ── default helpers ───────────────────────────────────────────────────────────

/// Dream memory consolidation configuration.
#[derive(Deserialize, Serialize, Validate)]
#[serde(rename_all = "camelCase", default)]
pub struct DreamConfig {
    /// Consolidation interval in whole hours. Must be ≥ 1. Default: 2.
    #[serde(alias = "interval_h", default = "default_dream_interval_h")]
    #[garde(range(min = 1))]
    pub interval_h: u32,

    /// Legacy cron expression override. When present, takes priority over
    /// `interval_h`. Excluded from serialised output.
    #[serde(alias = "cron", skip_serializing)]
    #[garde(skip)]
    pub cron: Option<String>,

    /// Optional Dream-specific model override. Accepts `modelOverride`,
    /// `model`, or `model_override` as JSON keys.
    #[serde(alias = "model", alias = "model_override")]
    #[garde(skip)]
    pub model_override: Option<String>,

    /// Maximum history entries processed per consolidation run. Must be ≥ 1.
    #[serde(alias = "max_batch_size", default = "default_dream_max_batch_size")]
    #[garde(range(min = 1))]
    pub max_batch_size: u32,

    /// Maximum tool calls allowed per Phase 2 run. Must be ≥ 1.
    #[serde(alias = "max_iterations", default = "default_dream_max_iterations")]
    #[garde(range(min = 1))]
    pub max_iterations: u32,
}

impl Default for DreamConfig {
    fn default() -> Self {
        Self {
            interval_h: default_dream_interval_h(),
            cron: None,
            model_override: None,
            max_batch_size: default_dream_max_batch_size(),
            max_iterations: default_dream_max_iterations(),
        }
    }
}

impl DreamConfig {
    const HOUR_MS: u64 = 3_600_000;

    /// Build the runtime schedule, preferring the legacy cron override if present.
    pub fn build_schedule(&self, timezone: &str) -> CronSchedule {
        if let Some(ref expr) = self.cron {
            return CronSchedule::Cron {
                expr: expr.clone(),
                tz: timezone.to_string(),
            };
        }
        CronSchedule::Every {
            every_ms: self.interval_h as u64 * Self::HOUR_MS,
        }
    }

    /// Return a human-readable schedule summary for logs and startup output.
    pub fn describe_schedule(&self) -> String {
        if let Some(ref expr) = self.cron {
            return format!("cron {expr} (legacy)");
        }
        format!("every {}h", self.interval_h)
    }
}

#[derive(Deserialize, Serialize, Default, Debug, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ProviderRetryMode {
    #[default]
    Standard,
    Persistent,
}

#[derive(Deserialize, Serialize, Validate)]
#[serde(rename_all = "camelCase", default)]
pub struct AgentsConfig {

    #[serde(alias = "workspace", default = "default_agent_workspace")]
    #[garde(skip)]
    pub workspace: String,

    #[serde(alias = "model", default = "default_agent_model")]
    #[garde(skip)]
    pub model: String,

    // Provider name (e.g. "anthropic", "openrouter") or "auto" for auto-detection
    #[serde(alias = "provider", default = "default_agent_provider")]
    #[garde(skip)]
    pub provider: String,

    #[serde(alias = "max_tokens", default = "default_agent_max_tokens")]
    #[garde(range(min = 1))]
    pub max_tokens: u32,

    #[serde(alias = "context_window_tokens", default = "default_agent_context_window_tokens")]
    #[garde(range(min = 1))]
    pub context_window_tokens: u32,

    #[serde(alias = "context_block_limit", skip_serializing)]
    #[garde(skip)]
    pub context_block_limit: Option<u32>,

    #[serde(alias = "temperature", default = "default_agent_temperature")]
    #[garde(range(min = 0.0, max = 1.0))]
    pub temperature: f32,

    #[serde(alias = "max_tool_iterations", default = "default_agent_max_tool_iterations")]
    #[garde(range(min = 1))]
    pub max_tool_iterations: u32,

    #[serde(alias = "max_tool_result_chars", default = "default_agent_max_tool_result_chars")]
    #[garde(range(min = 1))]
    pub max_tool_result_chars: u32,

    #[serde(alias = "provider_retry_mode", default = "default_agent_provider_retry_mode")]
    #[garde(skip)]
    pub provider_retry_mode: ProviderRetryMode,

    /// Enables LLM thinking mode. Accepted values vary by provider:
    /// `"low"`, `"medium"`, `"high"`, `"adaptive"`, etc.
    #[serde(alias = "reasoning_effort", default = "default_agent_reasoning_effort")]
    #[garde(skip)]
    pub reasoning_effort: Option<String>,

    #[serde(alias = "timezone", default = "default_agent_timezone")]
    #[garde(skip)]
    pub timezone: String,

    #[serde(alias = "dream", default = "default_agent_dream_config")]
    #[garde(skip)]
    pub dream: DreamConfig,
}

impl Default for AgentsConfig {
    fn default() -> Self {
        Self {
            workspace: default_agent_workspace(),
            model: default_agent_model(),
            provider: default_agent_provider(),
            max_tokens: default_agent_max_tokens(),
            context_window_tokens: default_agent_context_window_tokens(),
            context_block_limit: None,
            temperature: default_agent_temperature(),
            max_tool_iterations: default_agent_max_tool_iterations(),
            max_tool_result_chars: default_agent_max_tool_result_chars(),
            provider_retry_mode: default_agent_provider_retry_mode(),
            reasoning_effort: None,
            timezone: default_agent_timezone(),
            dream: default_agent_dream_config(),
        }
    }
}

/// Configuration for all LLM providers.
///
/// Each field holds the credentials and endpoint settings for one provider.
/// All fields default to an empty `ProviderConfig` so missing sections in the
/// config file are silently filled in with safe no-op values.
#[derive(Deserialize, Serialize, Validate)]
#[serde(rename_all = "camelCase", default)]
pub struct ProvidersConfig {
    /// Any OpenAI-compatible endpoint (custom deployments, local models, etc.).
    #[serde(alias = "custom")]
    #[garde(dive)]
    pub custom: ProviderConfig,

    /// Azure OpenAI — the `model` field is the deployment name.
    #[serde(alias = "azure_openai")]
    #[garde(dive)]
    pub azure_openai: ProviderConfig,

    /// Anthropic (Claude) provider.
    #[serde(alias = "anthropic")]
    #[garde(dive)]
    pub anthropic: ProviderConfig,

    /// OpenAI provider.
    #[serde(alias = "openai")]
    #[garde(dive)]
    pub openai: ProviderConfig,

    /// OpenRouter provider.
    #[serde(alias = "openrouter")]
    #[garde(dive)]
    pub openrouter: ProviderConfig,

    /// Gemini provider.
    #[serde(alias = "gemini")]
    #[garde(dive)]
    pub gemini: ProviderConfig,
}

impl Default for ProvidersConfig {
    fn default() -> Self {
        Self {
            custom: ProviderConfig::default(),
            azure_openai: ProviderConfig::default(),
            anthropic: ProviderConfig::default(),
            openai: ProviderConfig::default(),
            openrouter: ProviderConfig::default(),
            gemini: ProviderConfig::default(),
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
        assert_eq!(cfg.transcription_provider, "groq");
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
        assert_eq!(cfg.send_max_retries, default_send_max_retries());
    }

    #[test]
    fn test_retries_out_of_range_fails_validation() {
        let json = r#"{"sendMaxRetries": 11}"#;
        let cfg: ChannelsConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.validate().is_err());
        assert_eq!(cfg.transcription_provider, "groq");
        assert_eq!(cfg.send_progress, default_send_progress());
    }

    #[test]
    fn test_dream_defaults() {
        let cfg = DreamConfig::default();
        assert_eq!(cfg.interval_h, default_dream_interval_h());
        assert_eq!(cfg.max_batch_size, default_dream_max_batch_size());
        assert_eq!(cfg.max_iterations, default_dream_max_iterations());
        assert_eq!(cfg.model_override, None);
        assert_eq!(cfg.cron, None);
    }

    #[test]
    fn test_dream_deserialize_camel_case() {
        let interval_h = 4;
        let max_batch_size = 25;
        let max_iterations = 15;
        let json = format!("{{\"intervalH\": {interval_h}, \"maxBatchSize\": {max_batch_size}, \"maxIterations\": {max_iterations}}}");
        let cfg: DreamConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg.interval_h, interval_h);
        assert_eq!(cfg.max_batch_size, max_batch_size);
        assert_eq!(cfg.max_iterations, max_iterations);
        assert_eq!(cfg.model_override, None);
        assert_eq!(cfg.cron, None);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_dream_deserialize_snake_case_aliases() {
        let json = r#"{"interval_h": 6, "max_batch_size": 10, "max_iterations": 5}"#;
        let cfg: DreamConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.interval_h, 6);
        assert_eq!(cfg.max_batch_size, 10);
        assert_eq!(cfg.max_iterations, 5);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_dream_deserialize_model_override_aliases() {
        // "model" alias
        let cfg: DreamConfig = serde_json::from_str(r#"{"model": "gpt-4o"}"#).unwrap();
        assert_eq!(cfg.model_override.as_deref(), Some("gpt-4o"));

        // "model_override" alias
        let cfg: DreamConfig = serde_json::from_str(r#"{"model_override": "claude-3"}"#).unwrap();
        assert_eq!(cfg.model_override.as_deref(), Some("claude-3"));

        // "modelOverride" camelCase (from rename_all)
        let cfg: DreamConfig = serde_json::from_str(r#"{"modelOverride": "mistral"}"#).unwrap();
        assert_eq!(cfg.model_override.as_deref(), Some("mistral"));
    }

    #[test]
    fn test_dream_validation_rejects_zero_values() {
        let json = r#"{"intervalH": 0, "maxBatchSize": 0, "maxIterations": 0}"#;
        let cfg: DreamConfig = serde_json::from_str(json).unwrap();
        let result = cfg.validate();
        assert!(result.is_err());
    }

    #[test]
    fn test_dream_build_schedule_interval() {
        let cfg = DreamConfig { interval_h: 3, ..DreamConfig::default() };
        let schedule = cfg.build_schedule("UTC");
        assert_eq!(schedule, CronSchedule::Every { every_ms: 3 * 3_600_000 });
    }

    #[test]
    fn test_dream_build_schedule_cron_override() {
        let cfg = DreamConfig {
            cron: Some("0 2 * * *".to_string()),
            ..DreamConfig::default()
        };
        let schedule = cfg.build_schedule("Europe/London");
        assert_eq!(schedule, CronSchedule::Cron {
            expr: "0 2 * * *".to_string(),
            tz: "Europe/London".to_string(),
        });
    }

    #[test]
    fn test_dream_describe_schedule_interval() {
        let cfg = DreamConfig { interval_h: 6, ..DreamConfig::default() };
        assert_eq!(cfg.describe_schedule(), "every 6h");
    }

    #[test]
    fn test_dream_describe_schedule_cron() {
        let cfg = DreamConfig {
            cron: Some("0 3 * * *".to_string()),
            ..DreamConfig::default()
        };
        assert_eq!(cfg.describe_schedule(), "cron 0 3 * * * (legacy)");
    }

    #[test]
    fn test_dream_cron_excluded_from_serialization() {
        let cfg = DreamConfig {
            cron: Some("0 2 * * *".to_string()),
            interval_h: 4,
            ..DreamConfig::default()
        };
        let json = serde_json::to_string(&cfg).unwrap();
        // cron is marked skip_serializing so it must not appear in output
        assert!(!json.contains("cron"));
        // other fields are serialized normally
        assert!(json.contains("intervalH") || json.contains("interval_h") || json.contains("4"));
    }

    #[test]
    fn test_agents_config_default() {
        let cfg = AgentsConfig::default();
        assert_eq!(cfg.workspace, default_agent_workspace());
        assert_eq!(cfg.model, default_agent_model());
        assert_eq!(cfg.provider, default_agent_provider());
        assert_eq!(cfg.max_tokens, default_agent_max_tokens());
        assert_eq!(cfg.context_window_tokens, default_agent_context_window_tokens());
        assert_eq!(cfg.context_block_limit, None);
        assert_eq!(cfg.temperature, default_agent_temperature());
        assert_eq!(cfg.max_tool_iterations, default_agent_max_tool_iterations());
        assert_eq!(cfg.max_tool_result_chars, default_agent_max_tool_result_chars());
    }

    #[test]
    fn test_agents_config_agent_model() {
        let cfg: AgentsConfig = serde_json::from_str(r#"{"model": "mistral"}"#).unwrap();
        assert_eq!(cfg.model, "mistral");
        assert_eq!(cfg.workspace, default_agent_workspace());
        assert_eq!(cfg.provider, default_agent_provider());
        assert_eq!(cfg.max_tokens, default_agent_max_tokens());
        assert_eq!(cfg.context_window_tokens, default_agent_context_window_tokens());
        assert_eq!(cfg.context_block_limit, None);
        assert_eq!(cfg.temperature, default_agent_temperature());
        assert_eq!(cfg.max_tool_iterations, default_agent_max_tool_iterations());
        assert_eq!(cfg.max_tool_result_chars, default_agent_max_tool_result_chars());
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_provider_config_default() {
        let cfg = ProviderConfig::default();
        assert_eq!(cfg.api_key, String::new());
        assert_eq!(cfg.api_base, None);
        assert_eq!(cfg.extra_headers, None);
    }

    #[test]
    fn test_provider_config_deserialize_camel_case() {
        let json = r#"{"apiKey": "abc123", "apiBase": "https://api.provider.com", "extraHeaders": {"APP-Code": "123456"}}"#;
        let cfg: ProviderConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.api_key, "abc123");
        assert_eq!(cfg.api_base, Some("https://api.provider.com".to_string()));
        assert_eq!(cfg.extra_headers, Some(HashMap::from([("APP-Code".to_string(), "123456".to_string())])));
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_providers_config_deserialize_snake_case_aliases() {
        let json = r#"{"openrouter": {"apiKey": "abc123", "apiBase": "https://api.provider.com", "extraHeaders": {"APP-Code": "123456"}}}"#;
        let cfg: ProvidersConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.openrouter.api_key, "abc123");
        assert_eq!(cfg.openrouter.api_base, Some("https://api.provider.com".to_string()));
        assert_eq!(cfg.openrouter.extra_headers, Some(HashMap::from([("APP-Code".to_string(), "123456".to_string())])));
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_providers_config_default_all_empty() {
        let cfg = ProvidersConfig::default();
        for provider in [
            &cfg.custom, &cfg.azure_openai, &cfg.anthropic,
            &cfg.openai, &cfg.openrouter,
        ] {
            assert_eq!(provider.api_key, "");
            assert_eq!(provider.api_base, None);
            assert_eq!(provider.extra_headers, None);
        }
    }

    #[test]
    fn test_providers_config_absent_providers_default() {
        // Only "anthropic" is provided; all other providers should have empty defaults.
        let json = r#"{"anthropic": {"apiKey": "sk-ant-123"}}"#;
        let cfg: ProvidersConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.anthropic.api_key, "sk-ant-123");
        assert_eq!(cfg.openai.api_key, "");
        assert_eq!(cfg.openrouter.api_key, "");
        assert_eq!(cfg.azure_openai.api_key, "");
        assert_eq!(cfg.custom.api_key, "");
    }

    #[test]
    fn test_providers_config_multiple_providers() {
        let json = r#"{
            "openai": {"apiKey": "sk-openai"},
            "anthropic": {"apiKey": "sk-anthropic"},
            "openrouter": {"apiKey": "sk-openrouter"}
        }"#;
        let cfg: ProvidersConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.openai.api_key, "sk-openai");
        assert_eq!(cfg.anthropic.api_key, "sk-anthropic");
        assert_eq!(cfg.openrouter.api_key, "sk-openrouter");
        assert_eq!(cfg.azure_openai.api_key, "");
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_providers_config_azure_openai_snake_case_alias() {
        let json = r#"{"azure_openai": {"apiKey": "az-key", "apiBase": "https://mydeployment.openai.azure.com"}}"#;
        let cfg: ProvidersConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.azure_openai.api_key, "az-key");
        assert_eq!(cfg.azure_openai.api_base, Some("https://mydeployment.openai.azure.com".to_string()));
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_providers_config_azure_openai_camel_case() {
        let json = r#"{"azureOpenai": {"apiKey": "az-key"}}"#;
        let cfg: ProvidersConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.azure_openai.api_key, "az-key");
    }

    #[test]
    fn test_providers_config_custom_provider() {
        let json = r#"{"custom": {"apiKey": "local-key", "apiBase": "http://localhost:11434/v1"}}"#;
        let cfg: ProvidersConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.custom.api_key, "local-key");
        assert_eq!(cfg.custom.api_base, Some("http://localhost:11434/v1".to_string()));
        assert!(cfg.validate().is_ok());
    }
}