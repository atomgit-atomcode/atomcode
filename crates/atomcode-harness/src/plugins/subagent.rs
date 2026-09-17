//! Subagents: the use case both composability axes exist for.
//!
//! A subagent is a second agent in the same process with **its own world** —
//! its own tool catalog, potentially its own filesystem, its own policy — and
//! it must not be able to escape the constraints its parent runs under. That is
//! exactly the pair of properties realms provide:
//!
//! - **spatially**, the child mounts into a realm layered over the parent's, so
//!   the tools and listeners it installs are invisible to the parent and to its
//!   siblings;
//! - **temporally**, the child's whole footprint is one fiber, so finishing (or
//!   failing) reverts every registration it made, with nothing to clean up by
//!   hand.
//!
//! The escape-proofing is not a check this module performs — it falls out of
//! realm visibility running one way. A credential gate or an approval policy
//! installed at the root is still in the child's chain; nothing the child
//! installs reaches back up.

use std::sync::Arc;

use async_trait::async_trait;
use atomcode_kernel::tool::{RiskLevel, Tool, ToolContext, ToolResult};
use atomcode_plexus::{Context, Disposable, Plugin};
use serde::Deserialize;
use serde_json::{json, Value};

use atomcode_kernel::provider::ReasoningEffort;
use atomcode_plexus::{Next, Waterfall};

use crate::events::{
    AgentRequest, ModelRequest, ModelResponse, RequestError, TurnProgress, TurnStopping,
};
use crate::seams::{
    AgentLoopSvc, AgentsSvc, SessionSvc, StopReason, SubagentOutcome, Subagents, SubagentsSvc,
    SystemPromptSvc, ToolBox, ToolsSvc,
};

use super::tools::{contribute_prompt, mount};

/// Ends a delegated turn after its own budget, independent of the parent's.
pub(crate) struct ChildRoundCap {
    pub(crate) max_steps: u32,
}

#[async_trait]
impl atomcode_plexus::Listener<TurnStopping> for ChildRoundCap {
    async fn call(&self, progress: &TurnProgress) -> Option<StopReason> {
        // Zero is "no budget of its own", the way the product's `[subagent]
        // max_rounds` has always read it — not a budget of nothing.
        (self.max_steps > 0 && progress.rounds >= self.max_steps).then_some(StopReason::MaxRounds)
    }
}

