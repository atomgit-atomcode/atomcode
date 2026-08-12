//! Pooled language-server manager: one [`LspClient`] per project root and server
//! command, started lazily and reused across compatible extensions. Ported from
//! production `lsp/manager.rs` (the event-channel / telemetry
//! coupling is dropped; absence of a server binary degrades gracefully).

use super::client::LspClient;
use super::registry::{extension_to_language_id, LspServerRegistry};
use super::types::{Diagnostic, Location};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

/// How long to wait after syncing a document for the server to publish diagnostics.
const SETTLE_DELAY_MS: u64 = 350;
const STARTUP_TIMEOUT_SECS: u64 = 30;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ClientKey {
    root: PathBuf,
    command: String,
    args: Vec<String>,
}

pub struct LspManager {
    /// (canonical project root, server command/args) → running client. Extensions that
    /// map to the same server share a process, while different workspaces remain isolated.
    clients: Mutex<HashMap<ClientKey, Arc<LspClient>>>,
    /// Only callers starting the same project/language serialize with each other;
    /// a slow Java server must not block an already-running Rust server lookup.
    startup_locks: Mutex<HashMap<ClientKey, Arc<Mutex<()>>>>,
    /// Startup failures are sticky for this manager generation. Repeating a model tool
    /// call must not create a spawn/timeout loop; a runtime rebuild gives the user a
    /// clean retry after fixing PATH/config.
    unavailable: Mutex<HashMap<ClientKey, String>>,
    registry: LspServerRegistry,
    settle_delay_ms: u64,
}

impl LspManager {
    pub fn new() -> Self {
        Self::with_registry(LspServerRegistry::with_defaults())
    }
    pub fn with_registry(registry: LspServerRegistry) -> Self {
        Self::with_registry_and_delay(registry, SETTLE_DELAY_MS)
    }

    pub fn with_registry_and_delay(registry: LspServerRegistry, settle_delay_ms: u64) -> Self {
        Self {
            clients: Mutex::new(HashMap::new()),
            startup_locks: Mutex::new(HashMap::new()),
            unavailable: Mutex::new(HashMap::new()),
            registry,
            settle_delay_ms,
        }
    }
    pub fn settle_delay_ms(&self) -> u64 {
        self.settle_delay_ms
    }

    fn ext_of(path: &Path) -> Option<String> {
        path.extension()?
            .to_str()
            .map(|ext| ext.to_ascii_lowercase())
    }

    fn canonical(path: &Path) -> PathBuf {
        std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
    }

    fn server_root(workspace_root: &Path, path: &Path, markers: &[String]) -> PathBuf {
        let workspace_root = Self::canonical(workspace_root);
        let absolute_path = if path.is_absolute() {
            Self::canonical(path)
        } else {
            Self::canonical(&workspace_root.join(path))
        };
        let mut directory = absolute_path.parent().unwrap_or(&workspace_root);
        if !directory.starts_with(&workspace_root) {
            return workspace_root;
        }
        if !markers.is_empty() {
            loop {
                if markers.iter().any(|marker| directory.join(marker).exists()) {
                    return directory.to_path_buf();
                }
                if directory == workspace_root {
                    break;
                }
                let Some(parent) = directory.parent() else {
                    break;
                };
                directory = parent;
            }
        }
        workspace_root
    }

    fn client_key(root: PathBuf, config: &super::registry::LspServerConfig) -> ClientKey {
        ClientKey {
            root,
            command: config.command.clone(),
            args: config.args.clone(),
        }
    }

