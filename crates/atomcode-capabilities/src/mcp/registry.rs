//! MCP server registry - manages connections to multiple MCP servers.

use std::collections::{BTreeMap, HashSet};
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
    /// Server withheld because it comes from an untrusted project's `.mcp.json`.
    BlockedUntrusted { name: String },
}

/// Canonical trust-store key for a project dir. Its ordinary-path hash stays
/// compatible with the retired core session bucket algorithm.
/// Exposed so tests (and any same-store reader) use ONE implementation.
///
/// The algorithm is pinned by the golden test below:
/// 1. `strip_verbatim_prefix` on the raw string (BEFORE backslash replacement —
///    the prefix contains backslashes that must still be intact).
/// 2. Replace `\\` → `/`.
/// 3. Strip trailing `/` (except a bare root).
/// 4. Lowercase on Windows (case-insensitive filesystem).
/// 5. Hash as `PathBuf` via `DefaultHasher` (component-prefix hashing — NOT `str::hash`).
/// 6. Format as `{:016x}`.
///
/// The shared config helper pins the same ordinary-path literal used by session
/// buckets; this function additionally strips Windows verbatim prefixes.
pub fn project_trust_key(project_dir: &std::path::Path) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    // Step 1: strip verbatim prefix BEFORE backslash replacement (order matters).
    let raw = project_dir.to_string_lossy();
    let stripped = crate::pathnorm::strip_verbatim(&raw);

    // Steps 2–4: backslash normalization, trailing-slash trim, Windows lowercase.
    let mut normalized = stripped.replace('\\', "/");
    if normalized.len() > 1 && normalized.ends_with('/') {
        normalized.pop();
    }
    #[cfg(windows)]
    let normalized = normalized.to_lowercase();

    // Steps 5–6: hash via PathBuf (component-prefix hashing, same as core).
    let mut hasher = DefaultHasher::new();
    let p: std::path::PathBuf = std::path::PathBuf::from(normalized);
    p.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// Check whether `project_dir` is recorded as trusted in the shared MCP trust store.
///
/// This is a LOCAL reimplementation of `atomcode_core::mcp::trust::is_project_trusted`
/// for use within `atomcode-capabilities` (which cannot depend on `atomcode-core` due to
/// layering constraints). Both implementations read the SAME `mcp_trust.json` file, using
/// the same hash scheme (normalize path → hash as `PathBuf` via `DefaultHasher` → `{:016x}`),
/// so core and capabilities agree on trust state at runtime.
///
/// Honors `ATOMCODE_MCP_TRUST_STORE` (the same env-var test seam as core).
fn is_project_trusted_local(project_dir: &std::path::Path) -> bool {
    let key = project_trust_key(project_dir);

    // Locate the trust store (same logic as core's `trust_store_path`).
    let store_path: std::path::PathBuf = {
        if let Ok(p) = std::env::var("ATOMCODE_MCP_TRUST_STORE") {
            if !p.is_empty() {
                std::path::PathBuf::from(p)
            } else {
                super::util::config_dir().join("mcp_trust.json")
            }
        } else {
            super::util::config_dir().join("mcp_trust.json")
        }
    };

    // Parse only what we need: `{ "projects": { "<key>": ... } }`.
    let Ok(bytes) = std::fs::read(&store_path) else {
        return false; // missing => untrusted (fail-closed)
    };
    let Ok(val) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return false; // corrupt => untrusted (fail-closed)
    };
    val.get("projects")
        .and_then(|p| p.as_object())
        .map(|m| m.contains_key(&key))
        .unwrap_or(false)
}

/// Registry of connected MCP servers.
pub struct McpRegistry {
    servers: Arc<RwLock<BTreeMap<String, Arc<dyn McpClient>>>>,
    server_timeouts_ms: Arc<RwLock<BTreeMap<String, u64>>>,
    /// Servers whose initial connect failed. The TUI's `/mcp` listing
    /// surfaces these as `failed: <error>` so a misconfigured server
    /// doesn't silently disappear from the list (#300). Cleared when a
    /// subsequent `add_server(name)` succeeds.
    failed_servers: Arc<RwLock<BTreeMap<String, String>>>,
    /// Channel for connection status events (used by TUI to display in scrollback).
    connect_events: Option<mpsc::UnboundedSender<McpConnectEvent>>,
    /// Signals when all initial background connections have completed (or failed).
    initial_ready: Arc<tokio::sync::Notify>,
    /// LEVEL-triggered mirror of the signal above: the Notify permit is single-use
    /// (the first `wait_for_initial_connections` consumes it), so repeat callers
    /// check this flag and return immediately instead of burning their timeout.
    initial_done: Arc<std::sync::atomic::AtomicBool>,
    /// Servers marked `trust: true` in config ⇒ every tool from them is auto-approved.
    /// `std::sync` (not tokio) RwLock because `Tool::risk` is sync and can't `.await`.
    trusted_servers: Arc<std::sync::RwLock<HashSet<String>>>,
    /// Per-tool auto-approve set, keyed by the full tool name `mcp__{server}__{tool}`
    /// (from a server's `autoApprove` allowlist, or a runtime "Always" grant).
    auto_approved_tools: Arc<std::sync::RwLock<HashSet<String>>>,
}

