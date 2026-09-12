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
//!   screen through the same `LayoutOp::Show` a keystroke, a slash command and
//!   the model all go through, rather than through a special case in `assemble`.
//! * `--audit` sees every panel, so "mounted but never drawn" and "named by a
//!   layout but never mounted" become findings instead of surprises.
//! * A third crate adds a panel with a `View` impl and a row. No edit here.

use std::sync::Arc;

use async_trait::async_trait;
use atomcode_plexus::{Context, Plugin};
use serde_json::Value;

use crate::command::Commands;
use crate::layout::{LayoutOp, Side};
use crate::module::{Modules, Mounted, Producer};
use crate::modules::{input, live, status, team, tip, todo, transcript};
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

# How a question is drawn. Remove this row and questions still arrive, still
# answer and still record — as plain lines at the foot of the stream. That is
# the fallback this row improves on, not a branch it replaces.
[[insert]]
name = "tui-ask-card"

[[insert]]
name = "tui-commands-screen"

[[insert]]
name = "tui-commands-session"

[[insert]]
name = "tui-commands-tree"

[[insert]]
name = "tui-commands-layout"

# Last, because it lists the others.
[[insert]]
name = "tui-commands-help"
"#;

/// Every row this crate ships, for a registry to take in one call.
///
/// A launcher that listed them by hand would silently miss the next one added.
pub fn catalog() -> Vec<std::sync::Arc<dyn Plugin>> {
    vec![
        Arc::new(TranscriptPanel),
        Arc::new(StatusPanel),
        Arc::new(LivePanel),
        Arc::new(TipPanel),
        Arc::new(InputPanel),
        Arc::new(MascotPanel),
        Arc::new(TeamPanel),
        Arc::new(TodoPanel),
        Arc::new(AskCardRow),
        Arc::new(ScreenCommandsRow),
        Arc::new(SessionCommandsRow),
        Arc::new(TreeCommandsRow),
        Arc::new(LayoutCommandsRow),
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
/// It mounts a view *and* puts itself on screen, and it does the second half
/// through `LayoutOp::Show` — the same op a keystroke, a `/show mascot` and the
/// model's `adjust_layout` all produce. Before this it was a branch inside
/// `assemble` that built a different region tree, which meant the one thing the
/// layout vocabulary existed for was the one thing that bypassed it.
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
/// Placed the way the team strip is — bottom, no size, so it is as tall as the
/// plan and no taller. That matters more here than there: the panel asks for
/// `Hug(0)` and draws nothing until the model has planned something, so a
/// session that never calls `todowrite` pays a row, not a panel of "no tasks".
pub struct TodoPanel;

#[async_trait]
impl Plugin for TodoPanel {
    fn name(&self) -> &'static str {
        "tui-panel-todo"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tui-modules", "tui-layout"]
    }
    fn description(&self) -> &'static str {
        "the task list the model is working through, and what is left of it"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let mods = ctx.require::<ModulesSvc>().map_err(|e| e.to_string())?;
        let layout = ctx.require::<LayoutSvc>().map_err(|e| e.to_string())?;
        let view = Arc::new(Mounted::<todo::Todo>::new());
        let id = <todo::Todo as crate::module::View>::id();
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

/// The approval card: the modal a question is put in.
///
/// Not a panel — it takes no room until something asks — and not a branch in
/// the host either. The host hands the question to whoever fills
/// `tui-ask-view`; this row is who that is by default, and a product with a
/// different idea of what an approval should look like replaces the row rather
/// than patching the loop.
pub struct AskCardRow;

#[async_trait]
impl Plugin for AskCardRow {
    fn name(&self) -> &'static str {
        "tui-ask-card"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["tui-ask-view"]
    }
    fn description(&self) -> &'static str {
        "draw a question as a card: who is asking, what the call does, what each answer means"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let _ = ctx
            .provide::<crate::plugin::AskViewSvc>(Arc::new(crate::ask::Card))
            .map_err(|e| e.to_string())?;
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
    "what this session is and how to end it"
);
commands!(
    TreeCommandsRow,
    "tui-commands-tree",
    crate::commands::TreeCommands,
    "inspect and reconfigure the running plugin tree from the screen"
);

/// Layout commands need the layout and the module list, so they are written out
/// rather than generated — the dependencies are the interesting part.
pub struct LayoutCommandsRow;

#[async_trait]
impl Plugin for LayoutCommandsRow {
    fn name(&self) -> &'static str {
        "tui-commands-layout"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tui-commands", "tui-layout", "tui-modules"]
    }
    fn description(&self) -> &'static str {
        "/show, /hide, /swap, /preset — the second of the three ways into a layout"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let all = ctx.require::<CommandsSvc>().map_err(|e| e.to_string())?;
        let set = Arc::new(crate::commands::LayoutCommands {
            layout: ctx.require::<LayoutSvc>().map_err(|e| e.to_string())?,
            modules: ctx.require::<ModulesSvc>().map_err(|e| e.to_string())?,
        });
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