/// The provider a delegated child should run on, or `None` for "inherit".
///
/// One place, because `task` and `team` must answer this identically — two
/// resolutions of "which model" is how a fleet ends up half on the model the
/// person picked and half on something else.
///
/// Every failure names what IS on offer. A model that guessed an id gets the
/// list back and can pick again; a model that guessed and was silently given
/// the default would never learn.
pub(crate) async fn resolve_child_model(
    ctx: &Context,
    model: Option<&str>,
    who: crate::seams::Chose,
) -> Result<Option<Arc<dyn atomcode_kernel::provider::LlmProvider>>, String> {
    let Some(id) = model.map(str::trim).filter(|m| !m.is_empty()) else {
        return Ok(None);
    };
    let Some(models) = ctx.service::<crate::seams::ModelsSvc>() else {
        return Err(
            "this tree has no model catalog, so `model` cannot be honoured; omit it to run \
             on this conversation's model"
                .into(),
        );
    };
    let offered = crate::seams::choices(models.as_ref(), who);
    if !offered.iter().any(|m| m.id == id) {
        let names = offered
            .iter()
            .map(|m| m.id.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(match (who, names.is_empty()) {
            // Written in a file by a person: the only thing that can be wrong
            // is that no such model exists, so say that rather than lecturing
            // them about accounts they own.
            (crate::seams::Chose::Person, true) => {
                format!("`{id}` is not a model this host knows about, and neither is anything else")
            }
            (crate::seams::Chose::Person, false) => {
                format!("`{id}` is not a model this host knows about; it has: {names}")
            }
            (crate::seams::Chose::Model, true) => {
                format!("`{id}` is not available to delegate to, and neither is anything else")
            }
            (crate::seams::Chose::Model, false) => {
                format!("`{id}` is not available to delegate to; you may use: {names}")
            }
        });
    }
    models.provider(&id).await.map(Some)
}

/// The thinking level one delegated child runs at, on its own realm.
///
/// Scoped to the child for the same reason its tools are: the level is a fact
/// about the job that child was given, so two children delegated at different
/// levels must not share an answer. Registered with `prepend`, because this is
/// more specific than the session's `reasoning-effort` row — which still fills
/// in for any child that asks for nothing.
/// Writes one role's thinking tier onto every request that member makes.
///
/// Deliberately narrow: it sets the level and touches nothing else, so the
/// session-wide row, the provider's own `thinking_type`, and every other
/// per-call option keep working underneath it. Registered on the member's realm
/// with `prepend`, because a role that names a tier knows better than the
/// session default it would otherwise inherit.
pub(crate) struct RoleEffort {
    pub(crate) effort: ReasoningEffort,
}

impl RoleEffort {
    /// Put this level on every request `session` makes, and say so when that
    /// session is described. One registration for both, so the level an agent
    /// is described with is the one its requests carry.
    pub(crate) fn mount(self, realm: &Context, session: String) -> Vec<Disposable> {
        let effort = self.effort;
        vec![
            realm.on_waterfall::<AgentRequest>(Arc::new(self), true),
            realm.on_emit::<crate::events::DescribeAgent>(
                move |describing: &crate::events::Describing| {
                    let mut description =
                        describing.description.lock().expect("description poisoned");
                    if description.session == session {
                        description.reasoning_effort = Some(effort);
                    }
                },
            ),
        ]
    }
}

#[async_trait]
impl Waterfall<AgentRequest> for RoleEffort {
    async fn handle(
        &self,
        req: &mut ModelRequest,
        next: Next<'_, AgentRequest>,
    ) -> Result<ModelResponse, RequestError> {
        req.options.reasoning_effort = Some(self.effort);
        next.run(req).await
    }
}

/// The thinking level a delegated child should run at, validated against what
/// the model it will run on actually accepts.
///
/// Two checks, and the second is the one worth having: `max` is a real level
/// and a model that only advertises `low`/`high` will drop it on the floor at
/// the adapter. A knob that is silently discarded is worse than one that is
/// refused, because the person reads the transcript and concludes the level did
/// not help.
pub(crate) fn resolve_child_effort(
    models: Option<&std::sync::Arc<dyn crate::seams::Models>>,
    on: Option<&str>,
    effort: Option<&str>,
) -> Result<Option<atomcode_kernel::provider::ReasoningEffort>, String> {
    let Some(asked) = effort.map(str::trim).filter(|e| !e.is_empty()) else {
        return Ok(None);
    };
    let level =
        atomcode_kernel::provider::ReasoningEffort::from_config(Some(asked)).ok_or_else(|| {
            format!(
                "`{asked}` is not a thinking level; use one of: {}",
                crate::REASONING_EFFORT_LEVELS.join(", ")
            )
        })?;
    // Which model this will run on: the one named, else the conversation's.
    let target = on
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .map(str::to_string)
        .or_else(|| models.and_then(|m| m.current()));
    if let (Some(models), Some(target)) = (models, target) {
        if let Some(info) = models.list().into_iter().find(|m| m.id == target) {
            // An empty list means the deployment said nothing, not that the
            // model accepts nothing — the same "silence is not a no" rule the
            // rest of this catalog runs on.
            if !info.effort_levels.is_empty()
                && !info.effort_levels.iter().any(|l| l == level.as_str())
            {
                return Err(format!(
                    "`{target}` does not take `{asked}`; it accepts: {}",
                    info.effort_levels.join(", ")
                ));
            }
        }
    }
    Ok(Some(level))
}

/// Runs a child agent in an isolated realm of the same process.
struct InProcessSubagents {
    ctx: Context,
    /// Tools a child may use. A subset of the parent's catalog, resolved by
    /// name at spawn time so a tool unmounted since is simply not offered.
    allowed_tools: Vec<String>,
    max_rounds: u32,
}

#[async_trait]
impl Subagents for InProcessSubagents {
    fn describe(&self) -> String {
        // Resolved against the LIVE catalog, exactly as `spawn` resolves it.
        // Reporting the configured list instead named tools the tree does not
        // have — `web_search` in a tree with no `tool-web` row — and this string
        // is what `describe_self` hands the model when it asks what delegating
        // would buy it. A description of a capability has to be filtered by the
        // same thing the capability is.
        let mounted = self.ctx.service::<ToolsSvc>();
        let available: Vec<&str> = self
            .allowed_tools
            .iter()
            .filter(|name| mounted.as_ref().is_some_and(|t| t.get(name).is_some()))
            .map(String::as_str)
            .collect();
        format!(
            "in-process, isolated realm, tools: {}",
            available.join(", ")
        )
    }

    async fn spawn(&self, work: crate::seams::Delegation<'_>) -> SubagentOutcome {
        // Both resolved before the child exists: a bad id, or a level this model
        // will not honour, must fail the tool call — not leave a half-created
        // agent behind, and certainly not run anyway at a level nobody asked for.
        let catalog = self.ctx.service::<crate::seams::ModelsSvc>();
        let effort = match resolve_child_effort(catalog.as_ref(), work.model, work.effort) {
            Ok(effort) => effort,
            Err(e) => return SubagentOutcome::failed(e),
        };
        // The model wrote this argument this turn, so it is held to what it may
        // pick unprompted. A person naming one goes through `team`'s role files.
        let model =
            match resolve_child_model(&self.ctx, work.model, crate::seams::Chose::Model).await {
                Ok(model) => model,
                Err(e) => return SubagentOutcome::failed(e),
            };
        let (task, instructions) = (work.task, work.instructions);
        let (Some(agents), Some(driver), Some(parent_tools)) = (
            self.ctx.service::<AgentsSvc>(),
            self.ctx.service::<AgentLoopSvc>(),
            self.ctx.service::<ToolsSvc>(),
        ) else {
            return SubagentOutcome::failed("subagents need `agents`, `agent-loop` and `tools`");
        };

        // A real agent, in a realm of its own, visible in the registry — a UI
        // watching delegated work sees it the same way it sees any other agent.
        let restricted = Arc::new(ToolBox::new());
        for name in &self.allowed_tools {
            if let Some(tool) = parent_tools.get(name) {
                if let Err(e) = restricted.register(tool) {
                    return SubagentOutcome::failed(e);
                }
            }
        }
        let prompts = Arc::new(crate::seams::PromptRegistry::new());
        prompts.contribute("subagent", 0, instructions);

        // Its own conversation, its own tools and its own prompt, composed
        // before anyone can see it. The parent's session is the one whose turn
        // this tool call is running in. Not persisted: a delegated child's
        // transcript is the parent's business, not a session of its own.
        let parent = crate::agent::current()
            .and_then(|c| c.service::<SessionSvc>())
            .map(|log| log.id().to_string());
        let child_session = format!("sub-{}", crate::agent::mint_session_id());
        let delegated_llm = self.ctx.service::<crate::seams::DelegatedLlmSvc>();
        let mut req = crate::agent::CreateAgent::new()
            .id(child_session.clone())
            .persist(false)
            .setup(Box::new(move |realm: &Context| {
                let mut held = vec![
                    realm
                        .provide::<ToolsSvc>(restricted)
                        .map_err(|e| e.to_string())?,
                    // Marks it delegated, for `delegation-bounds`: a child reads,
                    // and its tools could write nowhere but its workspace.
                    realm
                        .provide::<crate::seams::DelegationLaneSvc>(Arc::new(
                            crate::seams::DelegationLane {
                                scopes: vec!["**".to_string()],
                            },
                        ))
                        .map_err(|e| e.to_string())?,
                    realm
                        .provide::<SystemPromptSvc>(prompts)
                        .map_err(|e| e.to_string())?,
                ];
                // The child's own `llm`, on its own realm: lookup walks up, so
                // the parent keeps the model it had and a sibling delegated
                // elsewhere is unaffected.
                // A named model, or — inheriting the conversation's — the
                // host's delegated one, when it keeps a child's spend apart.
                if let Some(model) = model.clone().or_else(|| delegated_llm.clone()) {
                    held.push(
                        realm
                            .provide::<crate::seams::LlmSvc>(model)
                            .map_err(|e| e.to_string())?,
                    );
                }
                if let Some(effort) = effort {
                    held.extend(RoleEffort { effort }.mount(realm, child_session));
                }
                Ok(held)
            }));
        if let Some(parent) = parent {
            req = req.parent(parent);
        }
        let child = match agents.create(&self.ctx, req).await {
            Ok(child) => child,
            Err(e) => return SubagentOutcome::failed(e),
        };
        let log = child.session();

        let round_cap = child
            .ctx()
            .on_serial::<TurnStopping>(Arc::new(ChildRoundCap {
                max_steps: self.max_rounds,
            }));

        // The parent whose tool call this is. Its turn being stopped stops the
        // child's: the delegation is part of that turn (`docs/adr/0023` §9).
        let parent_turn = crate::agent::current()
            .and_then(|ctx| ctx.service::<SessionSvc>())
            .and_then(|log| agents.by_session(log.id()))
            .map(|parent| parent.cancel_token());

        // Driven like every other agent: the task goes in as a message, and the
        // delegation lasts until the turn it starts has ended.
        let (ended_tx, ended_rx) = tokio::sync::oneshot::channel::<()>();
        let ended_tx = std::sync::Mutex::new(Some(ended_tx));
        let watched = child.session_id().to_string();
        // On the tree, not the child's realm: a fact is announced where the loop
        // commits it, and visibility only runs upward.
        let ending = self.ctx.on_emit::<crate::events::SessionEventCommitted>(
            move |committed: &crate::session::Committed| {
                if committed.session == watched
                    && matches!(
                        committed.event,
                        crate::session::SessionEvent::TurnEnd { .. }
                    )
                {
                    if let Some(tx) = ended_tx.lock().expect("ended poisoned").take() {
                        let _ = tx.send(());
                    }
                }
            },
        );
        let driven = super::handle::drive(&self.ctx, child.clone());
        let _ = driven
            .handle
            .commands
            .send(atomcode_kernel::event::AgentCommand::SendMessage {
                text: task.to_string(),
                images: Vec::new(),
            });
        let _ = driver;
        let mut ended_rx = ended_rx;
        tokio::select! {
            _ = &mut ended_rx => {}
            _ = async {
                match &parent_turn {
                    Some(token) => token.cancelled().await,
                    None => std::future::pending().await,
                }
            } => {
                child.cancel();
                let _ = ended_rx.await;
            }
        }
        ending.dispose();
        let super::handle::Driven { handle, done, .. } = driven;
        drop(handle);
        let _ = done.await;

        round_cap.dispose();
        let outcome = turn_outcome(&log.events());
        // Removing the agent tears down what was mounted for it alone.
        agents.remove(child.id());

        SubagentOutcome {
            transcript_len: log.len(),
            ..outcome
        }
    }
}

/// What the child's one turn came to, read off its log: how it ended, what it
/// last said, how many steps and calls it took.
fn turn_outcome(events: &[crate::session::LoggedEvent]) -> SubagentOutcome {
    use crate::session::SessionEvent;
    let mut outcome = SubagentOutcome {
        text: String::new(),
        rounds: 0,
        tool_calls: 0,
        stop: StopReason::Stopped,
        error: None,
        transcript_len: events.len(),
    };
    for logged in events {
        match &logged.event {
            SessionEvent::StepStart { .. } => outcome.rounds += 1,
            SessionEvent::AssistantMessage {
                text, tool_calls, ..
            } => {
                outcome.tool_calls += tool_calls.len() as u32;
                if !text.is_empty() {
                    outcome.text = text.clone();
                }
            }
            SessionEvent::TurnEnd { stop, error, .. } => {
                outcome.stop = *stop;
                outcome.error = error.clone();
            }
            _ => {}
        }
    }
    outcome
}

// ---- the model-facing tool ----------------------------------------------

#[derive(Deserialize)]
struct TaskArgs {
    /// What the child should accomplish.
    task: String,
    /// Optional extra standing instructions for the child.
    #[serde(default)]
    instructions: Option<String>,
    /// A selection id from the model catalog. Absent ⇒ this conversation's model.
    #[serde(default)]
    model: Option<String>,
    /// How hard the child should think. Absent ⇒ whatever the tree is set to.
    #[serde(default)]
    effort: Option<String>,
}

const DEFAULT_INSTRUCTIONS: &str = "\
You are a subagent handling one delegated task. You have a reduced tool set and no \
ability to change the parent's state. Investigate, do the work the task describes, and \
finish with a compact report of what you found or changed. Do not ask questions — you \
have no one to ask.";

struct TaskTool {
    ctx: Context,
    /// Every tool a child may be handed only reads. Then delegating is reading
    /// too, and asking about it would be asking about a search.
    read_only: bool,
}

#[async_trait]
impl Tool for TaskTool {
    fn name(&self) -> &str {
        "task"
    }
    fn description(&self) -> &str {
        "Delegate a self-contained task to a subagent with its own reduced tool set. Use it \
         for work that would otherwise flood this conversation with intermediate output — a \
         broad search, a survey of unfamiliar code. The subagent cannot ask you questions, so \
         state the task completely."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task": { "type": "string", "description": "The complete task, stated so it needs no follow-up" },
                "instructions": { "type": "string", "description": "Extra standing instructions for the subagent" },
                "model": { "type": "string", "description": "Run this subagent on a different model: a selection id from `describe_self(aspect=\"models\")`. Omit to use this conversation's model." },
                "effort": { "type": "string", "description": "How hard the subagent should think, for a model that takes the knob — the levels each model accepts are in `describe_self(aspect=\"models\")`. Omit to leave this conversation's setting in charge." }
            },
            "required": ["task"]
        })
    }
    /// The child runs under the same root policy, but it does run real tools,
    /// so the call itself is a decision worth gating.
    fn risk(&self, _args: &str) -> RiskLevel {
        if self.read_only {
            RiskLevel::Safe
        } else {
            RiskLevel::Risky
        }
    }
    fn always_grant_scope(&self, _args: &str) -> String {
        "task".into()
    }

    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolResult {
        let args: TaskArgs = match serde_json::from_str(args) {
            Ok(args) => args,
            Err(e) => {
                return ToolResult {
                    call_id: String::new(),
                    content: format!("invalid arguments: {e}"),
                    is_error: true,
                    images: vec![],
                }
            }
        };
        let Some(subagents) = self.ctx.service::<SubagentsSvc>() else {
            return ToolResult {
                call_id: String::new(),
                content: "no `subagents` provider is mounted".into(),
                is_error: true,
                images: vec![],
            };
        };
        let instructions = args.instructions.as_deref().unwrap_or(DEFAULT_INSTRUCTIONS);
        // A line when the child starts and one when it ends, on this call: the
        // only word a front end without the child's log gets of delegated work.
        ctx.progress.emit(format!(
            "↻ {}",
            args.task.lines().next().unwrap_or_default()
        ));
        let outcome = subagents
            .spawn(crate::seams::Delegation {
                task: &args.task,
                instructions,
                model: args.model.as_deref(),
                effort: args.effort.as_deref(),
            })
            .await;
        ctx.progress.emit(match (&outcome.error, outcome.stop) {
            (Some(error), _) => format!("✗ failed · {error}"),
            (None, StopReason::Stopped) => "✓ done".to_string(),
            (None, stop) => format!("✗ {stop:?}"),
        });
        ToolResult {
            call_id: String::new(),
            content: outcome.report(),
            is_error: outcome.error.is_some(),
            images: vec![],
        }
    }
}

