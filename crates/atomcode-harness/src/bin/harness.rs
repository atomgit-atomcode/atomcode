//! `harness` — the launcher.
//!
//! It resolves a profile into a config tree, mounts it, and hands control to
//! whichever plugin fills the `ui` slot. It knows nothing about models, tools,
//! turns, or terminals: those all arrive as rows. The full-screen front end is
//! a crate of its own with a launcher of its own, `atui`.
//!
//! ```text
//! harness "fix the build"                  # the default profile
//! harness --profile repl                   # an interactive terminal session
//! harness --profile web                    # an HTTP server with a live stream
//! harness --profile sdk                    # JSON-RPC on stdio, for a program
//! harness --list-profiles                  # what is available, and from where
//! harness --profile web --dump-config      # what would run
//! ```

use std::process::ExitCode;

use atomcode_harness::launch::{Flag, Launch, HELP_SHARED};
use atomcode_harness::plugins;
use atomcode_harness::profile::Profiles;

#[tokio::main]
async fn main() -> ExitCode {
    let help = format!("{HELP}\n\n{HELP_SHARED}");
    let mut launch = match Launch::new("oneshot", Vec::new()).parse(
        std::env::args().skip(1),
        &help,
        |_, _, _| Flag::NotMine,
    ) {
        Ok(launch) => launch,
        Err(code) => return code,
    };

    let profiles = Profiles::builtin().with_home();
    let catalog = plugins::catalog();
    if let Some(code) = launch.preflight(&profiles, &catalog) {
        return code;
    }
    let mounted = match launch.mount(catalog, &profiles).await {
        Ok(mounted) => mounted,
        Err(code) => return code,
    };
    if let Some(code) = mounted.inspect(&profiles, &[]) {
        return code;
    }
    mounted.hand_over().await
}

const HELP: &str = "\
harness — AtomCode's coding agent as a plugin tree

The launcher resolves a profile into a config tree, mounts it, and hands control
to whichever plugin fills the `ui` slot. Every flag below picks a profile or
stacks a patch layer; none of them is a code path.

USAGE:
    harness [OPTIONS] [PROMPT]

PROFILES:
    -p, --profile <NAME>   named assembly (default: oneshot)
    -i, --repl             shorthand for --profile repl
        --web              shorthand for --profile web
        --port <N>         where the web front end listens (default 7878)
        --sdk              shorthand for --profile sdk
        --headless         shorthand for --profile headless
        --ui <NAME>        swap the front end under any profile
                           (the full-screen one is `atui`, its own launcher)";
