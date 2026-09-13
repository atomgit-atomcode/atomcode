//! `Context` — what a plugin is handed, and the only thing it needs.
//!
//! A context is a cheap handle bound to three things: the shared runtime root
//! (services, events, fibers), the **fiber** that owns whatever gets registered
//! through it, and the **realm** its service lookups resolve in. Cloning keeps
//! all three, which is why a plugin can hand its context to a spawned task and
//! still have its registrations reverted on unload.

use std::sync::{Arc, Weak};

use futures::future::BoxFuture;

use crate::error::{PlexusError, Result};
use crate::event::{AsyncListener, Event, EventBus, Mode, SyncListener, Waterfall};
use crate::fiber::{Disposable, FiberId, FiberStatus, FiberTable, ROOT_FIBER};
use crate::plugin::PluginRegistry;
use crate::realm::{RealmTree, ROOT_REALM};
use crate::service::{RealmId, ServiceKey, ServiceTable};

pub(crate) struct Root {
    pub services: ServiceTable,
    pub events: EventBus,
    pub fibers: FiberTable,
    pub registry: PluginRegistry,
    /// One tree, shared by the service table and the event bus. Scoping a
    /// service and scoping a listener must mean the same thing, and the surest
    /// way to keep two answers identical is to have one of them.
    pub realms: Arc<RealmTree>,
}

impl Root {
    pub fn new(registry: PluginRegistry) -> Arc<Self> {
        let realms = Arc::new(RealmTree::new());
        Arc::new(Self {
            services: ServiceTable::new(realms.clone()),
            events: EventBus::new(realms.clone()),
            fibers: FiberTable::new(),
            registry,
            realms,
        })
    }
}

#[derive(Clone)]
pub struct Context {
    pub(crate) root: Arc<Root>,
    pub(crate) fiber: FiberId,
    pub(crate) realm: RealmId,
}

impl Context {
    pub(crate) fn new(root: Arc<Root>) -> Self {
        Self {
            root,
            fiber: ROOT_FIBER,
            realm: ROOT_REALM,
        }
    }

    pub(crate) fn for_fiber(&self, fiber: FiberId) -> Self {
        Self {
            root: self.root.clone(),
            fiber,
            realm: self.realm,
        }
    }

    /// The config row id this context belongs to (`<root>` for the host).
    pub fn entry(&self) -> String {
        self.root
            .fibers
            .get(self.fiber)
            .map_or_else(|| "<root>".into(), |f| f.entry.clone())
    }

    fn label(&self) -> String {
        self.root
            .fibers
            .get(self.fiber)
            .map_or_else(|| "<root>".into(), |f| f.label())
    }

    fn record(&self, effect: Box<dyn FnOnce() + Send>) -> Disposable {
        self.root
            .fibers
            .record(self.fiber, effect)
            // A context outlives its fiber only if someone held it past unload;
            // the effect is then already moot, so file it against the root.
            .unwrap_or_else(|| {
                self.root
                    .fibers
                    .record(ROOT_FIBER, Box::new(|| {}))
                    .expect("root fiber always exists")
            })
    }

    /// File an arbitrary undo with this plugin's fiber — cordis's `ctx.effect()`.
    ///
    /// Use it for state a plugin installed somewhere the runtime cannot see: a
    /// tool added to a registry, a row written to a table, a spawned task to
    /// cancel. Anything registered through a typed `Context` method files its own
    /// undo already.
    pub fn effect(&self, undo: impl FnOnce() + Send + 'static) -> Disposable {
        self.record(Box::new(undo))
    }

    // ---- services -------------------------------------------------------

    /// Fill this realm's slot for `K`. Reverted when the owning fiber unloads.
    pub fn provide<K: ServiceKey>(&self, value: Arc<K::Face>) -> Result<Disposable> {
        self.root
            .services
            .provide::<K>(self.realm, value, self.fiber, self.label())?;
        let root = Arc::downgrade(&self.root);
        let realm = self.realm;
        let fiber = self.fiber;
        Ok(self.record(Box::new(move || {
            if let Some(root) = Weak::upgrade(&root) {
                root.services.revoke::<K>(realm, fiber);
            }
        })))
    }

