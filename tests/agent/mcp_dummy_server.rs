//! Minimal MCP server implementations for integration tests.

use rmcp::ServerHandler;
use rmcp::handler::server::{router::tool::ToolRouter, wrapper::Parameters};
use rmcp::model::{ServerCapabilities, ServerInfo};
use rmcp::{schemars, tool, tool_handler, tool_router};
use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct HelloServer {
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

impl HelloServer {
    pub fn new() -> Self {
        Self { tool_router: Self::tool_router() }
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SayHelloRequest {
    #[schemars(description = "The name to greet")]
    name: String,
}

#[tool_router]
impl HelloServer {
    #[tool(description = "Say hello to someone by name")]
    fn say_hello(&self, Parameters(SayHelloRequest { name }): Parameters<SayHelloRequest>) -> String {
        format!("Hello, {}!", name)
    }
}

#[tool_handler]
impl ServerHandler for HelloServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
    }
}
