//! The product: a coding agent, with this screen in front of it.
//!
//! Everything else in this crate is a *front end* — a surface, panels, command
//! sets, the pump that drives an agent. This module is the other half: which
//! agent. It is the one place that says "the full-screen TUI is a coding
//! agent", and it says it by naming coding's rows, not by restating them.
//!
//! # Why it is a bundle and a profile, and not a second mount path
//!
//! The launcher already has one way to build a tree: `Profiles::resolve` stacks
//! bundles, then the profile's own edits, then the user's home patch, then any
//! `--patch` overlay. Adding coding's rows *through that path* is what keeps a
//! person's own patch file last — the property ADR 0017 is about. A second
//! mount function beside it (`on_harness::mount_swappable`, written for a host
//! that has a provider object and no config tree) would have meant `atui`
//! growing its own layer order, and the user's `harness.patch.toml` silently
//! ceasing to apply.
//!
//! # Which layers, in which order
//!
//! ```text
//! infra                 (the harness machine: registries, session log, loop)
//! tui-app               CODING_DEFAULTS + the two patches below
//!                       + CODING_ROWS, with the working dir substituted in
//! repl-app              the harness's interactive-terminal assembly
//! tui-panels            this crate's surface and panels
//! the profile's edits   swaps ui-repl for ui-tui2, silences `trace`, stands
//!                       the standalone asker down, drops `ui-handle`
//! $ATOMCODE_HOME/harness.patch.toml     the person
//! --patch overlays      the person, again
//! ```
//!
//! The order is not decoration. `tui-panels` has to come after `repl-app`
//! because `Op::Insert` replaces a row with the same id: restating a row to
//! adjust it would revert whatever every layer below — including the person's
//! home patch — had done to it. Between `repl-app` and the user's layer the two
//! bundles are laid down in dependency order (`ui` before the panels that
//! `inject` it), and only after both does the profile's own edit layer run,
//! where a `[[patch]]` sees every row it addresses.
//!
//! # `ui-handle` is the one row coding has that this product does not
//!
//! Coding's assembly is driven through `ui-handle`, which hands the agent to
//! `CodingRuntimeHandle` as an `AgentHandle`. This product's agent is driven by
//! its own pump, inside the `ui-tui2` row, and both rows fill `ui` — so
//! `ui-handle` is disabled here rather than removed. Removed would say the row
//! does not exist; disabled says it is coding's driver protocol and this is not
//! that driver, which is the true statement, and it leaves a row to re-enable
//! for anyone who wants the protocol.
//!
//! # The provider comes from the tree, not from the host
//!
//! `mount_swappable` builds the provider, keeps it in a `ProviderSlots` table
//! and mounts `llm-injected` over it, because that host already holds
//! credentials and a model catalog before the tree exists. Here `llm` stays
//! `llm-atomcode-config`: the row reads `~/.atomcode/config.toml` itself, which
//! is what makes `--offline`, `--model` and `--env-model` patches to one row.
//! `models-host` and `llm-utility-selected` stay disabled for the matching
//! reason — the `models` seam is one the host fills, and this host has no table
//! to fill it from. `model-catalog` is mounted anyway: it reads the seam
//! opportunistically (`uses`, not `inject`) and falls silent when there is
//! none, and it is the row that will light up on the day a host does provide
//! one.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use atomcode_coding::on_harness::{coding_overlay, Presence, CODING_DEFAULTS};
use atomcode_harness::plugins;
use atomcode_harness::profile::Profiles;
use atomcode_plexus::PluginRegistry;

use crate::plugin::{HeadlessSurfacePlugin, TerminalSurfacePlugin, TuiUiPlugin};
use crate::rows;

/// The bundle that is the coding product: what the model can do, who it is, and
/// the gates it asks through.
const APP_BUNDLE: &str = "tui-app";

/// The bundle that is this front end: a surface, and one row per panel.
const PANELS_BUNDLE: &str = "tui-panels";

/// The profile a person names: `atui -p tui` is the default and the only one
/// that is a product.
pub const PROFILE: &str = "tui";

/// The assembly, resolved into the two things a launcher needs.
pub struct Assembly {
    bundles: Vec<(String, String)>,
    layers: Vec<String>,
}

impl Assembly {
    /// The row catalog: the harness's, this crate's, and coding's discipline.
    ///
    /// Registration is not mounting — a registered row that no layer names is
    /// inert — but it is what decides whether a row *can* be mounted, and that
    /// is the whole reason this product depends on `atomcode-coding`: five of
    /// the rows its list names (coding's persona, its verify cadence, its
    /// execution boundary, both halves of its skill steering) are implemented
    /// in no other crate. A tree that named them without registering them would
    /// fail to mount rather than quietly ship without them.
    pub fn catalog(&self) -> PluginRegistry {
        let mut c = plugins::catalog();
        c.register(Arc::new(TuiUiPlugin))
            .register(Arc::new(TerminalSurfacePlugin))
            .register(Arc::new(HeadlessSurfacePlugin));
        for row in rows::catalog() {
            c.register(row);
        }
        for row in atomcode_coding::on_harness::plugins() {
            c.register(row);
        }
        c
    }

