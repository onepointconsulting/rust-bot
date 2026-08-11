use garde::Validate;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Where a preset field value is applied on the resulting `MCPServerConfig`.
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum McpPresetFieldTargetKind {
    Env,
    UrlParam,
    Arg,
    Header,
}

fn default_mcp_preset_field_secret() -> bool {
    true
}
fn default_mcp_preset_field_required() -> bool {
    true
}

/// User-supplied configuration field a built-in MCP preset needs (an API key, token, etc.), and how to plug that value into the resulting MCPServerConfig
#[derive(Debug, Deserialize, Serialize, Validate, Clone)]
#[serde(rename_all = "camelCase")]
pub struct McpPresetField {

    /// The field's identifier, used as the query-param key when the WebUI submits a value (e.g. "browserbase_api_key")
    #[serde(alias = "name")]
    #[garde(skip)]
    pub name: String,

    /// Human-readable label shown in the settings UI (e.g. "Browserbase API key")
    #[serde(alias = "label")]
    #[garde(skip)]
    pub label: String,

    /// `(kind, name)` describing where the value is written — e.g. `("env", "BRAVE_API_KEY")`,
    /// `("url_param", "browserbaseApiKey")`, `("arg", "--api-key")`, `("header", "Authorization")`.
    #[serde(alias = "target")]
    #[garde(skip)]
    pub target: (McpPresetFieldTargetKind, String),

    /// Whether the value should be treated/displayed as sensitive (defaults True).
    #[serde(alias = "secret", default = "default_mcp_preset_field_secret")]
    #[garde(skip)]
    pub secret: bool,

    /// Whether the field must be provided when installing the preset (defaults True).
    #[serde(alias = "required", default = "default_mcp_preset_field_required")]
    #[garde(skip)]
    pub required: bool,

    /// The name of an environment variable that can supply the value automatically (also used to report "configured via env" status)
    #[serde(alias = "env_var", default)]
    #[garde(skip)]
    pub env_var: String,

    /// Example text shown in the input field (e.g. "ghp_...")
    #[serde(alias = "placeholder", default)]
    #[garde(skip)]
    pub placeholder: String,
}

/// Transport used by a built-in MCP preset (`stdio` / `streamableHttp` / `sse` / `oauth`).
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum McpPresetTransport {
    Stdio,
    StreamableHttp,
    Sse,
    Oauth,
}

/// Built-in MCP server preset shown in the WebUI settings catalog.
#[derive(Debug, Deserialize, Serialize, Validate, Clone)]
#[serde(rename_all = "camelCase")]
pub struct McpPreset {
    /// How the preset connects: stdio, streamableHttp, sse, or oauth.
    #[serde(alias = "transport")]
    #[garde(skip)]
    pub transport: McpPresetTransport,
}

/// Sanitize structured MCP preset mentions sent by the WebUI.
pub fn normalize_mcp_preset_mentions(raw: Option<&serde_json::Value>) -> Vec<HashMap<String, String>> {
    let Some(serde_json::Value::Array(items)) = raw else {
        return vec![];
    };
    vec![]
}