//! What the agent knows about itself.
//!
//! A harness assembled at runtime has no fixed feature set, so an agent running
//! on it cannot answer "what am I made of" from anything baked in at compile
//! time. Two ways to fix that, and only one of them survives:
//!
//! * **Write the composition into the prompt.** The moment it is written down it
//!   is a second representation of the tree, and the copy is what drifts. This
//!   is the exact failure `seam_map` was built to avoid — "nothing here is
//!   hand-maintained, so nothing here can go stale".
//! * **Give the model a door to the live tree.** The answer is generated from
//!   the running objects on every call, so it cannot be out of date.
//!
//! This row does the second. The prompt fragment it contributes carries only
//! what is invariant *for the whole session* — the identity, the session id, the
//! log path — plus the one behavioural instruction that matters: when you don't
//! know what you are made of, call the tool instead of searching the repository
//! and guessing. Guessing is what an agent does when nobody gave it a door.

use std::sync::Arc;

use async_trait::async_trait;
use atomcode_kernel::tool::{RiskLevel, Tool, ToolContext, ToolResult};
use atomcode_plexus::{Context, Plugin};
use serde_json::{json, Value};

use crate::seams::{OperationsSvc, SessionPersistenceSvc, SessionSvc, ToolsSvc};

/// Describe a knob this row owns, and take the description away with the row.
///
/// The three-role convention applied to documentation: the row that implements
/// a behaviour is the only one that can describe it without the description
/// being a guess. A central FAQ would list capabilities that are not mounted
/// and miss ones that are — which is exactly the failure the agent was already
/// making on its own.
pub fn describes(ctx: &Context, topic: &str, rank: i32, text: impl Into<String>) {
    let Some(ops) = ctx.service::<OperationsSvc>() else {
        return;
    };
    ops.contribute(topic, rank, text.into());
    let topic = topic.to_string();
    let ops = ops.clone();
    let _ = ctx.effect(move || ops.remove(&topic));
}

/// Everything the tool reads, captured as live handles rather than as text.
///
/// Holding the `Context` is the point: `service_names()` is re-read on every
/// call, so a row mounted or dropped after this tool was built still shows up.
struct Introspect {
    ctx: Context,
}

impl Introspect {
    fn session(&self) -> String {
        let Some(log) = self.ctx.service::<SessionSvc>() else {
            return "session: no session log is mounted in this tree.".into();
        };
        let id = log.id().to_string();
        let where_ = self
            .ctx
            .service::<SessionPersistenceSvc>()
            .and_then(|store| store.location(&id))
            .unwrap_or_else(|| {
                "nowhere — no session-persistence row is mounted, so this \
                 session is in memory only and ends when the process does"
                    .into()
            });
        format!(
            "session id: {id}\nturn: {}\nevents logged so far: {}\nevent log: {where_}",
            log.current_turn(),
            log.len(),
        )
    }

    fn services(&self) -> String {
        let mut names = self.ctx.service_names();
        names.sort_unstable();
        format!(
            "The service slots filled and visible from this agent's realm \
             ({} of them). A slot is an address; the plugin behind it is a \
             value the config picked, so the presence of `llm` says a model \
             adapter is mounted, not which one.\n\n{}",
            names.len(),
            names
                .chunks(6)
                .map(|c| format!("  {}", c.join(", ")))
                .collect::<Vec<_>>()
                .join("\n")
        )
    }

    fn operations(&self) -> String {
        match self.ctx.service::<OperationsSvc>() {
            Some(ops) if !ops.ids().is_empty() => format!(
                "How to work this system. Each entry was written by the row that \
                 implements it, so nothing here describes a capability that is \
                 not mounted.\n\n{}",
                ops.render()
            ),
            _ => "No row has described a knob in this tree.".into(),
        }
    }

