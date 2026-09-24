//! Connections a runtime keeps across the capability trees it builds.
//!
//! Each generation of a coding runtime prepares its own [`McpRegistry`] — its own
//! approvals, aliases, cancellation and events (`docs/adr/0002`). What a
//! generation may take over from the one before is only the connection itself: a
//! stdio child or an HTTP session that is already up, for the same server,
//! configured the same way, in the same directory. Switching sessions then
//! reuses the servers instead of restarting every one of them.
//!
//! This module is the mechanism. The pool is held by the runtime that owns the
//! generations and handed to each prepare; nothing here is process-wide.
//!
//! [`McpRegistry`]: super::registry::McpRegistry

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::client::{McpClient, McpToolInfo};
use super::config::{McpConfigSource, McpServerConfig, McpTransportConfig};
use super::types::ServerStatus;

/// What a connection was made from. Two connections are interchangeable only
/// when every field is equal.
///
/// Compared field by field rather than hashed, so a refusal can say which field
/// changed. What a registry decides for itself — `trust: true`, `autoApprove` —
/// is left out: every generation seeds those from the config it read. OAuth
/// tokens are left out because an HTTP client reads them on each request; a
/// sign-out clears the whole pool instead.
#[derive(Debug, Clone, PartialEq)]
pub struct McpConnectionIdentity {
    /// The project the connection serves: the stdio child's working directory,
    /// and the key the project's trust is recorded under.
    pub project_dir: PathBuf,
    /// Whether the project was trusted when the connection was made.
    pub project_trusted: bool,
    pub name: String,
    pub source: McpConfigSource,
    /// Resolved transport: `${VAR}` already expanded when the config was read.
    pub transport: McpTransportConfig,
}

impl McpConnectionIdentity {
    pub fn new(project_dir: &Path, project_trusted: bool, config: &McpServerConfig) -> Self {
        Self {
            project_dir: project_dir.to_path_buf(),
            project_trusted,
            name: config.name.clone(),
            source: config.source,
            transport: config.config.clone(),
        }
    }

    /// The first field in which `self` differs from `other`, for diagnostics.
    pub fn first_difference(&self, other: &Self) -> Option<&'static str> {
        if self.project_dir != other.project_dir {
            Some("project_dir")
        } else if self.project_trusted != other.project_trusted {
            Some("project_trusted")
        } else if self.name != other.name {
            Some("name")
        } else if self.source != other.source {
            Some("source")
        } else if self.transport != other.transport {
            Some("transport")
        } else {
            None
        }
    }
}

/// A connection a registry may take over.
#[derive(Clone)]
pub struct PooledConnection {
    pub identity: McpConnectionIdentity,
    /// Already initialised. The same `Arc` the registries hold: the stdio child
    /// ends when the last of them lets go.
    pub client: Arc<dyn McpClient>,
    /// The server's `initialize` instructions, normalised.
    pub instructions: Option<String>,
    pub timeout_ms: u64,
    /// The server's tools as last listed. Handed over with the connection so the
    /// taking generation offers exactly the same definitions — the request cache
    /// prefix does not move — before it has listed them itself.
    pub tools: Vec<McpToolInfo>,
}

/// The pool: at most one connection per server name.
///
/// `generation` goes up every time the pool is cleared. A connection being
/// made when the pool was cleared (a reload came in while a server was still
/// starting) is not let in afterwards: [`Self::put`] refuses a connection
/// started under an older generation.
#[derive(Default)]
pub struct McpConnectionPool {
    inner: Mutex<PoolInner>,
}

impl std::fmt::Debug for McpConnectionPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.lock();
        f.debug_struct("McpConnectionPool")
            .field("generation", &inner.generation)
            .field("servers", &inner.entries.keys().collect::<Vec<_>>())
            .finish()
    }
}

#[derive(Default)]
struct PoolInner {
    generation: u64,
    entries: BTreeMap<String, PooledConnection>,
}

impl McpConnectionPool {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, PoolInner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The current generation, to pass back to [`Self::put`].
    pub fn generation(&self) -> u64 {
        self.lock().generation
    }

    /// The connection for `identity`, if the pool holds one made the same way
    /// and it is still connected.
    pub fn take_over(&self, identity: &McpConnectionIdentity) -> Option<PooledConnection> {
        let inner = self.lock();
        let entry = inner.entries.get(&identity.name)?;
        if let Some(field) = entry.identity.first_difference(identity) {
            tracing::debug!(
                target: "atomcode::mcp",
                server = %identity.name,
                field,
                "not reusing the connection: its configuration changed"
            );
            return None;
        }
        if !matches!(entry.client.status(), ServerStatus::Connected) {
            tracing::debug!(
                target: "atomcode::mcp",
                server = %identity.name,
                "not reusing the connection: it is no longer connected"
            );
            return None;
        }
        Some(entry.clone())
    }

    /// Offer a connection made under `generation`. Refused — and `false`
    /// returned — when the pool has been cleared since.
    pub fn put(&self, generation: u64, connection: PooledConnection) -> bool {
        let mut inner = self.lock();
        if inner.generation != generation {
            return false;
        }
        inner
            .entries
            .insert(connection.identity.name.clone(), connection);
        true
    }

    /// Update what a pooled server offers, when it is still the same connection.
    pub fn record_tools(&self, name: &str, client: &Arc<dyn McpClient>, tools: Vec<McpToolInfo>) {
        let mut inner = self.lock();
        if let Some(entry) = inner.entries.get_mut(name) {
            if Arc::ptr_eq(&entry.client, client) {
                entry.tools = tools;
            }
        }
    }

