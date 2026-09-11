//! Providers for the execution world.
//!
//! `fs-local` and `bash-local` are the local machine; `fs-readonly` is the same
//! disk with writes refused. The implementations live in
//! `atomcode_capabilities::world`, next to the tools that go through them —
//! these rows only choose which one to mount.
//!
//! There used to be a third seam here, `subprocess` (an argv runner), with the
//! shell built on top of it so that swapping the process provider relocated
//! bash. It went when the shell seam became a process handle: "which shell
//! exists and how is its tree reaped" is one question, answered by one
//! provider, and an argv seam underneath it had no second consumer — it was
//! plumbing that existed to make a claim, not to carry anything. The claim
//! survives, one level up: swap `shell` and bash relocates.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use atomcode_capabilities::world::{LocalFs, LocalShell};
use atomcode_plexus::{Context, Plugin};
use serde::Deserialize;
use serde_json::Value;

use crate::seams::{FileSystem, FsSvc, ShellSvc};

// ---- fs-local -----------------------------------------------------------

#[derive(Debug, Deserialize, Default)]
struct FsRow {
    #[serde(default)]
    root: Option<String>,
    #[serde(default)]
    read_only: bool,
}

pub struct FsLocalPlugin;

#[async_trait]
impl Plugin for FsLocalPlugin {
    fn name(&self) -> &'static str {
        "fs-local"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["fs"]
    }
    fn description(&self) -> &'static str {
        "the local disk, fenced to a root"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        let row: FsRow = parse(config)?;
        let root = row
            .root
            .map(PathBuf::from)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        let _ = ctx
            .provide::<FsSvc>(if row.read_only {
                Arc::new(LocalFs::read_only(root)) as Arc<dyn FileSystem>
            } else {
                Arc::new(LocalFs::new(root))
            })
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

/// The same world with writes refused. A separate row rather than a flag,
/// because "which world am I in" should be visible in `--dump-config`.
pub struct FsReadOnlyPlugin;

#[async_trait]
impl Plugin for FsReadOnlyPlugin {
    fn name(&self) -> &'static str {
        "fs-readonly"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["fs"]
    }
    fn description(&self) -> &'static str {
        "the local disk with every mutation refused"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        let row: FsRow = parse(config)?;
        let root = row
            .root
            .map(PathBuf::from)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        let _ = ctx
            .provide::<FsSvc>(Arc::new(LocalFs::read_only(root)))
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

// ---- bash-local ---------------------------------------------------------

/// This machine's shell — the production spawn, with its tty detach, job
/// object / process-group reaping and code-page decoding, behind the seam.
///
/// No `program` knob: which shell binary a world uses is the world's own
/// answer (Git Bash vs `cmd.exe` on Windows is *detected*, not configured), and
/// a row that overrode it would be the assembly deciding a fact for a machine it
/// may not be running on.
pub struct BashLocalPlugin;

#[async_trait]
impl Plugin for BashLocalPlugin {
    fn name(&self) -> &'static str {
        "bash-local"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["shell"]
    }
    fn description(&self) -> &'static str {
        "this machine's shell, with its process tree reaped on kill"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let _ = ctx
            .provide::<ShellSvc>(Arc::new(LocalShell))
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

fn parse<T: for<'de> Deserialize<'de> + Default>(config: &Value) -> Result<T, String> {
    if config.is_null() {
        return Ok(T::default());
    }
    serde_json::from_value(config.clone()).map_err(|e| format!("bad config: {e}"))
}