#[derive(Debug, Deserialize)]
struct SubagentRow {
    /// Tools a child may use. Deliberately explicit rather than "everything
    /// minus a deny list": a tool added later should not silently widen what
    /// delegated work can do.
    #[serde(default = "default_tools")]
    allowed_tools: Vec<String>,
    #[serde(default = "default_rounds")]
    max_rounds: u32,
}

impl Default for SubagentRow {
    fn default() -> Self {
        Self {
            allowed_tools: default_tools(),
            max_rounds: default_rounds(),
        }
    }
}

fn default_tools() -> Vec<String> {
    crate::plugins::team::EXPLORE_TOOLS
        .iter()
        .map(|s| s.to_string())
        .collect()
}

fn default_rounds() -> u32 {
    12
}

pub struct SubagentPlugin;

#[async_trait]
impl Plugin for SubagentPlugin {
    fn name(&self) -> &'static str {
        "subagent-in-process"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tools", "llm", "agents"]
    }
    fn uses(&self) -> &'static [&'static str] {
        // Read at spawn time to derive the child's world from the parent's.
        &["subagents", "agent-loop", "system-prompt"]
    }
    fn provides(&self) -> &'static [&'static str] {
        &["subagents"]
    }
    fn description(&self) -> &'static str {
        "run a child agent in an isolated realm, with a reduced tool set"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        let row: SubagentRow = if config.is_null() {
            SubagentRow::default()
        } else {
            serde_json::from_value(config.clone()).map_err(|e| format!("bad config: {e}"))?
        };
        let _ = ctx
            .provide::<SubagentsSvc>(Arc::new(InProcessSubagents {
                ctx: ctx.clone(),
                allowed_tools: row.allowed_tools.clone(),
                max_rounds: row.max_rounds,
            }))
            .map_err(|e| e.to_string())?;
        mount(
            ctx,
            vec![Arc::new(TaskTool {
                ctx: ctx.clone(),
                read_only: row
                    .allowed_tools
                    .iter()
                    .all(|tool| crate::plugins::team::EXPLORE_TOOLS.contains(&tool.as_str())),
            }) as Arc<dyn Tool>],
        )?;
        // This row's guidance for this row's tool, and nothing else: the rules a parameter list
        // cannot carry — when NOT to delegate, and how to run several at once without them
        // colliding. It used to sit in the coding persona, which described the chain assembly's
        // `task` (`subagent_type`/`difficulty`) in the same breath, so the model was told about
        // a tool with different parameters than the one it actually had.
        contribute_prompt(
            ctx,
            "subagent",
            57,
            &format!(
                "`task` delegates a self-contained job to a subagent with a reduced tool set \
                 (the ones this tree actually mounts, out of: {}). Use it for work whose \
                 intermediate output you do not need to see — a broad read-only sweep, or \
                 several independent subtasks at once. Not for a single read or grep you can do \
                 here in one call: a lone subagent costs a slow extra model round for no gain. \
                 State each task completely, and when several run at once give them \
                 non-overlapping scopes — two of them editing one file collide. What comes back \
                 is a claim: weigh it, verify what matters.",
                row.allowed_tools.join(", ")
            ),
        );
        Ok(())
    }
}

