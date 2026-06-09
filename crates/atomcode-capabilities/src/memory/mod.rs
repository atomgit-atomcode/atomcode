//! User-driven persistent memory (`memory.md`) — the L1 port of production's
//! `atomcode_core::config::memory`.
//!
//! Two markdown stores, the same files production uses (so old/new stacks share one
//! memory, the same coexistence stance as [`crate::session`]; sole known divergence:
//! under `sudo` without `$ATOMCODE_HOME` — see [`config_dir`]):
//! - **global**: `$ATOMCODE_HOME/memory.md` (else `~/.atomcode/memory.md`)
//! - **project**: `<project_root>/.atomcode/memory.md`
//!
//! Entries are plain `- ` bullet lines — human-editable, git-diffable. [`MemoryStore`]
//! is the byte-compatible load/append/remove/merge engine (ported verbatim);
//! [`MemoryHook`] is the one NEW piece: at `session_start` on a FRESH session it
//! pushes the merged entries as a system message right after the persona (a stable
//! prefix, cache-safe); on RESUME it RECONCILES the snapshot's frozen copy with the
//! current `memory.md` — refresh / inject / remove — matching production, whose
//! rebuilt-per-process system prompt picks up memory edits across a `--resume` too.
//!
//! v1 is USER-driven only: `/remember`, `/forget`, `/memory` are driver (D-tier)
//! slash-commands writing through [`MemoryStore`]; there are deliberately NO
//! model-facing remember/forget tools. A mid-session `/remember` therefore takes
//! effect when the session next STARTS (fresh or resumed) — production's semantics.

use std::path::PathBuf;

pub mod hook;
pub mod store;
pub use hook::MemoryHook;
pub use store::MemoryStore;

/// The atomcode config/data root: `$ATOMCODE_HOME` if set & non-empty, else
/// `~/.atomcode`. Mirrors `atomcode_core::config::Config::config_dir` minus the
/// `$SUDO_USER` resolution — the same deliberate simplification (and the same
/// duplication-pending-a-shared-`paths`-helper) as `session::config_dir` and
/// `mcp::util::config_dir`.
pub(crate) fn config_dir() -> PathBuf {
    if let Ok(p) = std::env::var("ATOMCODE_HOME") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".atomcode")
}
