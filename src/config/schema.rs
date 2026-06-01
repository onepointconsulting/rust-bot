use std::{collections::HashMap, path::PathBuf};

use garde::Validate;
use serde::{Deserialize, Serialize};

use crate::{providers::registry::{find_by_name, providers}, utils::helpers::expand_tilde_path};

// ── ProviderConfig ────────────────────────────────────────────────────────────

/// LLM provider configuration.
#[derive(Debug, Deserialize, Serialize, Validate)]
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

// ── ChannelsConfig ────────────────────────────────────────────────────────────

fn default_send_progress() -> bool { true }
fn default_send_max_retries() -> u8 { 3 }
fn default_transcription_provider() -> String { "groq".to_string() }

/// Configuration for chat channels.
///
/// Built-in and plugin channel configs are stored in `extra`. Each channel
/// parses its own config independently. Per-channel `"streaming": true`
/// enables streaming output (requires a `send_delta` implementation).
#[derive(Debug, Deserialize, Serialize, Validate)]
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

// ── CronSchedule / DreamConfig ────────────────────────────────────────────────

/// Represents a cron-based or interval-based schedule used by Dream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum CronSchedule {
    /// A standard cron expression schedule.
    Cron { expr: String, tz: String },
    /// A fixed millisecond interval schedule.
    Every { every_ms: u64 },
}

fn default_dream_interval_h() -> u32 { 2 }
fn default_dream_max_batch_size() -> u32 { 20 }
fn default_dream_max_iterations() -> u32 { 10 }

/// Dream memory consolidation configuration.
#[derive(Debug, Deserialize, Serialize, Validate)]
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

// ── AgentsConfig ──────────────────────────────────────────────────────────────

#[derive(Deserialize, Serialize, Default, Debug, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ProviderRetryMode {
    #[default]
    Standard,
    Persistent,
}

fn default_agent_workspace() -> String { "~/.rust-bot/workspace".to_string() }
fn default_agent_model() -> String { "anthropic/claude-opus-4-6".to_string() }
fn default_agent_provider() -> String { "auto".to_string() }
fn default_agent_max_tokens() -> u32 { 8192 }
fn default_agent_context_window_tokens() -> u32 { 65_536 }
fn default_agent_temperature() -> f32 { 0.1 }
fn default_agent_max_tool_iterations() -> u32 { 100 }
fn default_agent_max_tool_result_chars() -> u32 { 16_000 }
fn default_agent_provider_retry_mode() -> ProviderRetryMode { ProviderRetryMode::Standard }
fn default_agent_reasoning_effort() -> Option<String> { None }
fn default_agent_timezone() -> String { "UTC".to_string() }
fn default_agent_dream_config() -> DreamConfig { DreamConfig::default() }

#[derive(Debug, Deserialize, Serialize, Validate)]
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

// ── ProvidersConfig ───────────────────────────────────────────────────────────

/// Configuration for all LLM providers.
///
/// Each field holds the credentials and endpoint settings for one provider.
/// All fields default to an empty `ProviderConfig` so missing sections in the
/// config file are silently filled in with safe no-op values.
#[derive(Debug, Deserialize, Serialize, Validate)]
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

impl ProvidersConfig {
    /// Return the [`ProviderConfig`] for the given provider name (e.g. `"openai"`).
    ///
    /// This is the Rust equivalent of Python's `getattr(self.providers, spec.name, None)`.
    pub fn get_by_name(&self, name: &str) -> Option<&ProviderConfig> {
        match name {
            "custom"       => Some(&self.custom),
            "azure_openai" => Some(&self.azure_openai),
            "anthropic"    => Some(&self.anthropic),
            "openai"       => Some(&self.openai),
            "openrouter"   => Some(&self.openrouter),
            "gemini"       => Some(&self.gemini),
            _              => None,
        }
    }
}

// ── HeartbeatConfig ───────────────────────────────────────────────────────────

fn default_heartbeat_enabled() -> bool { true }
fn default_heartbeat_interval_s() -> u32 { 30 * 60 }
fn default_heartbeat_keep_recent_messages() -> u32 { 8 }

/// Heartbeat service configuration.
#[derive(Debug, Deserialize, Serialize, Validate)]
#[serde(rename_all = "camelCase", default)]
pub struct HeartbeatConfig {
    /// Whether the heartbeat service is active.
    #[serde(alias = "enabled", default = "default_heartbeat_enabled")]
    #[garde(skip)]
    pub enabled: bool,

    /// Interval between heartbeats in seconds. Default: 1800 (30 minutes).
    #[serde(alias = "interval_s", default = "default_heartbeat_interval_s")]
    #[garde(range(min = 1))]
    pub interval_s: u32,

    /// Number of recent messages to retain for context. Default: 8.
    #[serde(alias = "keep_recent_messages", default = "default_heartbeat_keep_recent_messages")]
    #[garde(range(min = 1))]
    pub keep_recent_messages: u32,
}

impl Default for HeartbeatConfig {
    fn default() -> Self {
        Self {
            enabled: default_heartbeat_enabled(),
            interval_s: default_heartbeat_interval_s(),
            keep_recent_messages: default_heartbeat_keep_recent_messages(),
        }
    }
}

