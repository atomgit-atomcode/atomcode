//! Model-facing tools that reach the outside **only** through the execution
//! world seams.
//!
//! Point `fs` or `shell` somewhere else and every one of these follows, which
//! is what makes "run this agent against a sandbox" a config change rather than
//! a parallel tool implementation.
//!
//! # These used to be a second implementation. They are not.
//!
//! This module once carried its own `read_file` / `write_file` / `edit_file` /
//! `list_directory` / `bash` — ~560 lines that existed only because the
//! production tools in `atomcode-capabilities` reached the disk and spawned
//! processes directly, and so could not be pointed anywhere else. The default
//! tree therefore ran those lines while the shipped agent ran the real ones,
//! and they agreed on nothing finer than "it reads files" / "it runs commands":
//! encoding detection, line endings, oversize handling, diff application,
//! destructive-command classification, tty detach and process-tree reaping were
//! all different, and the differential rig could not see it because it compares
//! event streams rather than tool output.
//!
//! The seams moved down instead: `atomcode_capabilities::world` now sits under
//! the real tools, so these rows mount *those*, handed a world. This row and the
//! production `tool-fs` / `tool-bash` rows differ by one argument.

use std::sync::Arc;

use async_trait::async_trait;
use atomcode_capabilities::tools::{
    BashTool, EditFileTool, GlobTool, GrepTool, ListDirTool, ReadFileTool, WriteFileTool,
};
use atomcode_kernel::tool::Tool;
use atomcode_plexus::{Context, Plugin};
use serde_json::Value;

use crate::seams::{FsSvc, ShellSvc};

pub struct FsWorldToolsPlugin;

#[async_trait]
impl Plugin for FsWorldToolsPlugin {
    fn name(&self) -> &'static str {
        "tool-fs-world"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tools", "fs"]
    }
    fn description(&self) -> &'static str {
        "read/write/edit/list, routed through the `fs` seam"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let fs = ctx.require::<FsSvc>().map_err(|e| e.to_string())?;
        // The SAME implementations `tool-fs` mounts, handed a world instead of
        // the local disk. There is no second copy of `read_file` any more: this
        // row and that one differ by one argument, which is the whole claim the
        // `fs` seam was built to make.
        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(ReadFileTool::with_world(false, fs.clone())),
            Arc::new(WriteFileTool::with_world(fs.clone())),
            Arc::new(EditFileTool::with_world(fs.clone())),
            Arc::new(ListDirTool::with_world(fs.clone())),
        ];
        super::tools::mount(ctx, tools)?;
        // The model is told which world it is in, because "the file is not
        // there" and "the file is not there *in this world*" are different
        // problems and only one of them is the model's to solve.
        super::tools::contribute_prompt(
            ctx,
            "tool-fs-world",
            50,
            &format!(
                "Files you read and write live in this execution world: {}. Read a file before \
                 editing it, and prefer `edit_file` over rewriting a whole file.",
                fs.describe()
            ),
        );
        Ok(())
    }
}

// ---- search -------------------------------------------------------------

pub struct SearchWorldToolsPlugin;

#[async_trait]
impl Plugin for SearchWorldToolsPlugin {
    fn name(&self) -> &'static str {
        "tool-search-world"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tools", "fs"]
    }
    fn description(&self) -> &'static str {
        "grep and glob, routed through the `fs` seam"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let fs = ctx.require::<FsSvc>().map_err(|e| e.to_string())?;
        // These were the two tools a fenced world could not contain: they walked
        // the host disk with their own `ignore::WalkBuilder`. The walk and the
        // search are the world's now (`FileSystem::walk` / `search`), so a
        // fenced root refuses them before an entry is read.
        super::tools::mount(
            ctx,
            vec![
                Arc::new(GrepTool::with_world(fs.clone())),
                Arc::new(GlobTool::with_world(fs.clone())),
            ],
        )?;
        super::tools::contribute_prompt(
            ctx,
            "tool-search-world",
            52,
            "Prefer `grep`/`glob` over listing directories by hand when you are looking for \
             something by name or content.",
        );
        Ok(())
    }
}

// ---- bash ---------------------------------------------------------------

pub struct BashWorldToolPlugin;

#[async_trait]
impl Plugin for BashWorldToolPlugin {
    fn name(&self) -> &'static str {
        "tool-bash-world"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tools", "shell"]
    }
    fn description(&self) -> &'static str {
        "shell execution, routed through the `shell` seam"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let shell = ctx.require::<ShellSvc>().map_err(|e| e.to_string())?;
        // The production `bash` — destructive-command classifier, timeout clamp
        // and its advice, terminal-output sanitising — handed a world. What used
        // to be a 150-line stand-in with a 16-word read-only allow-list is gone;
        // the timeout ceiling and output framing are the tool's own, as they are
        // for `tool-bash`.
        super::tools::mount(ctx, vec![Arc::new(BashTool::with_world(shell.clone()))])?;
        super::tools::contribute_prompt(
            ctx,
            "tool-bash-world",
            51,
            &format!(
                "Shell commands run through: {}. Quote paths that contain spaces.",
                shell.describe()
            ),
        );
        Ok(())
    }
}
