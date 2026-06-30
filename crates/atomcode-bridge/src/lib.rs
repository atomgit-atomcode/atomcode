//! # atomcode-bridge — the engine-swap seam (strangler pattern, done properly)
//!
//! Presents `atomcode-core`'s driver-facing protocol — an [`AgentClient`] command
//! sender + an `AgentEvent` receiver, the exact two channels tuix / the headless
//! CLI / every legacy driver already speak — but the engine behind the channels is
//! the NEW stack (`atomcode-kernel` + `atomcode-capabilities` + `atomcode-coding`'s
//! prepare/assemble full assembly).
//!
//! WHY: rebuilding each driver over the new stack is a months-scale port (tuix UX,
//! daemon/webui, login, bg, plugins). Swapping the engine UNDER the drivers makes
//! every feature work on day one, behind a runtime switch with instant rollback —
//! the drivers cannot tell the difference except where the new engine is honest
//! about not yet supporting something (plan mode → warning, /undo → UndoFailed).
//!
//! Session persistence note: legacy drivers PERSIST SESSIONS THEMSELVES (the
//! `messages` snapshots on TurnComplete/Error/etc. exist for that) — that keeps
//! working untouched. The bridge ALSO keeps the new stack's L1 persistence on,
//! under a bridge-owned session id: that's what makes `SetMessages` (resume) and
//! `ClearConversation` (new session) implementable as engine respawns, and gives
//! cross-session `recall` for free.

pub mod convert;
mod runtime;
mod sign;

pub use runtime::{build_provider_for_acp, spawn_bridged_runtime, BridgeConfig};
