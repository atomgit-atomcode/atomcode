//! Rows whose substance the coding runtime already owns.
//!
//! The runtime keeps state a tree cannot build for itself: the native session
//! (id, stored conversation, lease), and kernel lifecycle hooks constructed
//! against that session — the snapshot writer above all. These rows hand that
//! state to the tree the way `llm-injected` hands it the provider: the host
//! registers a plugin instance holding the value, and a row names it.
//!
//! # `session-native`
//!
//! Stands in for the harness's `session` row. Same seam (`session-defaults`),
//! with the id the runtime's native binding already has and — instead of
//! "resume from the log store", which would replay the JSONL journal — the
//! stored native conversation as a seed. Native is the master; see
//! [`crate::native_log`].
//!
//! # `kernel-hooks`
//!
//! Runs one host-built [`LifecycleHooks`] at the harness's moments. The hook's
//! own code is the judgement; the row only decides when to call it, so the
//! chain and the tree cannot disagree about what a hook does — only about when,
//! and that is written down here:
//!
//! | kernel hook | harness moment |
//! |---|---|
//! | `session_start` | first `agent/pre-step`; messages it appends ride as reminders |
//! | `user_prompt_submit` | `agent/pre-step`, first step of a turn; `Err` rejects the input |
//! | `turn_start` | `agent/request`, first request of a turn, after the prompt is logged |
//! | `pre_request` | `agent/request`, before delegating; only what it APPENDS is kept, as an ephemeral tail |
//! | `pre_request_options`, `on_request` | `agent/request`, before delegating |
//! | `on_model_response` | `agent/request`, after a successful response |
//! | `offer_continuation` | `agent/request`, after a response that ends the round; queued as a harness message |
//! | `on_error` | `agent/request`, after a failed request |
//! | `turn_complete` | `turn/finishing` — awaited, before the driver learns the turn ended |
//!
//! Not bridged, and why: a `pre_request` that rewrites existing messages would
//! put content in front of the model that the log cannot explain (its edits are
//! dropped); `on_text_delta` / `on_reasoning_delta` mutate a stream the harness
//! commits verbatim; `on_rate_limit` belongs to the retry policy; `session_end`
//! has no harness moment. A hook that needs one of these gets a row of its own.
//!
//! # `kernel-middleware`
//!
//! The same, for a host-built [`ToolMiddleware`](atomcode_kernel::middleware::ToolMiddleware)
//! on `tools/execute`: `before` → Proceed runs the call, Allow runs it marked
//! pre-approved, Deny refuses it; `after` → Block appends the reason to the
//! result. Only observers and deciders that never ask are bridged — the kernel's
//! round-trip context here has nobody on the other end, so an `Ask` fails closed.
//!
//! Only the conversation's own agent is served. A delegated child runs its turns
//! through the same events, and a snapshot writer that heard them would file the
//! child's work under the parent's session.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use async_trait::async_trait;
use atomcode_capabilities::session::SessionManager;
use atomcode_harness::agent::{Agent, MessageOrigin};
use atomcode_harness::events::{
    AgentRequest, ModelRequest, ModelResponse, PreStep, RequestError, StepDecision, TurnFinishing,
};
use atomcode_harness::seams::{
    AgentsSvc, SessionDefaults, SessionDefaultsSvc, SessionSvc, SystemPromptSvc, TurnOutcome,
};
use atomcode_harness::session::{InjectionOrigin, SessionEvent};
use atomcode_kernel::hook::{LifecycleHooks, TurnCtx};
use atomcode_kernel::message::{Conversation, Message, MessageMeta, SessionSnapshot};
use atomcode_plexus::{Context, Listener, Next, Plugin, Waterfall};
use serde_json::Value;

/// The session a tree's own agent is created with.
#[derive(Clone, Default)]
pub struct SessionSeed {
    /// The native binding's id. `None` for a sessionless runtime, which lets the
    /// tree mint one.
    pub id: Option<String>,
    /// The stored conversation to continue from.
    pub snapshot: Option<SessionSnapshot>,
    /// The store the session is kept in. `None` exactly when `id` is: a
    /// sessionless runtime keeps nothing.
    pub store: Option<Arc<SessionManager>>,
}

impl std::fmt::Debug for SessionSeed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionSeed")
            .field("id", &self.id)
            .field("snapshot", &self.snapshot)
            .field("store", &self.store.as_ref().map(|store| store.root()))
            .finish()
    }
}

/// `session-native`: the runtime's session, handed to the tree.
pub(crate) struct SessionNativePlugin(pub(crate) Arc<SessionSeed>);

impl SessionNativePlugin {
    /// What this row keeps, said by this row.
    ///
    /// It displaced the harness's `session` row and pushed the JSONL row aside
    /// to a journal, and for a while said nothing — so the only description of
    /// sessions the agent could find was that journal's, and it told people
    /// their conversation lived in a file nothing reads back. Displacing a row
    /// means inheriting what it was responsible for saying (the `model` row
    /// learned the same, see `on_harness`).
    ///
    /// The commands named are the product's contract for continuing a session,
    /// not one front end's: a terminal UI that replaces the current one keeps
    /// `/resume`, which is why this row can name it.
    fn describe(&self, ctx: &Context) {
        use atomcode_harness::plugins::self_knowledge::{describes, describes_live};
        use atomcode_harness::seams::Aspect;

        let (Some(id), Some(store)) = (self.0.id.clone(), self.0.store.clone()) else {
            describes(
                ctx,
                "sessions",
                10,
                "SESSIONS. This conversation is not kept in the AtomCode session \
                 store: it is not in the session list and cannot be resumed later.",
            );
            describes_live(ctx, Aspect::Session, "sessions/this-session", 10, |_| {
                Some(
                    "kept in: no session store — this conversation cannot be resumed later"
                        .to_string(),
                )
            });
            return;
        };

        describes(
            ctx,
            "sessions",
            10,
            format!(
                "SESSIONS. Sessions are kept per project in the AtomCode session \
                 store; this project's is `{root}`. For a session id: \
                 `<id>.snapshot` is the conversation — a resume rebuilds it from \
                 that file and from nothing else; `<id>.meta` holds its name, \
                 working directory and per-turn statistics; `<id>.jsonl` is its \
                 turn-by-turn transcript.\n\
                 Continuing a session is the person's to do, never yours, and never \
                 by editing these files or any configuration: `/resume` in the \
                 terminal UI, `atomcode --continue` (the latest) or `atomcode \
                 --resume <id-or-name>` when starting — `atomcode -p \"…\" --resume \
                 <id>` for a one-shot run — or picking it from the session list in \
                 the web UI.",
                root = store.root().display(),
            ),
        );
        describes_live(
            ctx,
            Aspect::Session,
            "sessions/this-session",
            10,
            move |_| {
                let mut lines = Vec::new();
                match store.snapshot_path(&id) {
                    Ok(path) if path.exists() => lines.push(format!(
                        "kept in: {} (the session store; a resume rebuilds from this file)",
                        path.display()
                    )),
                    Ok(path) => lines.push(format!(
                        "kept in: {} (the session store — not written yet)",
                        path.display()
                    )),
                    Err(error) => lines.push(format!("kept in: the session store ({error})")),
                }
                // Read when asked: a `/rename`, or the namer finishing, changes
                // it mid-session.
                if let Ok(meta) = store.read_meta(&id) {
                    lines.push(format!("name in the session list: {}", meta.name));
                    lines.push(format!("created at: {} (unix ms)", meta.created_at));
                    lines.push(format!("working directory: {}", meta.working_dir));
                    if let Some(fork) = &meta.fork_info {
                        lines.push(format!(
                            "forked from: {} (after its first {} messages)",
                            fork.parent_id, fork.base_message_count
                        ));
                    }
                }
                Some(lines.join("\n"))
            },
        );
    }
}

