//! Panels and command sets, as rows.
//!
//! Before this file the UI *had* a module registry but no way to reach it from
//! a config tree: `assemble` called `add_view` three times and a `--mascot`
//! flag lit up an `if`. That is a plugin architecture in the Rust sense —
//! trait, registry, conformance suite — and not in the sense that matters here,
//! where "a capability is a row" is the whole convention. A panel that cannot
//! be `[[remove]]`d, does not appear in `--audit`, and cannot be added by a
//! crate that does not exist yet is not a plugin; it is a hard-coded panel with
//! a trait in front of it.
//!
//! So: one row per panel, one row per command set. The registries are unchanged
//! — they were already right — and `assemble` now builds an *empty* screen that
//! the tree fills.
//!
//! What this buys, concretely:
//!
//! * `[[remove]] id = "tui-panel-status"` gives a screen with no status bar,
//!   and nothing else notices.
//! * `--mascot` stops being a boolean on the UI row and becomes
//!   `[[insert]] name = "tui-panel-mascot"` — and that row puts itself on
//!   screen through `LayoutOp::Show`, rather than through a special case in
//!   `assemble`.
//! * `--audit` sees every panel, so "mounted but never drawn" and "named by a
//!   layout but never mounted" become findings instead of surprises.
//! * A third crate adds a panel with a `View` impl and a row. No edit here.

use std::sync::Arc;

use async_trait::async_trait;
use atomcode_plexus::{Context, Plugin};
use serde::Deserialize;
use serde_json::Value;

use crate::command::Commands;
use crate::layout::{LayoutOp, Side};
use crate::module::{Modules, Mounted, Producer};
use crate::modules::{
    ask, input, live, raster, status, steering, team, tip, todo, transcript, welcome,
};
use crate::plugin::{CommandsSvc, LayoutSvc, ModulesSvc};

/// The screen, panel by panel — the one place that says what a full UI is made
/// of.
///
/// The launcher and the end-to-end tests both take it from here. They used to
/// each keep a copy, and the copies disagreed the moment panels became rows:
/// the launcher grew nine new lines and the tests silently kept painting an
/// empty screen. A duplicated fact does not stay duplicated, it diverges — so
/// there is one.
pub const SCREEN: &str = r#"
# The opening block of a new session. It is a block in the stream (a producer),
# not a panel — so it scrolls away with the conversation: scroll back up and it is
# still there, rather than sitting on screen taking a row forever.
#
# Replace the whole thing with `[[patch]] id = "tui-panel-welcome"` and another
# `name`; `[[remove]]` it and the conversation simply starts with what was said.
# What this build calls itself. Before the welcome block, which reads it.
# `[[patch]] id = "tui-brand"` with a `config` is how a downstream build changes
# its name, its licence and its mascot without touching Rust — see `BrandRow`.
[[insert]]
name = "tui-brand"

[[insert]]
name = "tui-panel-welcome"

[[insert]]
name = "tui-panel-transcript"

[[insert]]
name = "tui-panel-status"

# Between the conversation and the field, because it belongs to the composer:
# its place is written in `host::composer`, not claimed with a `Show` op —
# neither of `Show`'s sides is the side of the input box. Remove this row and
# the composer closes up around the field.
[[insert]]
name = "tui-panel-live"

# The reserved row directly above the field, kept whether or not it has anything
# to say. On by default and placed by `host::composer` with the live line above
# it, for the same reason: it is part of the composer, and `Show` names neither
# side of the input box. Removing it gives the row back to the conversation.
[[insert]]
name = "tui-panel-tip"

[[insert]]
name = "tui-panel-input"

# On by default, unlike the mascot and the team strip: the list is only there
# when the model has planned something, and a plan nobody can see is a plan
# nobody follows. Mounted and placed by its own row, so `[[remove]]` takes it
# off the screen.
[[insert]]
name = "tui-panel-todo"

# On by default: it takes no room until the person types during a turn, and the
# gap it fills is otherwise blank on every screen — words already sent, not yet
# in the log, and no longer in the field. Rides the stream's tail with `todo`,
# `live` and `ask`.
[[insert]]
name = "tui-panel-steering"

# Off by default, on with `--mascot`. A row, not a boolean on the UI row.
[[insert]]
name = "tui-panel-mascot"
disabled = true

# Off by default too: a screen that never delegates has no team to show, and a
# panel that is always there saying "no members" is a row of chrome. Turn it on
# where the team row is on.
[[insert]]
name = "tui-panel-team"
disabled = true