// ---- telling the model the catalog exists --------------------------------

/// One line in the system prompt, and not one word more.
///
/// The catalog itself is answered on demand (`describe_self(aspect="models")`)
/// rather than inlined, for two reasons that point the same way:
///
/// 1. **it changes**. A login, a `/model`, an edited config — the list is not a
///    fact about the build, it is a fact about right now. Inlined, it would be
///    stale between the moment it was rendered and the moment it was read.
/// 2. **the prompt is a cache prefix**. A fragment that moved with the catalog
///    would invalidate the cached system prefix every time a model was added or
///    a plan changed. This one carries no count, no names, no ordering —
///    nothing that can move — so the prefix survives a catalog that does not.
///
/// Which is why there is a criterion in `tests/subagent.rs` asserting this
/// fragment renders byte-identically against two different catalogs. A number
/// in this string is the easiest possible regression and the hardest to notice.
///
/// It names no tool either: which delegation tools take a `model` id is in each
/// tool's own description. This line once said `task` and `team` both did, and
/// an assembly whose `team` takes none sent that to the model on every request.
const CATALOG_POINTER: &str = "Work can be delegated to a model other than this one, where a delegation tool takes a `model` id. Call `describe_self(aspect=\"models\")` for what is on offer right now — the list changes with logins and model switches, so read it when you need it rather than assuming.";

