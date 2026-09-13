//! The plugin contract, and the registry that resolves a config row's `name` to
//! an implementation.
//!
//! # What "everything is a plugin" means in a compiled language
//!
//! cordis loads plugins from npm at runtime. Rust links its plugins at compile
//! time — but that is not where the property lives. What matters is that no
//! plugin is *privileged*: the model adapter, the tool registry, the agent loop
//! itself are rows in a config tree, each replaceable by a patch, each unloadable
//! without the others knowing. A build picks which plugins exist; a config picks
//! which ones run and how they are wired.
//!
//! Out-of-process extension keeps working the way it already does in this
//! codebase — an MCP server, a subprocess, or (later) a wasm module is just
//! another plugin that bridges into the same seams.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::context::Context;

/// One unit of composition.
///
/// `apply` installs effects into `ctx` — services, listeners, sub-plugins — and
/// every one of them is reverted when the plugin unloads. It must not stash
/// registrations anywhere the fiber cannot see.
#[async_trait]
pub trait Plugin: Send + Sync + 'static {
    /// The name config rows use.
    fn name(&self) -> &'static str;

    /// Service names this plugin needs before it can apply. Mounting is driven
    /// by these, not by row order: a provider mounted "after" its consumer in
    /// the file still activates first.
    fn inject(&self) -> &'static [&'static str] {
        &[]
    }

    /// Services this plugin reads **when they are there**, and runs without when
    /// they are not.
    ///
    /// The difference from [`inject`](Self::inject) is activation, not
    /// intention: an injected service is waited for, a used one is resolved per
    /// call and may be absent. Both are consumption, and both belong on the
    /// capability map — a seam whose only readers are optional still has
    /// readers, and a map that says "consumed by nothing" about a service half
    /// the tree calls is worse than no map.
    fn uses(&self) -> &'static [&'static str] {
        &[]
    }

    /// Services this plugin fills. Purely declarative — used by
    /// `--dump-config`, and to tell "provider missing" from "provider failed"
    /// when mounting stalls.
    fn provides(&self) -> &'static [&'static str] {
        &[]
    }

    /// The `(slot, item)` pairs this plugin puts **into** shared catalogs.
    ///
    /// Note this is *not* [`provides`](Self::provides): a contributor usually
    /// only `inject`s the slot (it reads the catalog to add to it), while the
    /// holder is a different row. So the slot has to be named here — `tools`
    /// has one holder and a dozen contributors, and the map could say who owns
    /// the catalog but not who put anything in it.
    ///
    /// Declarative only. The value is still handed over in
    /// [`apply`](Self::apply) — that is where the `Context` is, and where a
    /// contribution may legitimately depend on services resolved at mount time.
    /// This is the *name*, so the map and `--audit` can see it.
    fn contributes(&self) -> &'static [(&'static str, &'static str)] {
        &[]
    }

    /// One-line description for introspection output.
    fn description(&self) -> &'static str {
        ""
    }

    /// Install this plugin's effects. Errors are surfaced with the row id
    /// attached; returning `Err` leaves nothing half-registered only if the
    /// plugin has been careful, so register the fallible parts first.
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String>;
}

/// Name → implementation. A build's plugin catalog.
#[derive(Default)]
pub struct PluginRegistry {
    plugins: HashMap<&'static str, Arc<dyn Plugin>>,
}

impl PluginRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a plugin. Panics on a duplicate name: two implementations
    /// answering to one name would make a config row ambiguous, and that is a
    /// build-time mistake, not a runtime condition.
    pub fn register(&mut self, plugin: Arc<dyn Plugin>) -> &mut Self {
        let name = plugin.name();
        assert!(
            self.plugins.insert(name, plugin).is_none(),
            "plugin `{name}` registered twice"
        );
        self
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Plugin>> {
        self.plugins.get(name).cloned()
    }

    pub fn names(&self) -> Vec<&'static str> {
        let mut names: Vec<_> = self.plugins.keys().copied().collect();
        names.sort_unstable();
        names
    }
}