#[async_trait]
impl Plugin for SessionNativePlugin {
    fn name(&self) -> &'static str {
        "session-native"
    }
    fn uses(&self) -> &'static [&'static str] {
        &["operations"]
    }
    fn provides(&self) -> &'static [&'static str] {
        &["session-defaults"]
    }
    fn description(&self) -> &'static str {
        "the coding runtime's native session: its id, and its stored conversation as the seed"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        self.describe(ctx);
        let seed = self
            .0
            .snapshot
            .as_ref()
            .map(|snapshot| crate::native_log::seed_from_snapshot(snapshot, 1))
            .unwrap_or_default();
        let _ = ctx
            .provide::<SessionDefaultsSvc>(Arc::new(SessionDefaults {
                id: self.0.id.clone(),
                resume: false,
                seed,
            }))
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

/// Host-built lifecycle hooks, by the name a `kernel-hooks` row asks for.
#[derive(Default)]
pub struct HostHooks {
    hooks: RwLock<BTreeMap<String, Arc<dyn LifecycleHooks>>>,
}

impl HostHooks {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn insert(&self, name: impl Into<String>, hook: Arc<dyn LifecycleHooks>) {
        self.hooks
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(name.into(), hook);
    }

    /// The names a mount turns into rows, in a stable order.
    pub fn names(&self) -> Vec<String> {
        self.hooks
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect()
    }

    fn get(&self, name: &str) -> Option<Arc<dyn LifecycleHooks>> {
        self.hooks
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
            .cloned()
    }

    /// The row layer that mounts every hook this table holds.
    pub(crate) fn rows(&self) -> atomcode_plexus::Layer {
        self.names()
            .iter()
            .fold(atomcode_plexus::Layer::new(), |layer, name| {
                let entry =
                    atomcode_plexus::Entry::with_id(format!("kernel-hooks-{name}"), "kernel-hooks")
                        .with(NamedHook { hook: name })
                        .expect("a hook name is a string");
                layer.insert(entry)
            })
    }
}

#[derive(serde::Serialize)]
struct NamedHook<'a> {
    hook: &'a str,
}

#[derive(serde::Serialize)]
struct NamedMiddleware<'a> {
    middleware: &'a str,
}

/// `kernel-hooks`: one host-built lifecycle hook at the harness's moments.
pub(crate) struct KernelHooksPlugin(pub(crate) Arc<HostHooks>);

#[derive(serde::Deserialize)]
struct KernelHooksRow {
    hook: String,
}

#[async_trait]
impl Plugin for KernelHooksPlugin {
    fn name(&self) -> &'static str {
        "kernel-hooks"
    }
    fn uses(&self) -> &'static [&'static str] {
        &["agents", "system-prompt"]
    }
    fn description(&self) -> &'static str {
        "a lifecycle hook the coding runtime built, run at the harness's moments"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        let row: KernelHooksRow =
            serde_json::from_value(config.clone()).map_err(|e| format!("bad config: {e}"))?;
        let hook = self
            .0
            .get(&row.hook)
            .ok_or_else(|| format!("the host registered no hook named `{}`", row.hook))?;
        let bridge = Arc::new(Bridge {
            ctx: ctx.clone(),
            hook,
            session_started: AtomicBool::new(false),
            turn_started: Mutex::new(None),
        });
        let _ = ctx.on_waterfall::<PreStep>(bridge.clone(), false);
        let _ = ctx.on_waterfall::<AgentRequest>(bridge.clone(), false);
        let _ = ctx.on_parallel::<TurnFinishing>(bridge);
        Ok(())
    }
}

struct Bridge {
    ctx: Context,
    hook: Arc<dyn LifecycleHooks>,
    session_started: AtomicBool,
    /// The turn `turn_start` already ran for. A retry re-runs the request chain
    /// below it; the turn still started once.
    turn_started: Mutex<Option<u64>>,
}

impl Bridge {
    /// The agent whose turn this is — when it is the conversation's own.
    fn root_agent(&self) -> Option<Arc<Agent>> {
        let scoped = atomcode_harness::agent::scoped(&self.ctx);
        let session = scoped.service::<SessionSvc>()?;
        let agent = self.ctx.service::<AgentsSvc>()?.by_session(session.id())?;
        agent.parent().is_none().then_some(agent)
    }

    /// The conversation as the native store keeps it.
    fn conversation(&self, agent: &Agent) -> Conversation {
        let system = atomcode_harness::agent::scoped(&self.ctx)
            .service::<SystemPromptSvc>()
            .map(|prompts| prompts.render());
        crate::native_log::conversation_from_log(system, &agent.session().events())
    }

    fn turn_ctx(&self, agent: &Agent, turn: u64, round: u32) -> TurnCtx {
        let session = agent.session();
        let request_id = session
            .events()
            .iter()
            .filter_map(|logged| match &logged.event {
                SessionEvent::AssistantMessage {
                    meta: Some(meta), ..
                } => Some(meta.request_id),
                _ => None,
            })
            .max()
            .unwrap_or(0);
        TurnCtx {
            session_id: Some(Arc::from(session.id())),
            turn_id: turn,
            request_id,
            round,
            ..TurnCtx::default()
        }
    }
}

#[async_trait]
impl Waterfall<PreStep> for Bridge {
    async fn handle(&self, decision: &mut StepDecision, next: Next<'_, PreStep>) -> StepDecision {
        let Some(agent) = self.root_agent() else {
            return next.run(decision).await;
        };
        if !self.session_started.swap(true, Ordering::SeqCst) {
            let mut convo = self.conversation(&agent);
            let before = convo.messages.len();
            self.hook
                .session_start(&mut convo, agent.seed_len() > 0)
                .await;
            for message in convo.messages.drain(before..) {
                decision
                    .injections
                    .push((message.text, InjectionOrigin::Reminder));
            }
        }
        if decision.step == 1 {
            if let Some(text) = &mut decision.message {
                if let Err(reason) = self.hook.user_prompt_submit(text).await {
                    decision.rejected = Some(reason);
                    return decision.clone();
                }
            }
        }
        next.run(decision).await
    }
}

#[async_trait]
impl Waterfall<AgentRequest> for Bridge {
    async fn handle(
        &self,
        req: &mut ModelRequest,
        next: Next<'_, AgentRequest>,
    ) -> Result<ModelResponse, RequestError> {
        let Some(agent) = self.root_agent() else {
            return next.run(req).await;
        };
        let first = {
            let mut started = self.turn_started.lock().unwrap_or_else(|e| e.into_inner());
            let first = *started != Some(req.turn);
            *started = Some(req.turn);
            first
        };
        if first {
            let mut convo = self.conversation(&agent);
            self.hook.turn_start(&mut convo).await;
        }
        let ctx = self.turn_ctx(&agent, req.turn, req.round);
        // `pre_request` may only ADD to the end. What a hook appends is an
        // ephemeral tail for this request — a reminder, a nudge — and rides the
        // same way the harness's own tails do: past the log check, never logged.
        // A hook that rewrote the history instead would be putting words in
        // front of the model that no log can explain, so its edits are dropped.
        let mut proposed = req.messages.clone();
        let before = proposed.len();
        self.hook.pre_request(&mut proposed, &ctx).await;
        if proposed.len() > before && proposed[..before] == req.messages[..] {
            req.messages.extend(proposed.drain(before..));
        }
        self.hook
            .pre_request_options(&req.messages, &mut req.options, &ctx)
            .await;
        self.hook
            .on_request(&req.messages, &req.tools, &req.options, &ctx)
            .await;

        match next.run(req).await {
            Ok(response) => {
                let mut message =
                    Message::assistant(response.text.clone(), response.tool_calls.clone());
                if !response.reasoning.is_empty() {
                    message.reasoning = Some(response.reasoning.clone());
                }
                let tokens = response.usage.unwrap_or_default();
                let ctx_window = self
                    .ctx
                    .service::<atomcode_harness::seams::LlmSvc>()
                    .map(|provider| provider.context_window())
                    .unwrap_or(0);
                message.meta = Some(MessageMeta {
                    tokens,
                    used_tokens: tokens.prompt,
                    ctx_window,
                    round: req.round,
                    turn_id: req.turn,
                    request_id: ctx.request_id + 1,
                    session_id: ctx.session_id.as_deref().map(str::to_string),
                    ..MessageMeta::default()
                });
                self.hook.on_model_response(&mut message).await;

                if response.tool_calls.is_empty() && !response.truncated {
                    let mut messages = req.messages.clone();
                    messages.push(message);
                    let convo = Conversation {
                        messages,
                        cache_epoch: 0,
                    };
                    if let Some(text) = self.hook.offer_continuation(&convo).await {
                        agent.inbox().send_from(text, MessageOrigin::Harness);
                    }
                }
                Ok(response)
            }
            Err(error) => {
                self.hook.on_error(&error.to_string()).await;
                Err(error)
            }
        }
    }
}

