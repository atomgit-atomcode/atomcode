//! LspManager — lazily starts and manages LSP clients per file extension.
//!
//! Provides a unified interface for diagnostics, file notifications, and
//! lifecycle management across multiple language servers.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use tokio::sync::{mpsc, RwLock};

use super::client::LspClient;
use super::registry::LspServerRegistry;
use super::types::Diagnostic;
use crate::config::LspConfig;

/// Status events emitted by `LspManager` when servers start, fail, or
/// hit non-fatal trouble. Mirrors `mcp::McpConnectEvent` so the TUI
/// event loop can render them as scrollback lines instead of having
/// the manager write directly to stderr (which leaks into the input
/// box while the renderer owns the screen — see `lsp/mod.rs` for the
/// full plumbing rationale).
#[derive(Debug, Clone)]
pub enum LspConnectEvent {
    /// A language server was successfully started for the given extension.
    Started { command: String, ext: String },
    /// Starting the server failed; LSP is best-effort, so the agent loop
    /// continues without diagnostics for that file type.
    Failed {
        command: String,
        ext: String,
        error: String,
    },
    /// Non-fatal trouble (e.g. shutdown error during teardown). Routed
    /// to the trace log by the TUI rather than scrollback so it doesn't
    /// churn the UI for things the user can't act on.
    Warning { ext: String, message: String },
}

/// Extension-to-language_id mapping for LSP `textDocument/didOpen`.
fn extension_to_language_id(ext: &str) -> &str {
    match ext {
        "rs" => "rust",
        "ts" => "typescript",
        "tsx" => "typescriptreact",
        "js" => "javascript",
        "jsx" => "javascriptreact",
        "py" => "python",
        "go" => "go",
        "java" => "java",
        "c" => "c",
        "cpp" | "cc" | "cxx" => "cpp",
        "cs" => "csharp",
        "rb" => "ruby",
        "php" => "php",
        "swift" => "swift",
        "kt" | "kts" => "kotlin",
        "scala" => "scala",
        _ => ext,
    }
}

/// Manages lifecycle of multiple language server clients.
///
/// Lazily starts LSP servers on-demand based on file extension.
/// Each extension maps to at most one running server instance.
/// Servers are started when first needed and remain running until
/// explicitly shut down or the manager is dropped.
pub struct LspManager {
    /// Running clients keyed by file extension.
    clients: Arc<RwLock<HashMap<String, Arc<LspClient>>>>,
    /// Server registry (default + user overrides).
    registry: LspServerRegistry,
    /// Project root for LSP initialize.
    project_root: PathBuf,
    /// Whether LSP integration is enabled.
    enabled: bool,
    /// Time in milliseconds to wait after file sync before reading diagnostics.
    diagnostics_settle_delay_ms: u64,
    /// Optional channel for connection status events. Some(tx) when a
    /// listener (TUI event loop) is consuming them; None in headless
    /// mode where eprintln-to-stderr would be visible to CI logs anyway
    /// — but we still don't print, since send-on-None is just a no-op
    /// and the headless path doesn't need the diagnostics.
    connect_events: Option<mpsc::UnboundedSender<LspConnectEvent>>,
}

impl LspManager {
    /// Create a new LSP manager without an event channel. Status changes
    /// are silently dropped — appropriate for tests and headless mode
    /// where no UI consumes them.
    pub fn new(
        project_root: PathBuf,
        registry: LspServerRegistry,
        enabled: bool,
        diagnostics_settle_delay_ms: u64,
    ) -> Self {
        Self {
            clients: Arc::new(RwLock::new(HashMap::new())),
            registry,
            project_root,
            enabled,
            diagnostics_settle_delay_ms,
            connect_events: None,
        }
    }

    /// Create a new LSP manager paired with a receiver for connection
    /// status events. The TUI event loop consumes the receiver and
    /// renders `Started` / `Failed` events as scrollback lines next to
    /// the existing MCP rendering.
    pub fn with_event_channel(
        project_root: PathBuf,
        registry: LspServerRegistry,
        enabled: bool,
        diagnostics_settle_delay_ms: u64,
    ) -> (Self, mpsc::UnboundedReceiver<LspConnectEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let mgr = Self {
            clients: Arc::new(RwLock::new(HashMap::new())),
            registry,
            project_root,
            enabled,
            diagnostics_settle_delay_ms,
            connect_events: Some(tx),
        };
        (mgr, rx)
    }

    /// Send a status event to the listener, if any. No-op when no
    /// channel is wired (`new()` constructor or after the receiver was
    /// dropped — `send` returns Err which we ignore intentionally).
    fn emit(&self, event: LspConnectEvent) {
        if let Some(tx) = &self.connect_events {
            let _ = tx.send(event);
        }
    }

