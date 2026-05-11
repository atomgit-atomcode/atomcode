//! MCP server registry - manages connections to multiple MCP servers.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::{mpsc, RwLock};

use super::client::{McpClient, McpToolInfo};
use super::config::{load_mcp_config, McpServerConfig};
use super::transport_http::HttpClient;
use super::transport_stdio::StdioClient;
use super::types::ServerStatus;

/// Connection status event sent to listeners when servers connect or fail.
#[derive(Debug, Clone)]
pub enum McpConnectEvent {
    /// Server connected successfully.
    Connected { name: String },
    /// Server connection failed.
    Failed { name: String, error: String },
    /// Non-fatal warning (e.g. tools/list failed after connect).
    Warning { name: String, message: String },
}

/// Registry of connected MCP servers.
pub struct McpRegistry {
    servers: Arc<RwLock<BTreeMap<String, Arc<dyn McpClient>>>>,
    server_timeouts_ms: Arc<RwLock<BTreeMap<String, u64>>>,
    /// Channel for connection status events (used by TUI to display in scrollback).
    connect_events: Option<mpsc::UnboundedSender<McpConnectEvent>>,
    /// Signals when all initial background connections have completed (or failed).
    initial_ready: Arc<tokio::sync::Notify>,
}

impl McpRegistry {
    /// Create a new empty registry.
    pub fn new() -> Self {
        Self {
            servers: Arc::new(RwLock::new(BTreeMap::new())),
            server_timeouts_ms: Arc::new(RwLock::new(BTreeMap::new())),
            connect_events: None,
            initial_ready: Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// Create a registry with a channel for connection events.
    pub fn with_event_channel() -> (Self, mpsc::UnboundedReceiver<McpConnectEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Self {
                servers: Arc::new(RwLock::new(BTreeMap::new())),
                server_timeouts_ms: Arc::new(RwLock::new(BTreeMap::new())),
                connect_events: Some(tx),
                initial_ready: Arc::new(tokio::sync::Notify::new()),
            },
            rx,
        )
    }

    /// Get a clone of the event sender, if configured.
    pub fn event_sender(&self) -> Option<mpsc::UnboundedSender<McpConnectEvent>> {
        self.connect_events.clone()
    }

    /// Load MCP configuration and start connecting to servers in the background.
    /// Returns immediately with an empty registry; servers are added as they connect.
    /// Connection status events are sent through the internal channel if configured.
    pub fn from_config_background(project_dir: &std::path::Path) -> Self {
        Self::from_config_background_with_events(project_dir, None)
    }

    /// Load MCP configuration and start connecting to servers in the background,
    /// with an external event channel for TUI status display.
    pub fn from_config_background_with_events(
        project_dir: &std::path::Path,
        event_tx: Option<mpsc::UnboundedSender<McpConnectEvent>>,
    ) -> Self {
        let mut registry = Self::new();
        // Merge external channel with internal one
        let combined_tx = event_tx.or(registry.connect_events.clone());
        registry.connect_events = combined_tx.clone();

        let configs = match load_mcp_config(project_dir) {
            Ok(c) => c,
            Err(e) => {
                if let Some(tx) = &combined_tx {
                    let _ = tx.send(McpConnectEvent::Failed {
                        name: "config".to_string(),
                        error: format!("Failed to load config: {}", e),
                    });
                }
                return registry;
            }
        };

        if !configs.is_empty() {
            let servers = registry.servers.clone();
            let server_timeouts_ms = registry.server_timeouts_ms.clone();
            let initial_ready = registry.initial_ready.clone();
            tokio::spawn(async move {
                // Connect servers in parallel
                let tasks: Vec<_> = configs
                    .into_iter()
                    .map(|config| {
                        let servers = servers.clone();
                        let server_timeouts_ms = server_timeouts_ms.clone();
                        let tx = combined_tx.clone();
                        async move {
                            let name = config.name.clone();
                            let timeout_ms = config.timeout_ms();
                            let mut client: Box<dyn McpClient> = match &config.config {
                                super::config::McpTransportConfig::Stdio {
                                    command,
                                    args,
                                    env,
                                    timeout_ms,
                                } => Box::new(StdioClient::new(
                                    name.clone(),
                                    command.clone(),
                                    args.clone(),
                                    env.clone(),
                                    *timeout_ms,
                                )),
                                super::config::McpTransportConfig::Http {
                                    url,
                                    headers,
                                    auth,
                                    timeout_ms,
                                } => Box::new(HttpClient::new(
                                    name.clone(),
                                    url.clone(),
                                    headers.clone(),
                                    auth.clone(),
                                    *timeout_ms,
                                )),
                            };

                            match client.initialize().await {
                                Ok(_result) => {
                                    let mut servers = servers.write().await;
                                    servers.insert(name.clone(), Arc::from(client));
                                    drop(servers);
                                    let mut timeouts = server_timeouts_ms.write().await;
                                    timeouts.insert(name.clone(), timeout_ms);
                                    if let Some(tx) = tx {
                                        let _ = tx.send(McpConnectEvent::Connected {
                                            name: name.clone(),
                                        });
                                    }
                                }
                                Err(e) => {
                                    if let Some(tx) = tx {
                                        let _ = tx.send(McpConnectEvent::Failed {
                                            name: name.clone(),
                                            error: format!("{}", e),
                                        });
                                    }
                                }
                            }
                        }
                    })
                    .collect();

                // Wait for all connections to complete (each has its own timeout)
                futures::future::join_all(tasks).await;
                // Signal that initial connections are done
                initial_ready.notify_waiters();
            });
        } else {
            // No servers configured — signal immediately
            registry.initial_ready.notify_waiters();
        }

        registry
    }

    /// Load MCP configuration and connect to all servers (blocking).
    /// Prefer `from_config_background` for non-blocking startup.
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
                auth,
                timeout_ms,
            } => Box::new(HttpClient::new(
                config.name.clone(),
                url.clone(),
                headers.clone(),
                auth.clone(),
                *timeout_ms,
            )),
        };

        client.initialize().await?;

        let mut servers = self.servers.write().await;
        servers.insert(config.name.clone(), Arc::from(client));
        drop(servers);
        let mut timeouts = self.server_timeouts_ms.write().await;
        timeouts.insert(config.name.clone(), config.timeout_ms());

        Ok(())
    }

    /// Timeout budget for a slow tools/list operation on a connected server.
    ///
    /// The transport already has its own request timeout. This outer budget adds
    /// a small grace period so TUI background tasks do not cancel a request right
    /// before the transport timeout/error can surface.
    pub async fn list_tools_timeout(&self, server_name: &str) -> Duration {
        let configured_ms = {
            let timeouts = self.server_timeouts_ms.read().await;
            timeouts.get(server_name).copied().unwrap_or(30_000)
        };
        Duration::from_millis(configured_ms.saturating_add(5_000))
    }

    /// Get all available tools from all connected servers.
    pub async fn list_all_tools(&self) -> Vec<McpToolInfo> {
        // Never hold the registry lock across an .await: list_tools can be slow and
        // status/reload should remain responsive.
        let server_snapshot: Vec<(String, Arc<dyn McpClient>)> = {
            let servers = self.servers.read().await;
            servers
                .iter()
                .map(|(name, client)| (name.clone(), Arc::clone(client)))
                .collect()
        };
        let mut all_tools = Vec::new();

        for (server_name, client) in server_snapshot {
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
                    if let Some(tx) = &self.connect_events {
                        let _ = tx.send(McpConnectEvent::Warning {
                            name: server_name.clone(),
                            message: format!("tools/list failed: {}", e),
                        });
                    } else {
                        eprintln!("[mcp] Failed to list tools from {}: {}", server_name, e);
                    }
                }
            }
        }

        all_tools
    }

    /// Get tools from a single connected server.
    pub async fn list_tools_for_server(&self, server_name: &str) -> Vec<McpToolInfo> {
        let client = {
            let servers = self.servers.read().await;
            servers.get(server_name).map(Arc::clone)
        };
        let Some(client) = client else {
            if let Some(tx) = &self.connect_events {
                let _ = tx.send(McpConnectEvent::Warning {
                    name: server_name.to_string(),
                    message: "tools/list skipped: server not found".to_string(),
                });
            }
            return Vec::new();
        };

        match client.list_tools().await {
            Ok(result) => result
                .tools
                .into_iter()
                .map(|tool| McpToolInfo {
                    server_name: server_name.to_string(),
                    tool_name: tool.name,
                    description: tool.description,
                    input_schema: tool.input_schema,
                })
                .collect(),
            Err(e) => {
                if let Some(tx) = &self.connect_events {
                    let _ = tx.send(McpConnectEvent::Warning {
                        name: server_name.to_string(),
                        message: format!("tools/list failed: {}", e),
                    });
                } else {
                    eprintln!("[mcp] Failed to list tools from {}: {}", server_name, e);
                }
                Vec::new()
            }
        }
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

    /// Wait for initial background connections to complete (or timeout).
    /// Returns immediately if no background connections are pending.
    pub async fn wait_for_initial_connections(&self, timeout: Duration) {
        let _ = tokio::time::timeout(timeout, self.initial_ready.notified()).await;
    }

    /// Get an Arc clone for sharing across threads.
    pub fn share(&self) -> Arc<Self> {
        Arc::new(Self {
            servers: self.servers.clone(),
            server_timeouts_ms: self.server_timeouts_ms.clone(),
            connect_events: self.connect_events.clone(),
            initial_ready: self.initial_ready.clone(),
        })
    }
}

impl McpServerConfig {
    fn timeout_ms(&self) -> u64 {
        match &self.config {
            super::config::McpTransportConfig::Stdio { timeout_ms, .. }
            | super::config::McpTransportConfig::Http { timeout_ms, .. } => {
                timeout_ms.unwrap_or(30_000)
            }
        }
    }
}

impl Default for McpRegistry {
    fn default() -> Self {
        Self::new()
    }
}
