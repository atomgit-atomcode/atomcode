//! Library surface for the `atomcode` binary.
//!
//! Exists so integration tests (e.g. `tests/script_parity.rs`) and the binary
//! share testable modules. The bulk of the CLI still lives in `main.rs`; only
//! modules that need to be reachable from `tests/` belong here.

// Redirect ATOMCODE_HOME to a temp dir before this lib crate's tests run, so they
// don't pollute the real ~/.atomcode (mirrors the bin's ctor in main.rs).
#[cfg(test)]
#[ctor::ctor]
fn _isolate_atomcode_home() {
    atomcode_kernel::test_support::isolate_home();
}

#[cfg(unix)]
pub mod askpass;
pub mod uninstall;

/// ACP (Agent Client Protocol) stdio server — lets atomcode be driven by Zed /
/// multi-agent orchestrators over stdin/stdout. Wired up by the `atomcode acp`
/// subcommand in `main.rs`; the engine/dispatch/translate/permission internals
/// live here. Does not depend on `atomcode-core` (v2 stack only).
pub mod acp;

/// `atomcode --tui`: the full-screen UI of `atomcode-tui`, in an App of its own,
/// driving the product runtime through the handle protocol and host control.
pub mod tui_front {
    use std::sync::Arc;

    use atomcode_coding::front_end::{connect, FrontEnd};
    use atomcode_coding::{CodingAgentConfig, CodingRuntime};
    use atomcode_tui::launch::{self, Screen};

    /// The screen, mounted and connected to `runtime` — which was started with
    /// `front_end` in its prepare options — and not yet running.
    pub async fn mount(
        runtime: CodingRuntime,
        front_end: Arc<FrontEnd>,
        config: CodingAgentConfig,
        screen: &Screen,
    ) -> Result<launch::Mounted, String> {
        let connection = connect(runtime, front_end, config)?;
        launch::mount(screen, &[], connection).await
    }

    /// Run the screen until the person leaves.
    pub async fn run(
        runtime: CodingRuntime,
        front_end: Arc<FrontEnd>,
        config: CodingAgentConfig,
        screen: &Screen,
    ) -> Result<(), String> {
        let mounted = mount(runtime, front_end, config, screen).await?;
        let ctx = mounted.app.context();
        mounted.ui.run(&ctx, None).await
    }
}