#[async_trait]
impl Listener<TurnFinishing> for Bridge {
    async fn call(&self, outcome: &TurnOutcome) -> Option<()> {
        let agent = self.root_agent()?;
        // A turn refused before its first step ran nothing, and the chain files
        // no turn for a rejected prompt either.
        if outcome.steps == 0 {
            return None;
        }
        let convo = self.conversation(&agent);
        let reason = outcome.stop.folded_for_runtime_drivers();
        let ctx = self.turn_ctx(&agent, outcome.turn, outcome.rounds);
        self.hook.turn_complete(&convo, &reason, &ctx).await;
        None
    }
}

// ---- the person's switches ------------------------------------------------

/// `modes-host`: the runtime's live switches, as the tree's `modes` service.
///
/// The same `Arc`s `set_mode` writes, so a toggle reaches every row that reads
/// them at its next decision — nothing remounts.
pub(crate) struct ModesHostPlugin(pub(crate) atomcode_harness::seams::Modes);

#[async_trait]
impl Plugin for ModesHostPlugin {
    fn name(&self) -> &'static str {
        "modes-host"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["modes"]
    }
    fn description(&self) -> &'static str {
        "the coding runtime's plan and accept-edits switches, read live"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let _ = ctx
            .provide::<atomcode_harness::seams::ModesSvc>(Arc::new(self.0.clone()))
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

/// `grants-host`: the session's "always allow" answers, kept by the runtime.
///
/// A tree's approval seam remembers grants in itself, and a tree is rebuilt on
/// undo and restore — so a person who said "always" would be asked again after
/// every undo. The runtime's store lives as long as the session's decisions do:
/// kept across undo and restore, carried across a config reload, fresh for a new
/// session — the lifetime the chain's approval middleware already gives it.
pub(crate) struct GrantsHostPlugin(
    pub(crate) Arc<dyn atomcode_capabilities::tools::PermissionStore>,
);

#[async_trait]
impl Plugin for GrantsHostPlugin {
    fn name(&self) -> &'static str {
        "grants-host"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["grants"]
    }
    fn description(&self) -> &'static str {
        "the coding runtime's session grants, so a rebuilt tree remembers what the person allowed"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let _ = ctx
            .provide::<atomcode_harness::seams::GrantsSvc>(self.0.clone())
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

/// `plan-mode-live`: plan mode as the coding product has it, switched live.
///
/// The harness's own `plan-mode` row is a different policy — every tool without
/// a read-only hint refused, and the instruction in the system prompt — and it
/// is mount-time. This one asks [`crate::plan_mode::plan_verdict`], the judgement
/// the kernel middleware asks: built-in mutating tools refused, read-only MCP
/// tools run, other MCP tools asked about with a session grant. The reminder
/// rides as a request tail, so toggling plan mode never rewrites the cached
/// prompt prefix.
pub(crate) struct PlanModeLivePlugin(
    pub(crate) Arc<dyn atomcode_capabilities::tools::PermissionStore>,
);

struct PlanModeLive {
    ctx: Context,
    mcp_grants: Arc<dyn atomcode_capabilities::tools::PermissionStore>,
}

impl PlanModeLive {
    fn active(&self) -> bool {
        self.ctx
            .service::<atomcode_harness::seams::ModesSvc>()
            .is_some_and(|modes| modes.plan.load(Ordering::Relaxed))
    }
}

#[async_trait]
impl Waterfall<atomcode_harness::events::ToolsExecute> for PlanModeLive {
    async fn handle(
        &self,
        exec: &mut atomcode_harness::events::ToolExec,
        next: Next<'_, atomcode_harness::events::ToolsExecute>,
    ) -> atomcode_kernel::tool::ToolResult {
        use crate::plan_mode::{plan_mcp_denied, plan_verdict, PlanVerdict};
        let Some(tool) = self
            .ctx
            .service::<atomcode_harness::seams::ToolsSvc>()
            .and_then(|tools| tools.get(&exec.call.name))
        else {
            return next.run(exec).await;
        };
        let refuse = |call_id: String, content: String| atomcode_kernel::tool::ToolResult {
            call_id,
            content,
            is_error: true,
            images: vec![],
        };
        match plan_verdict(self.active(), &exec.call, &tool, self.mcp_grants.as_ref()) {
            PlanVerdict::Proceed => next.run(exec).await,
            PlanVerdict::Blocked(message) => refuse(exec.call.id.clone(), message),
            PlanVerdict::Granted => {
                // The person allowed this one in plan mode: their own answer,
                // so a boundary below honors it.
                exec.authorization = atomcode_harness::events::Authorization::ByPerson;
                next.run(exec).await
            }
            PlanVerdict::AskMcp => {
                let Some(policy) = self.ctx.service::<atomcode_harness::seams::ApprovalSvc>()
                else {
                    // Nobody to ask: a forced question that silently became a
                    // yes would let plan mode write.
                    return refuse(exec.call.id.clone(), plan_mcp_denied(&exec.call.name));
                };
                let asking: Arc<dyn atomcode_kernel::tool::Tool> = Arc::new(McpInPlanMode(tool));
                match policy.decide(&exec.call, &asking).await {
                    atomcode_harness::seams::Decision::Allow => {
                        exec.authorization = atomcode_harness::events::Authorization::ByPerson;
                        next.run(exec).await
                    }
                    atomcode_harness::seams::Decision::Deny(_) => {
                        refuse(exec.call.id.clone(), plan_mcp_denied(&exec.call.name))
                    }
                }
            }
        }
    }
}

#[async_trait]
impl Waterfall<AgentRequest> for PlanModeLive {
    async fn handle(
        &self,
        req: &mut ModelRequest,
        next: Next<'_, AgentRequest>,
    ) -> Result<ModelResponse, RequestError> {
        if self.active() {
            req.messages
                .push(atomcode_capabilities::reminder::synthetic_system_reminder(
                    crate::plan_mode::PLAN_MODE_REMINDER_BODY,
                ));
        }
        next.run(req).await
    }
}

/// A mutating MCP tool, as plan mode asks about it: risky whatever the server
/// claims, and granted per tool for the session.
struct McpInPlanMode(Arc<dyn atomcode_kernel::tool::Tool>);

