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
//!
//! HarmonyOS PC is where those two machines look split and are not: the agent runs
//! inside the device's own app sandbox, which exports no `$DISPLAY` and whose
//! system image carries no `xdg-open` — while the person is sitting at that very
//! device. `aa`, the device-side ability manager, is the way to their screen, so
//! that is what `opener-local` provides there (see [`OhosOpener`]).

use std::sync::Arc;

use async_trait::async_trait;
use atomcode_capabilities::tools::{LocalOpener, OpenFileTool, OpenTarget, Opener};
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
        // HarmonyOS PC reaches the person through the device's own browser rather
        // than a desktop launcher: see `OhosOpener`.
        let opener: Arc<dyn Opener> = if cfg!(target_env = "ohos") {
            Arc::new(OhosOpener)
        } else {
            Arc::new(LocalOpener)
        };
        let _ = ctx
            .provide::<OpenerSvc>(opener)
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

/// HarmonyOS PC's screen, reached through the device's own ability manager.
///
/// The wrapper exists for the line the person reads ([`Opener::describe`]): "this
/// device's own browser via `aa`". The `aa` invocation itself has one home —
/// `capabilities::open_file::open_local_url`, where the argv shape is pinned by a
/// test — so the URL branch delegates instead of keeping a copy that could drift
/// from it. Paths keep [`LocalOpener`]'s answer: `aa` carries a URL and nothing
/// else, so a path is a refusal naming the path to open by hand.
struct OhosOpener;

#[async_trait]
impl Opener for OhosOpener {
    fn describe(&self) -> String {
        "this device's own browser via `aa`".to_string()
    }

    async fn open(&self, target: &OpenTarget) -> Result<String, String> {
        match target {
            OpenTarget::Path(_) => LocalOpener.open(target).await,
            OpenTarget::Url(url) => {
                atomcode_capabilities::tools::open_file::open_local_url(url).await
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The one line the person reads names the tool that actually opens the link.
    #[test]
    fn ohos_opener_says_what_it_opens_with() {
        assert!(OhosOpener.describe().contains("aa"));
    }
}
