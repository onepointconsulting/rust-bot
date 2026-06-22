use rust_bot::config::{loader::save_config, schema::{Config, McpServerConfig, McpTransportType, ToolsConfig}};
use std::collections::HashMap;

use crate::config::helpers::read_mcp_env;

#[test]
fn test_create_mcp_server_config() {
    
    let mut headers = HashMap::new();
    let (mcp_server_url, mcp_headers_jwt, _mcp_test_prompt) = read_mcp_env();
    headers.insert("Authorization".to_string(), mcp_headers_jwt.to_string());
    let mcp_server_config = McpServerConfig {
        transport_type: Some(McpTransportType::Sse),
        command: "".to_string(),
        args: Vec::new(),
        env: HashMap::new(),
        url: mcp_server_url.to_string(),
        headers,
        tool_timeout: 30,
        enabled_tools: Vec::new(),
    };
    let cfg = Config {
        tools: ToolsConfig {
            mcp_servers: HashMap::from([("ems".to_string(), mcp_server_config)]),
            ..ToolsConfig::default()
        },
        ..Config::default()
    };
    assert_eq!(cfg.tools.mcp_servers.len(), 1);
    let temp_path = std::env::temp_dir().join(format!(
        "rust-bot-mcp-config-{}.json",
        uuid::Uuid::new_v4()
    ));
    save_config(&cfg, Some(temp_path));
}