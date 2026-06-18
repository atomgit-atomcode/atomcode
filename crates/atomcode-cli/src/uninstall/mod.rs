//! Uninstall flow shared between the `atomcode uninstall` subcommand
//! and `scripts/uninstall.sh` / `uninstall.ps1`.
//!
//! Spec: docs/superpowers/specs/2026-05-08-uninstall-design.md

pub mod actions;
pub mod paths;
pub mod scan;

mod frontend;
pub use frontend::{run, Args};

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
    pub const DEFAULTS: Self = Self {
        binary: true,
        credentials: false,
        state: true,
    };
    pub const PURGE: Self = Self {
        binary: true,
        credentials: true,
        state: true,
    };
    pub const KEEP_DATA: Self = Self {
        binary: true,
        credentials: false,
        state: false,
    };
}

// ── Outcome ───────────────────────────────────────────────────────────────────

#[derive(Debug, Default)]
pub struct Outcome {
    pub removed: Vec<PathBuf>,
    pub kept: Vec<PathBuf>,
    pub failed: Vec<(PathBuf, String)>,
    pub backups: Vec<PathBuf>,
}

// ── ExecuteContext ────────────────────────────────────────────────────────────

#[derive(Default)]
pub struct ExecuteContext {
    /// Pairs of (rc file path, install prefix) to clean.
    pub rc_files: Vec<(PathBuf, String)>,
    /// Literal install dir string for Windows registry filter (e.g., "%LOCALAPPDATA%\\AtomCode").
    #[cfg(windows)]
    pub windows_install_dir_literal: Option<String>,
    /// Expanded install dir string for Windows registry filter (e.g., "C:\\Users\\theo\\AppData\\Local\\AtomCode").
    #[cfg(windows)]
    pub windows_install_dir_expanded: Option<String>,
}

// ── execute() ────────────────────────────────────────────────────────────────

/// Execute the uninstall plan in spec-prescribed order:
/// 1) State (least load-bearing)
/// 2) Credentials
/// 3) rmdir $ATOMCODE_HOME if empty
/// 4) PATH cleanup (rc / Windows User PATH)
/// 5) Binary self-update artifacts
/// 6) Self-delete (last)
pub fn execute(
    plan: &scan::Plan,
    decisions: Decisions,
    self_delete: &dyn actions::SelfDeleteStrategy,
    ctx: Option<ExecuteContext>,
) -> std::io::Result<Outcome> {
    let mut out = Outcome::default();

    let want = |g: Group| -> bool {
        match g {
            Group::Binary => decisions.binary,
            Group::Credentials => decisions.credentials,
            Group::State => decisions.state,
        }
    };

    // --- 1. State ---
    if want(Group::State) {
        for it in plan.items.iter().filter(|i| i.group == Group::State) {
            match actions::remove_path(&it.path, it.needs_privilege) {
                Ok(()) => out.removed.push(it.path.clone()),
                Err(e) => out.failed.push((it.path.clone(), e.to_string())),
            }
        }
    } else {
        for it in plan.items.iter().filter(|i| i.group == Group::State) {
            out.kept.push(it.path.clone());
        }
    }

    // --- 2. Credentials ---
    if want(Group::Credentials) {
        for it in plan.items.iter().filter(|i| i.group == Group::Credentials) {
            match actions::remove_path(&it.path, it.needs_privilege) {
                Ok(()) => out.removed.push(it.path.clone()),
                Err(e) => out.failed.push((it.path.clone(), e.to_string())),
            }
        }
    } else {
        for it in plan.items.iter().filter(|i| i.group == Group::Credentials) {
            out.kept.push(it.path.clone());
        }
    }

    // --- 3. rmdir $ATOMCODE_HOME/ if empty ---
    if plan.atomcode_dir.exists() {
        let _ = std::fs::remove_dir(&plan.atomcode_dir); // ignore non-empty error
    }

    // --- 4. PATH cleanup ---
    if decisions.binary {
        if let Some(c) = ctx.as_ref() {
            for (rc, prefix) in &c.rc_files {
                match actions::apply_unix_path_cleanup(rc, prefix) {
                    Ok(r) if r.modified => {
                        out.removed.push(rc.clone());
                        if let Some(b) = r.backup_path {
                            out.backups.push(b);
                        }
                    }
                    Ok(_) => {}
                    Err(e) => out.failed.push((rc.clone(), e.to_string())),
                }
            }
            #[cfg(windows)]
            if let (Some(lit), Some(exp)) = (
                c.windows_install_dir_literal.as_ref(),
                c.windows_install_dir_expanded.as_ref(),
            ) {
                match actions::apply_windows_path_cleanup(lit, exp) {
                    Ok(true) => out.removed.push(PathBuf::from("HKCU\\Environment\\Path")),
                    Ok(false) => {}
                    Err(e) => out
                        .failed
                        .push((PathBuf::from("HKCU\\Environment\\Path"), e.to_string())),
                }
            }
        }
    }

    // --- 5. Binary group EXCEPT the binary itself ---
    if decisions.binary {
        for it in plan
            .items
            .iter()
            .filter(|i| i.group == Group::Binary && i.path != plan.binary_path)
        {
            match actions::remove_path(&it.path, it.needs_privilege) {
                Ok(()) => out.removed.push(it.path.clone()),
                Err(e) => out.failed.push((it.path.clone(), e.to_string())),
            }
        }
        // --- 6. Self-delete (last) ---
        match self_delete.run(&plan.binary_path) {
            Ok(()) => out.removed.push(plan.binary_path.clone()),
            Err(e) => out.failed.push((plan.binary_path.clone(), e.to_string())),
        }
    } else {
        for it in plan.items.iter().filter(|i| i.group == Group::Binary) {
            out.kept.push(it.path.clone());
        }
    }

    Ok(out)
}