#[async_trait]
impl atomcode_kernel::tool::Tool for McpInPlanMode {
    fn name(&self) -> &str {
        self.0.name()
    }
    fn description(&self) -> &str {
        self.0.description()
    }
    fn parameters_schema(&self) -> Value {
        self.0.parameters_schema()
    }
    fn risk(&self, _args: &str) -> atomcode_kernel::tool::RiskLevel {
        atomcode_kernel::tool::RiskLevel::Risky
    }
    fn always_grant_scope(&self, _args: &str) -> String {
        // The whole tool, for the session: the chain grants by tool name.
        self.0.name().to_string()
    }
    async fn execute(
        &self,
        args: &str,
        ctx: &atomcode_kernel::tool::ToolContext,
    ) -> atomcode_kernel::tool::ToolResult {
        self.0.execute(args, ctx).await
    }
}

#[async_trait]
impl Plugin for PlanModeLivePlugin {
    fn name(&self) -> &'static str {
        "plan-mode-live"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tools"]
    }
    fn uses(&self) -> &'static [&'static str] {
        &["modes", "approval"]
    }
    fn description(&self) -> &'static str {
        "plan mode switched live: mutating tools refused, MCP tools asked about, a request-tail reminder"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let live = Arc::new(PlanModeLive {
            ctx: ctx.clone(),
            mcp_grants: self.0.clone(),
        });
        let _ = ctx.on_waterfall::<atomcode_harness::events::ToolsExecute>(live.clone(), true);
        let _ = ctx.on_waterfall::<AgentRequest>(live, false);
        Ok(())
    }
}

// ---- the skill catalog, as prepare loaded it --------------------------------

/// `skills-host`: the skill registry the runtime's prepare loaded.
///
/// The harness's `skills` row loads the standard directories itself and can only
/// ADD to them; a host that was told exactly which directories to use (or none)
/// could not say so, and plugin skills — loaded under their plugin's namespace —
/// had no way in at all. This row serves the registry prepare built, and puts its
/// catalog in the prompt the way the chain does: rendered once, skills the
/// project's instructions name first when the budget cuts.
pub(crate) struct SkillsHostPlugin(pub(crate) LoadedSkills);

/// The skill catalog prepare loaded, and where it looked.
#[derive(Clone)]
pub struct LoadedSkills {
    pub registry: Arc<atomcode_capabilities::skills::SkillRegistry>,
    /// The catalog as rendered for the prompt.
    pub catalog: Option<String>,
    /// Scanned in this order; a skill in a later directory replaces an earlier
    /// one of the same name.
    pub dirs: Vec<std::path::PathBuf>,
    /// Where a new skill goes — `(every project, this project)` — when `dirs`
    /// is the standard list. `None` when the driver named its own directories.
    pub install: Option<(std::path::PathBuf, std::path::PathBuf)>,
    /// Installed plugins whose skills were loaded, under each plugin's name.
    pub plugins: Vec<String>,
}

impl SkillsHostPlugin {
    /// The directories this runtime actually scanned — a driver may name its
    /// own — rather than the standard list the harness row describes.
    fn describe(&self, ctx: &Context) {
        let loaded = &self.0;
        let mut text = format!("SKILLS — {} loaded.", loaded.registry.len());
        if loaded.dirs.is_empty() && loaded.plugins.is_empty() {
            text.push_str(" This runtime was started without skill directories.");
        } else {
            if !loaded.dirs.is_empty() {
                text.push_str(
                    " A skill is `<name>/SKILL.md` (with any files it uses beside it), or \
                     a single `<name>.md`, inside one of these directories, scanned in \
                     this order — a later one's skill replaces an earlier one's of the \
                     same name:",
                );
                for dir in &loaded.dirs {
                    text.push_str(&format!("\n  {}", dir.display()));
                }
                text.push_str(
                    "\nThe file starts with frontmatter, one `key: value` per line:\n\
                     ---\n\
                     name: <name>   (letters, digits, `-`, `_`; defaults to the directory \
                     or file name)\n\
                     description: <when to use it — this is what gets it chosen>\n\
                     ---\n\
                     then the instructions. `$ARGUMENTS` in them is replaced by what the \
                     skill was invoked with. A file whose name is invalid is skipped \
                     without a warning.\n",
                );
            }
            if let Some((every_project, this_project)) = &loaded.install {
                text.push_str(&format!(
                    "To add a skill for the person, write it under `{}` for this project \
                     or `{}` for every project — ask which if they did not say. \
                     ",
                    this_project.display(),
                    every_project.display(),
                ));
            }
            text.push_str(
                "A new session loads it; in the terminal UI, `/plugin reload` loads it \
                 into this one.\n",
            );
            if !loaded.plugins.is_empty() {
                text.push_str(&format!(
                    "Installed plugins' skills are loaded as `<plugin>:<skill>`, from: {}.\n",
                    loaded.plugins.join(", ")
                ));
            }
            text.push_str("`list_skills` shows what is loaded; `use_skill` loads one.");
        }
        atomcode_harness::plugins::self_knowledge::describes(ctx, "skills", 13, text);
    }
}

#[async_trait]
impl Plugin for SkillsHostPlugin {
    fn name(&self) -> &'static str {
        "skills-host"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tools", "system-prompt"]
    }
    fn uses(&self) -> &'static [&'static str] {
        &["operations"]
    }
    fn provides(&self) -> &'static [&'static str] {
        &["skills"]
    }
    fn description(&self) -> &'static str {
        "the skill catalog the coding runtime loaded, plugin skills included"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        self.describe(ctx);
        let LoadedSkills {
            registry, catalog, ..
        } = &self.0;
        let _ = ctx
            .provide::<atomcode_harness::seams::SkillsSvc>(registry.clone())
            .map_err(|e| e.to_string())?;
        let toolbox = ctx
            .require::<atomcode_harness::seams::ToolsSvc>()
            .map_err(|e| e.to_string())?;
        for tool in [
            Arc::new(atomcode_capabilities::skills::UseSkillTool::new(
                registry.clone(),
            )) as Arc<dyn atomcode_kernel::tool::Tool>,
            Arc::new(atomcode_capabilities::skills::ListSkillsTool::new(
                registry.clone(),
            )),
        ] {
            let name = tool.name().to_string();
            toolbox.register(tool)?;
            let toolbox = toolbox.clone();
            let _ = ctx.effect(move || toolbox.unregister(&name));
        }
        if let (Some(catalog), Some(prompts)) = (
            catalog.as_ref().filter(|c| !c.trim().is_empty()),
            ctx.service::<SystemPromptSvc>(),
        ) {
            let (id, rank) = crate::on_harness::SKILLS_FRAGMENT;
            prompts.contribute(id, rank, catalog.clone());
            let _ = ctx.effect(move || prompts.remove(id));
        }
        Ok(())
    }
}

// ---- the settings file ------------------------------------------------------

/// `config-file`: what this runtime took from `config.toml`, said by the runtime
/// that read it.
///
/// Mounted only for a runtime `CodingRuntimeConfig::from_config` built — the
/// descriptions are that function's neighbours in `config.rs` and the settings
/// catalog's own renderer — so a runtime no file configured is never told to
/// edit one.
pub(crate) struct ConfigFilePlugin(pub(crate) std::path::PathBuf);

#[async_trait]
impl Plugin for ConfigFilePlugin {
    fn name(&self) -> &'static str {
        "config-file"
    }
    fn uses(&self) -> &'static [&'static str] {
        &["operations"]
    }
    fn description(&self) -> &'static str {
        "what this runtime took from config.toml, and the settings in it that are safe to edit"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        use atomcode_harness::plugins::self_knowledge::{describes, describes_live};
        describes(
            ctx,
            "config-file",
            20,
            crate::config::describe_config_file(&self.0),
        );
        let catalog = atomcode_config::settings::describe_catalog(&self.0);
        describes_live(
            ctx,
            atomcode_harness::seams::Aspect::Settings,
            "config-file/settings",
            0,
            move |_| Some(catalog.clone()),
        );
        Ok(())
    }
}

// ---- tools the runtime adds -----------------------------------------------

