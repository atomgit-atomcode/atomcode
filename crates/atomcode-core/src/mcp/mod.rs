//! MCP (Model Context Protocol) integration module.
//!
//! This module provides support for connecting to MCP servers and exposing
//! their tools through AtomCode's ToolRegistry.

pub mod client;
pub mod config;
pub mod registry;
pub mod tool_adapter;
pub mod transport_http;
pub mod transport_stdio;
pub mod types;

pub use client::{McpClient, McpToolInfo};
pub use config::{
    load_mcp_config, merge_stdio_mcp_server_into_json_file, McpServerConfig, McpTransportConfig,
};
pub use registry::{McpConnectEvent, McpRegistry};
pub use tool_adapter::{McpToolAdapter, register_mcp_tools, register_mcp_tools_async};
pub use types::*;
