//! The runtime: turn a config tree plus a plugin registry into a live system,
//! and keep it patchable while it runs.

use std::collections::HashMap;
use std::sync::Arc;

use crate::context::{Context, Root};
use crate::error::{PlexusError, Result};
use crate::fiber::{FiberId, FiberStatus};
use crate::loader::{ConfigTree, Entry, Layer};
use crate::plugin::PluginRegistry;

pub struct App {
    root: Arc<Root>,
    tree: ConfigTree,
    /// Row id → the fiber currently realizing it.
    mounted: HashMap<String, FiberId>,
    /// Rows whose dependencies were not satisfiable when they were last tried.
    ///
    /// Kept rather than discarded, because "not yet" and "never" are different
    /// answers: a later patch that mounts the missing provider should bring
    /// these up without the caller re-listing them. This is the closest thing
    /// here to cordis's fibers waiting on their injections.
    pending: Vec<Entry>,
}

impl App {
    pub fn new(registry: PluginRegistry, tree: ConfigTree) -> Self {
        Self {
            root: Root::new(registry),
            tree,
            mounted: HashMap::new(),
            pending: Vec::new(),
        }
    }

    /// The host's own context. Registrations made through it belong to the root
    /// fiber and outlive every plugin.
    pub fn context(&self) -> Context {
        Context::new(self.root.clone())
    }

    pub fn tree(&self) -> &ConfigTree {
        &self.tree
    }

    /// Mount every enabled row.
    ///
    /// Order in the file is irrelevant: rows activate as their `inject`ed
    /// services appear, which is why a bundle can list a consumer above the
    /// provider it needs. Mounting runs to a fixed point; whatever is still
    /// waiting when no further progress is possible is a composition error, and
    /// the error names each row with the services it never got.
    pub async fn start(&mut self) -> Result<()> {
        let rows: Vec<Entry> = self.tree.active().cloned().collect();
        self.mount_all(rows).await?;
        // Startup is strict: a row that cannot come up is a configuration
        // error the operator should see now, not a background wait that turns
        // into a mystery later.
        self.fail_on_pending()
    }

    /// Mount what can be mounted and keep the rest waiting.
    ///
    /// The non-strict counterpart of [`start`](Self::start), for a host that
    /// expects to fill slots itself after the tree is up.
    pub async fn start_lenient(&mut self) -> Result<()> {
        let rows: Vec<Entry> = self.tree.active().cloned().collect();
        self.mount_all(rows).await
    }

