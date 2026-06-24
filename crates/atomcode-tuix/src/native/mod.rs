//! Native kernel runtime for the TUI (B2).
//!
//! Drives a kernel `CodingRuntime` directly and maps its events into [`UiEvent`],
//! replacing the `atomcode-bridge` translation membrane. The renderer consumes
//! `UiEvent` instead of `core::agent::AgentEvent`.
//!
pub(crate) mod adapter;
pub(crate) mod approval;
pub(crate) mod compaction;
pub(crate) mod convert;
pub(crate) mod event;
pub(crate) mod goal;
pub(crate) mod lifecycle;
pub(crate) mod translate;
