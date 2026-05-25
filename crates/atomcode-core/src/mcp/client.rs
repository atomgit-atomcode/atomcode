//! MCP client trait and common utilities.

use anyhow::Result;
use async_trait::async_trait;

use super::types::{InitializeResult, ListToolsResult, CallToolResult, ServerStatus};

/// Information about an MCP tool.
#[derive(Debug, Clone)]
pub struct McpToolInfo {
    pub server_name: String,
    pub tool_name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// MCP client trait for communicating with an MCP server.
#[async_trait]
pub trait McpClient: Send + Sync {
    /// Initialize the connection to the MCP server.
    async fn initialize(&mut self) -> Result<InitializeResult>;

    /// List available tools from the server.
    async fn list_tools(&self) -> Result<ListToolsResult>;

    /// Call a tool on the server.
    async fn call_tool(&self, tool_name: &str, arguments: serde_json::Value) -> Result<CallToolResult>;

    /// Get the server name.
    fn server_name(&self) -> &str;

    /// Get the current server status.
    fn status(&self) -> ServerStatus;
}