    /// Rows still waiting on a service, with what each is missing.
    pub fn pending(&self) -> Vec<(String, Vec<String>)> {
        let ctx = self.context();
        self.pending
            .iter()
            .map(|entry| {
                let missing = self
                    .root
                    .registry
                    .get(&entry.name)
                    .map(|p| {
                        p.inject()
                            .iter()
                            .filter(|n| !ctx.has_service(n))
                            .map(|n| n.to_string())
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                (entry.id.clone(), missing)
            })
            .collect()
    }

    fn fail_on_pending(&self) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        Err(PlexusError::Deadlock {
            pending: self.pending(),
        })
    }

    async fn mount_all(&mut self, mut pending: Vec<Entry>) -> Result<()> {
        // Fail on unknown names before mounting anything: a typo should not
        // leave half a system running.
        for entry in &pending {
            if self.root.registry.get(&entry.name).is_none() {
                return Err(PlexusError::UnknownPlugin {
                    entry: entry.id.clone(),
                    plugin: entry.name.clone(),
                });
            }
        }

        loop {
            let mut progressed = false;
            let mut still_pending = Vec::new();
            for entry in pending {
                let plugin = self
                    .root
                    .registry
                    .get(&entry.name)
                    .expect("presence checked above");
                let root_ctx = self.context();
                let ready = plugin
                    .inject()
                    .iter()
                    .all(|name| root_ctx.has_service(name));
                if !ready {
                    still_pending.push(entry);
                    continue;
                }
                let fiber = self.root.fibers.create(
                    entry.id.clone(),
                    entry.name.clone(),
                    crate::fiber::ROOT_FIBER,
                );
                let ctx = root_ctx.for_fiber(fiber);
                match plugin.apply(&ctx, &entry.config).await {
                    Ok(()) => {
                        if let Some(state) = self.root.fibers.get(fiber) {
                            state.set_status(FiberStatus::Active);
                        }
                        self.mounted.insert(entry.id.clone(), fiber);
                        progressed = true;
                    }
                    Err(message) => {
                        // Revert the partial footprint so a failed row leaves no
                        // half-registered services behind.
                        self.root.fibers.unload(fiber);
                        return Err(PlexusError::Apply {
                            entry: entry.id.clone(),
                            message,
                        });
                    }
                }
            }
            pending = still_pending;
            if pending.is_empty() || !progressed {
                break;
            }
        }

        // Whatever is left waits for a service that may yet arrive.
        self.pending = pending;
        Ok(())
    }

    /// Apply a patch layer to a *running* system.
    ///
    /// Rows whose plugin or config changed are unloaded and re-mounted; rows the
    /// layer removed or disabled are unloaded; new rows are mounted. Untouched
    /// rows keep running — their services and listeners are never disturbed,
    /// which is the difference between patching a tree and restarting a process.
    pub async fn patch(&mut self, layer: &Layer) -> Result<()> {
        let before: HashMap<String, Entry> = self
            .tree
            .entries
            .iter()
            .map(|e| (e.id.clone(), e.clone()))
            .collect();
        self.tree.apply(layer)?;
        let after: HashMap<String, Entry> = self
            .tree
            .entries
            .iter()
            .map(|e| (e.id.clone(), e.clone()))
            .collect();

        let mut to_mount = Vec::new();
        for (id, old) in &before {
            match after.get(id) {
                None => self.unload_row(id),
                Some(new) if new.disabled => {
                    if !old.disabled {
                        self.unload_row(id);
                    }
                }
                Some(new) if new.name != old.name || new.config != old.config || old.disabled => {
                    self.unload_row(id);
                    to_mount.push(new.clone());
                }
                Some(_) => {}
            }
        }
        for (id, new) in &after {
            if !before.contains_key(id) && !new.disabled {
                to_mount.push(new.clone());
            }
        }
        // Deterministic remount order: follow the tree, not the hash map.
        let order: HashMap<&str, usize> = self
            .tree
            .entries
            .iter()
            .enumerate()
            .map(|(i, e)| (e.id.as_str(), i))
            .collect();
        // Retry everything that was waiting: the point of this patch may be the
        // provider they were waiting for.
        let waiting = std::mem::take(&mut self.pending);
        for entry in waiting {
            if !to_mount.iter().any(|e| e.id == entry.id) {
                // Take the row's current shape, not the one it had when it
                // first failed — a patch may have fixed it in place.
                if let Some(current) = after.get(&entry.id) {
                    if !current.disabled {
                        to_mount.push(current.clone());
                    }
                }
            }
        }
        to_mount.sort_by_key(|e| order.get(e.id.as_str()).copied().unwrap_or(usize::MAX));
        self.mount_all(to_mount).await
    }

    fn unload_row(&mut self, id: &str) {
        if let Some(fiber) = self.mounted.remove(id) {
            self.root.fibers.unload(fiber);
        }
    }

    /// Unload everything, newest fiber first.
    pub fn stop(&mut self) {
        let mut ids: Vec<FiberId> = self.mounted.values().copied().collect();
        ids.sort_unstable();
        for fiber in ids.into_iter().rev() {
            self.root.fibers.unload(fiber);
        }
        self.mounted.clear();
    }

    /// Check the declared roles against what actually happened.
    ///
    /// A plugin's `provides` / `inject` lists are documentation until something
    /// compares them to reality, and documentation about wiring is exactly the
    /// kind that rots. This walks the mounted tree and reports every mismatch:
    ///
    /// - a row that declared `provides` and did not fill the slot (a silent
    ///   half-mount — the seam looks configured and is not);
    /// - a row that filled a slot it never declared (invisible to the capability
    ///   map, so nobody knows to look for a replacement);
    /// - a service nobody consumes (dead weight, or a consumer that resolved it
    ///   by some other means);
    /// - an `inject` naming a service no registered plugin provides (a typo, or
    ///   a row someone forgot to add).
    ///
    /// Returns an empty vec when the tree is consistent. Intended for a test and
    /// for a `--dump-seams`-style command, not for the hot path.
    pub fn audit(&self) -> Vec<AuditFinding> {
        self.audit_with_host(&[])
    }

    /// [`audit`](Self::audit), told which slots the host itself reads.
    ///
    /// A launcher that calls `agent-loop` directly is a consumer; without
    /// saying so, every such slot reads as dead weight and the real dead weight
    /// gets lost in the noise.
    pub fn audit_with_host(&self, host_consumed: &[&str]) -> Vec<AuditFinding> {
        self.audit_with(host_consumed, &[])
    }

    /// [`audit`](Self::audit), told what the host both reads and fills.
    ///
    /// A host can provide as well as consume — a launcher that owns the `App`
    /// is the only thing that can offer runtime reconfiguration, so it fills
    /// that slot itself. Without saying so, every consumer of it looks like it
    /// depends on a service nobody provides.
    pub fn audit_with(&self, host_consumed: &[&str], host_provided: &[&str]) -> Vec<AuditFinding> {
        let ctx = self.context();
        let live: Vec<&'static str> = ctx.service_names();
        let mut findings = Vec::new();

        // Everything any *registered* plugin claims it can provide. A seam whose
        // provider is merely not mounted is a config choice, not a defect.
        let mut providable: HashMap<&'static str, Vec<&'static str>> = HashMap::new();
        for name in self.root.registry.names() {
            let Some(plugin) = self.root.registry.get(name) else {
                continue;
            };
            for service in plugin.provides() {
                providable.entry(service).or_default().push(name);
            }
        }

        let mut consumed: HashMap<&'static str, Vec<String>> = HashMap::new();
        for (id, fiber) in &self.mounted {
            let Some(entry) = self.tree.entries.iter().find(|e| &e.id == id) else {
                continue;
            };
            let Some(plugin) = self.root.registry.get(&entry.name) else {
                continue;
            };
            for service in plugin.inject().iter().chain(plugin.uses()) {
                consumed.entry(service).or_default().push(id.clone());
            }
            for service in plugin.provides() {
                if !live.contains(service) {
                    findings.push(AuditFinding::DeclaredButNotProvided {
                        entry: id.clone(),
                        service,
                    });
                }
            }
            // The reverse — a slot filled by a fiber whose plugin never
            // declared it — is read off the service table's ownership records.
            for service in self.root.services.owned_by(*fiber) {
                if !plugin.provides().contains(&service) {
                    findings.push(AuditFinding::ProvidedButNotDeclared {
                        entry: id.clone(),
                        service,
                    });
                }
            }
        }

        for service in &live {
            if !consumed.contains_key(service) && !host_consumed.contains(service) {
                findings.push(AuditFinding::ProvidedButUnused { service });
            }
        }
        for (service, consumers) in &consumed {
            if !providable.contains_key(service) && !host_provided.contains(service) {
                findings.push(AuditFinding::InjectedButUnprovidable {
                    service,
                    consumers: consumers.clone(),
                });
            }
        }

        findings.sort_by_key(|f| f.sort_key());
        findings
    }

    /// What is actually running: rows, their plugins, their status, and the
    /// services each one filled.
    pub fn dump_runtime(&self) -> String {
        let ctx = self.context();
        let mut out = String::new();
        out.push_str("config tree:\n");
        out.push_str(&self.tree.dump());
        out.push_str("\nfibers:\n");
        for (id, entry, plugin, status) in self.root.fibers.snapshot() {
            out.push_str(&format!("- [{id}] {entry} <- {plugin} ({status:?})\n"));
        }
        out.push_str("\nservices:\n");
        for name in ctx.service_names() {
            out.push_str(&format!("- {name}\n"));
        }
        out
    }
}

/// A mismatch between the declared composition and the running one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuditFinding {
    /// The row said it provides this and the slot is empty.
    DeclaredButNotProvided {
        entry: String,
        service: &'static str,
    },
    /// The row filled a slot its plugin never declared.
    ProvidedButNotDeclared {
        entry: String,
        service: &'static str,
    },
    /// Nothing mounted injects this service.
    ProvidedButUnused { service: &'static str },
    /// A row injects a service no registered plugin can provide.
    InjectedButUnprovidable {
        service: &'static str,
        consumers: Vec<String>,
    },
}

impl AuditFinding {
    /// Whether this is a defect or an observation.
    ///
    /// A row that declared one thing and did another is broken. A service
    /// nobody reads is *usually* worth removing, but it is a legitimate state —
    /// a front end that can ask questions under a policy that never asks, say —
    /// so it must not fail a build on its own.
    pub fn is_defect(&self) -> bool {
        !matches!(self, Self::ProvidedButUnused { .. })
    }

    fn sort_key(&self) -> (u8, String) {
        match self {
            Self::DeclaredButNotProvided { entry, service } => (0, format!("{entry}:{service}")),
            Self::ProvidedButNotDeclared { entry, service } => (1, format!("{entry}:{service}")),
            Self::InjectedButUnprovidable { service, .. } => (2, service.to_string()),
            Self::ProvidedButUnused { service } => (3, service.to_string()),
        }
    }
}

impl std::fmt::Display for AuditFinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DeclaredButNotProvided { entry, service } => write!(
                f,
                "`{entry}` declares it provides `{service}` but the slot is empty"
            ),
            Self::ProvidedButNotDeclared { entry, service } => write!(
                f,
                "`{entry}` provides `{service}` without declaring it — the capability map cannot see it"
            ),
            Self::ProvidedButUnused { service } => {
                write!(f, "`{service}` is provided but nothing injects it")
            }
            Self::InjectedButUnprovidable { service, consumers } => write!(
                f,
                "`{service}` is injected by {} but no registered plugin provides it",
                consumers.join(", ")
            ),
        }
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.stop();
    }
}
