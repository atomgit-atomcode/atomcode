//! Uninstall flow shared between the `atomcode uninstall` subcommand
//! and `scripts/uninstall.sh` / `uninstall.ps1`.
//!
//! Spec: docs/superpowers/specs/2026-05-08-uninstall-design.md

pub mod actions;
pub mod paths;
pub mod scan;

use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Group {
    /// Binary + PATH edit. Required: declining = abort.
    Binary,
    /// Credentials & global config (auth.toml, mcp.json, config.toml, ATOMCODE.md).
    Credentials,
    /// Local state & extensions (history, telemetry, plugins, commands, skills, staged).
    State,
}

#[derive(Debug, Clone)]
pub struct Item {
    pub group: Group,
    pub path: PathBuf,
    pub size_bytes: u64,
    pub note: &'static str,
    pub needs_privilege: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decisions {
    pub binary: bool,
    pub credentials: bool,
    pub state: bool,
}

impl Decisions {
    pub const DEFAULTS: Self = Self { binary: true, credentials: false, state: true };
    pub const PURGE: Self = Self { binary: true, credentials: true, state: true };
    pub const KEEP_DATA: Self = Self { binary: true, credentials: false, state: false };
}