/// `host-tools`: tools the runtime built around its own state.
///
/// `schedule_wakeup` hands a wakeup to the runtime's `/loop` controller over a
/// channel the runtime owns — no row can build it, because no row has that
/// channel. The rest are the capability graph's own tools, whose contract a row
/// does not match; each brings the persona's guidance for it as a fragment that
/// leaves with it.
pub(crate) struct HostToolsPlugin(pub(crate) Vec<Arc<dyn atomcode_kernel::tool::Tool>>);

#[async_trait]
impl Plugin for HostToolsPlugin {
    fn name(&self) -> &'static str {
        "host-tools"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tools"]
    }
    fn uses(&self) -> &'static [&'static str] {
        &["system-prompt"]
    }
    fn description(&self) -> &'static str {
        "tools the coding runtime built itself: its controllers' and its capability graph's"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let toolbox = ctx
            .require::<atomcode_harness::seams::ToolsSvc>()
            .map_err(|e| e.to_string())?;
        let prompts = ctx.service::<SystemPromptSvc>();
        let mut guided = std::collections::BTreeSet::new();
        for tool in &self.0 {
            let name = tool.name().to_string();
            toolbox.register(tool.clone())?;
            if let (Some(prompts), Some((key, text))) =
                (&prompts, crate::persona::host_tool_guidance(&name))
            {
                if guided.insert(key) {
                    let id = format!("host-tool-{key}");
                    // Beside the rows that describe their own tools.
                    let rank = match key {
                        "ask" => 56,
                        "code-review" => 58,
                        _ => 57,
                    };
                    prompts.contribute(&id, rank, text);
                    let prompts = prompts.clone();
                    let _ = ctx.effect(move || prompts.remove(&id));
                }
            }
            let toolbox = toolbox.clone();
            let _ = ctx.effect(move || toolbox.unregister(&name));
        }
        Ok(())
    }
}

// ---- host-built tool middleware -------------------------------------------

/// Host-built tool middleware, by the name a `kernel-middleware` row asks for.
#[derive(Default)]
pub struct HostMiddleware {
    entries: RwLock<BTreeMap<String, Arc<dyn atomcode_kernel::middleware::ToolMiddleware>>>,
    /// Names a row in the list already mounts by hand. [`Self::rows`] skips
    /// these, or the middleware would run twice per call: once where the row
    /// puts it, once innermost from the auto-emitted row.
    claimed: RwLock<std::collections::BTreeSet<String>>,
}

impl HostMiddleware {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn insert(
        &self,
        name: impl Into<String>,
        middleware: Arc<dyn atomcode_kernel::middleware::ToolMiddleware>,
    ) {
        self.entries
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(name.into(), middleware);
    }

    /// Register a middleware that a named row in the list mounts itself.
    ///
    /// Use this whenever the position matters: the row states where the
    /// middleware sits among the gates, and [`Self::rows`] must not also append
    /// an innermost copy of it.
    pub fn insert_mounted_by_row(
        &self,
        name: impl Into<String>,
        middleware: Arc<dyn atomcode_kernel::middleware::ToolMiddleware>,
    ) {
        let name = name.into();
        self.claimed
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(name.clone());
        self.insert(name, middleware);
    }

    fn get(&self, name: &str) -> Option<Arc<dyn atomcode_kernel::middleware::ToolMiddleware>> {
        self.entries
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
            .cloned()
    }

    /// The row layer that mounts every middleware this table holds, except the
    /// ones a row in the list already mounts (see [`Self::insert_mounted_by_row`]).
    ///
    /// These land innermost, after every gate has had its say — right for an
    /// observer, wrong for a decider, which is why a decider gets its own row.
    pub(crate) fn rows(&self) -> atomcode_plexus::Layer {
        let claimed = self.claimed.read().unwrap_or_else(|e| e.into_inner());
        self.entries
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .filter(|name| !claimed.contains(*name))
            .fold(atomcode_plexus::Layer::new(), |layer, name| {
                let entry = atomcode_plexus::Entry::with_id(
                    format!("kernel-middleware-{name}"),
                    "kernel-middleware",
                )
                .with(NamedMiddleware { middleware: name })
                .expect("a middleware name is a string");
                layer.insert(entry)
            })
    }
}

/// `kernel-middleware`: one host-built tool middleware on `tools/execute`.
pub(crate) struct KernelMiddlewarePlugin(pub(crate) Arc<HostMiddleware>);

#[derive(serde::Deserialize)]
struct KernelMiddlewareRow {
    middleware: String,
    /// Among the gates rather than after them — for a middleware that decides,
    /// placed by patching the row whose position it takes.
    #[serde(default)]
    prepend: bool,
}

struct MiddlewareBridge {
    ctx: Context,
    middleware: Arc<dyn atomcode_kernel::middleware::ToolMiddleware>,
}

#[async_trait]
impl Waterfall<atomcode_harness::events::ToolsExecute> for MiddlewareBridge {
    async fn handle(
        &self,
        exec: &mut atomcode_harness::events::ToolExec,
        next: Next<'_, atomcode_harness::events::ToolsExecute>,
    ) -> atomcode_kernel::tool::ToolResult {
        use atomcode_kernel::middleware::{AfterOutcome, BeforeOutcome};
        let tool = self
            .ctx
            .service::<atomcode_harness::seams::ToolsSvc>()
            .and_then(|tools| tools.get(&exec.call.name));
        if let Some(tool) = &tool {
            // Nobody answers on this channel: a middleware that asks gets the
            // closed-channel answer, which every kernel gate reads as a refusal.
            let (events, _nobody) = tokio::sync::mpsc::unbounded_channel();
            let rt =
                atomcode_kernel::request::RequestCtx::new(events, Some(std::time::Duration::ZERO));
            match self.middleware.before(&mut exec.call, tool, &rt).await {
                BeforeOutcome::Proceed => {}
                // PRESUMED: a `[permissions] allow` rule is convenience the
                // person configured, not consent to this call. See
                // `Authorization` for the leak that made the difference matter.
                BeforeOutcome::Allow { .. } => {
                    exec.authorization = atomcode_harness::events::Authorization::Presumed
                }
                BeforeOutcome::Ask { reason } => {
                    return atomcode_kernel::tool::ToolResult {
                        call_id: exec.call.id.clone(),
                        content: reason.unwrap_or_else(|| {
                            format!("`{}` needs approval nobody can give here", exec.call.name)
                        }),
                        is_error: true,
                        images: vec![],
                    }
                }
                BeforeOutcome::Deny { reason }
                | BeforeOutcome::DenyTurn { reason }
                | BeforeOutcome::DenyTurnWithIntervention { reason, .. } => {
                    return atomcode_kernel::tool::ToolResult {
                        call_id: exec.call.id.clone(),
                        content: reason,
                        is_error: true,
                        images: vec![],
                    }
                }
            }
        }
        let mut result = next.run(exec).await;
        if let AfterOutcome::Block { reason } =
            self.middleware.after(&mut result, tool.as_ref()).await
        {
            result.content.push_str("\n\n");
            result.content.push_str(&reason);
            result.is_error = true;
        }
        result
    }
}

#[async_trait]
impl Plugin for KernelMiddlewarePlugin {
    fn name(&self) -> &'static str {
        "kernel-middleware"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tools"]
    }
    fn description(&self) -> &'static str {
        "a tool middleware the coding runtime built, on tools/execute"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        let row: KernelMiddlewareRow =
            serde_json::from_value(config.clone()).map_err(|e| format!("bad config: {e}"))?;
        let middleware = self.0.get(&row.middleware).ok_or_else(|| {
            format!(
                "the host registered no middleware named `{}`",
                row.middleware
            )
        })?;
        // Innermost by default: an observer records the call that actually runs,
        // after every gate has had its say — the chain registers these after the
        // gates too. A decider asks to sit among the gates instead.
        let _ = ctx.on_waterfall::<atomcode_harness::events::ToolsExecute>(
            Arc::new(MiddlewareBridge {
                ctx: ctx.clone(),
                middleware,
            }),
            row.prepend,
        );
        Ok(())
    }
}

