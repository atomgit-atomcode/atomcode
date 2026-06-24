//! Native kernel runtime for the TUI (B2).
//!
//! Drives a kernel `CodingRuntime` directly and maps its events into [`UiEvent`],
//! replacing the `atomcode-bridge` translation membrane. The renderer consumes
//! `UiEvent` instead of `core::agent::AgentEvent`.
//!
//! `dead_code` is allowed module-wide while this is being built incrementally: the
//! mapping + event vocab exist and are unit-tested, but their non-test consumers —
//! the adapter run loop (B2 ③) and the re-pointed 38-arm renderer (B2 ②) — land in
//! later steps. The allow is removed once those wire everything in.
#![allow(dead_code)]

pub(crate) mod adapter;
pub(crate) mod approval;
pub(crate) mod convert;
pub(crate) mod event;
pub(crate) mod lifecycle;
pub(crate) mod translate;