    /// Get the configured diagnostics settle delay in milliseconds.
    pub fn diagnostics_settle_delay_ms(&self) -> u64 {
        self.diagnostics_settle_delay_ms
    }

    /// Ensure a language server is running for the given file's extension.
    /// Returns `Ok(true)` if a server is (now) running, `Ok(false)` if no
    /// server is configured or the command is not installed.
    pub async fn ensure_server(&self, file_path: &Path) -> Result<bool> {
        if !self.enabled {
            return Ok(false);
        }

        let ext = match file_path.extension().and_then(|e| e.to_str()) {
            Some(e) => e.to_string(),
            None => return Ok(false),
        };

        // Fast path: check under read lock first.
        {
            let clients = self.clients.read().await;
            if clients.contains_key(&ext) {
                return Ok(true);
            }
        }

        // Look up server config.
        let config = match self.registry.get(&ext) {
            Some(c) => c.clone(),
            None => return Ok(false),
        };

        // Check if the command exists on PATH.
        if which::which(&config.command).is_err() {
            return Ok(false);
        }

        let language_id = extension_to_language_id(&ext);

        // Acquire write lock and double-check to prevent TOCTOU race.
        let mut clients = self.clients.write().await;
        if clients.contains_key(&ext) {
            return Ok(true);
        }

        // Start the client while holding the write lock.
        match LspClient::start(&config, &self.project_root, language_id).await {
            Ok(client) => {
                let arc = Arc::new(client);
                clients.insert(ext.clone(), arc);
                self.emit(LspConnectEvent::Started {
                    command: config.command.clone(),
                    ext,
                });
                Ok(true)
            }
            Err(e) => {
                // LSP is best-effort: a missing or broken language server
                // must not propagate as a tool error. Surface the failure
                // through the event channel so the TUI can render it in
                // scrollback (or, in headless mode with no listener, drop
                // it silently — `emit` is a no-op when no channel).
                self.emit(LspConnectEvent::Failed {
                    command: config.command.clone(),
                    ext,
                    error: e.to_string(),
                });
                Ok(false)
            }
        }
    }

    /// Get diagnostics for a specific file.
    /// Returns an empty vector if no server is running for that file type.
    pub async fn diagnostics(&self, path: &Path) -> Vec<Diagnostic> {
        let ext = match path.extension().and_then(|e| e.to_str()) {
            Some(e) => e.to_string(),
            None => return Vec::new(),
        };

        let clients = self.clients.read().await;
        match clients.get(&ext) {
            Some(client) => client.diagnostics(path).await,
            None => Vec::new(),
        }
    }

    /// Get all diagnostics from all running servers.
    /// Aggregates diagnostics across all file types with active servers.
    pub async fn all_diagnostics(&self) -> Vec<Diagnostic> {
        let clients = self.clients.read().await;
        let mut all = Vec::new();
        for client in clients.values() {
            all.extend(client.all_diagnostics().await);
        }
        all
    }

    /// Ensure the appropriate server is running, then notify it that a file changed.
    /// This triggers the server to re-analyze the file and publish updated diagnostics.
    /// Returns `Ok(true)` if a server received the notification, `Ok(false)` otherwise.
    pub async fn notify_file_changed(&self, path: &Path, content: &str) -> Result<bool> {
        if !self.ensure_server(path).await? {
            return Ok(false);
        }

        let ext = match path.extension().and_then(|e| e.to_str()) {
            Some(e) => e.to_string(),
            None => return Ok(false),
        };

        let clients = self.clients.read().await;
        if let Some(client) = clients.get(&ext) {
            let language_id = extension_to_language_id(&ext);
            // Use sync_document for proper didOpen/didChange versioning.
            client.sync_document(path, content, language_id).await?;
            return Ok(true);
        }

        Ok(false)
    }

    /// List the file extensions that have active servers.
    /// Useful for debugging and status display.
    pub async fn active_servers(&self) -> Vec<String> {
        let clients = self.clients.read().await;
        let mut exts: Vec<String> = clients.keys().cloned().collect();
        exts.sort();
        exts
    }

    /// Shutdown all running language servers gracefully.
    /// Sends shutdown request, exit notification, then kills the process.
    /// Errors are logged but not propagated.
    pub async fn shutdown(&self) {
        let mut clients = self.clients.write().await;
        for (ext, client) in clients.drain() {
            if let Err(e) = client.shutdown().await {
                // Shutdown errors are not actionable for the user — the
                // process is exiting anyway. Route to Warning so the TUI
                // can decide to log/swallow rather than scrollback-spam.
                self.emit(LspConnectEvent::Warning {
                    ext,
                    message: format!("shutdown error: {}", e),
                });
            }
        }
    }
}

