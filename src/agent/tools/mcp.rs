use async_trait::async_trait;
use serde_json::{json, Map, Value};

use std::collections::HashSet;
use std::time::Duration;
use http::HeaderName;
use rmcp::{Peer, RoleClient, ServiceExt};
use rmcp::service::ServiceError;
use rmcp::model::{CallToolRequestParams, RawContent, Tool as McpToolDef};
use rmcp::transport::TokioChildProcess;
use rmcp::transport::streamable_http_client::{
    StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
};
use crate::agent::tools::base::Tool;
use crate::agent::tools::mcp_file_ref::{has_file_reference, FileRefResolver};
use crate::config::schema::{McpServerConfig, McpTransportType};


/// Return the single non-null branch for nullable unions.
fn extract_nullable_branch(options: Value) -> Option<(Value, bool)> {
    if !options.is_array() {
        return None;
    }
    let mut non_null: Vec<Value> = Vec::new();
    let mut saw_null = false;
    for option in options.as_array().unwrap() {
        if !option.is_object() {
            return None;
        }
        if let Some(type_value) = option.get("type") {
            if let Some(type_str) = type_value.as_str() && type_str == "null" {
                saw_null = true;
                continue;
            }
        }
        non_null.push(option.clone());
    }
    if saw_null && non_null.len() == 1 {
        return Some((non_null[0].clone(), true));
    }
    None
}

/// Resolve a local JSON Schema `$ref` against collected `$defs` / `definitions`.
fn resolve_local_ref<'a>(ref_str: &str, defs: &'a Map<String, Value>) -> Option<&'a Value> {
    for prefix in ["#/$defs/", "#/definitions/"] {
        if let Some(name) = ref_str.strip_prefix(prefix) {
            return defs.get(name);
        }
    }
    None
}

/// Collect `$defs` / `definitions` from anywhere in the schema.
///
/// Some MCP/OpenAPI generators nest `$defs` on inner objects while still emitting
/// root-absolute refs like `#/$defs/Name` (EMS `saveEvent` does this).
fn collect_schema_defs(value: &Value, defs: &mut Map<String, Value>) {
    match value {
        Value::Array(items) => {
            for item in items {
                collect_schema_defs(item, defs);
            }
        }
        Value::Object(obj) => {
            for key in ["$defs", "definitions"] {
                if let Some(Value::Object(d)) = obj.get(key) {
                    for (k, v) in d {
                        defs.entry(k.clone()).or_insert_with(|| v.clone());
                    }
                }
            }
            for v in obj.values() {
                collect_schema_defs(v, defs);
            }
        }
        _ => {}
    }
}

/// Inline local `$ref`s so providers that reject `$defs` (e.g. Moonshot) accept the schema.
fn inline_json_schema_refs(schema: &Value) -> Value {
    let mut defs = Map::new();
    collect_schema_defs(schema, &mut defs);

    let mut visiting = HashSet::new();
    let mut result = inline_refs_node(schema, &defs, &mut visiting);
    if let Some(obj) = result.as_object_mut() {
        obj.remove("$defs");
        obj.remove("definitions");
    }
    result
}