// ---- the person's own hooks, as the runtime loaded them -------------------

/// `cc-hooks-host`: the `hooks.json` engine the runtime already built.
///
/// The harness's `cc-hooks` row loads `hooks.json` itself, which misses what only
/// the host knows: hooks a plugin contributed, and the transcript path a Stop
/// hook opens. The runtime's engine has both; this row mounts it at the same
/// moments `cc-hooks` would.
pub(crate) struct CcHooksHostPlugin(
    pub(crate) Arc<atomcode_capabilities::cc_hooks::CCExternalHooks>,
);

#[async_trait]
impl Plugin for CcHooksHostPlugin {
    fn name(&self) -> &'static str {
        "cc-hooks-host"
    }
    fn uses(&self) -> &'static [&'static str] {
        &["approval", "tools"]
    }
    fn description(&self) -> &'static str {
        "run the hooks.json engine the coding runtime loaded, plugin hooks included"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        crate::on_harness::mount_cc_hooks(ctx, self.0.clone());
        Ok(())
    }
}

// ---- the session context block --------------------------------------------

/// `session-context`: environment, project instructions and a git snapshot,
/// contributed to the system prompt once per tree.
///
/// The chain inserts this block as a leading system message at session start.
/// A tree has no conversation to insert into — its system prompt is the prompt
/// registry — so the block is a fragment, under the id the harness's
/// `project-instructions` row uses: this block already carries the instructions,
/// and two copies of AGENTS.md is worse than either. A continued session keeps
/// the git section it started with (see
/// [`SessionContextHook::block`](atomcode_capabilities::session::SessionContextHook::block)).
pub(crate) struct SessionContextPlugin {
    pub(crate) hook: Arc<atomcode_capabilities::session::SessionContextHook>,
    /// The system prompt the continued session was stored with, if any.
    pub(crate) stored: Option<String>,
}

#[async_trait]
impl Plugin for SessionContextPlugin {
    fn name(&self) -> &'static str {
        "session-context"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["system-prompt"]
    }
    fn description(&self) -> &'static str {
        "environment, project instructions and a session-start git snapshot, in the system prompt"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let block = self.hook.block(self.stored.as_deref());
        let Some(prompts) = ctx.service::<SystemPromptSvc>() else {
            return Ok(());
        };
        // Withdrawn with the row, like every other fragment.
        prompts.contribute("project-instructions", 1, block);
        let _ = ctx.effect(move || prompts.remove("project-instructions"));
        Ok(())
    }
}

// ---- MCP, as the runtime connected it ---------------------------------------

/// The runtime's MCP registry and the publication state its catalog shares.
pub(crate) struct McpPublication {
    pub(crate) registry: Arc<atomcode_capabilities::mcp::McpRegistry>,
    pub(crate) connect_rx:
        Option<tokio::sync::mpsc::UnboundedReceiver<atomcode_capabilities::mcp::McpConnectEvent>>,
    pub(crate) tool_names: Arc<RwLock<Vec<String>>>,
    pub(crate) publish_lock: Arc<tokio::sync::Mutex<()>>,
    pub(crate) publication_enabled: Arc<AtomicBool>,
    pub(crate) catalog_ready: tokio::sync::watch::Sender<bool>,
    /// Where this row hands the mounted catalog back, so `withdraw_mcp_tools`
    /// can take the published tools off it. Nothing else writes it.
    pub(crate) toolbox_slot: Arc<RwLock<Option<Arc<atomcode_harness::seams::ToolBox>>>>,
}

/// `mcp-host`: the MCP servers the runtime connected, and their tools in the tree.
///
/// One owner: the registry the runtime's prepare built and connects in the
/// background, which is what `mcp_status`, `wait_mcp_ready`, `withdraw_mcp_tools`
/// and a capability reload already act on. The harness's own `mcp` row would
/// connect the same servers a second time, from the tree. This row publishes
/// the runtime's registry instead — each server's tools as it connects, a full
/// reconciliation when the initial pass settles (that is what readiness means),
/// and nothing once the registry is withdrawn.
pub(crate) struct McpHostPlugin(pub(crate) Mutex<Option<McpPublication>>);

impl McpHostPlugin {
    pub(crate) fn new(publication: McpPublication) -> Self {
        Self(Mutex::new(Some(publication)))
    }
}

/// Replace the MCP tools this tree offers with `infos` (all of them) or add
/// `infos` (one server's) to them, under the publication lock.
async fn publish_mcp(
    toolbox: &Arc<atomcode_harness::seams::ToolBox>,
    registry: &Arc<atomcode_capabilities::mcp::McpRegistry>,
    infos: Vec<atomcode_capabilities::mcp::McpToolInfo>,
    publication: &McpShared,
    replace: bool,
) {
    let _guard = publication.publish_lock.lock().await;
    if !publication.publication_enabled.load(Ordering::Acquire) {
        return;
    }
    let adapters: Vec<Arc<dyn atomcode_kernel::tool::Tool>> = infos
        .into_iter()
        .filter_map(|info| {
            match atomcode_capabilities::mcp::McpToolAdapter::new(Arc::clone(registry), info) {
                Ok(adapter) => Some(Arc::new(adapter) as Arc<dyn atomcode_kernel::tool::Tool>),
                Err(error) => {
                    eprintln!("[mcp] tool publication skipped: {error}");
                    None
                }
            }
        })
        .collect();
    let mut names = publication
        .tool_names
        .write()
        .unwrap_or_else(|e| e.into_inner());
    if replace {
        for name in names.drain(..) {
            toolbox.unregister(&name);
        }
    }
    for adapter in adapters {
        let name = adapter.name().to_string();
        toolbox.unregister(&name);
        if toolbox.register(adapter).is_ok() && !names.contains(&name) {
            names.push(name);
        }
    }
    names.sort_unstable();
}

/// The publication state a publishing task shares with the runtime.
#[derive(Clone)]
struct McpShared {
    tool_names: Arc<RwLock<Vec<String>>>,
    publish_lock: Arc<tokio::sync::Mutex<()>>,
    publication_enabled: Arc<AtomicBool>,
}

