//! Realms: the spatial axis.
//!
//! A realm is a scope that can hold its own version of a slot and its own
//! listeners, layered over a parent. It is what lets two agents run in one
//! process with different tools, a different filesystem, or a different approval
//! policy, without either of them knowing the other exists.
//!
//! # Visibility runs one way
//!
//! Lookup walks *up*: a child realm sees what it defines plus everything its
//! ancestors define, and a parent sees nothing a child added. That asymmetry is
//! the whole design:
//!
//! - a policy installed at the root applies to every agent (a credential gate
//!   must not be escapable by spawning a subagent);
//! - anything an agent installs stays with that agent (a subagent's restricted
//!   tool set must not leak up into its parent's next turn).
//!
//! Both the service table and the event bus resolve through this same tree, so
//! "scoped to this agent" means the same thing for a service and for a listener.
//! Getting only one of the two right is worse than getting neither: the
//! isolation looks real and is not.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;

pub type RealmId = u64;

/// The realm every context starts in.
pub const ROOT_REALM: RealmId = 0;

#[derive(Default)]
pub(crate) struct RealmTree {
    parents: RwLock<HashMap<RealmId, RealmId>>,
    next: AtomicU64,
}

impl RealmTree {
    pub fn new() -> Self {
        Self {
            parents: RwLock::new(HashMap::new()),
            next: AtomicU64::new(1),
        }
    }

    pub fn fork(&self, parent: RealmId) -> RealmId {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.parents
            .write()
            .expect("realm tree poisoned")
            .insert(id, parent);
        id
    }

    pub fn parent_of(&self, realm: RealmId) -> Option<RealmId> {
        self.parents
            .read()
            .expect("realm tree poisoned")
            .get(&realm)
            .copied()
    }

    /// `realm` and every ancestor, nearest first.
    pub fn ancestry(&self, realm: RealmId) -> Vec<RealmId> {
        let mut chain = vec![realm];
        let mut current = realm;
        // A cycle is impossible by construction (a fork's parent always exists
        // already), but bound the walk anyway rather than hang a dispatch.
        while let Some(parent) = self.parent_of(current) {
            if chain.contains(&parent) {
                break;
            }
            chain.push(parent);
            current = parent;
        }
        chain
    }

    /// Is something registered in `owner` visible to work happening in `viewer`?
    ///
    /// True when `owner` is `viewer` or one of its ancestors — the one-way rule
    /// this module exists to enforce.
    pub fn visible_from(&self, owner: RealmId, viewer: RealmId) -> bool {
        if owner == viewer || owner == ROOT_REALM {
            return true;
        }
        self.ancestry(viewer).contains(&owner)
    }
}