    fn settings(&self) -> String {
        // Rendered from the same catalog the config system edits through, so a
        // setting that is added, renamed or retired changes this answer without
        // anyone remembering to.
        let mut out = format!(
            "User settings live in `{}`. {} of them are safely editable; each \
             line is `id — label (aliases) : accepted values → when it takes \
             effect`.\n\nNote what is deliberately absent: model, provider, \
             account, endpoint and credentials are NOT in this catalog. Those \
             are picked per row in the running tree — see the `operations` \
             aspect for how this tree gets its model.\n",
            atomcode_config::Config::default_path().display(),
            atomcode_config::settings::SETTINGS.len(),
        );
        for spec in atomcode_config::settings::SETTINGS {
            let values = match spec.kind {
                atomcode_config::settings::SettingKind::Boolean => "true | false".to_string(),
                atomcode_config::settings::SettingKind::OptionalBoolean => {
                    "true | false | unset".to_string()
                }
                atomcode_config::settings::SettingKind::Integer { min, max } => {
                    format!("{min}..={max}")
                }
                atomcode_config::settings::SettingKind::Choice(options) => options.join(" | "),
                atomcode_config::settings::SettingKind::Text => "text".to_string(),
            };
            out.push_str(&format!(
                "\n  {} — {} / {}{} : {} → {:?}",
                spec.id,
                spec.label_en,
                spec.label_zh,
                if spec.aliases.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", spec.aliases.join(", "))
                },
                values,
                spec.apply,
            ));
        }
        out
    }

    fn tools(&self) -> String {
        let Some(toolbox) = self.ctx.service::<ToolsSvc>() else {
            return "tools: no tool catalog is mounted.".into();
        };
        let defs = toolbox.defs();
        if defs.is_empty() {
            return "tools: the catalog is mounted but empty.".into();
        }
        let mut out = format!("{} tools are registered right now:\n", defs.len());
        for def in defs {
            // One line each: the model already has the full schemas in its
            // request, so repeating them here would burn context to say
            // something it can already see.
            let first = def
                .description
                .lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("")
                .trim();
            out.push_str(&format!("  {} — {}\n", def.name, first));
        }
        out
    }
}

pub struct DescribeSelf {
    inner: Introspect,
}

#[async_trait]
impl Tool for DescribeSelf {
    fn name(&self) -> &str {
        "describe_self"
    }

    fn description(&self) -> &str {
        "Report how this agent is actually assembled and how to work it: the \
         session id and where its event log is written, which service slots are \
         filled, which tools are registered, how to change things (model, \
         memory, plugins, layout — `aspect: operations`), and the user-settings \
         catalog including language (`aspect: settings`). Read this instead of \
         guessing or searching the repository whenever you are asked what you \
         are, what you can do, how to change something, or where your own state \
         is kept — every answer is generated from the running system, so none of \
         it can be out of date."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "aspect": {
                    "type": "string",
                    "enum": ["session", "services", "tools", "operations", "settings", "all"],
                    "description": "session = which session this is and where its log is; services = what is mounted; tools = the live catalog; operations = how to change things (model, memory, plugins, layout); settings = the user-settings catalog including language. Defaults to everything but settings."
                }
            }
        })
    }

    fn risk(&self, _args: &str) -> RiskLevel {
        RiskLevel::Safe
    }

    fn read_only_hint(&self) -> bool {
        true
    }

    async fn execute(&self, args: &str, _ctx: &ToolContext) -> ToolResult {
        // A malformed argument reports everything rather than failing: the tool
        // exists to answer "what am I", and refusing to answer over a typo in an
        // optional field would be the one failure mode that matters here.
        let aspect = serde_json::from_str::<Value>(args)
            .ok()
            .and_then(|v| v.get("aspect")?.as_str().map(str::to_string))
            .unwrap_or_else(|| "all".into());

        let body = match aspect.as_str() {
            "session" => self.inner.session(),
            "services" => self.inner.services(),
            "tools" => self.inner.tools(),
            "operations" => self.inner.operations(),
            "settings" => self.inner.settings(),
            // `all` deliberately omits `settings`: it is a 26-row table that
            // answers a question nobody asked most of the time, and a default
            // that dumps everything trains the reader to skim.
            _ => format!(
                "{}\n\n{}\n\n{}\n\n{}",
                self.inner.session(),
                self.inner.services(),
                self.inner.tools(),
                self.inner.operations()
            ),
        };
        ToolResult {
            call_id: String::new(),
            content: body,
            is_error: false,
            images: Vec::new(),
        }
    }
}

pub struct SelfKnowledgePlugin;

#[async_trait]
impl Plugin for SelfKnowledgePlugin {
    fn name(&self) -> &'static str {
        "self-knowledge"
    }
    fn inject(&self) -> &'static [&'static str] {
        // The fragment is the irreducible half — a row that contributes nothing
        // to the prompt cannot tell the agent to stop guessing.
        &["system-prompt"]
    }
    fn uses(&self) -> &'static [&'static str] {
        // Everything the tool reports is read at call time and degrades to a
        // plain statement of absence, so none of it is a hard dependency. That
        // is what lets this row mount in an eval tree with no catalog and no
        // store and still be correct about having neither.
        &["tools", "sessions", "session-persistence", "operations"]
    }
    fn description(&self) -> &'static str {
        "tell the model what it is assembled from, from the live tree"
    }

    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        // Deliberately free of anything session-specific.
        //
        // The obvious version of this fragment names the session id, so the
        // agent can answer "who am I" with no round trip. That version is
        // wrong: a system prompt carrying a per-session value is a different
        // system prompt for every session, and cross-session prefix caching
        // dies with it. The prompt says what is true of every AtomCode session;
        // what is true of *this* one is a tool call away, and one tool call is
        // very much cheaper than a permanently unique prompt.
        //
        // `tests/self_knowledge.rs::the_prompt_stays_identical_across_sessions`
        // is what keeps this honest.
        const INVARIANT: &str = "\
You are AtomCode, an agent assembled at runtime from plugin rows on the plexus \
kernel — the coding opinion, if you have one, is itself just another row. There is no fixed feature set — only the rows the running tree \
happens to have mounted, which is why you cannot know what you are from \
anything you were trained on.

