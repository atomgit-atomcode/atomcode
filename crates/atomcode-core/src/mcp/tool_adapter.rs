//! Tool adapter - exposes MCP tools through the Tool trait.

use anyhow::Result;
use async_trait::async_trait;
use serde_json::json;

use crate::tool::{ApprovalRequirement, Tool, ToolContext, ToolDef, ToolResult};

use super::client::McpToolInfo;
use super::registry::McpRegistry;

/// Adapter that wraps an MCP tool and implements the Tool trait.
pub struct McpToolAdapter {
    registry: std::sync::Arc<McpRegistry>,
    info: McpToolInfo,
    tool_name: String, // mcp__{server}__{tool}
}

impl McpToolAdapter {
    /// Create a new adapter for an MCP tool.
    pub fn new(registry: std::sync::Arc<McpRegistry>, info: McpToolInfo) -> Self {
        let tool_name = format!("mcp__{}__{}", info.server_name, info.tool_name);
        Self {
            registry,
            info,
            tool_name,
        }
    }

    /// Get the full tool name (mcp__{server}__{tool}).
    pub fn full_name(&self) -> &str {
        &self.tool_name
    }
}

#[async_trait]
impl Tool for McpToolAdapter {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: Box::leak(self.tool_name.clone().into_boxed_str()),
            description: if self.info.description.is_empty() {
                format!(
                    "MCP tool from server '{}'. See input schema for details.",
                    self.info.server_name
                )
            } else {
                format!(
                    "[MCP:{}] {}",
                    self.info.server_name, self.info.description
                )
            },
            parameters: self.info.input_schema.clone(),
        }
    }

    fn approval(&self, args: &str) -> ApprovalRequirement {
        // MCP tools require approval by default - they are external code.
        ApprovalRequirement::RequireApproval(format!(
            "MCP tool '{}' from server '{}' wants to execute with arguments: {}",
            self.info.tool_name, self.info.server_name, args
        ))
    }

    async fn execute(&self, args: &str, _ctx: &ToolContext) -> Result<ToolResult> {
        let arguments: serde_json::Value = if args.is_empty() || args == "{}" {
            json!({})
        } else {
            serde_json::from_str(args)?
        };

        let output = self
            .registry
            .call_tool(&self.info.server_name, &self.info.tool_name, arguments)
            .await?;

        Ok(ToolResult {
            call_id: String::new(),
            output,
            success: true,
        })
    }
}

/// Register all MCP tools from a registry into a ToolRegistry (sync version).
/// Use this at startup when you have mutable access to the registry.
pub fn register_mcp_tools(
    registry: &mut crate::tool::ToolRegistry,
    mcp_registry: std::sync::Arc<McpRegistry>,
    tools: Vec<McpToolInfo>,
) {
    for info in tools {
        let adapter = McpToolAdapter::new(mcp_registry.clone(), info);
        registry.register_sync(Box::new(adapter));
    }
}

/// Register all MCP tools from a registry into a ToolRegistry (async version).
/// Use this when registering tools dynamically after startup.
pub async fn register_mcp_tools_async(
    registry: &crate::tool::ToolRegistry,
    mcp_registry: std::sync::Arc<McpRegistry>,
    tools: Vec<McpToolInfo>,
) {
    for info in tools {
        let adapter = McpToolAdapter::new(mcp_registry.clone(), info);
        registry.register(Box::new(adapter)).await;
    }
}
