//! Pooled language-server manager: one [`LspClient`] per project root and server
//! command, started lazily and reused across compatible extensions. Ported from
//! production `lsp/manager.rs` (the event-channel / telemetry
//! coupling is dropped; absence of a server binary degrades gracefully).

use super::super::{lsp_policy::LspMode, LspSettings};
use super::client::LspClient;
use super::registry::{extension_to_language_id, LspServerRegistry};
use super::types::{Diagnostic, Location};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, RwLock};
use tokio_util::sync::CancellationToken;

/// How long to wait after syncing a document for the server to publish diagnostics.
const SETTLE_DELAY_MS: u64 = 350;
const STARTUP_TIMEOUT_SECS: u64 = 30;

#[derive(Clone, PartialEq, Eq, Hash)]
struct ClientKey {
    root: PathBuf,
    command: String,
    args: Vec<String>,
    identity: String,
}

impl std::fmt::Debug for ClientKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientKey")
            .field("root", &self.root)
            .field("command", &self.command)
            .field("identity", &"[private]")
            .finish_non_exhaustive()
    }
}

pub struct LspManager {
    policy: LspSettings,
    generation: RwLock<()>,
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

// Query budget covers all attempts and waits, not server initialization.
async fn retry_empty_query<T, F, Fut>(
    attempts: u32,
    interval: Duration,
    cancel: &CancellationToken,
    mut query: F,
    empty: impl Fn(&T) -> bool,
) -> Result<T, String>
where
    F: FnMut(CancellationToken) -> Fut,
    Fut: std::future::Future<Output = Result<T, String>>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    for attempt in 0..attempts {
        if cancel.is_cancelled() {
            return Err("LSP query cancelled".into());
        }
        let token = cancel.child_token();
        let request = query(token.clone());
        tokio::pin!(request);
        let value = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                token.cancel();
                let _ = request.await;
                return Err("LSP query cancelled".into());
            }
            _ = tokio::time::sleep_until(deadline) => {
                token.cancel();
                let _ = request.await;
                return Err("LSP query timed out (shared 30-second budget)".into());
            }
            result = &mut request => result?,
        };
        if !empty(&value) || attempt + 1 == attempts {
            return Ok(value);
        }
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err("LSP query cancelled".into()),
            _ = tokio::time::sleep_until(deadline) => return Err("LSP query timed out (shared 30-second budget)".into()),
            _ = tokio::time::sleep(interval) => {}
        }
    }
    Err("LSP query requires at least one attempt".into())
}