fn inline_refs_node(
    value: &Value,
    defs: &Map<String, Value>,
    visiting: &mut HashSet<String>,
) -> Value {
    match value {
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| inline_refs_node(item, defs, visiting))
                .collect(),
        ),
        Value::Object(obj) => {
            if let Some(Value::String(ref_str)) = obj.get("$ref") {
                if visiting.contains(ref_str) {
                    return json!({"type": "object", "properties": {}});
                }
                if let Some(resolved) = resolve_local_ref(ref_str, defs) {
                    visiting.insert(ref_str.clone());
                    let mut inlined = inline_refs_node(resolved, defs, visiting);
                    visiting.remove(ref_str);

                    if let Some(inlined_obj) = inlined.as_object_mut() {
                        for (k, v) in obj {
                            if k == "$ref" {
                                continue;
                            }
                            inlined_obj.insert(k.clone(), inline_refs_node(v, defs, visiting));
                        }
                    }
                    return inlined;
                }
                // Unresolvable $ref — drop it so strict providers (Moonshot) don't 400.
                log::warn!("Unresolved JSON Schema $ref in MCP tool parameters: {ref_str}");
                let mut out = Map::new();
                out.insert("type".to_string(), json!("object"));
                out.insert("properties".to_string(), json!({}));
                for (k, v) in obj {
                    if k == "$ref" {
                        continue;
                    }
                    out.insert(k.clone(), inline_refs_node(v, defs, visiting));
                }
                return Value::Object(out);
            }

            let mut out = Map::new();
            for (k, v) in obj {
                if k == "$defs" || k == "definitions" {
                    continue;
                }
                out.insert(k.clone(), inline_refs_node(v, defs, visiting));
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}


/// Normalize only nullable JSON Schema patterns for tool definitions.
///
/// Mirrors the Python helper `_normalize_schema_for_openai`:
/// - Non-object input → `{"type":"object","properties":{}}`
/// - `"type": ["T","null"]` → `"type":"T", "nullable":true`
/// - `anyOf`/`oneOf` with a single non-null branch → merged + `"nullable":true`
/// - Local `$ref` / `$defs` are inlined (Moonshot and similar reject `$defs`)
/// - `properties` and `items` are recursively normalized
/// - Object schemas always have `"properties"` and `"required"` keys
fn normalize_schema_for_openai(schema: &Value) -> Value {
    let inlined = inline_json_schema_refs(schema);
    normalize_schema_for_openai_inner(&inlined)
}

fn normalize_schema_for_openai_inner(schema: &Value) -> Value {
    let Some(obj) = schema.as_object() else {
        return json!({"type": "object", "properties": {}});
    };

    let mut normalized = obj.clone();

    // Expand nullable type arrays: ["string","null"] → type:"string", nullable:true
    if let Some(raw_type) = normalized.get("type").cloned() {
        if let Some(type_arr) = raw_type.as_array() {
            let non_null: Vec<&Value> = type_arr
                .iter()
                .filter(|t| t.as_str() != Some("null"))
                .collect();
            let saw_null = type_arr.iter().any(|t| t.as_str() == Some("null"));
            if saw_null && non_null.len() == 1 {
                normalized.insert("type".to_string(), non_null[0].clone());
                normalized.insert("nullable".to_string(), Value::Bool(true));
            }
        }
    }

    // Flatten nullable oneOf/anyOf unions
    'outer: for key in ["oneOf", "anyOf"] {
        if let Some(options) = normalized.get(key).cloned() {
            if let Some((branch, _)) = extract_nullable_branch(options) {
                normalized.remove(key);
                if let Some(branch_obj) = branch.as_object() {
                    for (k, v) in branch_obj {
                        normalized.insert(k.clone(), v.clone());
                    }
                }
                normalized.insert("nullable".to_string(), Value::Bool(true));
                break 'outer;
            }
        }
    }

    // Recursively normalize each property schema
    if let Some(props) = normalized.get("properties").cloned() {
        if let Some(props_obj) = props.as_object() {
            let new_props: Map<String, Value> = props_obj
                .iter()
                .map(|(name, prop)| {
                    let v = if prop.is_object() {
                        normalize_schema_for_openai_inner(prop)
                    } else {
                        prop.clone()
                    };
                    (name.clone(), v)
                })
                .collect();
            normalized.insert("properties".to_string(), Value::Object(new_props));
        }
    }

    // Recursively normalize items schema
    if let Some(items) = normalized.get("items").cloned() {
        if items.is_object() {
            normalized.insert("items".to_string(), normalize_schema_for_openai_inner(&items));
        }
    }

    // Recursively normalize composition / additionalProperties schemas
    for key in ["anyOf", "oneOf", "allOf"] {
        if let Some(Value::Array(options)) = normalized.get(key).cloned() {
            let normalized_options: Vec<Value> = options
                .iter()
                .map(|opt| {
                    if opt.is_object() {
                        normalize_schema_for_openai_inner(opt)
                    } else {
                        opt.clone()
                    }
                })
                .collect();
            normalized.insert(key.to_string(), Value::Array(normalized_options));
        }
    }
    if let Some(additional) = normalized.get("additionalProperties").cloned() {
        if additional.is_object() {
            normalized.insert(
                "additionalProperties".to_string(),
                normalize_schema_for_openai_inner(&additional),
            );
        }
    }

    // Non-object schemas are returned without the extra defaults
    if normalized.get("type").and_then(|t| t.as_str()) != Some("object") {
        return Value::Object(normalized);
    }

    // Object schemas must always carry properties and required
    normalized
        .entry("properties".to_string())
        .or_insert_with(|| json!({}));
    normalized
        .entry("required".to_string())
        .or_insert_with(|| json!([]));

    Value::Object(normalized)
}



// ── MCP client connection ─────────────────────────────────────────────────────

