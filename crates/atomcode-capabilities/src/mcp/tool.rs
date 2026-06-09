//! Kernel `Tool` adapter — surfaces a discovered MCP tool as a neutral kernel
//! `Tool`. Replaces core's `tool_adapter.rs` (which targeted the core Tool trait
//! with its `definition()`/`approval()` model). Here the adapter speaks the kernel
//! trait directly: split metadata accessors, `risk()` instead of `approval()`, and
//! a non-`Result` `execute` that maps every failure to `ToolResult { is_error: true }`
//! (the kernel PANIC CONTRACT: a tool must never panic).

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;

use atomcode_kernel::tool::{RiskLevel, Tool, ToolContext, ToolResult};

use super::client::McpToolInfo;
use super::registry::McpRegistry;

/// Wraps one MCP tool (`server` + `tool`) as a kernel `Tool`. The LLM sees it as
/// `mcp__{server}__{tool}`; calls route through the shared [`McpRegistry`].
pub struct McpToolAdapter {
    registry: Arc<McpRegistry>,
    server: String,
    tool: String,
    full_name: String,
    description: String,
    schema: serde_json::Value,
}

impl McpToolAdapter {
    /// Build an adapter from a discovered tool's [`McpToolInfo`] and the live
    /// registry that owns the server connection.
    pub fn new(registry: Arc<McpRegistry>, info: McpToolInfo) -> Self {
        let full_name = format!("mcp__{}__{}", info.server_name, info.tool_name);
        let description = if info.description.is_empty() {
            format!(
                "MCP tool from server '{}'. See input schema for details.",
                info.server_name
            )
        } else {
            format!("[MCP:{}] {}", info.server_name, info.description)
        };
        Self {
            registry,
            server: info.server_name,
            tool: info.tool_name,
            full_name,
            description,
            schema: info.input_schema,
        }
    }

    /// The mounted name (`mcp__{server}__{tool}`).
    pub fn full_name(&self) -> &str {
        &self.full_name
    }
}

#[async_trait]
impl Tool for McpToolAdapter {
    fn name(&self) -> &str {
        &self.full_name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters_schema(&self) -> serde_json::Value {
        self.schema.clone()
    }

    /// MCP servers are external, untrusted code; always `Risky` so the
    /// specialization's approval middleware gates the call (the kernel never
    /// sandboxes — see its trust-model contract). The `mcp__{server}__{tool}`
    /// name conveys the origin to the approval prompt.
    fn risk(&self, _args: &str) -> RiskLevel {
        RiskLevel::Risky
    }

    async fn execute(&self, args: &str, _ctx: &ToolContext) -> ToolResult {
        let trimmed = args.trim();
        let arguments: serde_json::Value = if trimmed.is_empty() || trimmed == "{}" {
            json!({})
        } else {
            match serde_json::from_str(trimmed) {
                Ok(v) => v,
                Err(e) => {
                    return ToolResult {
                        call_id: String::new(),
                        content: format!("invalid MCP tool arguments: {e}"),
                        is_error: true,
                    };
                }
            }
        };

        match self
            .registry
            .call_tool(&self.server, &self.tool, arguments)
            .await
        {
            Ok(content) => ToolResult {
                call_id: String::new(),
                content,
                is_error: false,
            },
            Err(e) => ToolResult {
                call_id: String::new(),
                content: e.to_string(),
                is_error: true,
            },
        }
    }
}