#[async_trait]
impl Plugin for McpHostPlugin {
    fn name(&self) -> &'static str {
        "mcp-host"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tools"]
    }
    fn provides(&self) -> &'static [&'static str] {
        &["mcp"]
    }
    fn uses(&self) -> &'static [&'static str] {
        &["operations"]
    }
    fn description(&self) -> &'static str {
        "the MCP servers the coding runtime connected, their tools published as they come up"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let Some(publication) = self.0.lock().unwrap_or_else(|e| e.into_inner()).take() else {
            return Err("mcp-host mounted twice from one publication".into());
        };
        // The harness `mcp` row this displaces told the agent servers are entries
        // on its row config — which nothing in this runtime reads. This is where
        // the runtime's servers actually come from.
        atomcode_harness::plugins::self_knowledge::describes(
            ctx,
            "mcp",
            14,
            format!(
                "MCP — how third-party tools get in without recompiling. Servers are \
                 configured in `{user}` (every project) and in `.mcp.json` at the \
                 project root (this project; a server of the same name there wins). \
                 The format:\n\
                 {{ \"mcpServers\": {{\n\
                 \x20 \"<name>\": {{ \"command\": \"npx\", \"args\": [\"-y\", \"<package>\"], \
                 \"env\": {{ \"KEY\": \"${{KEY}}\" }} }},\n\
                 \x20 \"<name>\": {{ \"url\": \"https://…/mcp\", \"headers\": \
                 {{ \"Authorization\": \"Bearer ${{TOKEN}}\" }} }}\n\
                 }} }}\n\
                 Optional per server: `timeout_ms`, `disabled`, `autoApprove` (tool names \
                 that skip approval), `trust` (every tool of the server skips approval), \
                 and for HTTP `auth: {{ \"type\": \"oauth\" }}`. `${{VAR}}` is read from the \
                 environment, so a secret need not be written into the file. Comments are \
                 allowed; trailing commas are not.\n\
                 To add a server for the person: write the entry (for a stdio server, \
                 `atomcode mcp add <name> <command> [args…]` writes `.mcp.json`, and \
                 `--global` the user file). A project's own servers stay unconnected until \
                 the person trusts the project with `/mcp trust`; after a file changes, \
                 `/mcp reload` reconnects; an OAuth server needs `/mcp login <name>`. \
                 Each server's tools join the catalog as `mcp__<server>__<tool>` and go \
                 through the same approval as everything else.",
                user = atomcode_harness::home().join("mcp.json").display(),
            ),
        );
        let McpPublication {
            registry,
            connect_rx,
            tool_names,
            publish_lock,
            publication_enabled,
            catalog_ready,
            toolbox_slot,
        } = publication;
        let _ = ctx
            .provide::<atomcode_harness::seams::McpSvc>(Arc::clone(&registry))
            .map_err(|e| e.to_string())?;
        let toolbox = ctx
            .require::<atomcode_harness::seams::ToolsSvc>()
            .map_err(|e| e.to_string())?;
        // Withdrawal happens from the runtime, not from here, and it has to
        // reach this catalog or the model keeps being offered what was revoked.
        *toolbox_slot.write().unwrap_or_else(|e| e.into_inner()) = Some(toolbox.clone());
        let shared = McpShared {
            tool_names,
            publish_lock,
            publication_enabled,
        };

        let task = {
            let toolbox = toolbox.clone();
            let registry = Arc::clone(&registry);
            let shared = shared.clone();
            tokio::spawn(async move {
                // A tree mounted after the servers came up (a rebuild) starts from
                // what is already connected.
                let now = registry.list_all_tools().await;
                publish_mcp(&toolbox, &registry, now, &shared, true).await;
                let mut connect_rx = connect_rx;
                let initial = {
                    let registry = Arc::clone(&registry);
                    async move { registry.wait_until_initial_connections_done().await }
                };
                tokio::pin!(initial);
                let cancelled = {
                    let registry = Arc::clone(&registry);
                    async move { registry.wait_for_cancellation().await }
                };
                tokio::pin!(cancelled);
                loop {
                    let event = async {
                        match connect_rx.as_mut() {
                            Some(rx) => rx.recv().await,
                            None => std::future::pending().await,
                        }
                    };
                    tokio::select! {
                        _ = &mut cancelled => {
                            // Withdrawn: late connections must not republish, and
                            // what was published leaves the catalog.
                            let _guard = shared.publish_lock.lock().await;
                            let mut names =
                                shared.tool_names.write().unwrap_or_else(|e| e.into_inner());
                            for name in names.drain(..) {
                                toolbox.unregister(&name);
                            }
                            break;
                        }
                        _ = &mut initial => {
                            let all = registry.list_all_tools().await;
                            publish_mcp(&toolbox, &registry, all, &shared, true).await;
                            catalog_ready.send_replace(true);
                            break;
                        }
                        event = event => match event {
                            Some(atomcode_capabilities::mcp::McpConnectEvent::Connected { name }) => {
                                let tools = registry.list_tools_for_server(&name).await;
                                publish_mcp(&toolbox, &registry, tools, &shared, false).await;
                            }
                            Some(_) => {}
                            None => connect_rx = None,
                        },
                    }
                }
            })
        };
        // The task and the tools leave with the row.
        let _ = ctx.effect(move || {
            task.abort();
            let mut names = shared.tool_names.write().unwrap_or_else(|e| e.into_inner());
            for name in names.drain(..) {
                toolbox.unregister(&name);
            }
        });
        Ok(())
    }
}

// ---- 429s, judged the way the chain judges them -----------------------------

/// `rate-limit-coding`: stands where `llm-rate-limit` stands, and decides a 429
/// with the same host hook the chain's kernel asks.
///
/// The harness's own row waits out a `Retry-After` and otherwise fails the turn.
/// The product has more to go on: on the CodingPlan gateway the account's usage
/// windows say whether the limit clears in seconds (wait and retry) or hours
/// (pause, and tell the person when) — and a 429 from anywhere else still gets
/// the kernel's default verdict, including its livelock fuse and its refusal to
/// retry a stream that had already shown the person some output. A pause ends
/// the turn as rate-limited, not as a failure.
pub(crate) struct RateLimitCodingPlugin(
    pub(crate) Option<Arc<dyn crate::rate_limit::RateLimitWindowSource>>,
);

#[derive(serde::Deserialize, Default)]
struct RateLimitCodingRow {
    #[serde(default)]
    base_url: String,
    #[serde(default)]
    max_attempts: Option<u32>,
}

/// The kernel's bound on consecutive waits for one 429 incident.
const MAX_RATE_LIMIT_WAITS: u32 = 5;
/// A first 429 with no hint recovers quietly after this, as the kernel does.
const QUIET_FIRST_RETRY: std::time::Duration = std::time::Duration::from_secs(1);

struct RateLimitCoding {
    ctx: Context,
    hook: crate::rate_limit::RateLimitHook,
}

impl RateLimitCoding {
    fn notice(&self, detail: String) {
        let scoped = atomcode_harness::agent::scoped(&self.ctx);
        if let Some(log) = scoped.service::<SessionSvc>() {
            atomcode_harness::session::commit(
                &scoped,
                &log,
                SessionEvent::Notice {
                    turn: log.current_turn(),
                    notice: atomcode_harness::session::NoticeKind::RateLimited,
                    detail,
                },
            );
        }
    }
}

#[async_trait]
impl Waterfall<AgentRequest> for RateLimitCoding {
    async fn handle(
        &self,
        req: &mut ModelRequest,
        next: Next<'_, AgentRequest>,
    ) -> Result<ModelResponse, RequestError> {
        use atomcode_kernel::hook::{RateLimitDecision, RateLimitHint};
        let mut waits = 0u32;
        loop {
            let error = match next.run(req).await {
                Ok(response) => return Ok(response),
                Err(error) if error.is_rate_limited() => error,
                Err(error) => return Err(error),
            };
            let provider = atomcode_kernel::stream::ProviderError {
                retryable: error.retryable,
                message: error.message.clone(),
                http_status: error.http_status,
                code: error.code.clone(),
                retry_after_secs: error.retry_after.map(|d| d.as_secs()),
            };
            let hint = RateLimitHint::from_provider_error(&provider, waits.saturating_add(1));
            let server_message = RateLimitHint::server_message(&provider);
            let verdict = if hint.terminal {
                None
            } else {
                self.hook.on_rate_limit(&hint).await
            };
            let quiet_first =
                verdict.is_none() && hint.retry_after_secs.is_none() && !hint.terminal;
            let decision = verdict.unwrap_or_else(|| RateLimitDecision::from_hint(&hint));
            let pause = |reset_at_display: String, reset_label: String, secs: Option<u64>| {
                Err(RequestError {
                    rate_limit_pause: Some(atomcode_harness::events::RateLimitPause {
                        reset_at_display,
                        reset_label,
                        secs_until_reset: secs,
                        server_message: server_message.clone(),
                    }),
                    ..error.clone()
                })
            };
            match decision {
                // A stream that already showed the person output cannot be
                // re-issued without showing it twice.
                RateLimitDecision::WaitAndRetry { secs } if error.partial.is_some() => {
                    return pause(String::new(), String::new(), Some(secs));
                }
                RateLimitDecision::WaitAndRetry { .. } if waits >= MAX_RATE_LIMIT_WAITS => {
                    return pause(String::new(), String::new(), None);
                }
                RateLimitDecision::WaitAndRetry { secs } => {
                    waits += 1;
                    let wait = if quiet_first && waits == 1 {
                        QUIET_FIRST_RETRY
                    } else {
                        self.notice(format!("rate limited; retrying in {secs}s"));
                        std::time::Duration::from_secs(secs)
                    };
                    // The loop races this request against the stop button, so a
                    // cancel drops the sleep with it.
                    tokio::time::sleep(wait).await;
                }
                RateLimitDecision::Pause {
                    reset_at_display,
                    reset_label,
                    secs_until_reset,
                } => return pause(reset_at_display, reset_label, secs_until_reset),
            }
        }
    }
}

