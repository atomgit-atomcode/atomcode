//! atomcode disk/TOML config system, extracted from the retiring `atomcode-core`.
//!
//! Leaf crate — depends only on `atomcode-telemetry` + serde/toml/anyhow, so every
//! stack layer (drivers, bridge, coding, and core itself during the transition) can
//! read `config.toml` without a dependency on `atomcode-core`. See
//! `docs/superpowers/plans/2026-07-11-extract-atomcode-config.md`.
