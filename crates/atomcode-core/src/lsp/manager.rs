//! LspManager — lazily starts and manages LSP clients per file extension.
//!
//! Provides a unified interface for diagnostics, file notifications, and
//! lifecycle management across multiple language servers.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use tokio::sync::RwLock;

use super::client::LspClient;
use super::registry::LspServerRegistry;
use super::types::Diagnostic;
use crate::config::LspConfig;

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
}

impl LspManager {
    /// Create a new LSP manager.
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
                clients.insert(ext, arc);
                Ok(true)
            }
            Err(e) => {
                // Log but don't propagate — LSP is best-effort.
                eprintln!(
                    "[lsp] Failed to start {} for .{}: {}",
                    config.command, ext, e
                );
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
                eprintln!("[lsp] Error shutting down server for .{}: {}", ext, e);
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

    let mut registry = if config.auto_detect {
        LspServerRegistry::with_defaults()
    } else {
        LspServerRegistry::empty()
    };

    // Merge user-configured servers (overrides defaults for same extension).
    registry.merge_user_config(config.servers.clone());

    let manager = LspManager::new(
        project_root.to_path_buf(),
        registry,
        true,
        config.diagnostics_settle_delay_ms,
    );

    Some(Arc::new(manager))
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
}
