//! # atomcode-shell — the driver shell (neutral provider/glue helpers)
//!
//! There used to be an `atomcode-bridge` crate: a `Bridge` state machine that presented
//! the new stack (`atomcode-kernel` + `atomcode-capabilities` + `atomcode-coding`) behind
//! `atomcode-core`'s legacy `AgentClient`/`AgentEvent` protocol so the existing drivers
//! ran the new engine unchanged. That strangler is COMPLETE: tuix consumes the kernel
//! natively (its own `native` adapter) and the daemon was always native
//! (`CodingRuntime::spawn`), so the translation runtime had no consumers and was deleted.
//!
//! This crate is what remained of that shell — the small, neutral glue the two drivers
//! (cli + daemon) share: a driver-supplied [`BridgeConfig`] + its mapping to a
//! `CodingAgentConfig` ([`coding_config`]), provider construction with the AtomGit signing
//! gateway ([`build_provider`]), and core↔kernel message [`convert`]ersions.
//! `build_provider` can't move into `atomcode-coding` (it uses
//! `atomcode_core::coding_plan::crypto` signing and coding is core-neutral by design), so
//! it lives here on the driver side — below cli/daemon, above core/kernel/capabilities.

pub mod convert;
mod runtime;
mod sign;

pub use runtime::{build_provider, coding_config, provider_factory, BridgeConfig};