    /// Keep only the connections `in_use` still holds (by name and the very same
    /// client). What is dropped closes once no registry holds it either.
    pub fn retain(&self, in_use: &BTreeMap<String, Arc<dyn McpClient>>) {
        let mut inner = self.lock();
        inner.entries.retain(|name, entry| {
            in_use
                .get(name)
                .is_some_and(|client| Arc::ptr_eq(client, &entry.client))
        });
    }

    /// Forget every connection and refuse any made before now. Used when the
    /// connections must be made again: a reload, a trust or sign-in change.
    pub fn clear(&self) {
        let mut inner = self.lock();
        inner.generation += 1;
        inner.entries.clear();
    }

    /// The server names the pool holds, for tests and diagnostics.
    pub fn names(&self) -> Vec<String> {
        self.lock().entries.keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::types::{CallToolResult, InitializeResult, ListToolsResult};
    use std::sync::atomic::{AtomicBool, Ordering};

    /// A connection whose liveness the test controls.
    struct FakeClient(AtomicBool);

    #[async_trait::async_trait]
    impl McpClient for FakeClient {
        async fn initialize(&mut self) -> anyhow::Result<InitializeResult> {
            anyhow::bail!("not used")
        }
        async fn list_tools(&self) -> anyhow::Result<ListToolsResult> {
            anyhow::bail!("not used")
        }
        async fn call_tool(
            &self,
            _tool_name: &str,
            _arguments: serde_json::Value,
        ) -> anyhow::Result<CallToolResult> {
            anyhow::bail!("not used")
        }
        fn server_name(&self) -> &str {
            "fake"
        }
        fn status(&self) -> ServerStatus {
            if self.0.load(Ordering::Acquire) {
                ServerStatus::Connected
            } else {
                ServerStatus::Disconnected
            }
        }
    }

    fn identity(dir: &str, command: &str) -> McpConnectionIdentity {
        McpConnectionIdentity {
            project_dir: PathBuf::from(dir),
            project_trusted: true,
            name: "srv".into(),
            source: McpConfigSource::User,
            transport: McpTransportConfig::Stdio {
                command: command.into(),
                args: Vec::new(),
                env: BTreeMap::new(),
                timeout_ms: None,
            },
        }
    }

    fn connection(identity: McpConnectionIdentity) -> (PooledConnection, Arc<FakeClient>) {
        let fake = Arc::new(FakeClient(AtomicBool::new(true)));
        let client: Arc<dyn McpClient> = fake.clone();
        let pooled = PooledConnection {
            identity,
            client,
            instructions: None,
            timeout_ms: 1_000,
            tools: Vec::new(),
        };
        (pooled, fake)
    }

    #[test]
    fn a_connection_made_the_same_way_is_handed_over() {
        let pool = McpConnectionPool::new();
        let (pooled, _) = connection(identity("/p", "srv"));
        assert!(pool.put(pool.generation(), pooled.clone()));
        let taken = pool
            .take_over(&identity("/p", "srv"))
            .expect("same identity");
        assert!(Arc::ptr_eq(&taken.client, &pooled.client));
    }

    #[test]
    fn any_difference_in_how_it_was_made_is_a_new_connection() {
        let pool = McpConnectionPool::new();
        let (pooled, _) = connection(identity("/p", "srv"));
        pool.put(pool.generation(), pooled);
        assert!(pool.take_over(&identity("/other", "srv")).is_none());
        assert!(pool.take_over(&identity("/p", "srv-v2")).is_none());
        let mut untrusted = identity("/p", "srv");
        untrusted.project_trusted = false;
        assert!(pool.take_over(&untrusted).is_none());
        assert_eq!(
            identity("/p", "srv").first_difference(&identity("/p", "srv-v2")),
            Some("transport")
        );
    }

    #[test]
    fn a_connection_that_is_no_longer_up_is_not_handed_over() {
        let pool = McpConnectionPool::new();
        let (pooled, fake) = connection(identity("/p", "srv"));
        pool.put(pool.generation(), pooled);
        fake.0.store(false, Ordering::Release);
        assert!(pool.take_over(&identity("/p", "srv")).is_none());
    }

    /// A server still starting when the pool was cleared (a reload came in) must
    /// not get into the cleared pool when it finishes.
    #[test]
    fn a_connection_started_before_a_clear_is_turned_away() {
        let pool = McpConnectionPool::new();
        let started_under = pool.generation();
        pool.clear();
        let (late, _) = connection(identity("/p", "srv"));
        assert!(!pool.put(started_under, late));
        assert!(pool.names().is_empty());
    }

    #[test]
    fn only_the_connections_in_use_are_kept() {
        let pool = McpConnectionPool::new();
        let (pooled, _) = connection(identity("/p", "srv"));
        pool.put(pool.generation(), pooled.clone());

        let mut in_use = BTreeMap::new();
        in_use.insert("srv".to_string(), Arc::clone(&pooled.client));
        pool.retain(&in_use);
        assert_eq!(pool.names(), vec!["srv".to_string()]);

        // The same name held by a different connection is not this one.
        let (other, _) = connection(identity("/p", "srv"));
        in_use.insert("srv".to_string(), other.client);
        pool.retain(&in_use);
        assert!(pool.names().is_empty());
    }
}