# Off by default too: a screen that never mounts a bitmap has nothing for this
# to draw, and an empty pane is a column of chrome. Mount one through
# `RastersSvc` and turn this on.
[[insert]]
name = "tui-panel-raster"
disabled = true

# How a question is drawn. Remove this row and questions still arrive, still
# answer and still record — as plain lines at the foot of the stream. That is
# the fallback this row improves on, not a branch it replaces.
[[insert]]
name = "tui-panel-ask"

# The keys, as a row. Remove it and the screen still runs — with nothing bound
# but typing, which is what makes the row worth having rather than a constant.
# A downstream build mounts its own after this one and names the presses it is
# taking (`Keymap::overrides`).
[[insert]]
name = "tui-keys-default"

[[insert]]
name = "tui-commands-screen"

[[insert]]
name = "tui-commands-session"

# Its own row so a downstream build can take the conversation somewhere else:
# `[[remove]]` this one and mount its own, or override one name and keep the
# other (`CommandSet::overrides`).
[[insert]]
name = "tui-commands-take-away"

# The agent's own commands, from its description. After the screen's and the
# session's, so a name one of those already has stays theirs.
[[insert]]
name = "tui-commands-agent"

# Last, because it lists the others.
[[insert]]
name = "tui-commands-help"
"#;

/// Every row this crate ships, for a registry to take in one call.
///
/// A launcher that listed them by hand would silently miss the next one added.
pub fn catalog() -> Vec<std::sync::Arc<dyn Plugin>> {
    vec![
        Arc::new(BrandRow),
        Arc::new(TranscriptPanel),
        Arc::new(WelcomePanel),
        Arc::new(StatusPanel),
        Arc::new(LivePanel),
        Arc::new(TipPanel),
        Arc::new(InputPanel),
        Arc::new(MascotPanel),
        Arc::new(TeamPanel),
        Arc::new(TodoPanel),
        Arc::new(SteeringPanel),
        Arc::new(AskPanel),
        Arc::new(RasterPanel),
        Arc::new(DefaultKeysRow),
        Arc::new(ScreenCommandsRow),
        Arc::new(SessionCommandsRow),
        Arc::new(TakeAwayCommandsRow),
        Arc::new(AgentCatalogCommandsRow),
        Arc::new(HelpCommandsRow),
    ]
}

/// One panel, one row.
///
/// A macro because the body is identical for every panel and the differences —
/// which type, which row name — are exactly what a reader wants to see side by
/// side. Adding a panel is one line here plus the `View` impl.
macro_rules! panel {
    ($plugin:ident, $row:literal, $view:ty, $about:literal) => {
        pub struct $plugin;

        #[async_trait]
        impl Plugin for $plugin {
            fn name(&self) -> &'static str {
                $row
            }
            fn inject(&self) -> &'static [&'static str] {
                &["tui-modules"]
            }
            fn description(&self) -> &'static str {
                $about
            }
            async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
                let mods = ctx.require::<ModulesSvc>().map_err(|e| e.to_string())?;
                let view = Arc::new(Mounted::<$view>::new());
                let id = <$view as crate::module::View>::id();
                mods.add_view(view)?;
                let m: Arc<Modules> = mods.clone();
                let _ = ctx.effect(move || m.remove_view(id));
                Ok(())
            }
        }
    };
}

panel!(
    StatusPanel,
    "tui-panel-status",
    status::Status,
    "the status line: what the agent is doing and for how long"
);
panel!(
    InputPanel,
    "tui-panel-input",
    input::Input,
    "the prompt line, its wrapping, and the slash menu"
);
// The live line: the one panel whose row mounts it and says nothing about where
// it goes. Its place is written by the composer (`crate::host::composer`),
// because the answer for a row that belongs against the input box is not a side
// `LayoutOp::Show` can name.
panel!(
    LivePanel,
    "tui-panel-live",
    live::Live,
    "the live line above the composer: what this turn is doing, for how long, and for how much"
);
// The reserved row: the second panel whose row mounts it and says nothing about
// where it goes. It belongs against the field, between the live line and the top
// rule, which is `host::composer`'s to write for the same reason the live line's
// is — `LayoutOp::Show` has two sides and neither is the side of the input box.
panel!(
    TipPanel,
    "tui-panel-tip",
    tip::Tip,
    "the reserved row above the field: right-aligned tips, blank most of the time"
);
// A cell-grid bitmap, repainted in place. Off by default: what it draws is
// whatever a row mounted, and a pane with nothing mounted is a column of chrome.
panel!(
    RasterPanel,
    "tui-panel-raster",
    raster::RasterPane,
    "a cell-grid bitmap: the nearest thing to pixels a terminal has, repainted in place"
);