/// The catalog as the agent reads it, rendered at the moment of asking: a login,
/// a `/model` or an edited config changes it mid-session.
fn describe_catalog(models: &dyn crate::seams::Models) -> String {
    let current = models.current();
    let offered = crate::seams::delegatable(models);
    if offered.is_empty() {
        return "MODELS. There is a model catalog but nothing in it to delegate to — not \
                even this conversation's own model, which means the catalog does not \
                contain it. Delegated work runs on this conversation's model."
            .into();
    }
    let ranked = offered.iter().any(|m| m.capable_rank.is_some());
    let rows = offered
        .iter()
        .map(|m| {
            format!(
                "  {id}{here} — {name}, ctx {ctx}{vision}{effort}{note}",
                id = m.id,
                here = if current.as_deref() == Some(m.id.as_str()) {
                    " (this conversation)"
                } else {
                    ""
                },
                name = m.display_name,
                ctx = m.context_window,
                vision = if m.supports_vision { ", vision" } else { "" },
                effort = if m.effort_levels.is_empty() {
                    String::new()
                } else {
                    format!(", effort {}", m.effort_levels.join("/"))
                },
                note = match &m.note {
                    Some(note) => format!("\n      {note}"),
                    None => String::new(),
                },
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "MODELS. What delegated work may run on. A delegation tool whose description \
         says it takes a `model` id takes one of these; leaving it out keeps the \
         conversation's own model.\n\n\
         {rows}\n\n\
         {why}\n\n\
         Models billed to another account are never here. That is a decision about \
         credentials and vendors rather than cost, so a cheaper model on another \
         account is still absent — and it is the person's to make, not yours.",
        why = if ranked {
            "Ordered weakest first where the deployment says so. Anything more capable \
             than this conversation is deliberately absent: choosing to spend more is \
             the person's decision, made when they picked this model."
        } else {
            "This deployment has not said which of these is more capable, so they are \
             not ordered and none is known to be cheaper. Pick on the facts above — \
             context window, vision, and whatever note the deployment wrote — not on \
             the name."
        }
    )
}

pub struct ModelCatalogPlugin;

#[async_trait]
impl Plugin for ModelCatalogPlugin {
    fn name(&self) -> &'static str {
        "model-catalog"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["system-prompt"]
    }
    fn uses(&self) -> &'static [&'static str] {
        // Not `inject`: a tree with no catalog is a tree with nothing to say
        // here, and this row falling silent is the right answer rather than a
        // reason to wait forever.
        &["models", "operations"]
    }
    fn description(&self) -> &'static str {
        "tells the model that delegation can pick a model, and lists them when asked"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let Some(models) = ctx.service::<crate::seams::ModelsSvc>() else {
            return Ok(());
        };
        // The list itself, on request — rendered from the live catalog each
        // time, so it follows a login or a model switch.
        let catalog = ctx.clone();
        crate::plugins::self_knowledge::describes_live(
            ctx,
            crate::seams::Aspect::Models,
            "model-catalog",
            0,
            move |_| {
                catalog
                    .service::<crate::seams::ModelsSvc>()
                    .map(|models| describe_catalog(models.as_ref()))
            },
        );
        // The conversation's own model is always delegatable, so a catalog that
        // offers only it offers no CHOICE — and telling the model to go look
        // would send it to a tool call that can only answer "the one you are
        // already on". One entry is the same as none, as far as this line goes.
        //
        // Note what this does and does not depend on: whether an alternative
        // exists, not how many there are. The string itself never moves.
        if crate::seams::delegatable(models.as_ref()).len() < 2 {
            return Ok(());
        }
        contribute_prompt(ctx, "model-catalog", 56, CATALOG_POINTER);
        Ok(())
    }
}
