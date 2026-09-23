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
//! # Runtime boundary
//! This module owns transport, discovery, trust, and tool adaptation. It does not
//! own a coding session transition or decide when discovered tools become visible.
//! The embedding runtime may connect in the background and atomically publish a new
//! per-turn tool catalog; non-interactive surfaces may instead await readiness.
//!
//! # Panic discipline (trust boundary)
//! An MCP server is an EXTERNAL process (a stdio child / a remote HTTP peer) whose
//! output AtomCode does not control. Production code in this module must turn every
//! malformed input (illegal JSON-RPC, unexpected schema, non-JSON stdout, binary
//! noise) into a RECOVERABLE `Result` error, NEVER a panic — the release profile is
//! `panic = "abort"`, so a single `unwrap()` on a bad server response would abort the
//! whole session instead of degrading (mark the server `Failed`, keep the session
//! alive). The lint below denies `unwrap`/`expect`/`panic!` in production to lock that
//! in against regressions; `#[cfg(test)]` code keeps them for assertions.
#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]

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
pub mod trust;
pub mod types;
mod util;

pub use client::{McpClient, McpToolInfo};
pub use config::{
    config_path_for_source, load_mcp_config, load_mcp_config_including_disabled,
    merge_http_oauth_mcp_server_into_json_file, merge_stdio_mcp_server_into_json_file,
    McpConfigSource, McpHttpAuthConfig, McpOAuthConfig, McpServerConfig, McpTransportConfig,
    McpTransportKind,
};
pub use oauth::{
    login_github_oauth, login_mcp_oauth, login_mcp_oauth_until, refresh_mcp_oauth_token,
    token_is_expired, McpOAuthLoginOptions, McpOAuthLoginStop, McpOAuthStep, McpOAuthToken,
    McpTokenStore,
};
pub use registry::{project_trust_key, ConnectAttempt, McpConnectEvent, McpRegistry};
pub use tool::{mcp_tool_full_name, sanitize_name_segment, McpToolAdapter};
pub use types::*;

/// Default bound used by callers that explicitly require initial MCP readiness.
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