fn hover_empty(value: &Value) -> bool {
    fn contents_empty(value: &Value) -> bool {
        match value {
            Value::Null => true,
            Value::String(text) => text.trim().is_empty(),
            Value::Array(items) => items.iter().all(contents_empty),
            Value::Object(obj) => obj.get("value").is_some_and(contents_empty),
            _ => false,
        }
    }
    value.is_null() || value.get("contents").is_some_and(contents_empty)
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
            policy: Default::default(),
            generation: RwLock::new(()),
            clients: Mutex::new(HashMap::new()),
            startup_locks: Mutex::new(HashMap::new()),
            unavailable: Mutex::new(HashMap::new()),
            registry,
            settle_delay_ms,
        }
    }
    pub fn with_policy(mut self, policy: LspSettings) -> Self {
        self.policy = policy;
        self
    }

    pub fn feature_mode(&self, path: &Path, operation: &str) -> LspMode {
        let service = Self::ext_of(path).and_then(|ext| self.registry.get(&ext));
        service
            .map(|s| s.options.features.mode(operation, &self.policy.features))
            .unwrap_or_else(|| self.policy.features.mode(operation, &Default::default()))
    }

    pub fn check_feature(&self, path: &Path, operation: &str) -> Result<(), String> {
        if Self::ext_of(path)
            .and_then(|ext| self.registry.get(&ext))
            .is_some_and(|server| server.options.disabled)
        {
            return Err("language server is disabled by configuration".into());
        }
        if self.feature_mode(path, operation) == LspMode::Off {
            return Err(format!("LSP {operation} is disabled by configuration"));
        }
        Ok(())
    }

    fn check_query(&self, path: &Path, operation: &str) -> Result<(), String> {
        self.check_feature(path, operation)?;
        if let Some(server) = Self::ext_of(path).and_then(|ext| self.registry.get(&ext)) {
            if server
                .options
                .retry_attempts
                .is_some_and(|n| !(1..=5).contains(&n))
                || server.options.retry_interval_ms.is_some_and(|n| n > 10000)
            {
                return Err(
                    "LSP retry_attempts must be 1..=5 and retry_interval_ms 0..=10000".into(),
                );
            }
        }
        Ok(())
    }

    pub async fn reload(&self, cancel: &CancellationToken) -> Result<Value, String> {
        let _generation = tokio::select! { guard = self.generation.write() => guard, _ = cancel.cancelled() => return Err("LSP reload cancelled".into()) };
        let clients = std::mem::take(&mut *self.clients.lock().await);
        let count = clients.len();
        // Detach first: a cancelled shutdown must never republish a stale connection.
        self.unavailable.lock().await.clear();
        self.startup_locks.lock().await.clear();
        futures::future::join_all(
            clients
                .into_values()
                .map(|client| async move { client.shutdown().await }),
        )
        .await;
        Ok(
            json!({"status":"reloaded","closed_clients":count,"message":"Initialization/settings files will be reread on the next query. Server command and policy changes require runtime reload."}),
        )
    }

    pub async fn status(&self) -> Value {
        let clients = self.clients.lock().await;
        json!({"source":"local","connections":clients.values().map(|client| client.status()).collect::<Vec<_>>(),"configured_extensions":self.registry.available_extensions(),"startup_failures":self.unavailable.lock().await.len(),"trace":self.policy.trace,"trace_file":self.policy.trace_file,"init_options_off":self.policy.init_options_off})
    }

    pub async fn probe(
        &self,
        root: &Path,
        path: &Path,
        cancel: &CancellationToken,
    ) -> Result<Value, String> {
        let started = tokio::time::Instant::now();
        let client = self.get_or_start_server(root, path, cancel).await?;
        let mut status = client.status();
        status["elapsed_ms"] = json!(started.elapsed().as_millis());
        status["trace_file"] = json!(self.policy.trace_file);
        let operations = [
            "definition",
            "hover",
            "references",
            "diagnostics",
            "document_symbols",
        ];
        status["features"] = Value::Object(operations.iter().map(|operation| ((*operation).to_owned(), json!({"mode":self.feature_mode(path,operation),"advertised":client.supports(operation)}))).collect());
        Ok(status)
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
            identity: String::new(),
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
        let _generation = tokio::select! { guard = self.generation.read() => guard, _ = cancel.cancelled() => return Err("LSP startup cancelled".into()) };
        let Some(extension) = Self::ext_of(path) else {
            return Err("file has no supported extension".into());
        };
        let Some(config) = self.registry.get(&extension) else {
            return Err(format!("no language server configured for .{extension}"));
        };
        let server_root = Self::server_root(root, path, &config.root_markers);
        let absolute_path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            root.join(path)
        };
        let prepared = tokio::select! {
            result = super::options::prepare(config,&self.policy,&server_root,&absolute_path) => result?,
            _ = cancel.cancelled() => return Err("LSP configuration resolution cancelled".into()),
        };
        let mut key = Self::client_key(server_root, config);
        let diagnostics_enabled = self.feature_mode(path, "diagnostics") != LspMode::Off;
        key.identity = json!([
            prepared.initialization,
            prepared.settings,
            diagnostics_enabled
        ])
        .to_string();
        let trace = super::trace::Trace::open(
            self.policy.trace_file.as_deref(),
            self.policy.trace,
            prepared.redactions.clone(),
        )?;
        trace.record(
            "configuration",
            &json!({"source":prepared.source,"root":key.root}),
        );
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
            LspClient::spawn_prepared(config, &key.root, prepared, trace, diagnostics_enabled),
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
        self.check_query(path, "definition")?;
        let client = self.sync_document(root, path, content, cancel).await?;
        if self.feature_mode(path, "definition") == LspMode::Auto && !client.supports("definition")
        {
            return Err(format!(
                "LSP server does not advertise support for {} (use mode=on to explicitly try)",
                "definition"
            ));
        }
        let (line, character) = client.wire_position(content, line, character)?;
        self.retry_query(
            path,
            cancel,
            |token| {
                let client = client.clone();
                async move { client.definition(path, line, character, &token).await }
            },
            |values: &Vec<Location>| values.is_empty(),
        )
        .await
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
        self.check_query(path, "references")?;
        let client = self.sync_document(root, path, content, cancel).await?;
        if self.feature_mode(path, "references") == LspMode::Auto && !client.supports("references")
        {
            return Err(format!(
                "LSP server does not advertise support for {} (use mode=on to explicitly try)",
                "references"
            ));
        }
        let (line, character) = client.wire_position(content, line, character)?;
        self.retry_query(
            path,
            cancel,
            |token| {
                let client = client.clone();
                async move { client.references(path, line, character, &token).await }
            },
            |values: &Vec<Location>| values.is_empty(),
        )
        .await
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
        self.check_query(path, "hover")?;
        let client = self.sync_document(root, path, content, cancel).await?;
        if self.feature_mode(path, "hover") == LspMode::Auto && !client.supports("hover") {
            return Err(format!(
                "LSP server does not advertise support for {} (use mode=on to explicitly try)",
                "hover"
            ));
        }
        let (line, character) = client.wire_position(content, line, character)?;
        self.retry_query(
            path,
            cancel,
            |token| {
                let client = client.clone();
                async move { client.hover(path, line, character, &token).await }
            },
            hover_empty,
        )
        .await
    }

    pub async fn document_symbols(
        &self,
        root: &Path,
        path: &Path,
        content: &str,
        cancel: &CancellationToken,
    ) -> Result<Value, String> {
        self.check_query(path, "document_symbols")?;
        let client = self.sync_document(root, path, content, cancel).await?;
        if self.feature_mode(path, "document_symbols") == LspMode::Auto
            && !client.supports("document_symbols")
        {
            return Err(
                "LSP server does not advertise document_symbols (use mode=on to explicitly try)"
                    .into(),
            );
        }
        self.retry_query(
            path,
            cancel,
            |token| {
                let client = client.clone();
                async move { client.document_symbols(path, &token).await }
            },
            |value| value.is_null() || value.as_array().is_some_and(Vec::is_empty),
        )
        .await
    }

    async fn retry_query<T, F, Fut>(
        &self,
        path: &Path,
        cancel: &CancellationToken,
        query: F,
        empty: impl Fn(&T) -> bool,
    ) -> Result<T, String>
    where
        F: FnMut(CancellationToken) -> Fut,
        Fut: std::future::Future<Output = Result<T, String>>,
    {
        let options = Self::ext_of(path)
            .and_then(|ext| self.registry.get(&ext))
            .map(|s| &s.options);
        retry_empty_query(
            options.and_then(|o| o.retry_attempts).unwrap_or(1),
            Duration::from_millis(options.and_then(|o| o.retry_interval_ms).unwrap_or(5000)),
            cancel,
            query,
            empty,
        )
        .await
    }

    pub async fn refresh_pull_diagnostics(
        &self,
        root: &Path,
        path: &Path,
        cancel: &CancellationToken,
    ) -> Result<(), String> {
        self.check_feature(path, "diagnostics")?;
        let client = self.get_or_start_server(root, path, cancel).await?;
        client.refresh_pull_diagnostics(path, cancel).await
    }

    pub async fn diagnostics(&self, path: &Path) -> Vec<Diagnostic> {
        if self.check_feature(path, "diagnostics").is_err() {
            return vec![];
        }
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
        clients
            .iter()
            .flat_map(|c| c.all_diagnostics())
            .filter(|diagnostic| {
                self.check_feature(Path::new(&diagnostic.file), "diagnostics")
                    .is_ok()
            })
            .collect()
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
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn document_symbols_open_once_and_retry_on_the_same_client() {
        use super::super::jsonrpc;
        use tokio::io::{AsyncWriteExt, BufReader};
        let workspace = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(workspace.path()).unwrap();
        let file = root.join("main.cj");
        let mut registry = LspServerRegistry::empty();
        registry.insert(
            "cj",
            super::super::registry::LspServerConfig {
                command: "mock-no-spawn".into(),
                args: vec![],
                root_markers: vec![],
                options: super::super::super::lsp_policy::LspOptions {
                    retry_attempts: Some(3),
                    retry_interval_ms: Some(5000),
                    ..Default::default()
                },
            },
        );
        let manager = LspManager::with_registry(registry);
        let (client_end, server_end) = tokio::io::duplex(8192);
        let (cr, cw) = tokio::io::split(client_end);
        let (sr, mut sw) = tokio::io::split(server_end);
        let server = tokio::spawn(async move {
            let mut reader = BufReader::new(sr);
            let mut opened = 0;
            let mut queries = 0;
            while let Ok(body) = jsonrpc::read_message(&mut reader).await {
                let message: Value = serde_json::from_slice(&body).unwrap();
                let result = match message["method"].as_str() {
                    Some("initialize") => {
                        assert_eq!(message.pointer("/params/capabilities/textDocument/documentSymbol/hierarchicalDocumentSymbolSupport"), Some(&json!(true)));
                        json!({"capabilities":{"documentSymbolProvider":true}})
                    }
                    Some("textDocument/didOpen") => {
                        opened += 1;
                        continue;
                    }
                    Some("textDocument/documentSymbol") => {
                        assert_eq!(opened, 1);
                        assert!(message["params"].get("position").is_none());
                        queries += 1;
                        if queries == 1 {
                            json!([])
                        } else {
                            json!([{"name":"启动","kind":12,"range":{"start":{"line":30,"character":0},"end":{"line":31,"character":0}},"selectionRange":{"start":{"line":30,"character":0},"end":{"line":30,"character":2}}}])
                        }
                    }
                    Some("shutdown") => Value::Null,
                    Some("exit") => break,
                    _ => continue,
                };
                sw.write_all(&jsonrpc::encode(
                    &serde_json::to_vec(
                        &json!({"jsonrpc":"2.0","id":message["id"],"result":result}),
                    )
                    .unwrap(),
                ))
                .await
                .unwrap();
            }
            (opened, queries)
        });
        let client = LspClient::connect(
            Box::new(cr),
            Box::new(cw),
            url::Url::from_file_path(&root).unwrap().to_string(),
        )
        .await
        .unwrap();
        let mut key = LspManager::client_key(root.clone(), manager.registry.get("cj").unwrap());
        key.identity = json!([Value::Null, json!({}), true]).to_string();
        manager.clients.lock().await.insert(key, Arc::new(client));
        let result = manager
            .document_symbols(&root, &file, "func 启动() {}", &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(result[0]["name"], "启动");
        manager.shutdown().await;
        assert_eq!(server.await.unwrap(), (1, 2));
    }

    #[test]
    fn invalid_retry_configuration_and_disabled_symbols_fail_before_startup() {
        let mut registry = missing_binary_registry();
        let mut config = registry.get("rs").unwrap().clone();
        config.options.retry_attempts = Some(0);
        registry.insert("rs", config.clone());
        let manager = LspManager::with_registry(registry);
        assert!(manager
            .check_query(Path::new("main.rs"), "hover")
            .unwrap_err()
            .contains("retry_attempts"));
        config.options.retry_attempts = Some(3);
        config.options.features.document_symbols = Some(LspMode::Off);
        let mut registry = LspServerRegistry::empty();
        registry.insert("rs", config);
        assert!(LspManager::with_registry(registry)
            .check_feature(Path::new("main.rs"), "document_symbols")
            .unwrap_err()
            .contains("disabled"));
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn empty_query_retries_hit_and_retains_last_empty_result() {
        for (attempts, hit, expected) in [(1, 2, 1), (3, 2, 2), (3, 9, 3)] {
            let calls = std::cell::Cell::new(0);
            let start = tokio::time::Instant::now();
            let result = retry_empty_query(
                attempts,
                Duration::from_secs(5),
                &CancellationToken::new(),
                |_| {
                    calls.set(calls.get() + 1);
                    let value = if calls.get() == hit { vec![42] } else { vec![] };
                    async move { Ok(value) }
                },
                Vec::is_empty,
            )
            .await
            .unwrap();
            assert_eq!(calls.get(), expected);
            assert_eq!(!result.is_empty(), expected == hit);
            assert_eq!(
                start.elapsed(),
                Duration::from_secs(5 * (expected - 1) as u64)
            );
        }
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn empty_query_errors_cancel_and_budget_do_not_retry_forever() {
        let calls = std::cell::Cell::new(0);
        let result = retry_empty_query(
            3,
            Duration::ZERO,
            &CancellationToken::new(),
            |_| {
                calls.set(calls.get() + 1);
                async { Err::<Vec<u8>, _>("protocol failure".into()) }
            },
            Vec::is_empty,
        )
        .await;
        assert_eq!(result.unwrap_err(), "protocol failure");
        assert_eq!(calls.get(), 1);
        let cancel = CancellationToken::new();
        let cancelling = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
            cancelling.cancel();
        });
        let result = retry_empty_query(
            3,
            Duration::from_secs(5),
            &cancel,
            |_| async { Ok(Vec::<u8>::new()) },
            Vec::is_empty,
        )
        .await;
        assert!(result.unwrap_err().contains("cancelled"));
        let start = tokio::time::Instant::now();
        let result = retry_empty_query(
            5,
            Duration::from_secs(10),
            &CancellationToken::new(),
            |_| async { Ok(Vec::<u8>::new()) },
            Vec::is_empty,
        )
        .await;
        assert!(result.unwrap_err().contains("shared 30-second budget"));
        assert_eq!(start.elapsed(), Duration::from_secs(30));
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn budget_cancels_the_in_flight_request_before_returning() {
        let finished = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let finished_query = finished.clone();
        let result = retry_empty_query(
            3,
            Duration::ZERO,
            &CancellationToken::new(),
            |token| {
                let finished = finished_query.clone();
                async move {
                    token.cancelled().await;
                    finished.store(true, std::sync::atomic::Ordering::SeqCst);
                    Err::<Vec<u8>, _>("cancelled".into())
                }
            },
            Vec::is_empty,
        )
        .await;
        assert!(result.unwrap_err().contains("timed out"));
        assert!(finished.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn hover_empty_checks_contents_not_the_presence_of_a_range() {
        for value in [
            Value::Null,
            json!({"contents": ""}),
            json!({"contents":{"kind":"markdown","value":" "},"range":{}}),
            json!({"contents":[]}),
        ] {
            assert!(hover_empty(&value));
        }
        assert!(!hover_empty(
            &json!({"contents":[{"language":"cj","value":"中文签名"}]})
        ));
    }

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
                options: Default::default(),
            },
        );
        r
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn reload_closes_clients_and_resolves_in_flight_requests() {
        use super::super::jsonrpc;
        use tokio::io::{AsyncWriteExt, BufReader};
        let workspace = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(workspace.path()).unwrap();
        let manager = Arc::new(LspManager::with_registry(missing_binary_registry()));
        let config = manager.registry.get("rs").unwrap();
        let (client_end, server_end) = tokio::io::duplex(8192);
        let (cr, cw) = tokio::io::split(client_end);
        let (sr, mut sw) = tokio::io::split(server_end);
        let (hover_started, hover_seen) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let mut reader = BufReader::new(sr);
            let mut hover_started = Some(hover_started);
            loop {
                let Ok(body) = jsonrpc::read_message(&mut reader).await else {
                    break;
                };
                let message: Value = serde_json::from_slice(&body).unwrap();
                match message["method"].as_str() {
                    Some("initialize") => {
                        let reply = json!({"jsonrpc":"2.0","id":message["id"],"result":{"capabilities":{"hoverProvider":true}}});
                        sw.write_all(&jsonrpc::encode(&serde_json::to_vec(&reply).unwrap()))
                            .await
                            .unwrap();
                    }
                    Some("textDocument/hover") => {
                        let _ = hover_started.take().unwrap().send(());
                    }
                    Some("shutdown") => {
                        let reply = json!({"jsonrpc":"2.0","id":message["id"],"result":null});
                        sw.write_all(&jsonrpc::encode(&serde_json::to_vec(&reply).unwrap()))
                            .await
                            .unwrap();
                    }
                    Some("exit") => break,
                    _ => {}
                }
            }
        });
        let client = LspClient::connect(
            Box::new(cr),
            Box::new(cw),
            url::Url::from_file_path(&root).unwrap().to_string(),
        )
        .await
        .unwrap();
        let mut key = LspManager::client_key(root.clone(), config);
        key.identity = json!([Value::Null, json!({}), true]).to_string();
        manager
            .clients
            .lock()
            .await
            .insert(key.clone(), Arc::new(client));
        manager
            .unavailable
            .lock()
            .await
            .insert(key, "old failure".into());
        let work = manager.clone();
        let file = root.join("main.rs");
        let query = tokio::spawn(async move {
            work.hover(
                &root,
                &file,
                "fn main() {}",
                1,
                1,
                &CancellationToken::new(),
            )
            .await
        });
        hover_seen.await.unwrap();
        assert_eq!(
            manager.reload(&CancellationToken::new()).await.unwrap()["closed_clients"],
            1
        );
        let error = query.await.unwrap().unwrap_err();
        assert!(
            error.contains("shutdown") || error.contains("stream closed"),
            "{error}"
        );
        assert!(!manager.has_servers().await);
        assert!(manager.unavailable.lock().await.is_empty());
        server.await.unwrap();
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
        let mut key =
            LspManager::client_key(std::fs::canonicalize(workspace.path()).unwrap(), config);
        key.identity = json!([Value::Null, json!({}), true]).to_string();
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
            options: Default::default(),
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