#[async_trait]
impl Plugin for RateLimitCodingPlugin {
    fn name(&self) -> &'static str {
        "rate-limit-coding"
    }
    fn description(&self) -> &'static str {
        "a 429 decided from the CodingPlan usage windows when on the gateway, the kernel's default otherwise"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        let row: RateLimitCodingRow = if config.is_null() {
            RateLimitCodingRow::default()
        } else {
            serde_json::from_value(config.clone()).map_err(|e| format!("bad config: {e}"))?
        };
        let hook = match &self.0 {
            Some(source) => {
                crate::rate_limit::RateLimitHook::with_source(row.base_url, source.clone())
            }
            None => crate::rate_limit::RateLimitHook::new(row.base_url),
        }
        .with_max_attempts(row.max_attempts);
        // Prepended, like the row it replaces: waiting has to happen outside every
        // other recovery, or each burns an attempt against a limit still in force.
        let _ = ctx.on_waterfall::<AgentRequest>(
            Arc::new(RateLimitCoding {
                ctx: ctx.clone(),
                hook,
            }),
            true,
        );
        Ok(())
    }
}

// ---- a compaction, stored when it commits -------------------------------------

/// `native-compaction-checkpoint`: a committed compaction reaches the native
/// store at once.
///
/// The chain hands the snapshot writer to the kernel as its compaction
/// checkpoint, so a compaction is durable before anyone is told it happened. A
/// tree commits a compaction as a log fact, and the native store otherwise hears
/// about it only when the turn ends — or never, for a `/compact` issued between
/// turns and followed by a quit. Written from the same commit, synchronously, so
/// the driver's `Compacted` arrives after the store has it.
pub(crate) struct NativeCompactionCheckpointPlugin(
    pub(crate) Arc<atomcode_capabilities::session::SnapshotHook>,
);

#[async_trait]
impl Plugin for NativeCompactionCheckpointPlugin {
    fn name(&self) -> &'static str {
        "native-compaction-checkpoint"
    }
    fn uses(&self) -> &'static [&'static str] {
        &["agents", "system-prompt"]
    }
    fn description(&self) -> &'static str {
        "write a committed compaction to the coding runtime's native session store"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let hook = self.0.clone();
        let listener_ctx = ctx.clone();
        let _ = ctx.on_emit::<atomcode_harness::events::SessionEventCommitted>(
            move |committed: &atomcode_harness::session::Committed| {
                if !matches!(committed.event, SessionEvent::Compacted { .. }) {
                    return;
                }
                let Some(agent) = listener_ctx
                    .service::<AgentsSvc>()
                    .and_then(|agents| agents.by_session(&committed.session))
                else {
                    return;
                };
                if agent.parent().is_some() {
                    return;
                }
                let system = listener_ctx
                    .service::<SystemPromptSvc>()
                    .map(|prompts| prompts.render());
                let convo =
                    crate::native_log::conversation_from_log(system, &agent.session().events());
                let snapshot = SessionSnapshot::from_conversation(&convo);
                use atomcode_kernel::checkpoint::CompactionCheckpoint;
                if let Err(error) = hook.save(&snapshot) {
                    eprintln!("[native-compaction-checkpoint] {error}");
                }
            },
        );
        Ok(())
    }
}

// ---- compaction, as the product writes it ---------------------------------

/// `compaction-coding`: stands where `compaction-tail` stands.
///
/// Pressure-triggered compaction stays the tree's model-free list — a compactor
/// that needs a model call cannot run when the provider is what is failing. A
/// `/compact` the person asks for is the chain's: the conversation's own model
/// writes the summary, updating the last one, steered by the focus they gave,
/// billed to the session through the side provider. Anything short of a written
/// summary falls back to the list.
pub(crate) struct CompactionCodingPlugin(pub(crate) atomcode_review::SharedReviewProvider);

#[derive(serde::Deserialize)]
struct CompactionCodingRow {
    #[serde(default = "default_compaction_threshold")]
    threshold: f32,
    #[serde(default = "default_keep_turns")]
    keep_turns: u64,
}

fn default_compaction_threshold() -> f32 {
    0.75
}

fn default_keep_turns() -> u64 {
    2
}

struct CodingCompaction {
    provider: atomcode_review::SharedReviewProvider,
    keep_turns: u64,
}

#[async_trait]
impl atomcode_harness::seams::Compaction for CodingCompaction {
    fn describe(&self) -> String {
        format!(
            "keep the last {} turn(s); list the rest, or have the model summarize them on request",
            self.keep_turns
        )
    }

    async fn compact(
        &self,
        log: &atomcode_harness::session::SessionLog,
    ) -> Option<atomcode_harness::seams::CompactionDecision> {
        use atomcode_harness::plugins::loop_policy;
        let span = loop_policy::settled_span(log, self.keep_turns)?;
        Some(atomcode_harness::seams::CompactionDecision {
            through: span.through,
            summary: loop_policy::listed_summary(&span),
        })
    }

    async fn compact_requested(
        &self,
        log: &atomcode_harness::session::SessionLog,
        focus: Option<&str>,
    ) -> Option<atomcode_harness::seams::CompactionDecision> {
        use atomcode_capabilities::compaction::{summarize_span, SUMMARY_TIMEOUT};
        use atomcode_harness::plugins::loop_policy;
        let span = loop_policy::settled_span(log, self.keep_turns)?;
        let provider = self.provider.read().ok().and_then(|slot| slot.clone());
        let written = match provider {
            Some(provider) => {
                let events: Vec<_> = log
                    .events()
                    .into_iter()
                    .filter(|logged| logged.seq <= span.through)
                    .collect();
                let messages = atomcode_harness::session::derive_messages(&events);
                tokio::time::timeout(
                    SUMMARY_TIMEOUT,
                    summarize_span(provider.as_ref(), &messages, focus),
                )
                .await
                .ok()
                .flatten()
            }
            None => None,
        };
        Some(atomcode_harness::seams::CompactionDecision {
            through: span.through,
            summary: written.unwrap_or_else(|| loop_policy::listed_summary(&span)),
        })
    }
}

#[async_trait]
impl Plugin for CompactionCodingPlugin {
    fn name(&self) -> &'static str {
        "compaction-coding"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["compaction"]
    }
    fn description(&self) -> &'static str {
        "history compaction: a model-free list under pressure, the conversation model's summary on request"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        let row: CompactionCodingRow = if config.is_null() {
            serde_json::from_value(serde_json::json!({})).map_err(|e| e.to_string())?
        } else {
            serde_json::from_value(config.clone()).map_err(|e| format!("bad config: {e}"))?
        };
        let _ = ctx
            .provide::<atomcode_harness::seams::CompactionSvc>(Arc::new(CodingCompaction {
                provider: self.0.clone(),
                keep_turns: row.keep_turns,
            }))
            .map_err(|e| e.to_string())?;
        atomcode_harness::plugins::loop_policy::mount_compaction_trigger(ctx, row.threshold);
        Ok(())
    }
}
