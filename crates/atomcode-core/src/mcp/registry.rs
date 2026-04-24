//! MCP server registry - manages connections to multiple MCP servers.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::Result;
use tokio::sync::RwLock;

use super::client::{McpClient, McpToolInfo};
use super::config::{load_mcp_config, McpServerConfig};
use super::transport_stdio::StdioClient;
use super::transport_http::HttpClient;
use super::types::ServerStatus;

/// Registry of connected MCP servers.
pub struct McpRegistry {
    servers: Arc<RwLock<BTreeMap<String, Box<dyn McpClient>>>>,
}

impl McpRegistry {
    /// Create a new empty registry.
    pub fn new() -> Self {
        Self {
            servers: Arc::new(RwLock::new(BTreeMap::new())),
        }
    }

    /// Load MCP configuration and connect to all servers.
    pub async fn from_config(project_dir: &std::path::Path) -> Self {
        let registry = Self::new();

        let configs = match load_mcp_config(project_dir) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[mcp] Failed to load config: {}", e);
                return registry;
            }
        };

        for config in configs {
            if let Err(e) = registry.add_server(config).await {
                eprintln!("[mcp] Failed to connect server: {}", e);
            }
        }

        registry
    }

    /// Add a server to the registry.
    pub async fn add_server(&self, config: McpServerConfig) -> Result<()> {
        let mut client: Box<dyn McpClient> = match &config.config {
            super::config::McpTransportConfig::Stdio {
                command,
                args,
                env,
                timeout_ms,
            } => Box::new(StdioClient::new(
                config.name.clone(),
                command.clone(),
                args.clone(),
                env.clone(),
                *timeout_ms,
            )),
            super::config::McpTransportConfig::Http {
                url,
                headers,
                timeout_ms,
            } => Box::new(HttpClient::new(
                config.name.clone(),
                url.clone(),
                headers.clone(),
                *timeout_ms,
            )),
        };

        client.initialize().await?;

        let mut servers = self.servers.write().await;
        servers.insert(config.name.clone(), client);

        Ok(())
    }

    /// Get all available tools from all connected servers.
    pub async fn list_all_tools(&self) -> Vec<McpToolInfo> {
        let servers = self.servers.read().await;
        let mut all_tools = Vec::new();

        for (server_name, client) in servers.iter() {
            match client.list_tools().await {
                Ok(result) => {
                    for tool in result.tools {
                        all_tools.push(McpToolInfo {
                            server_name: server_name.clone(),
                            tool_name: tool.name,
                            description: tool.description,
                            input_schema: tool.input_schema,
                        });
                    }
                }
                Err(e) => {
                    eprintln!("[mcp] Failed to list tools from {}: {}", server_name, e);
                }
            }
        }

        all_tools
    }

    /// Call a tool on a specific server.
    pub async fn call_tool(
        &self,
        server_name: &str,
        tool_name: &str,
        arguments: serde_json::Value,
    ) -> Result<String> {
        let servers = self.servers.read().await;
        let client = servers
            .get(server_name)
            .ok_or_else(|| anyhow::anyhow!("MCP server '{}' not found", server_name))?;

        let result = client.call_tool(tool_name, arguments).await?;

        // Extract text from content blocks
        let output = result
            .content
            .into_iter()
            .filter_map(|c| match c {
                super::types::ContentBlock::Text { text } => Some(text),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");

        if result.is_error {
            anyhow::bail!("MCP tool error: {}", output);
        }

        Ok(output)
    }

    /// Get the status of all servers.
    pub async fn server_statuses(&self) -> Vec<(String, ServerStatus)> {
        let servers = self.servers.read().await;
        servers
            .iter()
            .map(|(name, client)| (name.clone(), client.status()))
            .collect()
    }

    /// Get an Arc clone for sharing across threads.
    pub fn share(&self) -> Arc<Self> {
        Arc::new(Self {
            servers: self.servers.clone(),
        })
    }
}

impl Default for McpRegistry {
    fn default() -> Self {
        Self::new()
    }
}