// ── ApiConfig ─────────────────────────────────────────────────────────────────

fn default_api_host() -> String { "127.0.0.1".to_string() }
fn default_api_port() -> u16 { 8900 }
fn default_api_timeout() -> f64 { 120.0 }

/// OpenAI-compatible API server configuration.
#[derive(Debug, Deserialize, Serialize, Validate)]
#[serde(rename_all = "camelCase", default)]
pub struct ApiConfig {
    /// Bind address for the API server. Defaults to local-only `127.0.0.1`.
    #[serde(alias = "host", default = "default_api_host")]
    #[garde(skip)]
    pub host: String,

    /// TCP port the server listens on. Default: 8900.
    #[serde(alias = "port", default = "default_api_port")]
    #[garde(range(min = 1, max = 65535))]
    pub port: u16,

    /// Per-request timeout in seconds. Default: 120.0.
    #[serde(alias = "timeout", default = "default_api_timeout")]
    #[garde(range(min = 0.0))]
    pub timeout: f64,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            host: default_api_host(),
            port: default_api_port(),
            timeout: default_api_timeout(),
        }
    }
}

// ── GatewayConfig ─────────────────────────────────────────────────────────────

fn default_gateway_host() -> String { "0.0.0.0".to_string() }
fn default_gateway_port() -> u16 { 18790 }

/// Gateway/server configuration.
#[derive(Debug, Deserialize, Serialize, Validate)]
#[serde(rename_all = "camelCase", default)]
pub struct GatewayConfig {
    /// Bind address for the gateway. Defaults to `0.0.0.0` (all interfaces).
    #[serde(alias = "host", default = "default_gateway_host")]
    #[garde(skip)]
    pub host: String,

    /// TCP port the gateway listens on. Default: 18790.
    #[serde(alias = "port", default = "default_gateway_port")]
    #[garde(range(min = 1, max = 65535))]
    pub port: u16,

    /// Heartbeat service configuration.
    #[serde(alias = "heartbeat")]
    #[garde(dive)]
    pub heartbeat: HeartbeatConfig,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            host: default_gateway_host(),
            port: default_gateway_port(),
            heartbeat: HeartbeatConfig::default(),
        }
    }
}

// ── WebSearchConfig ───────────────────────────────────────────────────────────

fn default_web_search_provider() -> String { "brave".to_string() }
fn default_web_search_max_results() -> u32 { 5 }
fn default_web_search_timeout() -> u32 { 30 }

/// Web search tool configuration.
#[derive(Debug, Clone, Deserialize, Serialize, Validate)]
#[serde(rename_all = "camelCase", default)]
pub struct WebSearchConfig {
    /// Search backend: `"brave"`, `"tavily"`, `"duckduckgo"`, `"searxng"`, or `"jina"`.
    #[serde(alias = "provider", default = "default_web_search_provider")]
    #[garde(skip)]
    pub provider: String,

    /// API key for providers that require authentication.
    #[serde(alias = "api_key")]
    #[garde(skip)]
    pub api_key: String,

    /// Base URL for self-hosted backends such as SearXNG.
    #[serde(alias = "base_url")]
    #[garde(skip)]
    pub base_url: String,

    /// Maximum number of results to return per query. Default: 5.
    #[serde(alias = "max_results", default = "default_web_search_max_results")]
    #[garde(range(min = 1))]
    pub max_results: u32,

    /// Wall-clock timeout in seconds for each search operation. Default: 30.
    #[serde(alias = "timeout", default = "default_web_search_timeout")]
    #[garde(range(min = 1))]
    pub timeout: u32,
}

impl Default for WebSearchConfig {
    fn default() -> Self {
        Self {
            provider: default_web_search_provider(),
            api_key: String::new(),
            base_url: String::new(),
            max_results: default_web_search_max_results(),
            timeout: default_web_search_timeout(),
        }
    }
}

// ── WebToolsConfig ────────────────────────────────────────────────────────────

fn default_web_tools_enable() -> bool { true }

/// Web tools configuration.
#[derive(Debug, Deserialize, Serialize, Validate)]
#[serde(rename_all = "camelCase", default)]
pub struct WebToolsConfig {
    /// Enable or disable the web tools entirely. Default: `true`.
    #[serde(alias = "enable", default = "default_web_tools_enable")]
    #[garde(skip)]
    pub enable: bool,

    /// Optional HTTP or SOCKS5 proxy URL used for all web requests.
    /// Examples: `"http://127.0.0.1:7890"`, `"socks5://127.0.0.1:1080"`.
    #[serde(alias = "proxy")]
    #[garde(skip)]
    pub proxy: Option<String>,

    /// Web search backend configuration.
    #[serde(alias = "search")]
    #[garde(dive)]
    pub search: WebSearchConfig,
}

impl Default for WebToolsConfig {
    fn default() -> Self {
        Self {
            enable: default_web_tools_enable(),
            proxy: None,
            search: WebSearchConfig::default(),
        }
    }
}

// ── ExecToolConfig ────────────────────────────────────────────────────────────

