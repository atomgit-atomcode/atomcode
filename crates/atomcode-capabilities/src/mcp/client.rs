//! MCP client trait and common utilities.

use anyhow::Result;
use async_trait::async_trait;

use super::types::{CallToolResult, InitializeResult, ListToolsResult, ServerStatus};

/// Information about an MCP tool.
#[derive(Debug, Clone)]
pub struct McpToolInfo {
    pub server_name: String,
    pub tool_name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    /// From the server's `annotations.readOnlyHint` — true only when explicitly
    /// declared read-only. Lets plan mode allow read-only external queries.
    pub read_only: bool,
}

/// MCP client trait for communicating with an MCP server.
#[async_trait]
pub trait McpClient: Send + Sync {
    /// Initialize the connection to the MCP server.
    async fn initialize(&mut self) -> Result<InitializeResult>;

    /// List available tools from the server.
    async fn list_tools(&self) -> Result<ListToolsResult>;

    /// Call a tool on the server.
    async fn call_tool(
        &self,
        tool_name: &str,
        arguments: serde_json::Value,
    ) -> Result<CallToolResult>;

    /// Get the server name.
    fn server_name(&self) -> &str;

    /// Get the current server status.
    fn status(&self) -> ServerStatus;
}

/// Shared cancellation diagnostic; transports own their own uncertainty marker.
pub(super) const CANCELLED_TOOL_REQUEST: &str = "MCP tool call was cancelled after dispatch; result is unknown. Reconnect this server and re-observe before acting; the interrupted call was not replayed.";

/// Publish uncertainty synchronously before a cancelled RPC releases its locks.
/// The tag is a nonzero transport generation or HTTP request identity, not data
/// from a tool result. Normal terminals disarm only this guard, never clear an
/// uncertainty marker established by another concurrently cancelled request.
pub(super) struct ToolRequestGuard {
    abandoned: std::sync::Arc<std::sync::atomic::AtomicU64>,
    dispatched: std::sync::atomic::AtomicU64,
}
impl ToolRequestGuard {
    pub(super) fn new(abandoned: std::sync::Arc<std::sync::atomic::AtomicU64>) -> Self {
        Self {
            abandoned,
            dispatched: std::sync::atomic::AtomicU64::new(0),
        }
    }
    pub(super) fn dispatched(&self, tag: u64) {
        self.dispatched
            .store(tag, std::sync::atomic::Ordering::SeqCst);
    }
    pub(super) fn disarm(&self) {
        self.dispatched
            .store(0, std::sync::atomic::Ordering::SeqCst);
    }
}
impl Drop for ToolRequestGuard {
    fn drop(&mut self) {
        let tag = self.dispatched.load(std::sync::atomic::Ordering::SeqCst);
        if tag != 0 {
            self.abandoned
                .store(tag, std::sync::atomic::Ordering::SeqCst);
        }
    }
}
