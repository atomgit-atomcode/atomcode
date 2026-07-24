//! MCP type definitions.

use serde::{Deserialize, Serialize};

/// JSON-RPC request wrapper.
#[derive(Debug, Serialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: &'static str,
    pub id: u64,
    pub method: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

/// JSON-RPC response wrapper.
#[derive(Debug, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: u64,
    #[serde(default)]
    pub result: Option<serde_json::Value>,
    #[serde(default)]
    pub error: Option<JsonRpcError>,
}

/// JSON-RPC error.
#[derive(Debug, Deserialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
}

/// MCP initialize result.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
    pub protocol_version: String,
    pub capabilities: ServerCapabilities,
    pub server_info: ServerInfo,
}

/// Server capabilities.
#[derive(Debug, Deserialize, Default)]
pub struct ServerCapabilities {
    #[serde(default)]
    pub tools: Option<ToolsCapability>,
}

/// Tools capability marker.
#[derive(Debug, Deserialize)]
pub struct ToolsCapability {}

/// Server info.
#[derive(Debug, Deserialize)]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
}

/// Tool definition from MCP server.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpToolDefinition {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default, rename = "inputSchema")]
    pub input_schema: serde_json::Value,
    /// Optional MCP tool annotations (`readOnlyHint`, `destructiveHint`, …). Captured
    /// so plan mode can allow read-only external queries. Absent on servers that don't
    /// annotate → treated as unknown (not read-only).
    #[serde(default)]
    pub annotations: Option<McpToolAnnotations>,
}

/// MCP tool behavior hints from `tools/list` (`annotations` object). All optional and
/// advisory — a missing hint means "unknown", handled conservatively.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpToolAnnotations {
    /// The tool does not modify its environment (safe to run during read-only work).
    #[serde(default)]
    pub read_only_hint: Option<bool>,
    /// The tool may perform destructive updates (only meaningful when not read-only).
    #[serde(default)]
    pub destructive_hint: Option<bool>,
}

impl McpToolDefinition {
    /// True only when the server EXPLICITLY annotated this tool `readOnlyHint: true`
    /// AND did NOT also flag it `destructiveHint: true`. A tool that claims to be both
    /// read-only and destructive is contradictory self-attestation, so we fail closed
    /// and treat it as NOT read-only — matching codex, which forces approval whenever
    /// `destructiveHint: true`, even alongside `readOnlyHint`. Conservative throughout:
    /// unknown / unannotated → false.
    pub fn is_read_only(&self) -> bool {
        matches!(
            self.annotations,
            Some(McpToolAnnotations { read_only_hint: Some(true), destructive_hint, .. })
                if destructive_hint != Some(true)
        )
    }
}

/// List tools result.
#[derive(Debug, Deserialize)]
pub struct ListToolsResult {
    pub tools: Vec<McpToolDefinition>,
}

/// Tool call result content.
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum ContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image")]
    Image { data: String, mime_type: String },
    #[serde(rename = "resource")]
    Resource { resource: ResourceContent },
}

/// Resource content in tool result.
#[derive(Debug, Deserialize)]
pub struct ResourceContent {
    #[serde(default)]
    pub uri: String,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub blob: Option<String>,
    #[serde(default)]
    pub mime_type: Option<String>,
}

/// Tool call result.
#[derive(Debug, Deserialize)]
pub struct CallToolResult {
    pub content: Vec<ContentBlock>,
    #[serde(default)]
    pub is_error: bool,
}

/// Server status for display.
#[derive(Debug, Clone, PartialEq)]
pub enum ServerStatus {
    Connecting,
    Connected,
    BlockedUntrusted,
    Failed(String),
    Disconnected,
}

impl std::fmt::Display for ServerStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ServerStatus::Connecting => write!(f, "connecting"),
            ServerStatus::Connected => write!(f, "connected"),
            ServerStatus::BlockedUntrusted => write!(f, "blocked: untrusted project"),
            ServerStatus::Failed(e) => write!(f, "failed: {}", e),
            ServerStatus::Disconnected => write!(f, "disconnected"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_read_only_hint_annotation() {
        let def: McpToolDefinition = serde_json::from_value(serde_json::json!({
            "name": "query_users",
            "description": "list users",
            "inputSchema": {},
            "annotations": { "readOnlyHint": true, "destructiveHint": false }
        }))
        .unwrap();
        assert!(def.is_read_only());
    }

    #[test]
    fn unannotated_or_non_readonly_is_not_read_only() {
        // No annotations object at all.
        let bare: McpToolDefinition =
            serde_json::from_value(serde_json::json!({ "name": "x", "inputSchema": {} })).unwrap();
        assert!(!bare.is_read_only());
        // Annotations present but readOnlyHint absent/false.
        let write: McpToolDefinition = serde_json::from_value(serde_json::json!({
            "name": "delete_user", "inputSchema": {},
            "annotations": { "destructiveHint": true }
        }))
        .unwrap();
        assert!(!write.is_read_only());
    }

    #[test]
    fn destructive_hint_overrides_read_only_hint() {
        // Contradictory self-attestation: readOnlyHint AND destructiveHint both true.
        // Fail closed → NOT read-only, so it still requires approval (codex parity —
        // codex forces approval on destructiveHint:true even alongside readOnlyHint).
        let contradictory: McpToolDefinition = serde_json::from_value(serde_json::json!({
            "name": "wipe", "inputSchema": {},
            "annotations": { "readOnlyHint": true, "destructiveHint": true }
        }))
        .unwrap();
        assert!(
            !contradictory.is_read_only(),
            "destructiveHint:true must veto readOnlyHint"
        );
    }
}