fn default_exec_tool_enable() -> bool { true }
fn default_exec_tool_timeout() -> u32 { 60 }

/// Shell exec tool configuration.
#[derive(Debug, Deserialize, Serialize, Validate)]
#[serde(rename_all = "camelCase", default)]
pub struct ExecToolConfig {
    /// Enable or disable the shell exec tool. Default: `true`.
    #[serde(alias = "enable", default = "default_exec_tool_enable")]
    #[garde(skip)]
    pub enable: bool,

    /// Command execution timeout in seconds. Default: 60.
    #[serde(alias = "timeout", default = "default_exec_tool_timeout")]
    #[garde(range(min = 1))]
    pub timeout: u32,

    /// Extra directories appended to `PATH` inside the subprocess.
    #[serde(alias = "path_append")]
    #[garde(skip)]
    pub path_append: String,

    /// Sandbox backend: `""` (none) or `"bwrap"` (Bubblewrap).
    #[serde(alias = "sandbox")]
    #[garde(skip)]
    pub sandbox: String,
}

impl Default for ExecToolConfig {
    fn default() -> Self {
        Self {
            enable: default_exec_tool_enable(),
            timeout: default_exec_tool_timeout(),
            path_append: String::new(),
            sandbox: String::new(),
        }
    }
}

// ── MCPServerConfig ───────────────────────────────────────────────────────────

fn default_mcp_tool_timeout() -> u32 { 30 }
fn default_mcp_enabled_tools() -> Vec<String> { vec!["*".to_string()] }

/// Transport type for an MCP server connection.
#[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum McpTransportType {
    Stdio,
    Sse,
    StreamableHttp,
}

/// MCP server connection configuration (stdio or HTTP).
#[derive(Debug, Deserialize, Serialize, Validate)]
#[serde(rename_all = "camelCase", default)]
pub struct McpServerConfig {
    /// Transport type. Auto-detected from `command`/`url` when `None`.
    #[serde(alias = "type")]
    #[garde(skip)]
    pub transport_type: Option<McpTransportType>,

    /// Stdio: executable to run (e.g. `"npx"`).
    #[serde(alias = "command")]
    #[garde(skip)]
    pub command: String,

    /// Stdio: command-line arguments.
    #[serde(alias = "args")]
    #[garde(skip)]
    pub args: Vec<String>,

    /// Stdio: extra environment variables injected into the subprocess.
    #[serde(alias = "env")]
    #[garde(skip)]
    pub env: HashMap<String, String>,

    /// HTTP/SSE: endpoint URL.
    #[serde(alias = "url")]
    #[garde(skip)]
    pub url: String,

    /// HTTP/SSE: custom request headers.
    #[serde(alias = "headers")]
    #[garde(skip)]
    pub headers: HashMap<String, String>,

    /// Seconds before a tool call is cancelled. Default: 30.
    #[serde(alias = "tool_timeout", default = "default_mcp_tool_timeout")]
    #[garde(range(min = 1))]
    pub tool_timeout: u32,

    /// Tools to register. Accepts raw MCP names or wrapped `mcp_<server>_<tool>` names.
    /// `["*"]` registers all tools; `[]` registers none. Default: `["*"]`.
    #[serde(alias = "enabled_tools", default = "default_mcp_enabled_tools")]
    #[garde(skip)]
    pub enabled_tools: Vec<String>,
}

impl Default for McpServerConfig {
    fn default() -> Self {
        Self {
            transport_type: None,
            command: String::new(),
            args: Vec::new(),
            env: HashMap::new(),
            url: String::new(),
            headers: HashMap::new(),
            tool_timeout: default_mcp_tool_timeout(),
            enabled_tools: default_mcp_enabled_tools(),
        }
    }
}

// ── ToolsConfig ───────────────────────────────────────────────────────────────

/// Tools configuration.
#[derive(Debug, Deserialize, Serialize, Validate)]
#[serde(rename_all = "camelCase", default)]
pub struct ToolsConfig {
    /// Web tool configuration (search, proxy, etc.).
    #[serde(alias = "web")]
    #[garde(dive)]
    pub web: WebToolsConfig,

    /// Shell exec tool configuration.
    #[serde(alias = "exec")]
    #[garde(dive)]
    pub exec: ExecToolConfig,

    /// Restrict all tool file access to the agent workspace directory.
    #[serde(alias = "restrict_to_workspace")]
    #[garde(skip)]
    pub restrict_to_workspace: bool,

    /// Named MCP server configurations, keyed by server name.
    #[serde(alias = "mcp_servers")]
    #[garde(dive)]
    pub mcp_servers: HashMap<String, McpServerConfig>,

    /// CIDR ranges exempt from SSRF blocking (e.g. `["100.64.0.0/10"]` for Tailscale).
    #[serde(alias = "ssrf_whitelist")]
    #[garde(skip)]
    pub ssrf_whitelist: Vec<String>,
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            web: WebToolsConfig::default(),
            exec: ExecToolConfig::default(),
            restrict_to_workspace: false,
            mcp_servers: HashMap::new(),
            ssrf_whitelist: Vec::new(),
        }
    }
}