/// The opening block: a *producer*'s row, in the same shape as
/// `tui-panel-transcript`.
///
/// It claims no position. A block's place is its place in the stream, and the
/// stream is the line between the conversation and the panels (`docs/adr/0004`) —
/// so this row needs neither `LayoutSvc` nor `LayoutOp::Show`, unlike the panels
/// whose spot is written by `host::composer`.
pub struct WelcomePanel;

#[async_trait]
impl Plugin for WelcomePanel {
    fn name(&self) -> &'static str {
        "tui-panel-welcome"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tui-modules"]
    }
    fn description(&self) -> &'static str {
        "the opening block of a new session: the brand, the mascot, where you are, and a few commands worth knowing"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let mods = ctx.require::<ModulesSvc>().map_err(|e| e.to_string())?;
        // Whatever this build calls itself, or the shipped identity when no row
        // provides one: a screen with no brand row still opens.
        let brand = ctx
            .service::<crate::plugin::BrandSvc>()
            .unwrap_or_else(|| Arc::new(crate::content::Brand::default()));
        let producer = welcome::Welcome::new(brand);
        let id = producer.id();
        mods.add_producer(producer)?;
        let m: Arc<Modules> = mods.clone();
        let _ = ctx.effect(move || m.remove_producer(id));
        Ok(())
    }
}

/// The transcript is a *stream producer*, not a view: it has history, and its
/// settled blocks are unreachable by construction. Different registry, same
/// row shape.
pub struct TranscriptPanel;

#[async_trait]
impl Plugin for TranscriptPanel {
    fn name(&self) -> &'static str {
        "tui-panel-transcript"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tui-modules"]
    }
    fn description(&self) -> &'static str {
        "the conversation stream — the append-only half of the screen"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let mods = ctx.require::<ModulesSvc>().map_err(|e| e.to_string())?;
        let producer = transcript::Transcript::new();
        let id = producer.id();
        mods.add_producer(producer)?;
        let m: Arc<Modules> = mods.clone();
        let _ = ctx.effect(move || m.remove_producer(id));
        Ok(())
    }
}

/// The mascot: the row that proves the point.
///
/// It mounts a view *and* puts itself on screen, through `LayoutOp::Show`.
/// Before this it was a branch inside `assemble` that built a different region
/// tree. Whether it is on screen is whether this row is on (`--mascot`); nothing
/// toggles it at runtime.
pub struct MascotPanel;

#[async_trait]
impl Plugin for MascotPanel {
    fn name(&self) -> &'static str {
        "tui-panel-mascot"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tui-modules", "tui-layout"]
    }
    fn description(&self) -> &'static str {
        "an animated mascot on a one-row strip at the top"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let mods = ctx.require::<ModulesSvc>().map_err(|e| e.to_string())?;
        let layout = ctx.require::<LayoutSvc>().map_err(|e| e.to_string())?;
        let view = Arc::new(Mounted::<status::Mascot>::new());
        let id = <status::Mascot as crate::module::View>::id();
        mods.add_view(view)?;

        let known: Vec<String> = mods.view_ids().into_iter().map(str::to_string).collect();
        layout
            .apply(
                &LayoutOp::Show {
                    module: id.to_string(),
                    side: Side::Top,
                    size: Some(1),
                },
                &known,
            )
            .map_err(|e| format!("{e:?}"))?;

        let m: Arc<Modules> = mods.clone();
        let l = layout.clone();
        let _ = ctx.effect(move || {
            let known: Vec<String> = m.view_ids().into_iter().map(str::to_string).collect();
            let _ = l.apply(
                &LayoutOp::Hide {
                    module: id.to_string(),
                },
                &known,
            );
            m.remove_view(id);
        });
        Ok(())
    }
}

/// The team: who this agent delegated to and what they are doing.
///
/// It places itself the way the mascot does, through `LayoutOp::Show` — but at
/// the bottom and with no size, which is the difference that matters. `Show`
/// sizes a bottom panel by what it asks for rather than by a fixed number of
/// cells, so the panel grows a line per member and shrinks back when the team
/// is stopped. A fixed strip would be four blank rows for the six turns before
/// anyone is delegated to.
pub struct TeamPanel;

