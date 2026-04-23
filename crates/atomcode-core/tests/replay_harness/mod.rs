//! Replay harness — drive the atomcode framework using recorded session
//! jsonls as input (LLM responses + tool results) so framework-level
//! regressions are catchable in CI without burning LLM API credits.
//!
//! Scope: this module is test-only and lives entirely under `tests/`. No
//! production `src/` code imports from here. Changes to files in this
//! module affect ONLY `cargo test` output, never release binaries
//! (verified: Rust's integration-test model compiles `tests/` only when
//! running tests).
//!
//! Status (2026-04-23): Stage 2 in progress. Currently: fixture_loader +
//! replay_provider scaffolded. Still to build: mock_tools, minimal
//! agent_builder, actual end-to-end replay test.

pub mod agent_builder;
pub mod fixture_loader;
pub mod mock_tools;
pub mod replay_provider;
