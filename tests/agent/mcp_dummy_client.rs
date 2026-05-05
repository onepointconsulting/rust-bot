//! Minimal MCP client handler for integration tests (handles server→client RPC only).

use rmcp::ClientHandler;
use rmcp::model::ClientInfo;

#[derive(Debug, Clone, Default)]
pub struct DummyMcpClient;

impl ClientHandler for DummyMcpClient {
    fn get_info(&self) -> ClientInfo {
        ClientInfo::default()
    }
}
