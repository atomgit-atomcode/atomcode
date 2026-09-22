//! Typed events with cordis's five dispatch modes.
//!
//! An event is a type. It declares its payload, its return, and — as an
//! associated constant — the single mode it may be dispatched with. Registering
//! a listener with the wrong mode is caught at registration, not at the call site
//! months later:
//!
//! ```ignore
//! pub struct PreStep;
//! impl Event for PreStep {
//!     const NAME: &'static str = "agent/pre-step";
//!     const MODE: Mode = Mode::Waterfall;
//!     type Args = StepDecision;
//!     type Output = StepDecision;
//! }
//! ```
//!
//! # Waterfall is around-middleware
//!
//! A waterfall listener receives `(&mut Args, next)`. Calling `next` runs the
//! rest of the chain and hands back its result, which the listener may wrap on
//! the way out. Returning without calling `next` short-circuits — and for
//! single-decision events that is the intended way to own a decision. This is
//! the same shape as `ToolMiddleware::before` in the existing kernel, generalized
//! to any event so policy can attach without the kernel knowing it exists.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use futures::future::BoxFuture;

use crate::realm::{RealmId, RealmTree};

/// How an event is dispatched. Part of the event's public contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Sync observers, registration order, no return. Fire-and-forget.
    Emit,
    /// Async observers, all run concurrently, no return.
    Parallel,
    /// Async, registration order, first `Some` wins and stops the chain.
    Serial,
    /// Sync, registration order, first `Some` wins and stops the chain.
    Bail,
    /// Async around-middleware: each listener may rewrite args, delegate via
    /// `next`, wrap the result, or short-circuit.
    Waterfall,
}

pub trait Event: Send + Sync + 'static {
    /// Stable name, for diagnostics and the event catalog.
    const NAME: &'static str;
    const MODE: Mode;
    /// What listeners inspect (and, for `Waterfall`, may rewrite).
    type Args: Send + Sync + 'static;
    /// What a `Serial`/`Bail`/`Waterfall` listener returns. `()` for the
    /// observation-only modes.
    type Output: Send + Sync + 'static;
}

/// The rest of a waterfall chain, plus the terminal that runs when every
/// listener has delegated.
///
/// `Copy`, so a listener may delegate **more than once**: that is what makes a
/// retry, a fallback provider, or a speculative call expressible as middleware
/// instead of as a branch inside whatever it wraps. Each delegation re-runs the
/// listeners below it, which is the same semantics as calling `next()` twice in
/// cordis.
pub struct Next<'a, E: Event> {
    rest: &'a [Arc<dyn Waterfall<E>>],
    terminal: &'a Terminal<'a, E>,
}

// Hand-written rather than derived: the event type is a marker and never needs
// to be `Clone` itself.
impl<E: Event> Clone for Next<'_, E> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<E: Event> Copy for Next<'_, E> {}

type Terminal<'a, E> = dyn for<'b> Fn(&'b mut <E as Event>::Args) -> BoxFuture<'b, <E as Event>::Output>
    + Send
    + Sync
    + 'a;

impl<'a, E: Event> Next<'a, E> {
    /// Run the remaining listeners, then the terminal.
    ///
    /// Boxed rather than a plain `async fn` because the chain is mutually
    /// recursive: a listener awaits `next.run()`, which awaits the listener
    /// after it. Boxing breaks the infinitely-sized future.
    pub fn run<'b>(self, args: &'b mut E::Args) -> BoxFuture<'b, E::Output>
    where
        'a: 'b,
    {
        Box::pin(async move {
            match self.rest.split_first() {
                Some((head, rest)) => {
                    let next = Next {
                        rest,
                        terminal: self.terminal,
                    };
                    head.handle(args, next).await
                }
                None => (self.terminal)(args).await,
            }
        })
    }
}

/// A waterfall listener.
#[async_trait]
pub trait Waterfall<E: Event>: Send + Sync + 'static {
    async fn handle(&self, args: &mut E::Args, next: Next<'_, E>) -> E::Output;
}

/// An async observer (`Parallel`) or decider (`Serial`).
///
/// A trait rather than a boxed closure: a closure returning a future that
/// borrows its argument needs higher-ranked bounds Rust cannot infer at the call
/// site, and every listener would have to hand-write `Box::pin`. `#[async_trait]`
/// on an impl block reads like the plugin code it is.
#[async_trait]
pub trait Listener<E: Event>: Send + Sync + 'static {
    /// `None` means "no opinion" and lets the chain continue. `Parallel`
    /// listeners (whose `Output` is `()`) return `None`.
    async fn call(&self, args: &E::Args) -> Option<E::Output>;
}

pub type AsyncListener<E> = Arc<dyn Listener<E>>;

/// A sync observer (`Emit`) or decider (`Bail`).
pub type SyncListener<E> =
    Arc<dyn Fn(&<E as Event>::Args) -> Option<<E as Event>::Output> + Send + Sync>;

struct Registration {
    id: u64,
    /// The realm this listener was registered in. A dispatch only reaches it
    /// when the dispatching realm can see that one — the same one-way rule the
    /// service table uses. Without this, isolating an agent's services while
    /// leaving its listeners global would be isolation in name only.
    realm: RealmId,
    /// Listeners registered with `prepend` sort ahead of the rest. Within a
    /// group, registration order is preserved — which is load-bearing for
    /// policy that must inspect the exact bytes a later listener will act on.
    prepend: bool,
    seq: u64,
    /// One of `Arc<dyn Waterfall<E>>`, `AsyncListener<E>`, `SyncListener<E>`.
    handler: Box<dyn Any + Send + Sync>,
}

