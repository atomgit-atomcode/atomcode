//! Fibers: the lifetime of one mounted plugin instance, and the ledger of every
//! effect it installed.
//!
//! # Why a ledger rather than RAII guards
//!
//! Every registration a plugin makes — a service it provides, a listener it
//! attaches, a tool it mounts — is filed with the fiber that made it. Unloading
//! the fiber replays that ledger in reverse, so a plugin's footprint is exactly
//! what it installed and nothing else. This is what makes a config patch able to
//! *replace* a row at runtime instead of only adding to it.
//!
//! The [`Disposable`] handed back to the plugin is a **handle, not a guard**:
//! dropping it does nothing. A plugin that ignores the return value still has a
//! live registration (the fiber holds it), which is the ergonomic cordis has and
//! plain RAII does not. Calling [`Disposable::dispose`] reverts that one effect early.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

pub type FiberId = u64;

/// The root fiber owns effects installed by the host itself (before any plugin
/// mounts) and is never unloaded.
pub const ROOT_FIBER: FiberId = 0;

type Effect = Box<dyn FnOnce() + Send>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FiberStatus {
    /// Declared by a config row, waiting for its `inject` services to appear.
    Pending,
    /// `apply` ran to completion; its effects are live.
    Active,
    /// Its ledger has been replayed; the fiber is inert.
    Disposed,
}

pub(crate) struct FiberState {
    pub id: FiberId,
    /// The config row id (`llm-openai-compat`), which is what a patch addresses
    /// and what every diagnostic prints.
    pub entry: String,
    /// The plugin name the row resolved to.
    pub plugin: String,
    /// Fiber that mounted this one (`ROOT_FIBER` for top-level config rows).
    /// A parent's unload cascades to its children, so a plugin that mounts
    /// sub-plugins cleans them up without bookkeeping of its own.
    pub parent: FiberId,
    ledger: Mutex<Vec<Option<Effect>>>,
    status: Mutex<FiberStatus>,
}

impl FiberState {
    fn new(id: FiberId, entry: String, plugin: String, parent: FiberId) -> Self {
        Self {
            id,
            entry,
            plugin,
            parent,
            ledger: Mutex::new(Vec::new()),
            status: Mutex::new(FiberStatus::Pending),
        }
    }

    /// File an effect and return its slot index.
    fn record(&self, effect: Effect) -> usize {
        let mut ledger = self.ledger.lock().expect("fiber ledger poisoned");
        ledger.push(Some(effect));
        ledger.len() - 1
    }

    fn run_slot(&self, slot: usize) {
        let taken = {
            let mut ledger = self.ledger.lock().expect("fiber ledger poisoned");
            ledger.get_mut(slot).and_then(Option::take)
        };
        // Run outside the lock: an effect may itself dispose something.
        if let Some(effect) = taken {
            effect();
        }
    }

    /// Replay the whole ledger in reverse. Idempotent.
    fn dispose_all(&self) {
        let effects: Vec<Effect> = {
            let mut ledger = self.ledger.lock().expect("fiber ledger poisoned");
            ledger.drain(..).flatten().collect()
        };
        for effect in effects.into_iter().rev() {
            effect();
        }
        *self.status.lock().expect("fiber status poisoned") = FiberStatus::Disposed;
    }

    pub fn status(&self) -> FiberStatus {
        *self.status.lock().expect("fiber status poisoned")
    }

    pub fn set_status(&self, status: FiberStatus) {
        *self.status.lock().expect("fiber status poisoned") = status;
    }

    pub fn label(&self) -> String {
        if self.entry == self.plugin {
            self.entry.clone()
        } else {
            format!("{} ({})", self.entry, self.plugin)
        }
    }
}

/// A handle to one reversible effect.
///
/// Dropping it is a no-op — the owning fiber still holds the effect. Call
/// [`dispose`](Self::dispose) to revert this single registration before the
/// plugin unloads.
#[must_use = "ignoring a Disposable is fine (the fiber owns the effect); bind it to `_` to say so"]
pub struct Disposable {
    fiber: Weak<FiberState>,
    slot: usize,
}

impl Disposable {
    pub(crate) fn new(fiber: &Arc<FiberState>, slot: usize) -> Self {
        Self {
            fiber: Arc::downgrade(fiber),
            slot,
        }
    }

    /// Revert this one effect now. A second call, or a call after the owning
    /// fiber unloaded, does nothing.
    pub fn dispose(self) {
        if let Some(fiber) = self.fiber.upgrade() {
            fiber.run_slot(self.slot);
        }
    }
}

/// The set of live fibers. Owned by the runtime root and shared by every
/// [`Context`](crate::Context) clone.
pub(crate) struct FiberTable {
    next_id: AtomicU64,
    fibers: Mutex<HashMap<FiberId, Arc<FiberState>>>,
}

impl FiberTable {
    pub fn new() -> Self {
        let root = Arc::new(FiberState::new(
            ROOT_FIBER,
            "<root>".into(),
            "<root>".into(),
            ROOT_FIBER,
        ));
        root.set_status(FiberStatus::Active);
        let mut fibers = HashMap::new();
        fibers.insert(ROOT_FIBER, root);
        Self {
            next_id: AtomicU64::new(1),
            fibers: Mutex::new(fibers),
        }
    }

    pub fn create(&self, entry: String, plugin: String, parent: FiberId) -> FiberId {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let fiber = Arc::new(FiberState::new(id, entry, plugin, parent));
        self.fibers
            .lock()
            .expect("fiber table poisoned")
            .insert(id, fiber);
        id
    }

    pub fn get(&self, id: FiberId) -> Option<Arc<FiberState>> {
        self.fibers
            .lock()
            .expect("fiber table poisoned")
            .get(&id)
            .cloned()
    }

    pub fn record(&self, id: FiberId, effect: Effect) -> Option<Disposable> {
        let fiber = self.get(id)?;
        let slot = fiber.record(effect);
        Some(Disposable::new(&fiber, slot))
    }

    /// Unload a fiber and every fiber it mounted, children first.
    pub fn unload(&self, id: FiberId) {
        if id == ROOT_FIBER {
            return;
        }
        let children: Vec<FiberId> = {
            let fibers = self.fibers.lock().expect("fiber table poisoned");
            fibers
                .values()
                .filter(|f| f.parent == id)
                .map(|f| f.id)
                .collect()
        };
        for child in children {
            self.unload(child);
        }
        let fiber = self
            .fibers
            .lock()
            .expect("fiber table poisoned")
            .remove(&id);
        if let Some(fiber) = fiber {
            fiber.dispose_all();
        }
    }

    /// Every live fiber, for `--dump-config` style introspection.
    pub fn snapshot(&self) -> Vec<(FiberId, String, String, FiberStatus)> {
        let fibers = self.fibers.lock().expect("fiber table poisoned");
        let mut rows: Vec<_> = fibers
            .values()
            .filter(|f| f.id != ROOT_FIBER)
            .map(|f| (f.id, f.entry.clone(), f.plugin.clone(), f.status()))
            .collect();
        rows.sort_by_key(|r| r.0);
        rows
    }
}