/// Error returned by [`connect_mcp_server`].
#[derive(Debug)]
pub enum ConnectMcpError {
    /// No transport type could be determined from the config.
    UnknownTransport,
    /// Spawning the stdio subprocess failed.
    Io(std::io::Error),
    /// The MCP handshake with the server failed.
    Handshake(rmcp::service::ClientInitializeError),
    /// A header name in the config is invalid.
    InvalidHeader(String),
}

impl std::fmt::Display for ConnectMcpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownTransport => write!(f, "could not determine MCP transport type"),
            Self::Io(e) => write!(f, "stdio spawn error: {e}"),
            Self::Handshake(e) => write!(f, "MCP handshake failed: {e}"),
            Self::InvalidHeader(h) => write!(f, "invalid header name: {h}"),
        }
    }
}

impl std::error::Error for ConnectMcpError {}

/// A live MCP client session.
///
/// Holds both the [`Peer`] used to call tools and the underlying
/// [`rmcp::service::RunningService`] that drives the connection.
/// Dropping this value closes the connection.
pub struct McpClient {
    pub peer: Peer<RoleClient>,
    _service: rmcp::service::RunningService<RoleClient, ()>,
}

/// Connect to an MCP server described by `config`.
///
/// Selects the transport based on `config.transport_type`, auto-detecting when
/// `None`:
/// - command non-empty → [`McpTransportType::Stdio`]
/// - url non-empty     → [`McpTransportType::StreamableHttp`]
///
/// Returns a [`McpClient`] whose `peer` field can be passed directly to
/// [`MCPToolWrapper::new`].
pub async fn connect_mcp_server(config: &McpServerConfig) -> Result<McpClient, ConnectMcpError> {
    let transport_type = config.transport_type.as_ref().cloned().or_else(|| {
        if !config.command.is_empty() {
            Some(McpTransportType::Stdio)
        } else if !config.url.is_empty() {
            Some(McpTransportType::StreamableHttp)
        } else {
            None
        }
    }).ok_or(ConnectMcpError::UnknownTransport)?;

    match transport_type {
        McpTransportType::Stdio => connect_stdio(config).await,
        McpTransportType::Sse => connect_http(config, true).await,
        McpTransportType::StreamableHttp => connect_http(config, false).await,
    }
}

async fn connect_stdio(config: &McpServerConfig) -> Result<McpClient, ConnectMcpError> {
    let mut cmd = tokio::process::Command::new(&config.command);
    cmd.args(&config.args);
    for (k, v) in &config.env {
        cmd.env(k, v);
    }

    let transport = TokioChildProcess::new(cmd).map_err(ConnectMcpError::Io)?;
    let service = ().serve(transport).await.map_err(ConnectMcpError::Handshake)?;
    Ok(McpClient { peer: service.peer().clone(), _service: service })
}

async fn connect_http(config: &McpServerConfig, allow_stateless: bool) -> Result<McpClient, ConnectMcpError> {
    let mut transport_config = StreamableHttpClientTransportConfig::with_uri(config.url.as_str())
        .reinit_on_expired_session(true);

    // `Authorization` must go through the dedicated `auth_header` field so that
    // rmcp includes it on every request (including the initial handshake).
    // All other headers are forwarded as `custom_headers`.
    let mut custom_headers = std::collections::HashMap::new();
    for (name, value) in &config.headers {
        if name.eq_ignore_ascii_case("authorization") {
            transport_config = transport_config.auth_header(value.clone());
            log::info!("MCP Authorization header: {}", value);
        } else {
            let header_name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| ConnectMcpError::InvalidHeader(name.clone()))?;
            let header_value = http::HeaderValue::from_str(value)
                .map_err(|_| ConnectMcpError::InvalidHeader(name.clone()))?;
            custom_headers.insert(header_name, header_value);
        }
    }
    if !custom_headers.is_empty() {
        transport_config = transport_config.custom_headers(custom_headers);
    }

    transport_config.allow_stateless = allow_stateless;

    let transport = StreamableHttpClientTransport::from_config(transport_config);
    let service = ().serve(transport).await.map_err(ConnectMcpError::Handshake)?;
    Ok(McpClient { peer: service.peer().clone(), _service: service })
}


/// Keeps the MCP session alive while [`MCPToolWrapper`] values are in use.
///
/// Dropping [`LoadedMcpTools`] closes the connection — keep it in scope until
/// the runner (or any holder of the tool boxes) is finished.
pub struct LoadedMcpTools {
    pub client: McpClient,
    pub tools: Vec<Box<dyn Tool>>,
}