    /// Resolve `K`, walking up the realm chain. `None` when nothing provides it.
    pub fn service<K: ServiceKey>(&self) -> Option<Arc<K::Face>> {
        self.root.services.get::<K>(self.realm)
    }

    /// Resolve `K` or fail with the seam's name — the error a consumer should
    /// surface rather than silently degrading.
    pub fn require<K: ServiceKey>(&self) -> Result<Arc<K::Face>> {
        self.service::<K>()
            .ok_or(PlexusError::ServiceMissing { name: K::NAME })
    }

    /// Is a service available under this string name? What `inject` asks.
    pub fn has_service(&self, name: &str) -> bool {
        self.root.services.has_named(self.realm, name)
    }

    pub fn service_names(&self) -> Vec<&'static str> {
        self.root.services.visible_names(self.realm)
    }

    /// Derive a context whose registrations land in a fresh realm layered over
    /// this one — services **and** listeners both.
    ///
    /// Cordis's `isolate`. One session can swap its filesystem, restrict its
    /// tools, or install its own policy without touching its siblings, and
    /// anything the root installed still applies to it.
    pub fn isolate(&self) -> Self {
        Self {
            root: self.root.clone(),
            fiber: self.fiber,
            realm: self.root.realms.fork(self.realm),
        }
    }

    // ---- events: registration -------------------------------------------

    pub fn on_emit<E: Event<Output = ()>>(
        &self,
        listener: impl Fn(&E::Args) + Send + Sync + 'static,
    ) -> Disposable {
        let wrapped: SyncListener<E> = Arc::new(move |args| {
            listener(args);
            None
        });
        let id = self.root.events.on_sync::<E>(wrapped, self.realm, false);
        self.record_listener::<E>(id)
    }

    pub fn on_bail<E: Event>(
        &self,
        listener: impl Fn(&E::Args) -> Option<E::Output> + Send + Sync + 'static,
    ) -> Disposable {
        let id = self
            .root
            .events
            .on_sync::<E>(Arc::new(listener), self.realm, false);
        self.record_listener::<E>(id)
    }

    pub fn on_parallel<E: Event<Output = ()>>(&self, listener: AsyncListener<E>) -> Disposable {
        let id = self.root.events.on_async::<E>(listener, self.realm, false);
        self.record_listener::<E>(id)
    }

    pub fn on_serial<E: Event>(&self, listener: AsyncListener<E>) -> Disposable {
        let id = self.root.events.on_async::<E>(listener, self.realm, false);
        self.record_listener::<E>(id)
    }

    /// Attach around-middleware. `prepend` puts this listener ahead of the
    /// normally-registered ones — reserve it for policy that must inspect args
    /// before any peer can rewrite or short-circuit them.
    pub fn on_waterfall<E: Event>(
        &self,
        listener: Arc<dyn Waterfall<E>>,
        prepend: bool,
    ) -> Disposable {
        let id = self
            .root
            .events
            .on_waterfall::<E>(listener, self.realm, prepend);
        self.record_listener::<E>(id)
    }

    fn record_listener<E: Event>(&self, id: u64) -> Disposable {
        let root = Arc::downgrade(&self.root);
        self.record(Box::new(move || {
            if let Some(root) = Weak::upgrade(&root) {
                root.events.remove(std::any::TypeId::of::<E>(), id);
            }
        }))
    }

    // ---- events: dispatch -----------------------------------------------

    pub fn emit<E: Event<Output = ()>>(&self, args: &E::Args) {
        debug_assert_eq!(E::MODE, Mode::Emit);
        self.root.events.emit::<E>(args, self.realm);
    }

    pub fn bail<E: Event>(&self, args: &E::Args) -> Option<E::Output> {
        debug_assert_eq!(E::MODE, Mode::Bail);
        self.root.events.bail::<E>(args, self.realm)
    }

    pub async fn parallel<E: Event<Output = ()>>(&self, args: &E::Args) {
        debug_assert_eq!(E::MODE, Mode::Parallel);
        self.root.events.parallel::<E>(args, self.realm).await;
    }

    pub async fn serial<E: Event>(&self, args: &E::Args) -> Option<E::Output> {
        debug_assert_eq!(E::MODE, Mode::Serial);
        self.root.events.serial::<E>(args, self.realm).await
    }

    /// Run the registered chain around `terminal`, which is the behaviour that
    /// happens when every listener delegates (or when none is registered).
    pub async fn waterfall<E, F>(&self, args: &mut E::Args, terminal: F) -> E::Output
    where
        E: Event,
        F: for<'b> Fn(&'b mut E::Args) -> BoxFuture<'b, E::Output> + Send + Sync,
    {
        debug_assert_eq!(E::MODE, Mode::Waterfall);
        self.root
            .events
            .waterfall::<E, F>(args, self.realm, terminal)
            .await
    }

    /// How many listeners are attached to `E`. Introspection for tests and
    /// `--dump-config`-style output; never a control-flow input.
    pub fn listener_count<E: Event>(&self) -> usize {
        self.root
            .events
            .listener_count(std::any::TypeId::of::<E>(), self.realm)
    }

    // ---- sub-plugins ----------------------------------------------------

    /// Mount another plugin as a child of this one. The child unloads with its
    /// parent, so a plugin that composes others needs no teardown bookkeeping.
    ///
    /// Unlike top-level mounting, this does not wait: a parent is expected to
    /// have its children's dependencies in hand, so a missing one is a
    /// composition bug and fails loudly.
    pub async fn plugin(&self, name: &str, config: serde_json::Value) -> Result<FiberId> {
        let plugin = self
            .root
            .registry
            .get(name)
            .ok_or_else(|| PlexusError::UnknownPlugin {
                entry: self.entry(),
                plugin: name.to_string(),
            })?;
        let missing: Vec<String> = plugin
            .inject()
            .iter()
            .filter(|s| !self.has_service(s))
            .map(|s| s.to_string())
            .collect();
        if !missing.is_empty() {
            return Err(PlexusError::Deadlock {
                pending: vec![(format!("{}:{}", self.entry(), name), missing)],
            });
        }
        let child = self
            .root
            .fibers
            .create(name.to_string(), name.to_string(), self.fiber);
        let child_ctx = self.for_fiber(child);
        plugin
            .apply(&child_ctx, &config)
            .await
            .map_err(|e| PlexusError::Apply {
                entry: name.to_string(),
                message: e,
            })?;
        if let Some(state) = self.root.fibers.get(child) {
            state.set_status(FiberStatus::Active);
        }
        Ok(child)
    }

    /// Mount a plugin into a fresh realm layered over this one.
    ///
    /// The returned context is the child's: fill slots in it to give that
    /// subtree a different world — a restricted tool catalog, a sandboxed
    /// filesystem, a stricter approval policy — with none of it visible to the
    /// parent or its siblings.
    ///
    /// Fill the overrides *before* calling this: the plugin resolves its
    /// dependencies during `apply`, so a slot filled afterwards is one it
    /// already looked past.
    pub async fn plugin_isolated(
        &self,
        name: &str,
        config: serde_json::Value,
    ) -> Result<(FiberId, Context)> {
        let scoped = self.isolate();
        let fiber = scoped.plugin(name, config).await?;
        Ok((fiber, scoped))
    }

    /// Unload a fiber this context mounted, reverting its effects (and its
    /// children's) in reverse order.
    pub fn unload(&self, fiber: FiberId) {
        self.root.fibers.unload(fiber);
    }
}
