//! The service container: what `ctx.<key>` is in cordis, made compile-time typed.
//!
//! # The slot key is a Rust type
//!
//! cordis addresses services by string (`ctx.fs`) and recovers types through
//! TypeScript declaration merging. Rust has no declaration merging, so a slot is
//! keyed by a **marker type** that also carries the trait object stored in it:
//!
//! ```ignore
//! pub struct Fs;
//! impl ServiceKey for Fs {
//!     const NAME: &'static str = "fs";
//!     type Face = dyn FileSystem;
//! }
//!
//! ctx.provide::<Fs>(Arc::new(LocalFs::new()))?;   // provider
//! let fs = ctx.service::<Fs>()?;                  // consumer: Arc<dyn FileSystem>
//! ```
//!
//! The consumer never names `LocalFs` — that is the whole point of a seam. The
//! string `NAME` survives for the things strings are actually good at: config
//! rows, `inject` declarations, and diagnostics.
//!
//! # Realms
//!
//! A realm is cordis's `isolate`: a scope where a slot can hold a *different*
//! provider without disturbing anyone else. Lookup walks the realm's parent
//! chain, so a realm overrides only what it fills. This is how one session can
//! run a sandboxed filesystem while its siblings stay on the local one.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::error::{PlexusError, Result};
use crate::fiber::FiberId;
use crate::realm::RealmTree;

pub use crate::realm::{RealmId, ROOT_REALM};

/// What kind of slot this is — the first thing a reader needs to know about a
/// service, and the thing prose gets wrong first.
///
/// It lives on the definition rather than in a hand-kept table, so it cannot
/// drift from the code it describes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SeamMode {
    /// Core spine: one implementation, and swapping it is not a use case.
    /// A registry, a log, a shared index.
    Core,
    /// A replaceable capability. Several providers are expected, and a consumer
    /// must never name one of them.
    Seam,
    /// A composition point: something that exists so other rows can attach to
    /// it, not something with behaviour of its own.
    Bundle,
}

/// A typed service slot.
///
/// Implementors are zero-sized marker types. `Face` is what consumers receive:
/// normally `dyn SomeTrait` (a replaceable seam), occasionally a concrete type
/// (a registry with no alternative implementation).
///
/// # The three roles
///
/// A capability is not one package but three: the **definition** (this trait
/// impl, which owns the interface), one or more **providers** (plugins that
/// fill the slot), and the **consumers** (plugins that read it by key). A slot
/// with no provider is a hole; a slot with no consumer is dead weight; a
/// consumer that names a provider is a seam that has stopped being one. All
/// three are checked — see [`Plugin::provides`](crate::Plugin::provides) and
/// the runtime audit in [`App::audit`](crate::App::audit).
pub trait ServiceKey: Send + Sync + 'static {
    /// Stable name used in config rows, `inject` lists, and diagnostics.
    const NAME: &'static str;
    /// Whether this slot is meant to be replaced.
    const MODE: SeamMode;
    /// One line, for the generated capability map. Say what the slot *is*, not
    /// what the current provider happens to do.
    const TITLE: &'static str;
    /// The face consumers get back, behind an `Arc`.
    type Face: ?Sized + Send + Sync + 'static;
}

struct Slot {
    name: &'static str,
    /// Holds an `Arc<K::Face>`. `Arc<dyn Trait>` is itself `Sized`, so it
    /// round-trips through `Any` even when the face is unsized.
    value: Box<dyn Any + Send + Sync>,
    owner: FiberId,
    owner_label: String,
}

pub(crate) struct ServiceTable {
    slots: RwLock<HashMap<(RealmId, TypeId), Slot>>,
    /// `NAME -> TypeId`, so `inject: ["llm"]` (a string, written by a plugin
    /// author who must not have to name the marker type) can be answered.
    by_name: RwLock<HashMap<&'static str, TypeId>>,
    /// Shared with the event bus, so a slot and a listener scoped to the same
    /// realm mean the same thing.
    realms: Arc<RealmTree>,
}

impl ServiceTable {
    pub fn new(realms: Arc<RealmTree>) -> Self {
        Self {
            slots: RwLock::new(HashMap::new()),
            by_name: RwLock::new(HashMap::new()),
            realms,
        }
    }

