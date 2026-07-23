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

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
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
    load_mcp_config, merge_http_oauth_mcp_server_into_json_file,
    merge_stdio_mcp_server_into_json_file, McpHttpAuthConfig, McpOAuthConfig, McpServerConfig,
    McpTransportConfig,
};
pub use oauth::{
    login_github_oauth, login_mcp_oauth, refresh_mcp_oauth_token, McpOAuthLoginOptions,
    McpOAuthToken, McpTokenStore,
};
pub use registry::{project_trust_key, McpConnectEvent, McpRegistry};
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
    // Reuse a live registry for the same working dir + config. MCP servers are a
    // property of the PROJECT, not the session, so switching sessions (`/session`,
    // `/resume`) in an unchanged dir must not pay another server cold-start (an
    // `npx …` stdio server can take seconds). A cache hit skips the connect wait
    // and only rebuilds the cheap tool adapters (which just wrap the registry).
    let key = registry_cache_key(project_dir);
    if let Some(registry) = cached_registry(&key) {
        if registry_all_servers_connected(&registry).await {
            let adapters = adapters_for_registry(&registry).await;
            return (registry, adapters, Vec::new());
        }
        // A server is dead/failed. Reconnecting used to happen implicitly (every
        // session switch rebuilt the registry), which re-attempted failed servers
        // and let a transient failure self-heal. Preserve that: drop the stale
        // entry and reconnect below so the failed server gets another try.
        evict_cached_registry(&key);
    }

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let registry = McpRegistry::from_config_background_with_events(project_dir, Some(tx)).share();
    registry.wait_for_initial_connections(CONNECT_TIMEOUT).await;

    let adapters = adapters_for_registry(&registry).await;

    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }

    store_cached_registry(&key, registry.clone());
    (registry, adapters, events)
}

/// Rebuild the kernel `Tool` adapters from a (possibly reused) registry. The
/// expensive part of a connect (process spawn + `initialize` + the connect wait)
/// is already done; this only re-issues a `tools/list` over each LIVE connection,
/// which keeps the tool set fresh (never stale) at a fraction of a cold start.
async fn adapters_for_registry(registry: &Arc<McpRegistry>) -> Vec<Arc<dyn Tool>> {
    registry
        .list_all_tools()
        .await
        .into_iter()
        .map(|info| Arc::new(McpToolAdapter::new(registry.clone(), info)) as Arc<dyn Tool>)
        .collect()
}

/// Whether EVERY server reports `Connected`. Reuse only a fully-healthy registry:
/// if any server failed to connect, reconnect so it gets re-attempted (a rebuild
/// did this on every session switch, so an initial transient failure self-healed —
/// the cache must not defeat that). Returns false for an empty registry.
///
/// NOTE: this reads the LATCHED connect status, not live liveness — a server that
/// dies AFTER connecting still reports `Connected` (there is no transport-level
/// health check). So this gate catches initial-connect failures, not mid-session
/// crashes; recovering a server that died mid-session needs `/mcp reload`. That
/// matches the long-lived-connection model of comparable tools (connections are
/// reused across sessions; a dead server is re-spawned on an explicit reload).
async fn registry_all_servers_connected(registry: &Arc<McpRegistry>) -> bool {
    let statuses = registry.server_statuses().await;
    !statuses.is_empty()
        && statuses
            .iter()
            .all(|(_, status)| matches!(status, ServerStatus::Connected))
}

/// Process-wide cache of live MCP registries, keyed by canonical working dir +
/// a fingerprint of the resolved server config. A config edit changes the
/// fingerprint (→ miss → reconnect); a different project is a different key.
/// Entries hold the registry (and thus its child processes) alive for the
/// process lifetime, which is the point — the reuse.
type RegistryCache = Mutex<HashMap<(PathBuf, u64), Arc<McpRegistry>>>;

fn registry_cache() -> &'static RegistryCache {
    static CACHE: OnceLock<RegistryCache> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn registry_cache_key(project_dir: &Path) -> (PathBuf, u64) {
    let dir = crate::pathnorm::canonicalize(project_dir)
        .unwrap_or_else(|_| project_dir.to_path_buf());
    // `McpServerConfig` isn't Hash/Serialize; its Debug form is stable and covers
    // every field (command, args, env, transport, source), so hash that. A missing
    // config file is `Ok(empty)` (fingerprint of `[]`), not an error; the `Err`
    // arm is only a genuine read/parse failure (e.g. an editor mid-write of
    // `.mcp.json`), where a constant fingerprint keeps the key stable-ish for that
    // window — a reconnect on the next successful read is acceptable.
    let fingerprint = match load_mcp_config(project_dir) {
        Ok(configs) => {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            format!("{configs:?}").hash(&mut hasher);
            hasher.finish()
        }
        Err(_) => 0,
    };
    (dir, fingerprint)
}

fn cached_registry(key: &(PathBuf, u64)) -> Option<Arc<McpRegistry>> {
    registry_cache().lock().ok()?.get(key).cloned()
}

fn store_cached_registry(key: &(PathBuf, u64), registry: Arc<McpRegistry>) {
    if let Ok(mut cache) = registry_cache().lock() {
        // Keep at most one live registry per dir: a config edit produces a new
        // fingerprint, and the stale-config entry must be dropped so its child
        // processes don't linger for the process lifetime.
        cache.retain(|(dir, _), _| dir != &key.0);
        cache.insert(key.clone(), registry);
    }
}

fn evict_cached_registry(key: &(PathBuf, u64)) {
    if let Ok(mut cache) = registry_cache().lock() {
        cache.remove(key);
    }
}

/// Drop any cached registry for `project_dir`, forcing the next
/// [`connect_and_adapt`] to reconnect. `/mcp reload` calls this so a reload
/// genuinely re-spawns the servers (its whole purpose) instead of reusing the
/// cached connections.
pub fn invalidate_registry_cache(project_dir: &Path) {
    let dir = crate::pathnorm::canonicalize(project_dir)
        .unwrap_or_else(|_| project_dir.to_path_buf());
    if let Ok(mut cache) = registry_cache().lock() {
        cache.retain(|(cached_dir, _), _| cached_dir != &dir);
    }
}