When you are asked what you are made of, what you can do, how to change \
something about yourself — the model, memory, plugins, the layout, the \
language, where a file lives — which session this is, or where your own state \
is kept, call `describe_self` rather than guessing or searching the repository \
for clues. Reading the source of a build is not the \
same as reading the tree that is running, and only the tool reports the tree \
that is running.";

        // Rank 3: after the persona, before per-tool guidance. What the agent
        // *is* should be established before what any one tool wants.
        super::tools::contribute_prompt(ctx, "self-knowledge", 3, INVARIANT);

        describes(
            ctx,
            "composition",
            1,
            "HOW THIS SYSTEM IS PUT TOGETHER. Every capability is a *row* in a \
             config tree — model adapter, each tool group, memory, recall, \
             approval policy, the UI. A row names a plugin from the build's \
             catalog; the tree says which rows run and in what order.\n\
             To add one: `[[insert]] name = \"<plugin>\"` (optionally with \
             `config = {…}`). To retune one: `[[patch]] id = \"<row>\" \
             config = {…}` — note this REPLACES that row's config rather than \
             merging into it. To drop one: `[[remove]] id = \"<row>\"`, or \
             `[[patch]] … disabled = true`.\n\
             Third-party tools arrive as MCP servers rather than as compiled \
             plugins — see the `mcp` entry if that row is mounted. Call \
             `describe_self` with `aspect: services` for what is mounted right \
             now.",
        );

        // The tool half is optional so this row can mount in a tree with no
        // catalog — an eval harness, say — and still say who it is.
        if let Some(toolbox) = ctx.service::<ToolsSvc>() {
            let tool = Arc::new(DescribeSelf {
                inner: Introspect { ctx: ctx.clone() },
            });
            toolbox.register(tool)?;
            let toolbox = toolbox.clone();
            let _ = ctx.effect(move || toolbox.unregister("describe_self"));
        }
        Ok(())
    }
}

// ---- the other half: what the *repository* says --------------------------

/// The project's own standing instructions, as a prompt fragment.
///
/// `describe_self` answers "what am I made of" from the live tree. It cannot
/// answer "what does this repository expect of me" — that is not a runtime fact,
/// it is a file the repository maintains, and an agent that never reads it will
/// happily re-derive house rules from scratch every session.
///
/// The loader already existed in L1 (`AGENTS.md`, `CLAUDE.md`, `.atomcode.md`,
/// in that precedence). Nothing here mounted it, which is why an agent running
/// inside a repository that documents its own gates was still guessing at them.
///
/// Unlike the identity fragment this one is deliberately *not* invariant — it is
/// stable per project, which is the granularity a prompt cache keys on anyway.
pub struct ProjectInstructionsPlugin;

#[derive(Debug, serde::Deserialize, Default)]
struct InstructionsRow {
    /// Workspace root the project tier resolves against.
    #[serde(default)]
    project_root: Option<String>,
    /// Config root the global tier resolves against (`~/.atomcode`).
    #[serde(default)]
    home: Option<String>,
}

#[async_trait]
impl Plugin for ProjectInstructionsPlugin {
    fn name(&self) -> &'static str {
        "project-instructions"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["system-prompt"]
    }
    fn description(&self) -> &'static str {
        "read AGENTS.md / CLAUDE.md / .atomcode.md into the prompt"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        let row: InstructionsRow = if config.is_null() {
            InstructionsRow::default()
        } else {
            serde_json::from_value(config.clone()).map_err(|e| format!("bad config: {e}"))?
        };
        let project = row
            .project_root
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        let home = row
            .home
            .map(std::path::PathBuf::from)
            .unwrap_or_else(crate::home);

        let text = atomcode_capabilities::instructions::render_instructions(&home, &project);
        // A repository with no instructions file contributes nothing rather than
        // an empty header: a fragment that says nothing still costs a blank line
        // in every request, and `ids()` would report a contribution that is not
        // one.
        if !text.trim().is_empty() {
            super::tools::contribute_prompt(ctx, "project-instructions", 1, &text);
        }
        Ok(())
    }
}