    /// Ensure a server is running for `path`'s language, rooted at `root`. Returns
    /// an explanatory error (gracefully surfaced by the tool) if no server is configured,
    /// installed, or able to initialize within the startup bound.
    async fn get_or_start_server(
        &self,
        root: &Path,
        path: &Path,
        cancel: &CancellationToken,
    ) -> Result<Arc<LspClient>, String> {
        let Some(extension) = Self::ext_of(path) else {
            return Err("file has no supported extension".into());
        };
        let Some(config) = self.registry.get(&extension) else {
            return Err(format!("no language server configured for .{extension}"));
        };
        let server_root = Self::server_root(root, path, &config.root_markers);
        let key = Self::client_key(server_root, config);
        if let Some(client) = self.clients.lock().await.get(&key).cloned() {
            return Ok(client);
        }
        if let Some(error) = self.unavailable.lock().await.get(&key).cloned() {
            return Err(error);
        }
        let startup_lock = {
            let mut locks = self.startup_locks.lock().await;
            locks
                .entry(key.clone())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        let _startup_guard = tokio::select! {
            guard = startup_lock.lock() => guard,
            _ = cancel.cancelled() => return Err("language server startup cancelled".into()),
        };
        if let Some(client) = self.clients.lock().await.get(&key).cloned() {
            return Ok(client);
        }
        if let Some(error) = self.unavailable.lock().await.get(&key).cloned() {
            return Err(error);
        }
        // Binary not on PATH → graceful degrade (no error, no spawn).
        if which::which(&config.command).is_err() {
            let error = format!(
                "language server '{}' is not installed or not on PATH",
                config.command
            );
            self.unavailable.lock().await.insert(key, error.clone());
            return Err(error);
        }
        let startup = tokio::time::timeout(
            Duration::from_secs(STARTUP_TIMEOUT_SECS),
            LspClient::spawn(config, &key.root),
        );
        let startup = tokio::select! {
            result = startup => result,
            _ = cancel.cancelled() => return Err("language server startup cancelled".into()),
        };
        match startup {
            Ok(Ok(c)) => {
                let client = Arc::new(c);
                self.clients.lock().await.insert(key, client.clone());
                Ok(client)
            }
            Ok(Err(error)) => {
                self.unavailable.lock().await.insert(key, error.clone());
                Err(error)
            }
            Err(_) => {
                let error = format!(
                    "language server '{}' did not initialize within {STARTUP_TIMEOUT_SECS}s",
                    config.command
                );
                self.unavailable.lock().await.insert(key, error.clone());
                Err(error)
            }
        }
    }

    /// Compatibility probe retained for embedders that used the original diagnostics
    /// API. New callers should use the cancellable query methods below.
    pub async fn ensure_server(&self, root: &Path, path: &Path) -> bool {
        self.get_or_start_server(root, path, &CancellationToken::new())
            .await
            .is_ok()
    }

    /// Open/refresh a document so the server has the current in-memory contents.
    pub async fn sync_document(
        &self,
        root: &Path,
        path: &Path,
        content: &str,
        cancel: &CancellationToken,
    ) -> Result<Arc<LspClient>, String> {
        let client = self.get_or_start_server(root, path, cancel).await?;
        let Some(ext) = Self::ext_of(path) else {
            return Err("file has no supported extension".into());
        };
        let language_id = extension_to_language_id(&ext);
        if cancel.is_cancelled() {
            return Err("LSP document sync cancelled".into());
        }
        // Do not drop a write_all future midway through a framed JSON-RPC message: a
        // partial didOpen/didChange would corrupt the server stream. The tool bounds
        // document size, then observes cancellation again immediately after the write.
        client.sync_document(path, content, &language_id).await?;
        if cancel.is_cancelled() {
            return Err("LSP document sync cancelled".into());
        }
        Ok(client)
    }

    /// Compatibility wrapper for the former diagnostics tool API.
    pub async fn notify_file_changed(&self, root: &Path, path: &Path, content: &str) -> bool {
        self.sync_document(root, path, content, &CancellationToken::new())
            .await
            .is_ok()
    }

    pub async fn definition(
        &self,
        root: &Path,
        path: &Path,
        content: &str,
        line: u32,
        character: u32,
        cancel: &CancellationToken,
    ) -> Result<Vec<Location>, String> {
        let client = self.sync_document(root, path, content, cancel).await?;
        let (line, character) = client.wire_position(content, line, character)?;
        client.definition(path, line, character, cancel).await
    }

    pub async fn references(
        &self,
        root: &Path,
        path: &Path,
        content: &str,
        line: u32,
        character: u32,
        cancel: &CancellationToken,
    ) -> Result<Vec<Location>, String> {
        let client = self.sync_document(root, path, content, cancel).await?;
        let (line, character) = client.wire_position(content, line, character)?;
        client.references(path, line, character, cancel).await
    }

    pub async fn hover(
        &self,
        root: &Path,
        path: &Path,
        content: &str,
        line: u32,
        character: u32,
        cancel: &CancellationToken,
    ) -> Result<serde_json::Value, String> {
        let client = self.sync_document(root, path, content, cancel).await?;
        let (line, character) = client.wire_position(content, line, character)?;
        client.hover(path, line, character, cancel).await
    }

    pub async fn refresh_pull_diagnostics(
        &self,
        root: &Path,
        path: &Path,
        cancel: &CancellationToken,
    ) -> Result<(), String> {
        let client = self.get_or_start_server(root, path, cancel).await?;
        client.refresh_pull_diagnostics(path, cancel).await
    }

    pub async fn diagnostics(&self, path: &Path) -> Vec<Diagnostic> {
        let clients: Vec<_> = self.clients.lock().await.values().cloned().collect();
        clients
            .into_iter()
            .find_map(|client| {
                let diagnostics = client.diagnostics(path);
                (!diagnostics.is_empty()).then_some(diagnostics)
            })
            .unwrap_or_default()
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
            LspServerConfig {
                command: "atomcode-no-such-lsp-binary-xyz".into(),
                args: vec![],
                root_markers: vec![],
            },
        );
        r
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

    #[tokio::test]
    async fn waiting_for_same_server_startup_is_cancellable() {
        let manager = LspManager::with_registry(missing_binary_registry());
        let workspace = tempfile::tempdir().unwrap();
        let config = manager.registry.get("rs").unwrap();
        let key = LspManager::client_key(std::fs::canonicalize(workspace.path()).unwrap(), config);
        let startup_lock = Arc::new(Mutex::new(()));
        manager
            .startup_locks
            .lock()
            .await
            .insert(key, startup_lock.clone());
        let _guard = startup_lock.lock().await;
        let cancel = CancellationToken::new();
        cancel.cancel();
        let result = tokio::time::timeout(
            Duration::from_millis(100),
            manager.get_or_start_server(
                workspace.path(),
                &workspace.path().join("main.rs"),
                &cancel,
            ),
        )
        .await
        .expect("cancel should interrupt startup lock wait");
        match result {
            Err(error) => assert!(error.contains("cancelled")),
            Ok(_) => panic!("cancelled startup unexpectedly returned a client"),
        }
    }

    #[test]
    fn client_key_isolated_by_workspace_and_reuses_same_server() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let config = LspServerConfig {
            command: "typescript-language-server".into(),
            args: vec!["--stdio".into()],
            root_markers: vec![],
        };
        let ka = LspManager::client_key(a.path().to_path_buf(), &config);
        let kb = LspManager::client_key(b.path().to_path_buf(), &config);
        assert_ne!(ka, kb);
        assert_eq!(
            LspManager::client_key(a.path().to_path_buf(), &config),
            LspManager::client_key(a.path().to_path_buf(), &config)
        );
    }