    /// The profiles a launcher resolves against, with this product's bundles
    /// and profile added to the shipped ones.
    ///
    /// `.with_home()` is the launcher's to call, and deliberately after this:
    /// a `$ATOMCODE_HOME/profiles/tui.toml` is a deployment deciding what this
    /// product means, and it is nearer the person than the binary is.
    pub fn profiles(&self) -> Profiles {
        Profiles::builtin()
            .with_bundles(&self.bundles)
            .with_profile(
                PROFILE,
                vec![
                    "infra".to_string(),
                    APP_BUNDLE.to_string(),
                    "repl-app".to_string(),
                    PANELS_BUNDLE.to_string(),
                ],
                Some(self.layers.join("\n")),
                "the full-screen TUI: a coding agent with panels in front of it",
            )
    }
}

/// Build the product assembly for `working_dir`.
///
/// The working directory is read once, here, because three rows need it
/// substituted into their config (the loop's cwd, and the gates that decide
/// what is inside the workspace). It is the process's cwd: an `atui` session is
/// about the directory it was started in.
pub fn assembly() -> Assembly {
    let working_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let artifacts = working_dir.join(".atomcode").join("artifacts");
    Assembly {
        bundles: vec![
            (APP_BUNDLE.to_string(), app_bundle(&working_dir, &artifacts)),
            (PANELS_BUNDLE.to_string(), panels_bundle()),
        ],
        layers: vec![profile_edit()],
    }
}

/// Coding's rows, plus the two patches that are about this host rather than
/// about coding: the working directory `agent-loop` must be told, and the round
/// budget — which now lives in `CODING_DEFAULTS`, so every host that stacks that
/// list gets the same one.
///
/// `CODING_DEFAULTS` and `CODING_ROWS` are taken whole from `atomcode-coding`:
/// they are that crate's product decision, stated once, and a second copy here
/// is how two products come to disagree about what a coding agent can do. What
/// this host adds is only what coding's own host does differently:
///
/// * `agent-loop` gets the working directory. Left unset the row falls back to
///   the process's cwd, which is the same thing — but the value is also what
///   the gates below are told, and one of the two being implicit is how they
///   come to disagree.
/// * the file world is left **unfenced**, which is the `Presence::Attended`
///   half of coding's rule: a person is at this screen, so work outside the
///   workspace is a question answered through `approval-interactive` rather
///   than a refusal. `Presence::Headless` is the other half and the only
///   difference between the two trees.
/// * the persona is told to read the model from the tree (`model = ""`), since
///   the provider here is built by the `llm` row and can be swapped by
///   `--model` after this layer was written.
///
/// `ui-handle` comes along in `CODING_ROWS` and the profile's own layer disables
/// it — see the module docs.
fn app_bundle(working_dir: &Path, artifacts: &Path) -> String {
    // `toml_string`, not `{:?}` — see `atomcode_harness::bundle::toml_string`.
    // A working directory is whatever the user made, and `{:?}` writes a control
    // character as `\u{7f}`, which TOML does not accept.
    format!(
        "{CODING_DEFAULTS}\n\
         [[patch]]\nid = \"agent-loop\"\nconfig = {{ working_dir = {} }}\n\n\
\
         {rows}\n",
        atomcode_harness::bundle::toml_string(&working_dir.to_string_lossy()),
        rows = coding_overlay(working_dir, artifacts, Presence::Attended, ""),
    )
}

/// The surface, and one row per panel.
///
/// The panels come from [`rows::SCREEN`] rather than a list written here, so
/// that a panel added to that const arrives on this screen too. The one thing
/// restated is the team strip: `SCREEN` ships it off because a screen that
/// never delegates has no team, and this product mounts `team-in-process`, so
/// the strip is exactly what it is for.
fn panels_bundle() -> String {
    format!(
        "[[insert]]\nid = \"surface\"\nname = \"surface-terminal\"\n\n{SCREEN}\n\
         [[patch]]\nid = \"tui-panel-team\"\ndisabled = false\n",
        SCREEN = rows::SCREEN,
    )
}

/// The edits this profile makes to the layers beneath it.
///
/// A patch layer and not part of either bundle, because `Op::Insert` replaces a
/// row with the same id: a row's `disabled`/`config` can only be overruled by a
/// layer that runs *after* the one that restated it, and a third bundle in the
/// list runs before this, before the user's home patch and before every
/// `--patch`. Everything here names a row some layer below inserted.
fn profile_edit() -> String {
    r#"
# `ui-repl` is the line-based REPL this screen replaces. `config = {}` is
# explicit: a patch that names a new plugin keeps the old row's config, so the
# repl app's `banner`/`prompt` — knobs this row has no equivalent of — would
# otherwise arrive at it.
[[patch]]
id = "ui"
name = "ui-tui2"
config = {}

# The screen is the output; nothing else may write to it.
[[patch]]
id = "trace"
config = { stream = false, tools = false, summary = false }

# `ui-tui2` fills `user-questions`, so the standalone asker must stand down:
# two rows filling one slot is an error rather than a preference.
[[patch]]
id = "user-questions-unattended"
disabled = true

# `ui-handle` is coding's driver protocol — an `AgentHandle` for
# `CodingRuntimeHandle` to drive. This product's agent is driven by the pump
# inside `ui-tui2`, and both rows fill `ui`. Disabled rather than removed: the
# row is still the way to reach this tree from a runtime that speaks the
# protocol.
[[patch]]
id = "ui-handle"
disabled = true
"#
    .to_string()
}
