//! # atomcode-bridge — neutral driver provider helpers (the translation membrane is gone)
//!
//! This crate USED to be the engine-swap seam: a `Bridge` state machine that presented
//! the new stack (`atomcode-kernel` + `atomcode-capabilities` + `atomcode-coding`) behind
//! `atomcode-core`'s legacy `AgentClient`/`AgentEvent` protocol so the existing drivers
//! ran the new engine unchanged. That strangler is COMPLETE: tuix consumes the kernel
//! natively (its own `native` adapter) and the daemon was always native
//! (`CodingRuntime::spawn`), so the translation runtime has no consumers and was deleted.
//!
//! What remains is the small, neutral glue the two remaining drivers (cli + daemon) still
//! share: a driver-supplied [`BridgeConfig`] + its mapping to a `CodingAgentConfig`
//! ([`coding_config`]), provider construction with the AtomGit signing gateway
//! ([`build_provider`]), and core↔kernel message [`convert`]ersions. `build_provider`
//! can't move into `atomcode-coding` (it uses `atomcode_core::coding_plan::crypto` signing
//! and coding is core-neutral by design), so it stays on the driver side here. A future
//! cleanup may relocate these + drop the crate entirely; for now they live in one place.

pub mod convert;
mod runtime;
mod sign;

pub use runtime::{build_provider, coding_config, BridgeConfig};