// ── Config ────────────────────────────────────────────────────────────────────

/// Root configuration for the bot.
#[derive(Debug, Deserialize, Serialize, Validate)]
#[serde(rename_all = "camelCase", default)]
pub struct Config {
    /// Agent behaviour and model configuration.
    #[serde(alias = "agents")]
    #[garde(dive)]
    pub agents: AgentsConfig,

    /// Agent behaviour and model configuration.
    #[serde(alias = "channels")]
    #[garde(dive)]
    pub channels: ChannelsConfig,

    /// Agent behaviour and model configuration.
    #[serde(alias = "providers")]
    #[garde(dive)]
    pub providers: ProvidersConfig,

    /// Agent behaviour and model configuration.
    #[serde(alias = "api")]
    #[garde(dive)]
    pub api: ApiConfig,

    /// Agent behaviour and model configuration.
    #[serde(alias = "gateway")]
    #[garde(dive)]
    pub gateway: GatewayConfig,

    /// Agent behaviour and model configuration.
    #[serde(alias = "tools")]
    #[garde(dive)]
    pub tools: ToolsConfig,
}

impl Config {
    pub fn workspace_path(&self) -> PathBuf {
        let workspace = &self.agents.workspace;
        PathBuf::from(expand_tilde_path(workspace).as_ref())
    }

    fn match_provider(&self, model: Option<&str>) -> (Option<&ProviderConfig>, Option<String>) {
        let forced = self.agents.provider.clone();
        if forced != "auto" {
            let spec_option = find_by_name(forced.as_str());
            if let Some(spec) = spec_option {
                return (Some(&self.providers.custom), Some(spec.name.clone()));
            } else {
                return (None, None);
            }
        }

        let model_lower = model.map(|m| m.to_lowercase()).unwrap_or(self.agents.model.to_lowercase());
        let model_replaced = model_lower.replace('-', "_");
        let model_normalized = model_replaced.as_str();
        let model_prefix = if model_lower.contains('/') {
            model_lower.splitn(2, '/').next().unwrap_or(&model_lower)
        } else {
            ""
        };
        let normalized_prefix = model_prefix.replace('-', "_").to_lowercase();

        fn kw_matches(kw: &str, model_lower: &str, model_normalized: &str) -> bool {
            let kw_lower = kw.to_lowercase();
            model_lower.contains(kw_lower.as_str()) || model_normalized.contains(&kw_lower.replace('-', "_"))
        }


        for spec in providers() {
            let p = self.providers.get_by_name(&spec.name);
            if let Some(p) = p {
                if !model_prefix.is_empty() && normalized_prefix == spec.name && (spec.is_oauth || spec.is_local || !p.api_key.is_empty()) {
                    return (Some(p), Some(spec.name.clone()));
                }
                if spec.keywords.iter().any(|kw| kw_matches(kw, &model_lower, &model_normalized)) && (spec.is_oauth || spec.is_local || !p.api_key.is_empty()) {
                    return (Some(p), Some(spec.name.clone()));
                }
            }
        }

        // Fallback: configured local providers can route models without
        // provider-specific keywords (e.g. plain "llama3.2" on Ollama).
        // Prefer providers whose detect_by_base_keyword matches the configured
        // api_base (e.g. Ollama's "11434" in "http://localhost:11434") over
        // plain registry order.
        let mut local_fallback: Option<(&ProviderConfig, String)> = None;
        for spec in providers() {
            if !spec.is_local {
                continue;
            }
            let Some(p) = self.providers.get_by_name(&spec.name) else { continue };
            if p.api_base.is_none() {
                continue;
            }
            let api_base = p.api_base.as_deref().unwrap_or("");
            if !spec.detect_by_base_keyword.is_empty()
                && api_base.contains(&spec.detect_by_base_keyword)
            {
                return (Some(p), Some(spec.name));
            }
            if local_fallback.is_none() {
                local_fallback = Some((p, spec.name));
            }
        }
        if let Some((p, name)) = local_fallback {
            return (Some(p), Some(name));
        }

        // Last resort: return the first non-OAuth provider that has an API key configured.
        for spec in providers() {
            if spec.is_oauth {
                continue;
            }
            let Some(p) = self.providers.get_by_name(&spec.name) else { continue };
            if !p.api_key.is_empty() {
                return (Some(p), Some(spec.name));
            }
        }

        (None, None)
    }

    /// Get matched provider config (api_key, api_base, extra_headers). Falls back to first available.
    pub fn get_provider(&self, model: Option<&str>) -> Option<&ProviderConfig> {
        let (p, _) = self.match_provider(model);
        return p
    }

    pub fn get_provider_name(&self, model: Option<&str>) -> Option<String> {
        let (_, name) = self.match_provider(model);
        name
    }

    pub fn get_api_key(&self, model: Option<&str>) -> Option<String> {
        self.get_provider(model).map(|p| p.api_key.clone())
    }