/// Failure to connect or list tools when building [`LoadedMcpTools`].
#[derive(Debug)]
pub enum LoadMcpToolsError {
    Connect(ConnectMcpError),
    ListTools(ServiceError),
}

impl std::fmt::Display for LoadMcpToolsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Connect(e) => write!(f, "{e}"),
            Self::ListTools(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for LoadMcpToolsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Connect(e) => Some(e),
            Self::ListTools(e) => Some(e),
        }
    }
}

/// Connect using `config`, list tools from the server, and wrap each in [`MCPToolWrapper`].
///
/// `server_name` is the prefix passed to [`MCPToolWrapper::new`] (`mcp_{server_name}_{tool}`).
/// Per-tool timeouts use [`McpServerConfig::tool_timeout`] (seconds).
pub async fn load_mcp_tools_from_config(
    config: &McpServerConfig,
    server_name: &str,
) -> Result<LoadedMcpTools, LoadMcpToolsError> {
    load_mcp_tools_with_file_refs(config, server_name, None).await
}

/// Same as [`load_mcp_tools_from_config`], but every wrapped tool can expand
/// `file://` / `$file` arguments through `file_refs`.
///
/// Pass the resolver built from the agent's workspace scope so file references
/// obey the same sandbox as the filesystem tools. `None` disables expansion.
pub async fn load_mcp_tools_with_file_refs(
    config: &McpServerConfig,
    server_name: &str,
    file_refs: Option<FileRefResolver>,
) -> Result<LoadedMcpTools, LoadMcpToolsError> {
    let client = connect_mcp_server(config).await.map_err(LoadMcpToolsError::Connect)?;
    let tools_result = client
        .peer
        .list_tools(None)
        .await
        .map_err(LoadMcpToolsError::ListTools)?;
    let timeout = Duration::from_secs(u64::from(config.tool_timeout));
    let peer = client.peer.clone();
    let tools: Vec<Box<dyn Tool>> = tools_result
        .tools
        .iter()
        .map(|tool_def| {
            Box::new(MCPToolWrapper::with_file_refs(
                peer.clone(),
                server_name,
                tool_def,
                timeout,
                file_refs.clone(),
            )) as Box<dyn Tool>
        })
        .collect();
    Ok(LoadedMcpTools { client, tools })
}


pub struct MCPToolWrapper {
    session: Peer<RoleClient>,
    original_name: String,
    name: String,
    description: String,
    parameters: Value,
    tool_timeout: Duration,
    /// Expands `file://` / `$file` arguments just before dispatch. `None`
    /// disables the feature, in which case sentinels are sent through verbatim.
    file_refs: Option<FileRefResolver>,
}

impl MCPToolWrapper {
    pub fn new(
        session: Peer<RoleClient>,
        server_name: &str,
        tool_def: &McpToolDef,
        tool_timeout: Duration,
    ) -> Self {
        Self::with_file_refs(session, server_name, tool_def, tool_timeout, None)
    }

    /// Same as [`Self::new`] but with file-reference expansion enabled.
    ///
    /// The resolver carries the workspace sandbox, so references are subject to
    /// the same path restrictions as the filesystem tools.
    pub fn with_file_refs(
        session: Peer<RoleClient>,
        server_name: &str,
        tool_def: &McpToolDef,
        tool_timeout: Duration,
        file_refs: Option<FileRefResolver>,
    ) -> Self {
        let original_name = tool_def.name.to_string();
        let name = format!("mcp_{}_{}", server_name, tool_def.name);
        let description = tool_def
            .description
            .as_deref()
            .unwrap_or(&tool_def.name)
            .to_string();
        let json_object = tool_def.input_schema.as_ref();
        let parameters = normalize_schema_for_openai(&Value::Object(json_object.clone()));
        Self {
            session,
            original_name,
            name,
            description,
            parameters: parameters.clone(),
            tool_timeout,
            file_refs,
        }
    }
}



#[async_trait]
impl Tool for MCPToolWrapper {

    fn name(&self) -> String {
        self.name.clone()
    }

    fn description(&self) -> String {
        self.description.clone()
    }

    fn parameters(&self) -> Value {
        self.parameters.clone()
    }