#[async_trait]
impl Plugin for TeamPanel {
    fn name(&self) -> &'static str {
        "tui-panel-team"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tui-modules", "tui-layout"]
    }
    fn description(&self) -> &'static str {
        "the team strip: delegated members, what they are doing, what they last said"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let mods = ctx.require::<ModulesSvc>().map_err(|e| e.to_string())?;
        let layout = ctx.require::<LayoutSvc>().map_err(|e| e.to_string())?;
        let view = Arc::new(Mounted::<team::Team>::new());
        let id = <team::Team as crate::module::View>::id();
        mods.add_view(view)?;

        let known: Vec<String> = mods.view_ids().into_iter().map(str::to_string).collect();
        layout
            .apply(
                &LayoutOp::Show {
                    module: id.to_string(),
                    side: Side::Bottom,
                    size: None,
                },
                &known,
            )
            .map_err(|e| format!("{e:?}"))?;

        let m: Arc<Modules> = mods.clone();
        let l = layout.clone();
        let _ = ctx.effect(move || {
            let known: Vec<String> = m.view_ids().into_iter().map(str::to_string).collect();
            let _ = l.apply(
                &LayoutOp::Hide {
                    module: id.to_string(),
                },
                &known,
            );
            m.remove_view(id);
        });
        Ok(())
    }
}

/// The todo list: what the session is working through.
///
/// Mounted here, placed by `host::composer` — the same seam the live line and
/// the tip row use. It belongs in the composer, above the live line and against
/// the field, rather than at the bottom of the screen where it used to put
/// itself: the plan is part of what the current turn is doing, not a panel
/// about the session, and under the input box it was the last thing anyone
/// looked at.
///
/// It asks for `Hug(0)` and draws nothing until the model has planned
/// something, so a session that never calls `todowrite` pays no rows at all —
/// the flex closes up around it, exactly as it does for an idle live line.
pub struct TodoPanel;

#[async_trait]
impl Plugin for TodoPanel {
    fn name(&self) -> &'static str {
        "tui-panel-todo"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tui-modules"]
    }
    fn description(&self) -> &'static str {
        "the task list the model is working through, and what is left of it"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let mods = ctx.require::<ModulesSvc>().map_err(|e| e.to_string())?;
        let view = Arc::new(Mounted::<todo::Todo>::new());
        let id = <todo::Todo as crate::module::View>::id();
        mods.add_view(view)?;
        // Mount only. Where it goes is the composer's to write — `LayoutOp::Show`
        // has no side that means "above the field".
        let m: Arc<Modules> = mods.clone();
        let _ = ctx.effect(move || m.remove_view(id));
        Ok(())
    }
}

/// The panel for words typed while a turn was running and not yet handed to the
/// model.
///
/// Mount only, like the task list beside it: its place is the stream's tail
/// (`host::TAIL`), which no `Show` op names. Its rows come and go with
/// `Moment::steering` rather than with anything folded from the log — the log
/// has no such fact until the next round boundary — so there is no state to
/// keep and nothing to remove.
pub struct SteeringPanel;

#[async_trait]
impl Plugin for SteeringPanel {
    fn name(&self) -> &'static str {
        "tui-panel-steering"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tui-modules"]
    }
    fn description(&self) -> &'static str {
        "what you said while the model was working, until the model is handed it"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let mods = ctx.require::<ModulesSvc>().map_err(|e| e.to_string())?;
        let view = Arc::new(Mounted::<steering::Steering>::new());
        let id = <steering::Steering as crate::module::View>::id();
        mods.add_view(view)?;
        let m: Arc<Modules> = mods.clone();
        let _ = ctx.effect(move || m.remove_view(id));
        Ok(())
    }
}

/// The question panel: where a question is put, on the stream's tail.
///
/// Mount only, like the task list and the steering bars beside it: its place is
/// `host::TAIL`, which no `Show` op names.
///
/// A panel and not a modal any more. The modal was opened by the event loop out
/// of a seam (`tui-ask-view`); now a question is a module riding the tail — the
/// host was always asked for its height, so the question takes its rows the way
/// every other panel does, and there is no second rendering to keep in step with
/// it.
pub struct AskPanel;

#[async_trait]
impl Plugin for AskPanel {
    fn name(&self) -> &'static str {
        "tui-panel-ask"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tui-modules"]
    }
    fn description(&self) -> &'static str {
        "the question on screen: who is asking, what the call does, what each answer means"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let mods = ctx.require::<ModulesSvc>().map_err(|e| e.to_string())?;
        let view = Arc::new(Mounted::<ask::Ask>::new());
        let id = <ask::Ask as crate::module::View>::id();
        mods.add_view(view)?;
        let m: Arc<Modules> = mods.clone();
        let _ = ctx.effect(move || m.remove_view(id));
        Ok(())
    }
}

