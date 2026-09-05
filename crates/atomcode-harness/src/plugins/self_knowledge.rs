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

use crate::seams::{SessionPersistenceSvc, SessionSvc, ToolsSvc};

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
        "Report how this agent is actually assembled right now: its session id \
         and where its event log is written, which service slots are filled, \
         and which tools are registered. Read this instead of guessing or \
         searching the repository when you are asked what you are, what you can \
         do, or where your own state is kept — the answer is generated from the \
         running system, so it is never out of date."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "aspect": {
                    "type": "string",
                    "enum": ["session", "services", "tools", "all"],
                    "description": "Which part to report. Defaults to all."
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
            _ => format!(
                "{}\n\n{}\n\n{}",
                self.inner.session(),
                self.inner.services(),
                self.inner.tools()
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
        &["tools", "sessions", "session-persistence"]
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

When you are asked what you are made of, what you can do, which session this \
is, or where your own state is kept, call `describe_self` rather than guessing \
or searching the repository for clues. Reading the source of a build is not the \
same as reading the tree that is running, and only the tool reports the tree \
that is running.";

        // Rank 3: after the persona, before per-tool guidance. What the agent
        // *is* should be established before what any one tool wants.
        super::tools::contribute_prompt(ctx, "self-knowledge", 3, INVARIANT);

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