    async fn execute(&self, params: &serde_json::Value) -> String {
        // Substitute any `file://` / `$file` reference with the real file
        // content before dispatch, so binary payloads never have to be emitted
        // as tokens by the model. Failures are returned as tool output rather
        // than sent on, otherwise the server would receive the literal sentinel
        // and reply with a confusing type error.
        let expanded;
        let params = match &self.file_refs {
            Some(resolver) if has_file_reference(params) => match resolver.expand(params) {
                Ok((value, notes)) => {
                    for note in notes {
                        log::info!("MCP tool '{}' expanded file reference: {}", self.name, note);
                    }
                    expanded = value;
                    &expanded
                }
                Err(e) => {
                    log::error!("MCP tool '{}' file reference failed: {}", self.name, e);
                    return format!("(file reference error: {e})");
                }
            },
            _ => params,
        };

        let req = {
            let base = CallToolRequestParams::new(self.original_name.clone());
            match params.as_object().cloned() {
                Some(args) => base.with_arguments(args),
                None => base,
            }
        };

        let result = match tokio::time::timeout(self.tool_timeout, self.session.call_tool(req)).await {
            Err(_elapsed) => {
                log::warn!(
                    "MCP tool '{}' timed out after {}s",
                    self.name,
                    self.tool_timeout.as_secs()
                );
                return format!(
                    "(MCP tool call timed out after {}s)",
                    self.tool_timeout.as_secs()
                );
            }
            Ok(Err(e)) => {
                log::error!("MCP tool '{}' failed: {}", self.name, e);
                return format!("(MCP tool call failed: {})", e);
            }
            Ok(Ok(r)) => r,
        };

        let parts: Vec<String> = result
            .content
            .into_iter()
            .map(|block| match block.raw {
                RawContent::Text(t) => t.text,
                other => format!("{other:?}"),
            })
            .collect();

        if parts.is_empty() {
            "(no output)".to_string()
        } else {
            parts.join("\n")
        }
    }
    
    /// MCP servers validate their own arguments and their advertised schemas are
    /// often lossy (e.g. Java byte[] rendered as array-of-string while the server
    /// also accepts base64). Do not pre-reject; let the server decide.
    fn validate_params(&self, params: &serde_json::Value) -> Vec<String> {
        mcp_validate_params(params)
    }

    /// Pass arguments through to the MCP server unchanged.
    ///
    /// The default [`Tool::cast_params`] coerces values to match the advertised
    /// schema, which is actively harmful for MCP tools:
    ///
    /// * Remote schemas are frequently lossy or plain wrong. A Java `byte[]`
    ///   parameter is rendered as `{"type":"array","items":{"type":"string"}}`,
    ///   even though the server's Jackson deserializer also accepts a base64
    ///   string and an array of raw numbers.
    /// * The `"string"` branch of `_cast_value` uses `Value::to_string()`, which
    ///   is *serialisation*, not display. Under an `items: {type: string}`
    ///   schema an honest byte array `[255, 216]` is silently rewritten to
    ///   `["255", "216"]`, and structured objects get stringified into JSON text
    ///   that the server then rejects.
    ///
    /// The server is the authority on its own wire format, so we send exactly
    /// what the model produced.
    fn cast_params(&self, params: &serde_json::Value) -> serde_json::Value {
        mcp_cast_params(params)
    }
}

/// Argument validation for MCP tools: always accept.
///
/// Free function so the behaviour is unit-testable without a live MCP session.
/// See [`MCPToolWrapper::validate_params`] for the rationale.
fn mcp_validate_params(_params: &serde_json::Value) -> Vec<String> {
    Vec::new()
}

/// Argument casting for MCP tools: identity.
///
/// Free function so the behaviour is unit-testable without a live MCP session.
/// See [`MCPToolWrapper::cast_params`] for the rationale.
fn mcp_cast_params(params: &serde_json::Value) -> serde_json::Value {
    params.clone()
}