    fn parent_of(&self, realm: RealmId) -> Option<RealmId> {
        self.realms.parent_of(realm)
    }

    /// Fill a slot. Fails rather than overwriting: a replacement unloads the
    /// incumbent first (what a config patch does) or claims its own realm.
    pub fn provide<K: ServiceKey>(
        &self,
        realm: RealmId,
        value: Arc<K::Face>,
        owner: FiberId,
        owner_label: String,
    ) -> Result<()> {
        let key = (realm, TypeId::of::<K>());
        let mut slots = self.slots.write().expect("service table poisoned");
        if let Some(existing) = slots.get(&key) {
            return Err(PlexusError::ServiceConflict {
                name: K::NAME,
                held_by: existing.owner_label.clone(),
                claimed_by: owner_label,
            });
        }
        slots.insert(
            key,
            Slot {
                name: K::NAME,
                value: Box::new(value),
                owner,
                owner_label,
            },
        );
        self.by_name
            .write()
            .expect("service name table poisoned")
            .insert(K::NAME, TypeId::of::<K>());
        Ok(())
    }

    /// Empty a slot, but only if `owner` is the fiber that filled it. A fiber
    /// unloading cannot yank a service someone else re-provided in the meantime.
    pub fn revoke<K: ServiceKey>(&self, realm: RealmId, owner: FiberId) {
        let key = (realm, TypeId::of::<K>());
        let mut slots = self.slots.write().expect("service table poisoned");
        if slots.get(&key).is_some_and(|s| s.owner == owner) {
            slots.remove(&key);
        }
    }

    /// Resolve a slot, walking up the realm chain.
    pub fn get<K: ServiceKey>(&self, realm: RealmId) -> Option<Arc<K::Face>> {
        let type_id = TypeId::of::<K>();
        let slots = self.slots.read().expect("service table poisoned");
        let mut current = Some(realm);
        while let Some(r) = current {
            if let Some(slot) = slots.get(&(r, type_id)) {
                return slot.value.downcast_ref::<Arc<K::Face>>().cloned();
            }
            current = self.parent_of(r);
        }
        None
    }

    /// Is a service available under this *string* name, anywhere up the realm
    /// chain? This is the question `inject` asks, and the only reason the name
    /// index exists.
    pub fn has_named(&self, realm: RealmId, name: &str) -> bool {
        let Some(type_id) = self
            .by_name
            .read()
            .expect("service name table poisoned")
            .get(name)
            .copied()
        else {
            return false;
        };
        let slots = self.slots.read().expect("service table poisoned");
        let mut current = Some(realm);
        while let Some(r) = current {
            if slots.contains_key(&(r, type_id)) {
                return true;
            }
            current = self.parent_of(r);
        }
        false
    }

    /// Names of the slots a given fiber filled. The other half of the audit:
    /// a provider that never declared what it provides is as invisible to the
    /// capability map as one that declared and never delivered.
    /// The slots this fiber filled in one realm only.
    ///
    /// What the audit reads: a row's declared surface is what it provides to
    /// the tree, and a service it mounts into a forked realm — an agent's own
    /// log, a delegated child's tool set — is that agent's world, not the
    /// row's.
    pub fn owned_by_in(&self, owner: FiberId, realm: RealmId) -> Vec<&'static str> {
        let slots = self.slots.read().expect("service table poisoned");
        let mut names: Vec<&'static str> = slots
            .iter()
            .filter(|((r, _), slot)| *r == realm && slot.owner == owner)
            .map(|(_, slot)| slot.name)
            .collect();
        names.sort_unstable();
        names
    }

    /// Names of every filled slot visible from `realm`, for diagnostics.
    pub fn visible_names(&self, realm: RealmId) -> Vec<&'static str> {
        let slots = self.slots.read().expect("service table poisoned");
        let mut names = Vec::new();
        let mut current = Some(realm);
        while let Some(r) = current {
            for ((slot_realm, _), slot) in slots.iter() {
                if *slot_realm == r && !names.contains(&slot.name) {
                    names.push(slot.name);
                }
            }
            current = self.parent_of(r);
        }
        names.sort_unstable();
        names
    }
}