/// Build an LspManager from config, providing a unified entry point for CLI and daemon.
/// Returns `None` if LSP is disabled in config.
pub fn build_lsp_manager(config: &LspConfig, project_root: &Path) -> Option<Arc<LspManager>> {
    if !config.enabled {
        return None;
    }
    let registry = build_registry(config);
    let manager = LspManager::new(
        project_root.to_path_buf(),
        registry,
        true,
        config.diagnostics_settle_delay_ms,
    );
    Some(Arc::new(manager))
}

/// Build an LspManager paired with a receiver for connection-status
/// events. TUI mode wires the receiver into the event loop so server
/// start/failure surfaces as `✓ LSP server …` / `× LSP server …`
/// scrollback lines, matching the MCP server flow. Returns `None` when
/// LSP is disabled in config.
pub fn build_lsp_manager_with_events(
    config: &LspConfig,
    project_root: &Path,
) -> Option<(Arc<LspManager>, mpsc::UnboundedReceiver<LspConnectEvent>)> {
    if !config.enabled {
        return None;
    }
    let registry = build_registry(config);
    let (manager, rx) = LspManager::with_event_channel(
        project_root.to_path_buf(),
        registry,
        true,
        config.diagnostics_settle_delay_ms,
    );
    Some((Arc::new(manager), rx))
}