    #[test]
    fn server_root_uses_nearest_marker_without_escaping_workspace() {
        let workspace = tempfile::tempdir().unwrap();
        let nested = workspace.path().join("packages/app/src");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(workspace.path().join("Cargo.toml"), "").unwrap();
        std::fs::write(workspace.path().join("packages/app/Cargo.toml"), "").unwrap();
        let file = nested.join("main.rs");
        std::fs::write(&file, "fn main() {}\n").unwrap();

        assert_eq!(
            LspManager::server_root(workspace.path(), &file, &["Cargo.toml".into()]),
            std::fs::canonicalize(workspace.path().join("packages/app")).unwrap()
        );
        assert_eq!(
            LspManager::server_root(workspace.path(), Path::new("../outside.rs"), &[]),
            std::fs::canonicalize(workspace.path()).unwrap()
        );
    }

    #[cfg(feature = "lsp-e2e")]
    #[tokio::test]
    async fn typescript_language_server_resolves_real_definition() {
        assert!(
            which::which("typescript-language-server").is_ok(),
            "lsp-e2e requires typescript-language-server on PATH"
        );
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(
            workspace.path().join("package.json"),
            "{\"private\":true}\n",
        )
        .unwrap();
        std::fs::write(
            workspace.path().join("tsconfig.json"),
            "{\"compilerOptions\":{\"strict\":true}}\n",
        )
        .unwrap();
        let content = "interface Widget { value: string }\nconst item: Widget = { value: 'ok' };\n";
        let path = workspace.path().join("main.ts");
        std::fs::write(&path, content).unwrap();
        let character = content.lines().nth(1).unwrap().find("Widget").unwrap() as u32 + 1;
        let manager = LspManager::new();
        let locations = manager
            .definition(
                workspace.path(),
                &path,
                content,
                2,
                character,
                &CancellationToken::new(),
            )
            .await
            .expect("typescript definition request");
        assert!(
            locations.iter().any(|location| location.line == 1),
            "expected Widget declaration on line 1, got {locations:?}"
        );
        manager.shutdown().await;
    }
}
