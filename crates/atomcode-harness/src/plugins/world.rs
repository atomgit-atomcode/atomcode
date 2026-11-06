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
    /// Fence mutations to `root` and leave reads alone.
    ///
    /// The boundary a checkout a person stepped into wants: nothing outside it
    /// gets changed, and everything outside it is still readable. Not a second
    /// kind of `root` — the same root with the fence applied on one side only.
    #[serde(default)]
    writes_only: bool,
    /// Places outside `root` a mutation may still land.
    ///
    /// The machine's temp directory, in practice: a checkout that cannot make a
    /// scratch file breaks the toolchain without protecting anything.
    #[serde(default)]
    also_writable: Vec<String>,
    /// Existing resource directories or exact files; never grants mutations.
    #[serde(default)]
    also_readable: Vec<String>,
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
        "the local disk; fenced to a root only when one is given, and on the write side alone when asked"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        let row: FsRow = parse(config)?;
        // No root, no fence. The conversation's own agent works the way the
        // production stack does: anywhere on the disk, with the approval rows
        // deciding what it may change and where. A fence is a hard boundary
        // — nothing above it can talk it into a path — which is what a
        // delegated member's worktree, a read-only audit or a sandbox wants,
        // and what a person's main agent, reaching into ~/.cargo or a sibling
        // repository, does not.
        //
        // `writes_only` is the middle case, and the one a checkout wants: the
        // same root, applied to mutations alone.
        let mut world = match (row.root.map(PathBuf::from), row.read_only) {
            (Some(root), true) => LocalFs::read_only(root),
            (Some(root), false) if row.writes_only => {
                let mut world = LocalFs::writes_fenced(root);
                for dir in &row.also_writable {
                    world = world.also_writable(dir);
                }
                world
            }
            (Some(root), false) => LocalFs::new(root),
            (None, true) => LocalFs::read_only_unfenced(),
            (None, false) => LocalFs::unfenced(),
        };
        for path in &row.also_readable {
            world = world.also_readable(path);
        }
        let world: Arc<dyn FileSystem> = Arc::new(world);
        let _ = ctx.provide::<FsSvc>(world).map_err(|e| e.to_string())?;
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
        // Read-only is the point of this row; a root is optional, as above.
        let mut world = match row.root.map(PathBuf::from) {
            Some(root) => LocalFs::read_only(root),
            None => LocalFs::read_only_unfenced(),
        };
        for path in &row.also_readable {
            world = world.also_readable(path);
        }
        let world: Arc<dyn FileSystem> = Arc::new(world);
        let _ = ctx.provide::<FsSvc>(world).map_err(|e| e.to_string())?;
        Ok(())
    }
}

// ---- fs-saf -------------------------------------------------------------

/// The world inside a folder the person granted through the system picker.
///
/// The row exists so a build without all-files access can still hand the agent a
/// real place to work: `fs-local` is unreachable there (no `MANAGE_EXTERNAL_STORAGE`,
/// so nothing outside the app's own directory has a path), while a granted tree is
/// reachable as `content://` documents. Swapping the `fs` row to this name is the
/// whole switch — everything below it follows, because no tool touches a path
/// itself.
///
/// The platform half is installed by the host (`saf::install_backend`); without it
/// this row refuses to mount rather than providing a world that reads nothing.
pub struct FsSafPlugin;

#[derive(Debug, Deserialize, Default)]
struct SafRow {
    /// The path the world reports as its root. Synthetic — SAF has no paths — but
    /// tools print it, gates compare against it, and the model reads it.
    #[serde(default)]
    root: Option<String>,
    #[serde(default)]
    read_only: bool,
}

#[async_trait]
impl Plugin for FsSafPlugin {
    fn name(&self) -> &'static str {
        "fs-saf"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["fs"]
    }
    fn description(&self) -> &'static str {
        "the folder granted through Android's file picker (Storage Access Framework)"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        let row: SafRow = parse(config)?;
        let backend = atomcode_capabilities::saf::installed_backend().ok_or_else(|| {
            "no folder has been granted through the file picker in this build".to_string()
        })?;
        let root = std::path::PathBuf::from(row.root.unwrap_or_else(|| "/saf".to_string()));
        // The daemon's `/fs/*` routes work on real paths; they need to recognise
        // which path means "the granted tree" before they can browse or search it.
        atomcode_capabilities::saf::install_root(root.clone());
        let world = if row.read_only {
            atomcode_capabilities::saf::SafFs::read_only(root, backend)
        } else {
            atomcode_capabilities::saf::SafFs::new(root, backend)
        };
        let _ = ctx
            .provide::<FsSvc>(std::sync::Arc::new(world))
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
