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

/// The pool: at most one connection per server name, all of them the connections
/// of one registry — the one the runtime is using, its *owner*.
///
/// Only the owner adds to it ([`Self::put`], checked under the pool's lock).
/// Every other registry that is still finishing connections — a candidate that
/// is being built or failed, one superseded by a switch, one orphaned by a
/// prepare that failed half way — is turned away whenever it finishes: its
/// connections close with it instead of outliving it here, and none of them
/// can push the owner's connection for the same server out.
#[derive(Default)]
pub struct McpConnectionPool {
    inner: Mutex<PoolInner>,
}

impl std::fmt::Debug for McpConnectionPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.lock();
        f.debug_struct("McpConnectionPool")
            .field("owner", &inner.owner)
            .field("servers", &inner.entries.keys().collect::<Vec<_>>())
            .finish()
    }
}

#[derive(Default)]
struct PoolInner {
    /// The registry whose connections these are (`McpRegistry::id`). `None`
    /// after [`McpConnectionPool::clear`], until a registry settles.
    owner: Option<u64>,
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

    /// Offer a connection `registry` just made. Taken only when `registry` is the
    /// owner; `false` otherwise.
    pub fn put(&self, registry: u64, connection: PooledConnection) -> bool {
        let mut inner = self.lock();
        if inner.owner != Some(registry) {
            return false;
        }
        inner
            .entries
            .insert(connection.identity.name.clone(), connection);
        true
    }

    /// Make `registry` the owner, holding exactly `connections` — its own, as it
    /// reports them. What the pool held for another registry is let go and
    /// closes once nothing else holds it. When `registry` already was the owner,
    /// what it put since it was read is kept too.
    pub fn settle(&self, registry: u64, connections: Vec<PooledConnection>) {
        let mut inner = self.lock();
        let kept = if inner.owner == Some(registry) {
            std::mem::take(&mut inner.entries)
        } else {
            BTreeMap::new()
        };
        inner.owner = Some(registry);
        inner.entries = kept;
        for connection in connections {
            inner
                .entries
                .insert(connection.identity.name.clone(), connection);
        }
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

    /// Forget every connection, with no owner until a registry settles. Used when
    /// the connections must be made again: a reload, a trust or sign-in change.
    pub fn clear(&self) {
        let mut inner = self.lock();
        inner.owner = None;
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

    const OWNER: u64 = 1;
    const OTHER: u64 = 2;

    #[test]
    fn a_connection_made_the_same_way_is_handed_over() {
        let pool = McpConnectionPool::new();
        let (pooled, _) = connection(identity("/p", "srv"));
        pool.settle(OWNER, vec![pooled.clone()]);
        let taken = pool
            .take_over(&identity("/p", "srv"))
            .expect("same identity");
        assert!(Arc::ptr_eq(&taken.client, &pooled.client));
    }

    #[test]
    fn any_difference_in_how_it_was_made_is_a_new_connection() {
        let pool = McpConnectionPool::new();
        let (pooled, _) = connection(identity("/p", "srv"));
        pool.settle(OWNER, vec![pooled]);
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
        pool.settle(OWNER, vec![pooled]);
        fake.0.store(false, Ordering::Release);
        assert!(pool.take_over(&identity("/p", "srv")).is_none());
    }

    /// Only the registry in use adds to the pool. A candidate, a superseded one,
    /// one orphaned by a failed prepare — whichever finishes a connection late —
    /// is turned away, and cannot push the owner's connection for the same
    /// server out.
    #[test]
    fn only_the_registry_in_use_adds_to_the_pool() {
        let pool = McpConnectionPool::new();
        let (own, _) = connection(identity("/p", "srv"));
        pool.settle(OWNER, Vec::new());
        assert!(pool.put(OWNER, own.clone()));

        let (late, _) = connection(identity("/p", "srv"));
        assert!(!pool.put(OTHER, late));
        let held = pool.take_over(&identity("/p", "srv")).unwrap();
        assert!(Arc::ptr_eq(&held.client, &own.client));
    }

    /// A new owner holds exactly its own connections; what the old one held is
    /// let go.
    #[test]
    fn a_new_owner_holds_only_its_own_connections() {
        let pool = McpConnectionPool::new();
        let (old, _) = connection(identity("/a", "srv"));
        pool.settle(OWNER, vec![old]);
        pool.settle(OTHER, Vec::new());
        assert!(pool.names().is_empty());
        assert!(!pool.put(OWNER, connection(identity("/a", "srv")).0));
    }

    /// Settling the owner again keeps what it put since it was last read.
    #[test]
    fn settling_the_same_owner_keeps_what_it_put() {
        let pool = McpConnectionPool::new();
        pool.settle(OWNER, Vec::new());
        assert!(pool.put(OWNER, connection(identity("/p", "srv")).0));
        pool.settle(OWNER, Vec::new());
        assert_eq!(pool.names(), vec!["srv".to_string()]);
    }

    /// After a clear nobody adds until a registry settles: a server that was
    /// still starting when a reload cleared the pool does not get in.
    #[test]
    fn after_a_clear_nothing_gets_in_until_a_registry_settles() {
        let pool = McpConnectionPool::new();
        pool.settle(OWNER, Vec::new());
        pool.clear();
        assert!(!pool.put(OWNER, connection(identity("/p", "srv")).0));
        assert!(pool.names().is_empty());
    }
}