// ---- what this build calls itself -----------------------------------------

/// `name`, `licence`, and the mascot's art — a build's identity, as a row.
///
/// The point of the row is its config: a downstream build patches it in the
/// config tree and never touches Rust.
///
/// ```toml
/// [[patch]]
/// id = "tui-brand"
/// config = { name = "◆ LongCode", licence = "内部使用", mascot = { rows = [
///   "oo..oo", "..oo..",
/// ], palette = { o = 40 } } }
/// ```
///
/// `mascot = false` is a build with no art at all. Leaving a field out keeps
/// the shipped value for that field — a fork usually wants its own name and is
/// happy with everything else.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct BrandRowConfig {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    licence: Option<String>,
    #[serde(default)]
    mascot: Option<MascotConfig>,
}

/// Either art, or `false` for none.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum MascotConfig {
    None(bool),
    Art {
        rows: Vec<String>,
        /// Legend to 256-colour index. TOML keys are strings; a key that is not
        /// exactly one character is refused rather than silently truncated.
        palette: std::collections::BTreeMap<String, u8>,
    },
}

pub struct BrandRow;

#[async_trait]
impl Plugin for BrandRow {
    fn name(&self) -> &'static str {
        "tui-brand"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["tui-brand"]
    }
    fn description(&self) -> &'static str {
        "what this build calls itself: its name, its licence, its mascot"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        let row: BrandRowConfig = if config.is_null() {
            BrandRowConfig::default()
        } else {
            serde_json::from_value(config.clone()).map_err(|e| format!("bad config: {e}"))?
        };
        let shipped = crate::content::Brand::default();
        let mascot = match row.mascot {
            None => shipped.mascot,
            Some(MascotConfig::None(false)) => None,
            Some(MascotConfig::None(true)) => shipped.mascot,
            Some(MascotConfig::Art { rows, palette }) => {
                let mut legend = std::collections::BTreeMap::new();
                for (key, colour) in palette {
                    let mut chars = key.chars();
                    match (chars.next(), chars.next()) {
                        (Some(c), None) => {
                            legend.insert(c, colour);
                        }
                        _ => return Err(format!("palette key `{key}` is not a single character")),
                    }
                }
                Some(crate::content::Mascot {
                    rows,
                    palette: legend,
                })
            }
        };
        let brand = crate::content::Brand {
            name: row.name.unwrap_or(shipped.name),
            licence: row.licence.unwrap_or(shipped.licence),
            mascot,
        };
        let _ = ctx
            .provide::<crate::plugin::BrandSvc>(Arc::new(brand))
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

// ---- keys ----------------------------------------------------------------

/// The bindings this build ships, as a row.
///
/// The same shape as a command set: the registry is a slot the UI row provides,
/// this fills it, and unloading empties it again. What makes it worth a row
/// rather than a line in `assemble` is that a downstream build can drop it —
/// `[[remove]] id = "tui-keys-default"` — or mount its own after it and take
/// over the presses it names.
pub struct DefaultKeysRow;

#[async_trait]
impl Plugin for DefaultKeysRow {
    fn name(&self) -> &'static str {
        "tui-keys-default"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tui-keys"]
    }
    fn description(&self) -> &'static str {
        "the keys this build ships"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let keys = ctx
            .require::<crate::plugin::KeysSvc>()
            .map_err(|e| e.to_string())?;
        keys.add(&crate::keymap::Default_)?;
        let id = crate::keymap::Keymap::id(&crate::keymap::Default_);
        let k = keys.clone();
        let _ = ctx.effect(move || k.remove(id));
        Ok(())
    }
}

// ---- command sets --------------------------------------------------------

/// A command set that needs nothing but itself.
macro_rules! commands {
    ($plugin:ident, $row:literal, $set:expr, $about:literal) => {
        pub struct $plugin;

        #[async_trait]
        impl Plugin for $plugin {
            fn name(&self) -> &'static str {
                $row
            }
            fn inject(&self) -> &'static [&'static str] {
                &["tui-commands"]
            }
            fn description(&self) -> &'static str {
                $about
            }
            async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
                let all = ctx.require::<CommandsSvc>().map_err(|e| e.to_string())?;
                let set = Arc::new($set);
                let id = crate::command::CommandSet::id(&*set);
                all.add(set)?;
                let c: Arc<Commands> = all.clone();
                let _ = ctx.effect(move || c.remove(id));
                Ok(())
            }
        }
    };
}

