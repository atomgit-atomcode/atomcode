//! MCP (Model Context Protocol) capability: connect external MCP servers over
//! stdio / HTTP(SSE) (with OAuth), discover their tools, and surface them to a
//! kernel `Agent` as kernel `Tool`s (`mcp__{server}__{tool}`).
//!
//! Ported from `atomcode-core::mcp` into L1 with ZERO dependency on core:
//! - the Tool adapter ([`tool`]) targets the kernel trait,
//! - the home/config-dir + console helpers are local ([`util`]),
//! - the core telemetry block is dropped — a driver re-attaches it by observing
//!   [`McpConnectEvent`] (cross-cutting telemetry lives on a seam, not hard-coded
//!   in the registry).
//!
//! # Cache discipline
//! MCP tool defs are part of the provider request's cached prefix. Connect EAGERLY
//! (via [`connect_and_adapt`]) before the first turn so the tools are present from
//! turn 1 and the prefix stays stable. Changing the mounted tool set mid-session is
//! a non-goal (it invalidates the prefix); a `/mcp reload` is modeled as re-spawning
//! the agent with a freshly-built registry (a new prefix generation), never an
//! in-place mutation.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use atomcode_kernel::tool::{Tool, ToolRegistry};

pub mod client;
pub mod config;
pub mod oauth;
pub mod registry;
pub mod tool;
pub mod transport_http;
pub mod transport_stdio;
pub mod types;
mod util;

pub use client::{McpClient, McpToolInfo};
pub use config::{
    load_mcp_config, McpHttpAuthConfig, McpOAuthConfig, McpServerConfig, McpTransportConfig,
};
pub use oauth::{
    login_github_oauth, login_mcp_oauth, refresh_mcp_oauth_token, McpOAuthLoginOptions,
    McpOAuthToken, McpTokenStore,
};
pub use registry::{McpConnectEvent, McpRegistry};
pub use tool::McpToolAdapter;
pub use types::*;

/// Default bound on how long [`connect_and_adapt`] waits for initial server
/// connections before proceeding with whatever connected so far.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Register MCP tool adapters into `reg`; returns their `mcp__…` names so the
/// assembler can chain them into [`ToolRegistry::mount`]. MCP tools are discovered
/// at runtime, so there is no static `mcp_tool_names()` — the caller mounts exactly
/// the names returned here.
pub fn register_mcp_tools(reg: &mut ToolRegistry, adapters: Vec<Arc<dyn Tool>>) -> Vec<String> {
    let mut names = Vec::with_capacity(adapters.len());
    for adapter in adapters {
        names.push(adapter.name().to_string());
        reg.register(adapter);
    }
    names
}

/// High-level integration entry: load `.mcp.json` + `$ATOMCODE_HOME/mcp.json`,
/// connect all configured servers in parallel (each with its own timeout, the whole
/// wait bounded by [`CONNECT_TIMEOUT`]), discover their tools, and return
/// ready-to-mount kernel `Tool` adapters.
///
/// Returns the live [`McpRegistry`] (held — the adapters route calls through it),
/// the discovered adapters, and the connect events emitted so far (for a driver/UI
/// to surface connection status / failures). Servers that fail to connect are
/// skipped; their failure is in the returned events and in
/// [`McpRegistry::server_statuses`].
pub async fn connect_and_adapt(
    project_dir: &Path,
) -> (Arc<McpRegistry>, Vec<Arc<dyn Tool>>, Vec<McpConnectEvent>) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let registry = McpRegistry::from_config_background_with_events(project_dir, Some(tx)).share();
    registry.wait_for_initial_connections(CONNECT_TIMEOUT).await;

    let infos = registry.list_all_tools().await;
    let adapters: Vec<Arc<dyn Tool>> = infos
        .into_iter()
        .map(|info| Arc::new(McpToolAdapter::new(registry.clone(), info)) as Arc<dyn Tool>)
        .collect();

    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }

    (registry, adapters, events)
}
