//! File-backed persistent memory. The harness memory row owns production
//! injection as logged facts; stores and derived diagnostics live here.
//! `MemoryHook` remains an adapter for standalone kernel embedders.

pub mod context;
mod document;
pub mod entry;
pub mod hook;
pub mod input;
pub mod store;
pub use hook::MemoryHook;
pub use store::MemoryStore;

#[cfg(test)]
mod metadata_tests;