/// Shared registry construction used by both `build_lsp_manager`
/// constructors. Defaults + user overrides merged into one registry.
fn build_registry(config: &LspConfig) -> LspServerRegistry {
    let mut registry = if config.auto_detect {
        LspServerRegistry::with_defaults()
    } else {
        LspServerRegistry::empty()
    };
    registry.merge_user_config(config.servers.clone());
    registry
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_to_language_id_maps_common_langs() {
        assert_eq!(extension_to_language_id("rs"), "rust");
        assert_eq!(extension_to_language_id("ts"), "typescript");
        assert_eq!(extension_to_language_id("tsx"), "typescriptreact");
        assert_eq!(extension_to_language_id("py"), "python");
        assert_eq!(extension_to_language_id("go"), "go");
        assert_eq!(extension_to_language_id("java"), "java");
        assert_eq!(extension_to_language_id("js"), "javascript");
    }

    #[test]
    fn extension_to_language_id_unknown_returns_self() {
        assert_eq!(extension_to_language_id("xyz"), "xyz");
    }

    #[tokio::test]
    async fn disabled_manager_returns_false() {
        let registry = LspServerRegistry::with_defaults();
        let mgr = LspManager::new(PathBuf::from("/tmp"), registry, false, 150);
        let result = mgr.ensure_server(Path::new("test.rs")).await.unwrap();
        assert!(!result);
    }

    #[tokio::test]
    async fn no_config_for_extension_returns_false() {
        let registry = LspServerRegistry::with_defaults();
        let mgr = LspManager::new(PathBuf::from("/tmp"), registry, true, 150);
        let result = mgr.ensure_server(Path::new("test.xyz")).await.unwrap();
        assert!(!result);
    }

    #[tokio::test]
    async fn no_extension_returns_false() {
        let registry = LspServerRegistry::with_defaults();
        let mgr = LspManager::new(PathBuf::from("/tmp"), registry, true, 150);
        let result = mgr.ensure_server(Path::new("Makefile")).await.unwrap();
        assert!(!result);
    }

    #[tokio::test]
    async fn empty_diagnostics_for_unknown_file() {
        let registry = LspServerRegistry::with_defaults();
        let mgr = LspManager::new(PathBuf::from("/tmp"), registry, true, 150);
        let diags = mgr.diagnostics(Path::new("test.xyz")).await;
        assert!(diags.is_empty());
    }

    #[tokio::test]
    async fn active_servers_empty_initially() {
        let registry = LspServerRegistry::with_defaults();
        let mgr = LspManager::new(PathBuf::from("/tmp"), registry, true, 150);
        assert!(mgr.active_servers().await.is_empty());
    }

    #[tokio::test]
    async fn all_diagnostics_empty_initially() {
        let registry = LspServerRegistry::with_defaults();
        let mgr = LspManager::new(PathBuf::from("/tmp"), registry, true, 150);
        assert!(mgr.all_diagnostics().await.is_empty());
    }

    #[tokio::test]
    async fn shutdown_on_empty_is_noop() {
        let registry = LspServerRegistry::with_defaults();
        let mgr = LspManager::new(PathBuf::from("/tmp"), registry, true, 150);
        mgr.shutdown().await; // Should not panic.
    }

    #[test]
    fn build_lsp_manager_returns_none_when_disabled() {
        let config = LspConfig {
            enabled: false,
            auto_detect: true,
            servers: Default::default(),
            diagnostics_settle_delay_ms: 150,
        };
        let result = build_lsp_manager(&config, Path::new("/tmp"));
        assert!(result.is_none());
    }

    #[test]
    fn build_lsp_manager_returns_some_when_enabled() {
        let config = LspConfig {
            enabled: true,
            auto_detect: true,
            servers: Default::default(),
            diagnostics_settle_delay_ms: 150,
        };
        let result = build_lsp_manager(&config, Path::new("/tmp"));
        assert!(result.is_some());
    }

    #[test]
    fn build_lsp_manager_respects_auto_detect() {
        // auto_detect=false should start with empty registry
        let config = LspConfig {
            enabled: true,
            auto_detect: false,
            servers: Default::default(),
            diagnostics_settle_delay_ms: 150,
        };
        let result = build_lsp_manager(&config, Path::new("/tmp"));
        assert!(result.is_some());
        // The manager should have no servers configured (empty registry)
    }

    #[test]
    fn build_lsp_manager_merges_user_servers() {
        let mut servers = std::collections::HashMap::new();
        servers.insert(
            "xyz".to_string(),
            super::super::registry::LspServerConfig {
                command: "my-lsp".to_string(),
                args: vec![],
                root_markers: vec![],
            },
        );
        let config = LspConfig {
            enabled: true,
            auto_detect: true,
            servers,
            diagnostics_settle_delay_ms: 150,
        };
        let result = build_lsp_manager(&config, Path::new("/tmp"));
        assert!(result.is_some());
    }

    /// `with_event_channel` returns a paired `(manager, receiver)` and
    /// the receiver starts empty. Behaves identically to `new()` when
    /// no events have fired yet.
    #[tokio::test]
    async fn with_event_channel_yields_empty_receiver_initially() {
        let registry = LspServerRegistry::with_defaults();
        let (mgr, mut rx) =
            LspManager::with_event_channel(PathBuf::from("/tmp"), registry, true, 150);
        assert!(mgr.active_servers().await.is_empty());
        assert!(rx.try_recv().is_err(), "no events expected before any ensure_server call");
    }

    /// A failed `ensure_server` (server command not on PATH won't even
    /// reach the start path; a registered server pointing at a
    /// guaranteed-nonexistent binary will). We use a custom registry
    /// with a known-bad command + which::which check disabled by
    /// passing the .xyz extension so registry lookup hits but `which`
    /// will fail — that triggers `return Ok(false)` BEFORE the start
    /// attempt though, so we won't emit `Failed` (which is the right
    /// behavior: not installed ≠ failed). Verify with active_servers
    /// that nothing got registered AND no event fired.
    #[tokio::test]
    async fn ensure_server_silent_when_command_missing() {
        let mut servers = std::collections::HashMap::new();
        servers.insert(
            "xyz".to_string(),
            super::super::registry::LspServerConfig {
                command: "atomcode-lsp-does-not-exist".to_string(),
                args: vec![],
                root_markers: vec![],
            },
        );
        let mut registry = LspServerRegistry::empty();
        registry.merge_user_config(servers);
        let (mgr, mut rx) =
            LspManager::with_event_channel(PathBuf::from("/tmp"), registry, true, 150);
        let result = mgr.ensure_server(Path::new("test.xyz")).await.unwrap();
        assert!(!result, "missing command must return Ok(false)");
        // `which` failed BEFORE start_client — no event fires, agent
        // continues silently. This matches MCP's "command not found"
        // behavior and avoids spamming scrollback for projects that
        // don't have the language tooling installed.
        assert!(rx.try_recv().is_err(), "no event expected for missing command");
    }

    /// Sender drops cleanly: dropping the receiver before any send
    /// happens must not panic. Confirms `emit` handles the closed-
    /// receiver case (Result ignored).
    #[tokio::test]
    async fn emit_no_op_when_receiver_dropped() {
        let registry = LspServerRegistry::with_defaults();
        let (mgr, rx) =
            LspManager::with_event_channel(PathBuf::from("/tmp"), registry, true, 150);
        drop(rx);
        // Trigger the path that calls `emit` — non-existent file ext
        // takes the early-return; we explicitly call the emit helper
        // via a synthetic Warning to exercise the post-drop send.
        mgr.emit(LspConnectEvent::Warning {
            ext: "rs".to_string(),
            message: "synthetic post-drop emit".to_string(),
        });
        // No panic = pass.
    }
}
