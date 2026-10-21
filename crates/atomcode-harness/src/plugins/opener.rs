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

/// The stock HarmonyOS PC browser, and the ability `aa` starts to show it a URL.
///
/// Both come from the system image — the device's own scene board logs the pair as
/// `MainAbility/com.huawei.hmos.browser/entry/0`.
const OHOS_BROWSER_ABILITY: &str = "MainAbility";
const OHOS_BROWSER_BUNDLE: &str = "com.huawei.hmos.browser";

/// HarmonyOS PC's screen, reached through the device's own ability manager.
///
/// Not `LocalOpener`'s business there: the sandbox it looks at exports no
/// `$DISPLAY` and its image has no `xdg-open`, so it would refuse a URL on a
/// machine that has both a browser and somebody in front of it. Files do still go
/// to it — `aa` carries a URL and nothing else, so a path keeps the local answer:
/// a refusal naming the path to open by hand.
struct OhosOpener;

#[async_trait]
impl Opener for OhosOpener {
    fn describe(&self) -> String {
        "this device's own browser via `aa`".to_string()
    }

    async fn open(&self, target: &OpenTarget) -> Result<String, String> {
        let url = match target {
            OpenTarget::Url(url) => url,
            OpenTarget::Path(_) => return LocalOpener.open(target).await,
        };
        // The explicit ability plus the `viewData` action is what makes the browser
        // load the page: the bundle-only implicit form answers `start ability
        // successfully` without ever opening anything (verified on HarmonyOS PC).
        // `aa` exits as soon as the want is handed over, so the page appears seconds
        // later — the message says so rather than implying a window is already up.
        let mut cmd = std::process::Command::new("aa");
        cmd.arg("start")
            .arg("-a")
            .arg(OHOS_BROWSER_ABILITY)
            .arg("-b")
            .arg(OHOS_BROWSER_BUNDLE)
            .arg("-A")
            .arg("ohos.want.action.viewData")
            .arg("-U")
            .arg(url)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        match cmd.spawn() {
            Ok(_child) => Ok(format!(
                "Opened URL `{url}` via `aa`. The device's browser shows it a few seconds later."
            )),
            Err(e) => Err(format!(
                "open_file: failed to launch `aa`: {e}.\n\nURL for manual access:\n  {url}"
            )),
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
