//! Pooled language-server manager: one [`LspClient`] per file extension, started lazily
//! and reused. Ported from production `lsp/manager.rs` (the event-channel / telemetry
//! coupling is dropped; absence of a server binary degrades gracefully).

use super::client::LspClient;
use super::registry::{extension_to_language_id, LspServerRegistry};
use super::types::Diagnostic;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::Mutex;

/// How long to wait after syncing a document for the server to publish diagnostics.
const SETTLE_DELAY_MS: u64 = 350;

pub struct LspManager {
    /// ext → running client. The lock is held across server STARTUP so two calls don't
    /// race to spawn the same server.
    clients: Mutex<HashMap<String, Arc<LspClient>>>,
    registry: LspServerRegistry,
    settle_delay_ms: u64,
}

/// Result of syncing a document to its language server. Lets the calling tool report
/// connect-status to the driver without coupling the manager to a UI/event channel.
#[derive(Debug, Clone, PartialEq)]
pub enum LspSyncOutcome {
    /// Server is up (newly spawned or already running). When returned from
    /// `notify_file_changed`, the document was also synced to the server.
    Synced { server: String, newly_started: bool },
    /// No language server for this extension — unconfigured OR its binary is absent.
    Unsupported { ext: String },
    /// The server binary exists but spawning it failed.
    Failed { server: String, error: String },
}

impl LspManager {
    pub fn new() -> Self {
        Self::with_registry(LspServerRegistry::with_defaults())
    }
    pub fn with_registry(registry: LspServerRegistry) -> Self {
        Self { clients: Mutex::new(HashMap::new()), registry, settle_delay_ms: SETTLE_DELAY_MS }
    }
    pub fn settle_delay_ms(&self) -> u64 {
        self.settle_delay_ms
    }

    fn ext_of(path: &Path) -> Option<String> {
        path.extension()?.to_str().map(str::to_string)
    }

    /// Detailed outcome of ensuring a server for `path`'s language, rooted at `root`.
    async fn ensure_server_detailed(&self, root: &Path, path: &Path) -> LspSyncOutcome {
        let Some(ext) = Self::ext_of(path) else {
            return LspSyncOutcome::Unsupported { ext: String::new() };
        };
        if self.clients.lock().await.contains_key(&ext) {
            let server = self.registry.get(&ext).map(|c| c.command.clone()).unwrap_or_default();
            return LspSyncOutcome::Synced { server, newly_started: false };
        }
        let Some(config) = self.registry.get(&ext) else {
            return LspSyncOutcome::Unsupported { ext };
        };
        let server = config.command.clone();
        // Binary not on PATH → graceful degrade (no error, no spawn).
        if which::which(&config.command).is_err() {
            return LspSyncOutcome::Unsupported { ext };
        }
        let mut clients = self.clients.lock().await;
        if clients.contains_key(&ext) {
            return LspSyncOutcome::Synced { server, newly_started: false }; // another caller won the race
        }
        match LspClient::spawn(config, root).await {
            Ok(c) => {
                clients.insert(ext, Arc::new(c));
                LspSyncOutcome::Synced { server, newly_started: true }
            }
            Err(e) => LspSyncOutcome::Failed { server, error: e.to_string() },
        }
    }

    /// Ensure a server is running for `path`'s language, rooted at `root`. Returns
    /// `false` (gracefully) if no server is configured or its binary is not installed.
    pub async fn ensure_server(&self, root: &Path, path: &Path) -> bool {
        matches!(self.ensure_server_detailed(root, path).await, LspSyncOutcome::Synced { .. })
    }

    /// Open/refresh a document so the server re-analyzes it. The outcome tells the caller
    /// what happened (for status reporting). The document is synced only when `Synced`.
    pub async fn notify_file_changed(&self, root: &Path, path: &Path, content: &str) -> LspSyncOutcome {
        let outcome = self.ensure_server_detailed(root, path).await;
        if matches!(outcome, LspSyncOutcome::Synced { .. }) {
            if let Some(ext) = Self::ext_of(path) {
                let client = self.clients.lock().await.get(&ext).cloned();
                if let Some(client) = client {
                    let _ = client.sync_document(path, content, &extension_to_language_id(&ext)).await;
                }
            }
        }
        outcome
    }

    pub async fn diagnostics(&self, path: &Path) -> Vec<Diagnostic> {
        let Some(ext) = Self::ext_of(path) else {
            return Vec::new();
        };
        let client = self.clients.lock().await.get(&ext).cloned();
        client.map(|c| c.diagnostics(path)).unwrap_or_default()
    }

    pub async fn all_diagnostics(&self) -> Vec<Diagnostic> {
        let clients: Vec<_> = self.clients.lock().await.values().cloned().collect();
        clients.iter().flat_map(|c| c.all_diagnostics()).collect()
    }

    pub async fn has_servers(&self) -> bool {
        !self.clients.lock().await.is_empty()
    }

    pub async fn shutdown(&self) {
        let clients: Vec<_> = self.clients.lock().await.drain().map(|(_, c)| c).collect();
        for c in clients {
            c.shutdown().await;
        }
    }
}

impl Default for LspManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codeintel::lsp::registry::{LspServerConfig, LspServerRegistry};

    fn missing_binary_registry() -> LspServerRegistry {
        let mut r = LspServerRegistry::empty();
        r.insert(
            "rs",
            LspServerConfig { command: "atomcode-no-such-lsp-binary-xyz".into(), args: vec![], root_markers: vec![] },
        );
        r
    }

    #[tokio::test]
    async fn notify_reports_unsupported_when_binary_missing() {
        let mgr = LspManager::with_registry(missing_binary_registry());
        let d = tempfile::tempdir().unwrap();
        let outcome = mgr.notify_file_changed(d.path(), Path::new("a.rs"), "fn main() {}").await;
        assert_eq!(outcome, LspSyncOutcome::Unsupported { ext: "rs".into() });
    }

    #[tokio::test]
    async fn notify_reports_unsupported_for_unknown_extension() {
        let mgr = LspManager::with_registry(missing_binary_registry());
        let d = tempfile::tempdir().unwrap();
        let outcome = mgr.notify_file_changed(d.path(), Path::new("a.txt"), "x").await;
        assert_eq!(outcome, LspSyncOutcome::Unsupported { ext: "txt".into() });
    }

    #[tokio::test]
    async fn ensure_server_degrades_when_uninstalled() {
        let mgr = LspManager::with_registry(missing_binary_registry());
        let d = tempfile::tempdir().unwrap();
        // configured but binary missing → false
        assert!(!mgr.ensure_server(d.path(), Path::new("a.rs")).await);
        // unsupported extension → false
        assert!(!mgr.ensure_server(d.path(), Path::new("a.txt")).await);
        assert!(!mgr.has_servers().await);
    }

    #[tokio::test]
    async fn diagnostics_empty_without_server() {
        let mgr = LspManager::with_registry(missing_binary_registry());
        let d = tempfile::tempdir().unwrap();
        assert!(mgr.diagnostics(&d.path().join("a.rs")).await.is_empty());
        assert!(mgr.all_diagnostics().await.is_empty());
    }
}