    /// Get the API base URL for the given model.
    ///
    /// Returns the explicitly configured `api_base` first. For gateway and local
    /// providers that have no explicit base URL, falls back to the registry default.
    /// Standard providers resolve their base URL inside the provider constructor.
    pub fn get_api_base(&self, model: Option<&str>) -> Option<String> {
        let (p, name) = self.match_provider(model);

        if let Some(p) = p {
            if p.api_base.is_some() {
                return p.api_base.clone();
            }
        }

        if let Some(name) = name {
            if let Some(spec) = crate::providers::registry::find_by_name(&name) {
                if (spec.is_gateway || spec.is_local) && spec.default_api_base.is_some() {
                    return spec.default_api_base;
                }
            }
        }

        None
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            agents: AgentsConfig::default(),
            channels: ChannelsConfig::default(),
            providers: ProvidersConfig::default(),
            api: ApiConfig::default(),
            gateway: GatewayConfig::default(),
            tools: ToolsConfig::default(),
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

    // ── HeartbeatConfig ───────────────────────────────────────────────────────

    #[test]
    fn test_heartbeat_defaults() {
        let cfg = HeartbeatConfig::default();
        assert!(cfg.enabled);
        assert_eq!(cfg.interval_s, 30 * 60);
        assert_eq!(cfg.keep_recent_messages, 8);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_heartbeat_deserialize_camel_case() {
        let json = r#"{"enabled": false, "intervalS": 600, "keepRecentMessages": 4}"#;
        let cfg: HeartbeatConfig = serde_json::from_str(json).unwrap();
        assert!(!cfg.enabled);
        assert_eq!(cfg.interval_s, 600);
        assert_eq!(cfg.keep_recent_messages, 4);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_heartbeat_deserialize_snake_case_aliases() {
        let json = r#"{"enabled": true, "interval_s": 120, "keep_recent_messages": 16}"#;
        let cfg: HeartbeatConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.interval_s, 120);
        assert_eq!(cfg.keep_recent_messages, 16);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_heartbeat_absent_fields_use_defaults() {
        // Only "enabled" is supplied; other fields fall back to defaults.
        let json = r#"{"enabled": false}"#;
        let cfg: HeartbeatConfig = serde_json::from_str(json).unwrap();
        assert!(!cfg.enabled);
        assert_eq!(cfg.interval_s, default_heartbeat_interval_s());
        assert_eq!(cfg.keep_recent_messages, default_heartbeat_keep_recent_messages());
    }

    #[test]
    fn test_heartbeat_validation_rejects_zero_interval() {
        let json = r#"{"intervalS": 0}"#;
        let cfg: HeartbeatConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn test_heartbeat_validation_rejects_zero_keep_recent() {
        let json = r#"{"keepRecentMessages": 0}"#;
        let cfg: HeartbeatConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.validate().is_err());
    }

    // ── ApiConfig ─────────────────────────────────────────────────────────────

    #[test]
    fn test_api_defaults() {
        let cfg = ApiConfig::default();
        assert_eq!(cfg.host, "127.0.0.1");
        assert_eq!(cfg.port, 8900);
        assert_eq!(cfg.timeout, 120.0);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_api_deserialize_camel_case() {
        let json = r#"{"host": "0.0.0.0", "port": 9000, "timeout": 30.5}"#;
        let cfg: ApiConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.host, "0.0.0.0");
        assert_eq!(cfg.port, 9000);
        assert_eq!(cfg.timeout, 30.5);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_api_absent_fields_use_defaults() {
        let json = r#"{"port": 8080}"#;
        let cfg: ApiConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.host, default_api_host());
        assert_eq!(cfg.port, 8080);
        assert_eq!(cfg.timeout, default_api_timeout());
    }

    #[test]
    fn test_api_empty_object_uses_all_defaults() {
        let cfg: ApiConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(cfg.host, "127.0.0.1");
        assert_eq!(cfg.port, 8900);
        assert_eq!(cfg.timeout, 120.0);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_api_validation_rejects_port_zero() {
        let json = r#"{"port": 0}"#;
        let cfg: ApiConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn test_api_validation_rejects_negative_timeout() {
        let json = r#"{"timeout": -1.0}"#;
        let cfg: ApiConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.validate().is_err());
    }

    // ── GatewayConfig ─────────────────────────────────────────────────────────

    #[test]
    fn test_gateway_defaults() {
        let cfg = GatewayConfig::default();
        assert_eq!(cfg.host, "0.0.0.0");
        assert_eq!(cfg.port, 18790);
        // nested HeartbeatConfig should also have its own defaults
        assert!(cfg.heartbeat.enabled);
        assert_eq!(cfg.heartbeat.interval_s, 30 * 60);
        assert_eq!(cfg.heartbeat.keep_recent_messages, 8);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_gateway_deserialize_top_level_fields() {
        let json = r#"{"host": "127.0.0.1", "port": 9090}"#;
        let cfg: GatewayConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.host, "127.0.0.1");
        assert_eq!(cfg.port, 9090);
        // heartbeat not supplied → defaults
        assert!(cfg.heartbeat.enabled);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_gateway_deserialize_nested_heartbeat() {
        let json = r#"{"heartbeat": {"enabled": false, "intervalS": 300, "keepRecentMessages": 4}}"#;
        let cfg: GatewayConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.host, default_gateway_host());
        assert_eq!(cfg.port, default_gateway_port());
        assert!(!cfg.heartbeat.enabled);
        assert_eq!(cfg.heartbeat.interval_s, 300);
        assert_eq!(cfg.heartbeat.keep_recent_messages, 4);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_gateway_deserialize_snake_case_aliases() {
        let json = r#"{"host": "10.0.0.1", "port": 18791, "heartbeat": {"interval_s": 60}}"#;
        let cfg: GatewayConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.host, "10.0.0.1");
        assert_eq!(cfg.port, 18791);
        assert_eq!(cfg.heartbeat.interval_s, 60);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_gateway_validation_rejects_port_zero() {
        let json = r#"{"port": 0}"#;
        let cfg: GatewayConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn test_gateway_validation_propagates_nested_error() {
        // heartbeat.intervalS = 0 should fail garde validation on the nested struct
        let json = r#"{"heartbeat": {"intervalS": 0}}"#;
        let cfg: GatewayConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.validate().is_err());
    }

    // ── WebSearchConfig ───────────────────────────────────────────────────────

    #[test]
    fn test_web_search_defaults() {
        let cfg = WebSearchConfig::default();
        assert_eq!(cfg.provider, "duckduckgo");
        assert_eq!(cfg.api_key, "");
        assert_eq!(cfg.base_url, "");
        assert_eq!(cfg.max_results, 5);
        assert_eq!(cfg.timeout, 30);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_web_search_deserialize_camel_case() {
        let json = r#"{"provider": "brave", "apiKey": "bk-123", "maxResults": 10, "timeout": 60}"#;
        let cfg: WebSearchConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.provider, "brave");
        assert_eq!(cfg.api_key, "bk-123");
        assert_eq!(cfg.max_results, 10);
        assert_eq!(cfg.timeout, 60);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_web_search_deserialize_snake_case_aliases() {
        let json = r#"{"provider": "searxng", "api_key": "", "base_url": "http://localhost:8080", "max_results": 3, "timeout": 15}"#;
        let cfg: WebSearchConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.provider, "searxng");
        assert_eq!(cfg.base_url, "http://localhost:8080");
        assert_eq!(cfg.max_results, 3);
        assert_eq!(cfg.timeout, 15);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_web_search_absent_fields_use_defaults() {
        let json = r#"{"provider": "tavily"}"#;
        let cfg: WebSearchConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.provider, "tavily");
        assert_eq!(cfg.max_results, default_web_search_max_results());
        assert_eq!(cfg.timeout, default_web_search_timeout());
        assert_eq!(cfg.api_key, "");
        assert_eq!(cfg.base_url, "");
    }

    #[test]
    fn test_web_search_validation_rejects_zero_max_results() {
        let json = r#"{"maxResults": 0}"#;
        let cfg: WebSearchConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn test_web_search_validation_rejects_zero_timeout() {
        let json = r#"{"timeout": 0}"#;
        let cfg: WebSearchConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.validate().is_err());
    }

    // ── WebToolsConfig ────────────────────────────────────────────────────────

    #[test]
    fn test_web_tools_defaults() {
        let cfg = WebToolsConfig::default();
        assert!(cfg.enable);
        assert_eq!(cfg.proxy, None);
        // nested search should carry WebSearchConfig defaults
        assert_eq!(cfg.search.provider, "duckduckgo");
        assert_eq!(cfg.search.max_results, 5);
        assert_eq!(cfg.search.timeout, 30);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_web_tools_deserialize_disable() {
        let json = r#"{"enable": false}"#;
        let cfg: WebToolsConfig = serde_json::from_str(json).unwrap();
        assert!(!cfg.enable);
        assert_eq!(cfg.proxy, None);
        assert_eq!(cfg.search.provider, "duckduckgo");
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_web_tools_deserialize_proxy() {
        let json = r#"{"proxy": "http://127.0.0.1:7890"}"#;
        let cfg: WebToolsConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.proxy, Some("http://127.0.0.1:7890".to_string()));
        assert!(cfg.enable);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_web_tools_deserialize_socks5_proxy() {
        let json = r#"{"proxy": "socks5://127.0.0.1:1080"}"#;
        let cfg: WebToolsConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.proxy, Some("socks5://127.0.0.1:1080".to_string()));
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_web_tools_deserialize_nested_search() {
        let json = r#"{"search": {"provider": "brave", "apiKey": "bk-key", "maxResults": 8}}"#;
        let cfg: WebToolsConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.search.provider, "brave");
        assert_eq!(cfg.search.api_key, "bk-key");
        assert_eq!(cfg.search.max_results, 8);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_web_tools_validation_propagates_nested_error() {
        // search.maxResults = 0 should fail garde validation on the nested struct
        let json = r#"{"search": {"maxResults": 0}}"#;
        let cfg: WebToolsConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.validate().is_err());
    }

    // ── ExecToolConfig ────────────────────────────────────────────────────────

    #[test]
    fn test_exec_tool_defaults() {
        let cfg = ExecToolConfig::default();
        assert!(cfg.enable);
        assert_eq!(cfg.timeout, 60);
        assert_eq!(cfg.path_append, "");
        assert_eq!(cfg.sandbox, "");
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_exec_tool_deserialize_camel_case() {
        let json = r#"{"enable": false, "timeout": 120, "pathAppend": "/usr/local/bin", "sandbox": "bwrap"}"#;
        let cfg: ExecToolConfig = serde_json::from_str(json).unwrap();
        assert!(!cfg.enable);
        assert_eq!(cfg.timeout, 120);
        assert_eq!(cfg.path_append, "/usr/local/bin");
        assert_eq!(cfg.sandbox, "bwrap");
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_exec_tool_deserialize_snake_case_aliases() {
        let json = r#"{"enable": true, "timeout": 30, "path_append": "/opt/bin", "sandbox": ""}"#;
        let cfg: ExecToolConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.enable);
        assert_eq!(cfg.timeout, 30);
        assert_eq!(cfg.path_append, "/opt/bin");
        assert_eq!(cfg.sandbox, "");
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_exec_tool_absent_fields_use_defaults() {
        let json = r#"{"sandbox": "bwrap"}"#;
        let cfg: ExecToolConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.enable);
        assert_eq!(cfg.timeout, default_exec_tool_timeout());
        assert_eq!(cfg.path_append, "");
        assert_eq!(cfg.sandbox, "bwrap");
    }

    #[test]
    fn test_exec_tool_validation_rejects_zero_timeout() {
        let json = r#"{"timeout": 0}"#;
        let cfg: ExecToolConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.validate().is_err());
    }

    // ── McpServerConfig ───────────────────────────────────────────────────────

    #[test]
    fn test_mcp_server_defaults() {
        let cfg = McpServerConfig::default();
        assert_eq!(cfg.transport_type, None);
        assert_eq!(cfg.command, "");
        assert!(cfg.args.is_empty());
        assert!(cfg.env.is_empty());
        assert_eq!(cfg.url, "");
        assert!(cfg.headers.is_empty());
        assert_eq!(cfg.tool_timeout, 30);
        assert_eq!(cfg.enabled_tools, vec!["*"]);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_mcp_server_deserialize_stdio() {
        let json = r#"{
            "type": "stdio",
            "command": "npx",
            "args": ["-y", "@modelcontextprotocol/server-filesystem"],
            "env": {"NODE_ENV": "production"}
        }"#;
        let cfg: McpServerConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.transport_type, Some(McpTransportType::Stdio));
        assert_eq!(cfg.command, "npx");
        assert_eq!(cfg.args, vec!["-y", "@modelcontextprotocol/server-filesystem"]);
        assert_eq!(cfg.env.get("NODE_ENV").map(String::as_str), Some("production"));
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_mcp_server_deserialize_sse() {
        let json = r#"{
            "type": "sse",
            "url": "http://localhost:8080/sse",
            "headers": {"Authorization": "Bearer token123"}
        }"#;
        let cfg: McpServerConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.transport_type, Some(McpTransportType::Sse));
        assert_eq!(cfg.url, "http://localhost:8080/sse");
        assert_eq!(cfg.headers.get("Authorization").map(String::as_str), Some("Bearer token123"));
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_mcp_server_deserialize_streamable_http() {
        let json = r#"{"type": "streamableHttp", "url": "https://mcp.example.com/v1"}"#;
        let cfg: McpServerConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.transport_type, Some(McpTransportType::StreamableHttp));
        assert_eq!(cfg.url, "https://mcp.example.com/v1");
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_mcp_server_enabled_tools_subset() {
        let json = r#"{"enabledTools": ["read_file", "mcp_fs_write_file"]}"#;
        let cfg: McpServerConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.enabled_tools, vec!["read_file", "mcp_fs_write_file"]);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_mcp_server_enabled_tools_empty_disables_all() {
        let json = r#"{"enabledTools": []}"#;
        let cfg: McpServerConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.enabled_tools.is_empty());
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_mcp_server_snake_case_aliases() {
        let json = r#"{"tool_timeout": 90, "enabled_tools": ["search"]}"#;
        let cfg: McpServerConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.tool_timeout, 90);
        assert_eq!(cfg.enabled_tools, vec!["search"]);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_mcp_server_validation_rejects_zero_timeout() {
        let json = r#"{"toolTimeout": 0}"#;
        let cfg: McpServerConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.validate().is_err());
    }

    // ── ToolsConfig ───────────────────────────────────────────────────────────

    #[test]
    fn test_tools_config_defaults() {
        let cfg = ToolsConfig::default();
        assert!(cfg.web.enable);
        assert!(cfg.exec.enable);
        assert!(!cfg.restrict_to_workspace);
        assert!(cfg.mcp_servers.is_empty());
        assert!(cfg.ssrf_whitelist.is_empty());
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_tools_config_restrict_to_workspace() {
        let json = r#"{"restrictToWorkspace": true}"#;
        let cfg: ToolsConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.restrict_to_workspace);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_tools_config_snake_case_alias() {
        let json = r#"{"restrict_to_workspace": true}"#;
        let cfg: ToolsConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.restrict_to_workspace);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_tools_config_mcp_servers() {
        let json = r#"{
            "mcpServers": {
                "filesystem": {
                    "type": "stdio",
                    "command": "npx",
                    "args": ["-y", "@modelcontextprotocol/server-filesystem"]
                },
                "search": {
                    "type": "sse",
                    "url": "http://localhost:8080/sse"
                }
            }
        }"#;
        let cfg: ToolsConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.mcp_servers.len(), 2);
        let fs = cfg.mcp_servers.get("filesystem").unwrap();
        assert_eq!(fs.transport_type, Some(McpTransportType::Stdio));
        assert_eq!(fs.command, "npx");
        let search = cfg.mcp_servers.get("search").unwrap();
        assert_eq!(search.transport_type, Some(McpTransportType::Sse));
        assert_eq!(search.url, "http://localhost:8080/sse");
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_tools_config_ssrf_whitelist() {
        let json = r#"{"ssrfWhitelist": ["100.64.0.0/10", "192.168.0.0/16"]}"#;
        let cfg: ToolsConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.ssrf_whitelist, vec!["100.64.0.0/10", "192.168.0.0/16"]);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_tools_config_nested_web_override() {
        let json = r#"{"web": {"enable": false, "search": {"provider": "brave"}}}"#;
        let cfg: ToolsConfig = serde_json::from_str(json).unwrap();
        assert!(!cfg.web.enable);
        assert_eq!(cfg.web.search.provider, "brave");
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_tools_config_validation_propagates_nested_error() {
        // exec.timeout = 0 should bubble up through #[garde(dive)]
        let json = r#"{"exec": {"timeout": 0}}"#;
        let cfg: ToolsConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.validate().is_err());
    }

    // ── Config ────────────────────────────────────────────────────────────────

    #[test]
    fn test_config_defaults() {
        let cfg = Config::default();
        assert_eq!(cfg.agents.model, default_agent_model());
        assert_eq!(cfg.agents.workspace, default_agent_workspace());
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_config_empty_object_uses_all_defaults() {
        let cfg: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(cfg.agents.model, default_agent_model());
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_config_deserialize_nested_agents() {
        let json = r#"{"agents": {"model": "gpt-4o", "maxTokens": 4096}}"#;
        let cfg: Config = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.agents.model, "gpt-4o");
        assert_eq!(cfg.agents.max_tokens, 4096);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_config_snake_case_alias() {
        let model = "claude-3-5-sonnet";
        let json = format!("{{\"agents\": {{\"model\": \"{model}\"}}}}");
        let cfg: Config = serde_json::from_str(json.as_str()).unwrap();
        assert_eq!(cfg.agents.model, model);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_config_validation_propagates_nested_error() {
        // agents.maxTokens = 0 violates garde(range(min = 1))
        let json = r#"{"agents": {"maxTokens": 0}}"#;
        let cfg: Config = serde_json::from_str(json).unwrap();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn test_config_channels_config() {
        let json = r#"{"channels": {"sendProgress": false, "sendMaxRetries": 5, "transcriptionProvider": "test"}}"#;
        let cfg: Config = serde_json::from_str(json).unwrap();
        assert!(!cfg.channels.send_progress);
        assert_eq!(cfg.channels.send_max_retries, 5);
        assert_eq!(cfg.channels.transcription_provider, "test");
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_config_match_openai_compat_provider() {
        let json = r#"
{
"providers": {"openai": {"api_key": "test", "model": "gpt-5-mini"}},
"channels": {"sendProgress": false, "sendMaxRetries": 5, "transcriptionProvider": "test"}
}
        "#;
        let cfg: Config = serde_json::from_str(json).unwrap();
        let p = cfg.get_provider(Some("gpt-5-mini"));
        let name = cfg.get_provider_name(Some("gpt-5-mini"));
        assert!(p.is_some());
        assert_eq!(p.unwrap().api_key, "test");
        assert!(name.is_some());
        println!("provider: {:?}", name.unwrap());
    }

    #[test]
    fn test_config_match_openai_codex_provider() {
        let json = r#"
{
"providers": {"openai": {"api_key": "test", "model": "gpt-5.2-codex"}},
"channels": {"sendProgress": false, "sendMaxRetries": 5, "transcriptionProvider": "test"},
"tools": {"ssrfWhitelist": ["100.64.0.0/10", "192.168.0.0/16"]}
}
        "#;
        let cfg: Config = serde_json::from_str(json).unwrap();
        let p = cfg.get_provider(Some("gpt-5.2-codex"));
        let name = cfg.get_provider_name(Some("gpt-5.2-codex"));
        let ssrf_whitelist = cfg.tools.ssrf_whitelist.clone();
        assert!(p.is_some());
        assert_eq!(p.unwrap().api_key, "test");
        assert!(name.is_some());
        assert_eq!(ssrf_whitelist, vec!["100.64.0.0/10", "192.168.0.0/16"]);
        println!("provider: {:?}", name.unwrap());
    }
}