pub(crate) struct EventBus {
    listeners: RwLock<HashMap<TypeId, Vec<Registration>>>,
    next_id: AtomicU64,
    realms: Arc<RealmTree>,
}

impl EventBus {
    pub fn new(realms: Arc<RealmTree>) -> Self {
        Self {
            listeners: RwLock::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            realms,
        }
    }

    fn insert(
        &self,
        type_id: TypeId,
        realm: RealmId,
        prepend: bool,
        handler: Box<dyn Any + Send + Sync>,
    ) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let seq = id;
        let mut listeners = self.listeners.write().expect("event bus poisoned");
        let slot = listeners.entry(type_id).or_default();
        slot.push(Registration {
            id,
            realm,
            prepend,
            seq,
            handler,
        });
        // Stable order: prepended group first, registration order within groups.
        slot.sort_by_key(|r| (!r.prepend, r.seq));
        id
    }

    pub fn remove(&self, type_id: TypeId, id: u64) {
        if let Some(slot) = self
            .listeners
            .write()
            .expect("event bus poisoned")
            .get_mut(&type_id)
        {
            slot.retain(|r| r.id != id);
        }
    }

    pub fn on_waterfall<E: Event>(
        &self,
        listener: Arc<dyn Waterfall<E>>,
        realm: RealmId,
        prepend: bool,
    ) -> u64 {
        assert_eq!(
            E::MODE,
            Mode::Waterfall,
            "event `{}` is {:?}, not Waterfall",
            E::NAME,
            E::MODE
        );
        self.insert(TypeId::of::<E>(), realm, prepend, Box::new(listener))
    }

    pub fn on_async<E: Event>(
        &self,
        listener: AsyncListener<E>,
        realm: RealmId,
        prepend: bool,
    ) -> u64 {
        assert!(
            matches!(E::MODE, Mode::Parallel | Mode::Serial),
            "event `{}` is {:?}, not Parallel/Serial",
            E::NAME,
            E::MODE
        );
        self.insert(TypeId::of::<E>(), realm, prepend, Box::new(listener))
    }

    pub fn on_sync<E: Event>(
        &self,
        listener: SyncListener<E>,
        realm: RealmId,
        prepend: bool,
    ) -> u64 {
        assert!(
            matches!(E::MODE, Mode::Emit | Mode::Bail),
            "event `{}` is {:?}, not Emit/Bail",
            E::NAME,
            E::MODE
        );
        self.insert(TypeId::of::<E>(), realm, prepend, Box::new(listener))
    }

    /// Listeners for `type_id` that are visible from `realm`, in dispatch order.
    fn collect<T: Clone + 'static>(&self, type_id: TypeId, realm: RealmId) -> Vec<T> {
        let listeners = self.listeners.read().expect("event bus poisoned");
        let Some(slot) = listeners.get(&type_id) else {
            return Vec::new();
        };
        slot.iter()
            .filter(|r| self.realms.visible_from(r.realm, realm))
            .filter_map(|r| r.handler.downcast_ref::<T>().cloned())
            .collect()
    }

    pub fn emit<E: Event<Output = ()>>(&self, args: &E::Args, realm: RealmId) {
        for listener in self.collect::<SyncListener<E>>(TypeId::of::<E>(), realm) {
            listener(args);
        }
    }

    pub fn bail<E: Event>(&self, args: &E::Args, realm: RealmId) -> Option<E::Output> {
        for listener in self.collect::<SyncListener<E>>(TypeId::of::<E>(), realm) {
            if let Some(out) = listener(args) {
                return Some(out);
            }
        }
        None
    }

    pub async fn parallel<E: Event<Output = ()>>(&self, args: &E::Args, realm: RealmId) {
        let listeners = self.collect::<AsyncListener<E>>(TypeId::of::<E>(), realm);
        let futures: Vec<_> = listeners.iter().map(|l| l.call(args)).collect();
        futures::future::join_all(futures).await;
    }

    pub async fn serial<E: Event>(&self, args: &E::Args, realm: RealmId) -> Option<E::Output> {
        for listener in self.collect::<AsyncListener<E>>(TypeId::of::<E>(), realm) {
            if let Some(out) = listener.call(args).await {
                return Some(out);
            }
        }
        None
    }

    /// Run a waterfall chain around `terminal`.
    pub async fn waterfall<E, F>(
        &self,
        args: &mut E::Args,
        realm: RealmId,
        terminal: F,
    ) -> E::Output
    where
        E: Event,
        F: for<'b> Fn(&'b mut E::Args) -> BoxFuture<'b, E::Output> + Send + Sync,
    {
        let chain = self.collect::<Arc<dyn Waterfall<E>>>(TypeId::of::<E>(), realm);
        let next = Next {
            rest: &chain,
            terminal: &terminal,
        };
        next.run(args).await
    }

    /// How many listeners for `type_id` a dispatch from `realm` would reach.
    pub fn listener_count(&self, type_id: TypeId, realm: RealmId) -> usize {
        let listeners = self.listeners.read().expect("event bus poisoned");
        listeners.get(&type_id).map_or(0, |slot| {
            slot.iter()
                .filter(|r| self.realms.visible_from(r.realm, realm))
                .count()
        })
    }
}
