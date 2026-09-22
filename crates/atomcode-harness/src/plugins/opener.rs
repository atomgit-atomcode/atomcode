//! Showing a file to the person: the `opener` seam and the one tool on it.
//!
//! This is the one capability that is **not** part of the execution world. Every
//! other tool acts on the machine the agent runs on; `open_file` acts on the
//! machine the person is at. The two coincide in a terminal session on a laptop
//! and nowhere else — a daemon, a web front end, a remote sandbox all break it,
//! and a window opened on the server reaches nobody.
//!
//! So the opener is the **front end's** to provide, and only a front end with a
//! person at a display provides one: `opener-local` rides in the `repl` and
//! `tui` bundles. `tool-open-file` injects `opener`, which makes "you cannot open
//! files here" structural — in a headless, sdk or web tree the row does not
//! mount, rather than mounting and refusing every call. (deepseek-harness draws
//! the same line: path opening is a host capability the client invokes, never a
//! `ctx.fs` method.)

use std::sync::Arc;

use async_trait::async_trait;
use atomcode_capabilities::tools::{LocalOpener, OpenFileTool};
use atomcode_plexus::{Context, Plugin};
use serde_json::Value;

use crate::seams::OpenerSvc;

/// This machine's desktop. Right exactly when the person is sitting at the
/// machine the agent runs on — which is what a terminal front end means.
pub struct OpenerLocalPlugin;

#[async_trait]
impl Plugin for OpenerLocalPlugin {
    fn name(&self) -> &'static str {
        "opener-local"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["opener"]
    }
    fn description(&self) -> &'static str {
        "show files and URLs on this machine's desktop"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let _ = ctx
            .provide::<OpenerSvc>(Arc::new(LocalOpener))
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

pub struct OpenFileToolPlugin;

#[async_trait]
impl Plugin for OpenFileToolPlugin {
    fn name(&self) -> &'static str {
        "tool-open-file"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tools", "opener"]
    }
    fn description(&self) -> &'static str {
        "open_file, presented through whatever front end has the person"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let opener = ctx.require::<OpenerSvc>().map_err(|e| e.to_string())?;
        super::tools::mount(ctx, vec![Arc::new(OpenFileTool::with_opener(opener))])?;
        Ok(())
    }
}