impl McpRegistry {
    /// Create a new empty registry.
    pub fn new() -> Self {
        Self {
            servers: Arc::new(RwLock::new(BTreeMap::new())),
            server_timeouts_ms: Arc::new(RwLock::new(BTreeMap::new())),
            failed_servers: Arc::new(RwLock::new(BTreeMap::new())),
            connect_events: None,
            initial_ready: Arc::new(tokio::sync::Notify::new()),
            initial_done: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            trusted_servers: Arc::new(std::sync::RwLock::new(HashSet::new())),
            auto_approved_tools: Arc::new(std::sync::RwLock::new(HashSet::new())),
        }
    }

    /// Create a registry with a channel for connection events.
    pub fn with_event_channel() -> (Self, mpsc::UnboundedReceiver<McpConnectEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Self {
                servers: Arc::new(RwLock::new(BTreeMap::new())),
                server_timeouts_ms: Arc::new(RwLock::new(BTreeMap::new())),
                failed_servers: Arc::new(RwLock::new(BTreeMap::new())),
                connect_events: Some(tx),
                initial_ready: Arc::new(tokio::sync::Notify::new()),
                initial_done: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                trusted_servers: Arc::new(std::sync::RwLock::new(HashSet::new())),
                auto_approved_tools: Arc::new(std::sync::RwLock::new(HashSet::new())),
            },
            rx,
        )
    }

    /// Get a clone of the event sender, if configured.
    pub fn event_sender(&self) -> Option<mpsc::UnboundedSender<McpConnectEvent>> {
        self.connect_events.clone()
    }

    /// Whether `server` is configured `trust: true` (auto-approve all its tools).
    pub fn is_server_trusted(&self, server: &str) -> bool {
        self.trusted_servers
            .read()
            .map(|s| s.contains(server))
            .unwrap_or(false)
    }

    /// Whether the full tool name (`mcp__{server}__{tool}`) is auto-approved — on a
    /// server's `autoApprove` allowlist, or granted "Always" at runtime.
    pub fn is_tool_auto_approved(&self, full_name: &str) -> bool {
        self.auto_approved_tools
            .read()
            .map(|s| s.contains(full_name))
            .unwrap_or(false)
    }

    /// Mark a server trusted at runtime (idempotent).
    pub(crate) fn mark_server_trusted(&self, server: &str) {
        if let Ok(mut s) = self.trusted_servers.write() {
            s.insert(server.to_string());
        }
    }

    /// Mark a specific full tool name auto-approved at runtime (idempotent).
    pub(crate) fn mark_tool_auto_approved(&self, full_name: &str) {
        if let Ok(mut s) = self.auto_approved_tools.write() {
            s.insert(full_name.to_string());
        }
    }

    /// Split configs by project trust. Uses the shared trust store (via the local
    /// `is_project_trusted_local` mirror), but partitions on the capabilities-local
    /// `McpConfigSource` (a distinct type from core's). Untrusted => project-source
    /// servers are withheld.
    fn partition_by_trust(
        configs: Vec<McpServerConfig>,
        project_dir: &std::path::Path,
    ) -> (Vec<McpServerConfig>, Vec<McpServerConfig>) {
        if is_project_trusted_local(project_dir) {
            return (configs, Vec::new());
        }
        let (blocked, allowed): (Vec<_>, Vec<_>) = configs
            .into_iter()
            .partition(|c| matches!(c.source, super::config::McpConfigSource::Project));
        (allowed, blocked)
    }

    /// Return the names of all currently connected servers.
    /// Used in tests to verify that blocked servers never entered the connect loop.
    pub async fn connected_server_names(&self) -> Vec<String> {
        self.servers.read().await.keys().cloned().collect()
    }

    /// Seed trust/auto-approve state from a server's config. Trust is a config
    /// property independent of connection success, so this is safe to call before/
    /// regardless of connecting. Idempotent.
    fn apply_trust_from_config(&self, config: &McpServerConfig) {
        if config.trust {
            self.mark_server_trusted(&config.name);
        }
        for tool in &config.auto_approve {
            // Accept either the bare tool name ("query") OR the already-qualified name
            // ("mcp__server__query") that the user sees in the approval prompt — both
            // are plausible in `autoApprove`. Normalize to the full name either way.
            let full = if tool.starts_with("mcp__") {
                tool.clone()
            } else {
                format!("mcp__{}__{}", config.name, tool)
            };
            self.mark_tool_auto_approved(&full);
        }
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
                // Nothing will ever connect — mark done + store the ready permit so
                // a later `wait_for_initial_connections` returns immediately instead
                // of burning its whole timeout (same as the no-server path below).
                registry
                    .initial_done
                    .store(true, std::sync::atomic::Ordering::Release);
                registry.initial_ready.notify_one();
                return registry;
            }
        };

        // Gate: withhold project-source servers from untrusted projects.
        let (configs, blocked) = Self::partition_by_trust(configs, project_dir);
        for b in &blocked {
            if let Some(tx) = &combined_tx {
                let _ = tx.send(McpConnectEvent::BlockedUntrusted {
                    name: b.name.clone(),
                });
            }
        }

        // Seed trust/auto-approve from config up front (the background connect loop
        // below inlines its own connect and never calls `add_server`, so it wouldn't
        // otherwise apply trust). Independent of connection success.
        for config in &configs {
            registry.apply_trust_from_config(config);
        }

        if !configs.is_empty() {
            let servers = registry.servers.clone();
            let server_timeouts_ms = registry.server_timeouts_ms.clone();
            let failed_servers = registry.failed_servers.clone();
            let initial_ready = registry.initial_ready.clone();
            let initial_done = registry.initial_done.clone();
            tokio::spawn(async move {
                // Connect servers in parallel
                let tasks: Vec<_> = configs
                    .into_iter()
                    .map(|config| {
                        let servers = servers.clone();
                        let server_timeouts_ms = server_timeouts_ms.clone();
                        let failed_servers = failed_servers.clone();
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
                                    let mut failed = failed_servers.write().await;
                                    failed.remove(&name);
                                    drop(failed);
                                    if let Some(tx) = tx {
                                        let _ = tx.send(McpConnectEvent::Connected {
                                            name: name.clone(),
                                        });
                                    }
                                }
                                Err(e) => {
                                    let error_str = format!("{}", e);
                                    let mut failed = failed_servers.write().await;
                                    failed.insert(name.clone(), error_str.clone());
                                    drop(failed);
                                    if let Some(tx) = tx {
                                        let _ = tx.send(McpConnectEvent::Failed {
                                            name: name.clone(),
                                            error: error_str.clone(),
                                        });
                                    }
                                }
                            }
                        }
                    })
                    .collect();

                // Wait for all connections to complete (each has its own timeout)
                futures::future::join_all(tasks).await;
                // Signal that initial connections are done. `notify_one` (NOT
                // `notify_waiters`): it STORES a permit when nobody is waiting yet,
                // so a `wait_for_initial_connections` that starts AFTER this still
                // returns immediately. `notify_waiters` only wakes CURRENT waiters —
                // with the eager construct-then-wait call pattern (connect_and_adapt)
                // the signal would fire before the waiter subscribed and every
                // no-op wait would burn the full timeout.
                initial_done.store(true, std::sync::atomic::Ordering::Release);
                initial_ready.notify_one();
            });
        } else {
            // No servers configured — signal immediately (permit-storing, see above).
            registry
                .initial_done
                .store(true, std::sync::atomic::Ordering::Release);
            registry.initial_ready.notify_one();
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

        // Gate: withhold project-source servers from untrusted projects.
        let (configs, blocked) = Self::partition_by_trust(configs, project_dir);
        for b in &blocked {
            eprintln!("[mcp] withheld untrusted project server: {}", b.name);
        }

        for config in configs {
            if let Err(e) = registry.add_server(config).await {
                eprintln!("[mcp] Failed to connect server: {}", e);
            }
        }

        registry
    }

    /// Add a server to the registry.
    pub async fn add_server(&self, config: McpServerConfig) -> Result<()> {
        // Trust is config-based, not connection-based: record it up front so a tool's
        // risk() can consult it (and so a reconnect after a transient failure is still
        // trusted).
        self.apply_trust_from_config(&config);
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

        if let Err(e) = client.initialize().await {
            // Record the failure so `/mcp` still lists the server with a
            // `failed: <error>` status instead of silently dropping it
            // from the registry's view (#300).
            let mut failed = self.failed_servers.write().await;
            failed.insert(config.name.clone(), format!("{}", e));
            return Err(e);
        }

        let mut servers = self.servers.write().await;
        servers.insert(config.name.clone(), Arc::from(client));
        drop(servers);
        let mut timeouts = self.server_timeouts_ms.write().await;
        timeouts.insert(config.name.clone(), config.timeout_ms());
        let mut failed = self.failed_servers.write().await;
        failed.remove(&config.name);

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
                        let read_only = tool.is_read_only();
                        all_tools.push(McpToolInfo {
                            server_name: server_name.clone(),
                            tool_name: tool.name,
                            description: tool.description,
                            input_schema: tool.input_schema,
                            read_only,
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
                .map(|tool| {
                    let read_only = tool.is_read_only();
                    McpToolInfo {
                        server_name: server_name.to_string(),
                        tool_name: tool.name,
                        description: tool.description,
                        input_schema: tool.input_schema,
                        read_only,
                    }
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

    /// Get the status of all servers — connected ones from `servers`
    /// and any that failed their initial connect from `failed_servers`.
    /// `/mcp` displays the result, so dropping the failed entries would
    /// make a broken config look like "no servers configured" (#300).
    pub async fn server_statuses(&self) -> Vec<(String, ServerStatus)> {
        let servers = self.servers.read().await;
        let failed = self.failed_servers.read().await;
        let mut out: BTreeMap<String, ServerStatus> = servers
            .iter()
            .map(|(name, client)| (name.clone(), client.status()))
            .collect();
        for (name, err) in failed.iter() {
            // Connected wins if both somehow exist — a successful
            // reconnect should already have cleared the failed entry,
            // but be defensive against races.
            out.entry(name.clone())
                .or_insert_with(|| ServerStatus::Failed(err.clone()));
        }
        out.into_iter().collect()
    }

    /// Wait for initial background connections to complete (or timeout).
    /// Returns immediately if no background connections are pending.
    pub async fn wait_for_initial_connections(&self, timeout: Duration) {
        // Level check first: the Notify permit is single-use, so a SECOND caller
        // (the registry is handed to drivers) must not burn its timeout re-waiting
        // for a signal that already fired.
        if self.initial_done.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        let _ = tokio::time::timeout(timeout, self.initial_ready.notified()).await;
    }

    /// Get an Arc clone for sharing across threads.
    pub fn share(&self) -> Arc<Self> {
        Arc::new(Self {
            servers: self.servers.clone(),
            server_timeouts_ms: self.server_timeouts_ms.clone(),
            failed_servers: self.failed_servers.clone(),
            connect_events: self.connect_events.clone(),
            initial_ready: self.initial_ready.clone(),
            initial_done: self.initial_done.clone(),
            trusted_servers: self.trusted_servers.clone(),
            auto_approved_tools: self.auto_approved_tools.clone(),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// SECURITY: an untrusted project's `.mcp.json` stdio server must never be
    /// connected or spawned. The registry must emit `BlockedUntrusted` and leave
    /// the servers map empty.
    #[tokio::test]
    #[serial_test::serial]
    async fn untrusted_project_stdio_server_never_connects() {
        // Isolated trust store => project is untrusted.
        let store = tempfile::tempdir().unwrap();
        // SAFETY: test-only env mutation; #[serial] prevents concurrent tests from
        // racing on this variable.
        unsafe {
            std::env::set_var("ATOMCODE_MCP_TRUST_STORE", store.path().join("s.json"));
        }

        // A project dir containing a malicious .mcp.json (project-source stdio).
        let proj = tempfile::tempdir().unwrap();
        std::fs::write(
            proj.path().join(".mcp.json"),
            r#"{ "mcpServers": { "evil": { "command": "/nonexistent/pwn", "args": ["x"] } } }"#,
        )
        .unwrap();

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let reg = McpRegistry::from_config_background_with_events(proj.path(), Some(tx));
        // Give the background task a chance to run (it must NOT spawn).
        reg.wait_for_initial_connections(std::time::Duration::from_millis(500))
            .await;

        // No server connected.
        assert!(
            reg.connected_server_names().await.is_empty(),
            "no server should connect for an untrusted project"
        );

        // A BlockedUntrusted event was emitted for "evil".
        let mut saw_blocked = false;
        while let Ok(ev) = rx.try_recv() {
            if matches!(ev, McpConnectEvent::BlockedUntrusted { ref name } if name == "evil") {
                saw_blocked = true;
            }
        }
        assert!(saw_blocked, "expected BlockedUntrusted for evil");
    }

    /// `add_server` against a stdio command that cannot be spawned must
    /// still record the failure into `failed_servers`, so the `/mcp`
    /// status listing surfaces it as `failed: <error>` rather than
    /// silently dropping the server from view (#300).
    #[test]
    fn auto_approve_accepts_bare_and_qualified_tool_names() {
        let reg = McpRegistry::new();
        let cfg = McpServerConfig {
            name: "docs".to_string(),
            source: super::super::config::McpConfigSource::Project,
            disabled: false,
            config: super::super::config::McpTransportConfig::Stdio {
                command: "x".to_string(),
                args: vec![],
                env: Default::default(),
                timeout_ms: None,
            },
            trust: false,
            auto_approve: vec!["query".to_string(), "mcp__docs__search".to_string()],
        };
        reg.apply_trust_from_config(&cfg);
        assert!(
            reg.is_tool_auto_approved("mcp__docs__query"),
            "bare name should normalize"
        );
        assert!(
            reg.is_tool_auto_approved("mcp__docs__search"),
            "already-qualified should match"
        );
        assert!(!reg.is_tool_auto_approved("mcp__docs__other"));
        assert!(
            !reg.is_server_trusted("docs"),
            "trust:false must not trust the server"
        );
    }

    /// `project_trust_key` must produce the same key for a verbatim-prefixed path
    /// and its canonical (non-verbatim) equivalent, on every platform.
    /// This is the exact Windows regression this module was patched to fix:
    /// `canonicalize()` on Windows returns `\\?\C:\proj`; core strips that prefix
    /// before hashing, so capabilities must do the same or trust granted via the
    /// TUI/daemon (core) silently fails to unblock the coding agent (capabilities).
    #[test]
    fn trust_key_strips_verbatim_prefix_like_core() {
        use std::path::Path;
        assert_eq!(
            project_trust_key(Path::new(r"\\?\C:\proj")),
            project_trust_key(Path::new(r"C:\proj")),
            r"`\\?\C:\proj` and `C:\proj` must hash identically"
        );
        assert_eq!(
            project_trust_key(Path::new(r"\\?\UNC\srv\share")),
            project_trust_key(Path::new(r"\\srv\share")),
            r"`\\?\UNC\srv\share` and `\\srv\share` must hash identically"
        );
    }

    /// Cross-engine drift lock: pin the exact key produced by `project_trust_key`
    /// for a fixed Unix path so any change to the base algorithm (hasher / format /
    /// PathBuf component hashing) fails CI in this crate.
    ///
    /// The same literal is pinned by the shared session-bucket helper.
    #[cfg(unix)]
    #[test]
    fn trust_key_golden_matches_core_algorithm() {
        use std::path::Path;
        assert_eq!(
            project_trust_key(Path::new("/tmp/atomcode-trust-golden")),
            "8b6a67e0b2c06dae"
        );
    }

    #[tokio::test]
    async fn failed_stdio_connect_appears_in_server_statuses() {
        let registry = McpRegistry::new();
        let config = McpServerConfig {
            name: "broken".to_string(),
            source: super::super::config::McpConfigSource::Project,
            disabled: false,
            config: super::super::config::McpTransportConfig::Stdio {
                // Deliberately bogus binary so spawn() fails fast.
                command: "/nonexistent/atomcode-mcp-test-binary".to_string(),
                args: vec![],
                env: Default::default(),
                timeout_ms: Some(500),
            },
            trust: false,
            auto_approve: vec![],
        };

        let result = registry.add_server(config).await;
        assert!(result.is_err(), "expected initialize to fail");

        let statuses = registry.server_statuses().await;
        let broken = statuses
            .iter()
            .find(|(name, _)| name == "broken")
            .expect("failed server should still show in /mcp list");
        match &broken.1 {
            ServerStatus::Failed(_) => {}
            other => panic!("expected Failed status, got {:?}", other),
        }
    }
}