commands!(
    ScreenCommandsRow,
    "tui-commands-screen",
    crate::commands::ScreenCommands,
    "clear, scroll, fold — the screen's own verbs"
);
commands!(
    SessionCommandsRow,
    "tui-commands-session",
    crate::commands::SessionCommands,
    "the conversation: compact it, look at it, start another, go back to one"
);
commands!(
    TakeAwayCommandsRow,
    "tui-commands-take-away",
    crate::commands::TakeAwayCommands,
    "copy, save — taking the conversation out of the terminal"
);

/// The agent's catalog commands. It holds the connection, because what it lists
/// is what the agent on screen was described as offering.
pub struct AgentCatalogCommandsRow;

#[async_trait]
impl Plugin for AgentCatalogCommandsRow {
    fn name(&self) -> &'static str {
        "tui-commands-agent"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tui-commands", "tui-agent-client"]
    }
    fn description(&self) -> &'static str {
        "the agent's own commands, as its description lists them — run by name through the connection"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let all = ctx.require::<CommandsSvc>().map_err(|e| e.to_string())?;
        let client = ctx
            .require::<crate::plugin::AgentClientSvc>()
            .map_err(|e| e.to_string())?;
        let set = Arc::new(crate::commands::AgentCatalogCommands { client });
        let id = crate::command::CommandSet::id(&*set);
        all.add(set)?;
        let c: Arc<Commands> = all.clone();
        let _ = ctx.effect(move || c.remove(id));
        Ok(())
    }
}

/// `/help` lists whatever else is mounted, so it holds the registry it is in.
///
/// Mount it last: it reads the registry at dispatch time rather than at mount
/// time, so order does not change what it lists — but a reader should still see
/// that it is the one row that knows about all the others.
pub struct HelpCommandsRow;

#[async_trait]
impl Plugin for HelpCommandsRow {
    fn name(&self) -> &'static str {
        "tui-commands-help"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tui-commands"]
    }
    fn description(&self) -> &'static str {
        "/help — whatever command rows this tree happens to have mounted"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let all = ctx.require::<CommandsSvc>().map_err(|e| e.to_string())?;
        let set = Arc::new(crate::commands::HelpCommands { all: all.clone() });
        let id = crate::command::CommandSet::id(&*set);
        all.add(set)?;
        let c: Arc<Commands> = all.clone();
        let _ = ctx.effect(move || c.remove(id));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_plexus::{Layer, Op};

    /// The rows `SCREEN` inserts, by plugin name.
    fn screen_rows() -> Vec<String> {
        Layer::from_toml(SCREEN)
            .expect("SCREEN must parse")
            .ops
            .into_iter()
            .filter_map(|op| match op {
                Op::Insert(entries) => Some(entries),
                _ => None,
            })
            .flatten()
            .map(|e| e.name)
            .collect()
    }

    /// Every row `SCREEN` names must exist in `catalog()`.
    ///
    /// The failure this prevents is the one that actually happened while this
    /// file was being written: a row named in a layer but never registered
    /// mounts as "unknown plugin" at startup — on a user's terminal, not in a
    /// test.
    #[test]
    fn the_screen_only_names_rows_this_crate_ships() {
        let shipped: Vec<&str> = catalog().iter().map(|p| p.name()).collect();
        let named = screen_rows();
        assert!(!named.is_empty(), "SCREEN must name some rows");
        for name in &named {
            assert!(
                shipped.contains(&name.as_str()),
                "`{name}` is in SCREEN but not in catalog(); it would fail to mount"
            );
        }
    }

    /// …and the other direction, so a row that is added but never mounted is
    /// visible as a choice rather than as an oversight.
    #[test]
    fn every_shipped_row_is_either_mounted_or_deliberately_not() {
        let named = screen_rows();
        for plugin in catalog() {
            assert!(
                named.contains(&plugin.name().to_string()),
                "`{}` ships but no SCREEN line mentions it — add the row (with \
                 `disabled = true` if it is opt-in) so the choice is written down",
                plugin.name()
            );
        }
    }

    #[test]
    fn no_two_rows_share_a_name() {
        let mut names: Vec<&str> = catalog().iter().map(|p| p.name()).collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total, "duplicate row name in catalog()");
    }
}