#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── positive cases ────────────────────────────────────────────────────────

    #[test]
    fn test_nullable_string_returns_string_branch() {
        // Typical JSON Schema nullable string: anyOf: [{type:string},{type:null}]
        let options = json!([{"type": "string"}, {"type": "null"}]);
        let result = extract_nullable_branch(options);
        assert!(result.is_some());
        let (branch, is_nullable) = result.unwrap();
        assert_eq!(branch, json!({"type": "string"}));
        assert!(is_nullable);
    }

    #[test]
    fn test_nullable_object_returns_object_branch() {
        let options = json!([{"type": "null"}, {"type": "object", "properties": {}}]);
        let (branch, is_nullable) = extract_nullable_branch(options).unwrap();
        assert_eq!(branch["type"], "object");
        assert!(is_nullable);
    }

    #[test]
    fn test_null_branch_can_appear_first_or_last() {
        // null first
        let opts_null_first = json!([{"type": "null"}, {"type": "integer"}]);
        let (b1, _) = extract_nullable_branch(opts_null_first).unwrap();
        assert_eq!(b1["type"], "integer");

        // null last
        let opts_null_last = json!([{"type": "integer"}, {"type": "null"}]);
        let (b2, _) = extract_nullable_branch(opts_null_last).unwrap();
        assert_eq!(b2["type"], "integer");
    }

    // ── negative cases ────────────────────────────────────────────────────────

    #[test]
    fn test_non_array_input_returns_none() {
        assert!(extract_nullable_branch(json!({"type": "string"})).is_none());
        assert!(extract_nullable_branch(json!("string")).is_none());
        assert!(extract_nullable_branch(json!(null)).is_none());
    }

    #[test]
    fn test_no_null_branch_returns_none() {
        // Two non-null branches — not a simple nullable union.
        let options = json!([{"type": "string"}, {"type": "integer"}]);
        assert!(extract_nullable_branch(options).is_none());
    }

    #[test]
    fn test_multiple_non_null_branches_returns_none() {
        // null + two non-null types — ambiguous, should return None.
        let options = json!([{"type": "string"}, {"type": "integer"}, {"type": "null"}]);
        assert!(extract_nullable_branch(options).is_none());
    }

    #[test]
    fn test_array_containing_non_object_returns_none() {
        // A bare string in the array is not a valid JSON Schema type object.
        let options = json!(["string", {"type": "null"}]);
        assert!(extract_nullable_branch(options).is_none());
    }

    #[test]
    fn test_only_null_branch_returns_none() {
        let options = json!([{"type": "null"}]);
        assert!(extract_nullable_branch(options).is_none());
    }

    // ── normalize_schema_for_openai ───────────────────────────────────────────

    #[test]
    fn test_normalize_non_dict_returns_empty_object_schema() {
        for bad in [json!("string"), json!(42), json!(null), json!([1, 2])] {
            let result = normalize_schema_for_openai(&bad);
            assert_eq!(result, json!({"type": "object", "properties": {}}));
        }
    }

    #[test]
    fn test_normalize_nullable_type_list() {
        let schema = json!({"type": ["string", "null"]});
        let result = normalize_schema_for_openai(&schema);
        assert_eq!(result["type"], "string");
        assert_eq!(result["nullable"], true);
        // Non-object type: no properties/required injected
        assert!(result.get("properties").is_none());
    }

    #[test]
    fn test_normalize_nullable_type_list_null_first() {
        let schema = json!({"type": ["null", "integer"]});
        let result = normalize_schema_for_openai(&schema);
        assert_eq!(result["type"], "integer");
        assert_eq!(result["nullable"], true);
    }

    #[test]
    fn test_normalize_any_of_nullable() {
        let schema = json!({"anyOf": [{"type": "string"}, {"type": "null"}]});
        let result = normalize_schema_for_openai(&schema);
        assert_eq!(result["type"], "string");
        assert_eq!(result["nullable"], true);
        assert!(result.get("anyOf").is_none());
    }

    #[test]
    fn test_normalize_one_of_nullable() {
        let schema = json!({"oneOf": [{"type": "null"}, {"type": "number"}]});
        let result = normalize_schema_for_openai(&schema);
        assert_eq!(result["type"], "number");
        assert_eq!(result["nullable"], true);
        assert!(result.get("oneOf").is_none());
    }

    #[test]
    fn test_normalize_one_of_non_nullable_left_alone() {
        // Two non-null branches — not a simple nullable union, left untouched
        let schema = json!({"oneOf": [{"type": "string"}, {"type": "integer"}]});
        let result = normalize_schema_for_openai(&schema);
        assert!(result.get("oneOf").is_some());
        assert!(result.get("nullable").is_none());
    }

    #[test]
    fn test_normalize_object_gets_default_properties_and_required() {
        let schema = json!({"type": "object"});
        let result = normalize_schema_for_openai(&schema);
        assert_eq!(result["properties"], json!({}));
        assert_eq!(result["required"], json!([]));
    }

    #[test]
    fn test_normalize_object_existing_properties_preserved() {
        let schema = json!({"type": "object", "properties": {"name": {"type": "string"}}, "required": ["name"]});
        let result = normalize_schema_for_openai(&schema);
        assert_eq!(result["properties"]["name"]["type"], "string");
        assert_eq!(result["required"], json!(["name"]));
    }

    #[test]
    fn test_normalize_recursive_properties() {
        // A nested nullable field inside properties should also be normalized
        let schema = json!({
            "type": "object",
            "properties": {
                "tag": {"type": ["string", "null"]}
            }
        });
        let result = normalize_schema_for_openai(&schema);
        let tag = &result["properties"]["tag"];
        assert_eq!(tag["type"], "string");
        assert_eq!(tag["nullable"], true);
    }

    #[test]
    fn test_normalize_recursive_items() {
        let schema = json!({
            "type": "array",
            "items": {"anyOf": [{"type": "number"}, {"type": "null"}]}
        });
        let result = normalize_schema_for_openai(&schema);
        assert_eq!(result["items"]["type"], "number");
        assert_eq!(result["items"]["nullable"], true);
    }

    #[test]
    fn test_normalize_non_object_schema_no_extra_keys_injected() {
        let schema = json!({"type": "string", "minLength": 1});
        let result = normalize_schema_for_openai(&schema);
        assert!(result.get("properties").is_none());
        assert!(result.get("required").is_none());
        assert_eq!(result["minLength"], 1);
    }

    #[test]
    fn test_normalize_passthrough_non_dict_property_value() {
        // Property values that are not objects should pass through unchanged
        let schema = json!({
            "type": "object",
            "properties": {
                "raw": true
            }
        });
        let result = normalize_schema_for_openai(&schema);
        assert_eq!(result["properties"]["raw"], true);
    }

    #[test]
    fn test_normalize_inlines_defs_refs() {
        // Mirrors EMS / Moonshot failure: nested $ref to #/$defs/ISimpleDate
        let schema = json!({
            "type": "object",
            "properties": {
                "eventData": {
                    "type": "object",
                    "properties": {
                        "deleteDateList": {
                            "type": "array",
                            "items": { "$ref": "#/$defs/ISimpleDate" }
                        }
                    }
                }
            },
            "$defs": {
                "ISimpleDate": {
                    "type": "object",
                    "properties": {
                        "year": { "type": "integer" },
                        "month": { "type": "integer" },
                        "day": { "type": "integer" }
                    },
                    "required": ["year", "month", "day"]
                }
            }
        });
        let result = normalize_schema_for_openai(&schema);
        assert!(result.get("$defs").is_none());
        let items = &result["properties"]["eventData"]["properties"]["deleteDateList"]["items"];
        assert!(items.get("$ref").is_none());
        assert_eq!(items["type"], "object");
        assert_eq!(items["properties"]["year"]["type"], "integer");
        assert_eq!(items["required"], json!(["year", "month", "day"]));
    }

    #[test]
    fn test_normalize_inlines_nested_defs_with_root_absolute_ref() {
        // EMS places $defs under properties.eventData but refs #/$defs/...
        let schema = json!({
            "type": "object",
            "properties": {
                "eventData": {
                    "$defs": {
                        "ISimpleDate": { "type": "object" }
                    },
                    "type": "object",
                    "properties": {
                        "deleteDateList": {
                            "type": "array",
                            "items": { "$ref": "#/$defs/ISimpleDate" }
                        }
                    }
                }
            }
        });
        let result = normalize_schema_for_openai(&schema);
        let event_data = &result["properties"]["eventData"];
        assert!(event_data.get("$defs").is_none());
        let items = &event_data["properties"]["deleteDateList"]["items"];
        assert!(items.get("$ref").is_none());
        assert_eq!(items["type"], "object");
    }

    #[test]
    fn test_normalize_inlines_definitions_refs() {
        let schema = json!({
            "type": "object",
            "properties": {
                "when": { "$ref": "#/definitions/Date" }
            },
            "definitions": {
                "Date": { "type": "string", "format": "date" }
            }
        });
        let result = normalize_schema_for_openai(&schema);
        assert!(result.get("definitions").is_none());
        assert_eq!(result["properties"]["when"]["type"], "string");
        assert_eq!(result["properties"]["when"]["format"], "date");
    }

    #[test]
    fn test_normalize_breaks_circular_refs() {
        let schema = json!({
            "type": "object",
            "properties": {
                "node": { "$ref": "#/$defs/Node" }
            },
            "$defs": {
                "Node": {
                    "type": "object",
                    "properties": {
                        "child": { "$ref": "#/$defs/Node" }
                    }
                }
            }
        });
        let result = normalize_schema_for_openai(&schema);
        let child = &result["properties"]["node"]["properties"]["child"];
        assert!(child.get("$ref").is_none());
        assert_eq!(child["type"], "object");
    }

    // ── MCP argument passthrough ──────────────────────────────────────────────
    //
    // Regression coverage for binary/base64 uploads. A real MCP server (EMS,
    // Spring/Jackson) advertises a Java `byte[]` parameter as
    // `{"type":"array","items":{"type":"string"}}` while actually accepting a
    // base64 string, an array of numbers, and an array of numeric strings.
    // The default `Tool` behaviour rejected the base64 form outright and
    // rewrote numeric arrays, so both overrides must stay in place.

    /// Stub tool reproducing the lossy `saveEventImageForEventId` schema.
    struct LossySchemaTool;

    #[async_trait]
    impl Tool for LossySchemaTool {
        fn name(&self) -> String {
            "mcp_ems_saveEventImageForEventId".to_string()
        }
        fn description(&self) -> String {
            String::new()
        }
        fn parameters(&self) -> serde_json::Value {
            json!({
                "type": "object",
                "properties": {
                    "eventId": {"type": "integer", "format": "int64"},
                    "content": {"type": "array", "items": {"type": "string"}},
                    "imagePos": {"type": "integer", "format": "int32"},
                    "name": {"type": "string"}
                },
                "required": ["eventId", "content", "imagePos", "name"]
            })
        }
        async fn execute(&self, _params: &serde_json::Value) -> String {
            String::new()
        }
        // Deliberately inherits the default cast_params / validate_params.
    }

    /// The default trait impl is what broke base64 uploads: a plain string is
    /// rejected against an `array` schema before the request ever leaves.
    #[test]
    fn test_default_validation_rejects_base64_string() {
        let tool = LossySchemaTool;
        let params = json!({
            "eventId": 7255314,
            "content": "/9j/4AAQSkZJRg==",
            "imagePos": 4,
            "name": "probe.jpg"
        });
        let errors = tool.validate_params(&params);
        assert!(
            errors.iter().any(|e| e.contains("should be array")),
            "expected default impl to reject base64 string, got {errors:?}"
        );
    }

    /// The default cast also mangles an honest byte array into strings.
    #[test]
    fn test_default_cast_mangles_numeric_byte_array() {
        let tool = LossySchemaTool;
        let params = json!({"content": [255, 216, 255, 217]});
        let cast = tool.cast_params(&params);
        assert_eq!(
            cast["content"],
            json!(["255", "216", "255", "217"]),
            "default cast should stringify bytes (documents the bug being fixed)"
        );
    }

    /// `MCPToolWrapper::validate_params` must accept every wire form the server
    /// understands, including the base64 string the default impl rejected.
    #[test]
    fn test_mcp_validate_params_accepts_any_shape() {
        for content in [
            json!("/9j/4AAQSkZJRg=="),
            json!([255, 216, 255, 217]),
            json!(["255", "216"]),
            json!(null),
        ] {
            let params = json!({"content": content});
            // An empty error list means `ToolRegistry::prepare_call` proceeds
            // to execute instead of failing the call up front.
            let errors = mcp_validate_params(&params);
            assert!(
                errors.is_empty(),
                "MCP wrapper must not pre-reject params: {params}"
            );
        }
    }

    /// `MCPToolWrapper::cast_params` must be an identity transform so binary
    /// payloads reach the server byte-for-byte.
    #[test]
    fn test_mcp_cast_params_is_identity() {
        let params = json!({
            "eventId": 7255314,
            "content": [255, 216, 255, 217],
            "imagePos": 4,
            "name": "probe.jpg"
        });
        // Identity contract: bytes stay numbers, integers stay integers.
        let cast = mcp_cast_params(&params);
        assert_eq!(cast, params);
        assert_eq!(cast["content"][0], json!(255));
        assert!(cast["content"][0].is_number());
        assert!(cast["eventId"].is_number());
    }

    use crate::config::schema::McpServerConfig;

    #[tokio::test]
    async fn load_mcp_tools_fails_unknown_transport() {
        let cfg = McpServerConfig::default();
        assert!(matches!(
            load_mcp_tools_from_config(&cfg, "test").await,
            Err(LoadMcpToolsError::Connect(ConnectMcpError::UnknownTransport)),
        ));
    }
}
