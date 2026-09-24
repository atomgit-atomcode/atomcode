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
//! with the id the runtime's binding already has, resumed from that session's
//! log in the session store (`session-store`, [`crate::session_store`]). The
//! log is the session's one authority (`docs/adr/0024`): a rebuilt tree replays
//! it, whatever it was rebuilt for.
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
use atomcode_harness::session::{
    derive_messages_with_meta, InjectionOrigin, SessionEvent, SessionLog,
};
use atomcode_kernel::agent::CommandDescription;
use atomcode_kernel::hook::{LifecycleHooks, TurnCtx};
use atomcode_kernel::message::{Conversation, Message, MessageMeta, Role, SessionSnapshot};
use atomcode_plexus::{Context, Listener, Next, Plugin, Waterfall};
use serde_json::Value;

/// The session a tree's own agent is created with.
#[derive(Clone, Default)]
pub struct SessionSeed {
    /// The binding's id. `None` for a sessionless runtime, which lets the tree
    /// mint one.
    pub id: Option<String>,
    /// What a sessionless runtime kept in memory to continue from. A session in
    /// the store continues from its log instead.
    pub snapshot: Option<SessionSnapshot>,
    /// The store the session is kept in, with the lease. `None` exactly when
    /// `id` is: a sessionless runtime keeps nothing.
    pub stored: Option<crate::session_store::StoredSession>,
    /// Replay the stored log. False only for a session not published yet,
    /// which has none.
    pub resume: bool,
}

impl SessionSeed {
    fn store(&self) -> Option<Arc<SessionManager>> {
        self.stored.as_ref().map(|stored| stored.store.clone())
    }
}

impl std::fmt::Debug for SessionSeed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionSeed")
            .field("id", &self.id)
            .field("snapshot", &self.snapshot)
            .field("store", &self.store().as_ref().map(|store| store.root()))
            .field("resume", &self.resume)
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

        let (Some(id), Some(store)) = (self.0.id.clone(), self.0.store()) else {
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
                 `<id>.events` is the session's log — every fact of the \
                 conversation, appended as it happens; a resume replays that file \
                 and nothing else; `<id>.index` holds its name, working directory \
                 and per-turn statistics.\n\
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
                match store.events_path(&id) {
                    Ok(path) if path.exists() => lines.push(format!(
                        "kept in: {} (the session store; a resume replays this file)",
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
        "the coding runtime's session: its id, resumed from its log in the session store"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        self.describe(ctx);
        // Only a runtime with no store continues from a conversation in memory.
        let seed = match (&self.0.stored, &self.0.snapshot) {
            (None, Some(snapshot)) => {
                atomcode_capabilities::session::events::events_from_snapshot(snapshot, 1)
            }
            _ => Vec::new(),
        };
        let _ = ctx
            .provide::<SessionDefaultsSvc>(Arc::new(SessionDefaults {
                id: self.0.id.clone(),
                resume: self.0.stored.is_some() && self.0.resume,
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
    /// The log's projection with its stats, for the request being made — see
    /// [`HostHooks::logged_view`].
    logged_view: Mutex<Option<(LoggedViewKey, Arc<Vec<Message>>)>>,
}

/// Which request a [`HostHooks::logged_view`] was made for: the session, how
/// far its log had got, and the turn and round asking.
type LoggedViewKey = (String, usize, u64, u32);

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

    /// The log's projection with each assistant message's stats, made once per
    /// request and shared by every hook's bridge.
    ///
    /// Each hook has a bridge of its own, and each would otherwise copy the
    /// whole log and project it again on every request — a cost that grows with
    /// the session and is paid once per mounted hook. A view that is stale by
    /// some path the key does not see costs nothing worse than the stats:
    /// [`with_logged_meta`] only lends them to messages that line up.
    fn logged_view(&self, log: &SessionLog, turn: u64, round: u32) -> Arc<Vec<Message>> {
        let key = (log.id().to_string(), log.len(), turn, round);
        let mut cached = self.logged_view.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((at, view)) = cached.as_ref() {
            if *at == key {
                return view.clone();
            }
        }
        let view = Arc::new(derive_messages_with_meta(&log.events()));
        *cached = Some((key, view.clone()));
        view
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
            host: self.0.clone(),
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
    /// The table the hook came from, which keeps the request's log view.
    host: Arc<HostHooks>,
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
            // THIS request, which is one past the highest already logged —
            // `TurnCtx::request_id` is documented as "the id of THIS LLM
            // request, 1-based". It used to be the highest logged id, so a hook
            // reading it got the PREVIOUS request's id, and `0` on the first
            // round of a session. The response meta below then wrote
            // `ctx.request_id + 1` to get the right number, which is how the
            // two disagreed: one place computed it, the other compensated.
            request_id: request_id + 1,
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

/// `messages` with the stats the log recorded for each assistant message.
///
/// A request is projected from the log without them (`derive_messages`), so
/// that nothing about what the model is sent changes. The hooks it is handed to
/// were written against a kernel conversation whose assistant messages carry
/// them, and one reads them: the todo hook places each call by its turn and
/// round to know which ones its sidecar already reflects. Handed the bare
/// projection, no call had a place, none was laid over the sidecar, and after a
/// compaction the model marked a task completed and was shown it in progress
/// again — until the tool-loop guard stopped it for repeating the update.
///
/// The request is the system prompt, then the log's projection, then whatever
/// tails rode along; the stats go on the projection's span only when it lines
/// up message for message. When it does not, the messages go as they are.
fn with_logged_meta(messages: &[Message], logged: &[Message]) -> Vec<Message> {
    let mut out = messages.to_vec();
    let start = usize::from(
        out.first()
            .is_some_and(|m| m.role == Role::System && !m.synthetic),
    );
    let Some(span) = out.get_mut(start..start + logged.len()) else {
        return out;
    };
    let lines_up = span.iter().zip(logged).all(|(sent, kept)| {
        sent.role == kept.role && sent.text == kept.text && sent.tool_calls == kept.tool_calls
    });
    if lines_up {
        for (sent, kept) in span.iter_mut().zip(logged) {
            if sent.meta.is_none() {
                sent.meta = kept.meta.clone();
            }
        }
    }
    out
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
            // Which checkpoint this turn started from, in the log beside it:
            // what a rewind of the workspace to before it restores.
            if let Some(id) = self.hook.checkpoint_taken() {
                atomcode_harness::session::commit(
                    agent.ctx(),
                    &agent.session(),
                    SessionEvent::Checkpointed { turn: req.turn, id },
                );
            }
        }
        let ctx = self.turn_ctx(&agent, req.turn, req.round);
        // `pre_request` may only ADD to the end. What a hook appends is an
        // ephemeral tail for this request — a reminder, a nudge — and rides the
        // same way the harness's own tails do: past the log check, never logged.
        // A hook that rewrote the history instead would be putting words in
        // front of the model that no log can explain, so its edits are dropped.
        let logged = self.host.logged_view(&agent.session(), req.turn, req.round);
        let mut proposed = with_logged_meta(&req.messages, &logged);
        let handed = proposed.clone();
        let before = proposed.len();
        self.hook.pre_request(&mut proposed, &ctx).await;
        if proposed.len() > before && proposed[..before] == handed[..] {
            req.messages.extend(proposed.drain(before..));
        }
        self.hook
            .pre_request_options(&req.messages, &mut req.options, &ctx)
            .await;
        self.hook
            .on_request(&req.messages, &req.tools, &req.options, &ctx)
            .await;

        // The span a `MessageMeta` calls `elapsed_ms`: the request, from handing
        // it on to getting an answer back. The same one `agent_loop`'s
        // `response_meta` measures for the session log — measured again here
        // because the meta that reaches a hook is built here, not there.
        let started = std::time::Instant::now();
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
                    // Both were `..default()` — that is, zero — until the
                    // telemetry parity harness caught it: every `llm_chat` the
                    // product reported carried `duration_ms = 0`, so the whole
                    // latency series was flat. Nothing failed; the field was
                    // simply never filled in on this path.
                    elapsed_ms: started.elapsed().as_millis() as u64,
                    utilization: match ctx_window {
                        0 => 0.0,
                        window => tokens.prompt as f32 / window as f32,
                    },
                    round: req.round,
                    turn_id: req.turn,
                    // Already this request's id; see `turn_ctx`.
                    request_id: ctx.request_id,
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

/// `tools-host`: the tool catalog, over switches the runtime keeps.
///
/// Stands where the harness's `tools` row stands and builds the same catalog
/// the same way — the difference is whose switches it answers to. A person who
/// turned a tool off in this session did not mean "until the next `/model`",
/// and the tree is rebuilt for a good many reasons that have nothing to do with
/// the catalog (`docs/adr/0022` §2). The runtime holds the switches, so the
/// rebuilt catalog starts where the old one left off.
pub(crate) struct ToolsHostPlugin {
    pub(crate) switches: Arc<atomcode_harness::seams::ToolSwitches>,
    /// Where to publish the catalog once built, for a runtime that has to
    /// answer a front end about it. `None` for a host that only wants the
    /// switches to survive.
    pub(crate) slot: Option<Arc<RwLock<Option<Arc<atomcode_harness::seams::ToolBox>>>>>,
}

#[async_trait]
impl Plugin for ToolsHostPlugin {
    fn name(&self) -> &'static str {
        "tools-host"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["tools"]
    }
    fn uses(&self) -> &'static [&'static str] {
        &["commands", "operations"]
    }
    fn description(&self) -> &'static str {
        "the live tool catalog, over the runtime's own on/off switches"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        atomcode_harness::plugins::registries::mount_catalog(
            ctx,
            config,
            Some(self.switches.clone()),
        )?;
        if let Some(slot) = self.slot.clone() {
            let catalog = ctx
                .require::<atomcode_harness::seams::ToolsSvc>()
                .map_err(|e| e.to_string())?;
            *slot.write().unwrap_or_else(|e| e.into_inner()) = Some(catalog);
            // Out with the row: a runtime holding a catalog whose rows have
            // unloaded would answer about tools nobody can call.
            let _ = ctx.effect(move || {
                *slot.write().unwrap_or_else(|e| e.into_inner()) = None;
            });
        }
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
        // `commands` because this row took the harness `skills` row's place, and
        // that row registers the command catalog entries for skills. Replacing a
        // row means taking over what it did: without this, no skill is a command
        // in this runtime at all — a person's own skill included, and `/setup`
        // answered `NotFound` the first time it was forwarded here.
        &["operations", "commands"]
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
        atomcode_harness::plugins::tools::mount(
            ctx,
            vec![
                Arc::new(atomcode_capabilities::skills::UseSkillTool::new(
                    registry.clone(),
                )) as Arc<dyn atomcode_kernel::tool::Tool>,
                Arc::new(atomcode_capabilities::skills::ListSkillsTool::new(
                    registry.clone(),
                )),
            ],
        )?;
        if let Some(catalog) = catalog.as_ref().filter(|c| !c.trim().is_empty()) {
            let (id, rank) = crate::on_harness::SKILLS_FRAGMENT;
            atomcode_harness::plugins::tools::contribute_prompt(ctx, id, rank, catalog);
        }
        // The command catalog half of what the harness `skills` row did: `/skills`
        // and one command per user-invocable skill — this runtime's own `/setup`
        // among them, and anything a person wrote into `.atomcode/skills/`.
        //
        // Declared here rather than left to the row this one replaces, because a
        // swapped row is not mounted at all: the mechanism has to be named by the
        // row that took its place, or it silently stops existing.
        atomcode_harness::plugins::capabilities::register_skill_commands(ctx, registry)?;
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
        // `commands`: the product's `code_review` is a thing a person asks for
        // directly as well, as `/review`.
        &["system-prompt", "commands"]
    }
    fn description(&self) -> &'static str {
        "tools the coding runtime built itself: its controllers' and its capability graph's"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        atomcode_harness::plugins::tools::mount(ctx, self.0.clone())?;
        // The product disables the harness's `tool-code-review` row and mounts
        // its own reviewer here (the product's limits and provider slot), so
        // the command that row would have registered is this row's to register
        // — or `/review` does not exist on the product at all, which is what it
        // was until 2026-09-22.
        if let Some(review) = self.0.iter().find(|tool| tool.name() == "code_review") {
            atomcode_harness::plugins::capabilities::register_review_command(ctx, review.clone())?;
        }
        let mut guided = std::collections::BTreeSet::new();
        for tool in &self.0 {
            if let Some((key, text)) = crate::persona::host_tool_guidance(tool.name()) {
                if guided.insert(key) {
                    let id = format!("host-tool-{key}");
                    // Beside the rows that describe their own tools.
                    let rank = match key {
                        "ask" => 56,
                        "code-review" => 58,
                        _ => 57,
                    };
                    atomcode_harness::plugins::tools::contribute_prompt(ctx, &id, rank, text);
                }
            }
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
        // Withdrawn with the row, like every other fragment.
        atomcode_harness::plugins::tools::contribute_prompt(ctx, "project-instructions", 1, &block);
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

/// What `mcp-telemetry` needs: the second stream of connection events, and the
/// meter to report them through.
pub(crate) struct McpConnectMeter {
    pub(crate) events:
        tokio::sync::mpsc::UnboundedReceiver<atomcode_capabilities::mcp::McpConnectEvent>,
    pub(crate) meter: crate::telemetry::McpTelemetry,
}

/// `mcp-telemetry`: one report per MCP connection attempt.
///
/// Its own row rather than a few lines inside `mcp-host`, for the reason the
/// MCP module has stated since the port — cross-cutting reporting belongs on
/// the seam, not hard-coded into whoever else happens to be listening. Read
/// concretely: `mcp-host`'s job is to publish tools, it only cares about
/// `Connected`, and a meter living inside it would be switched off by every
/// change to what that row listens for. Here, the two cannot drift.
///
/// Mounted only when the host has both MCP and a telemetry sink, so a tree
/// without one simply has no such row (`--dump-config` shows this).
pub(crate) struct McpTelemetryPlugin(pub(crate) Mutex<Option<McpConnectMeter>>);

#[async_trait]
impl Plugin for McpTelemetryPlugin {
    fn name(&self) -> &'static str {
        "mcp-telemetry"
    }
    fn uses(&self) -> &'static [&'static str] {
        &["mcp"]
    }
    fn description(&self) -> &'static str {
        "reports each MCP server connection attempt to the host's telemetry"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let Some(McpConnectMeter { mut events, meter }) =
            self.0.lock().unwrap_or_else(|e| e.into_inner()).take()
        else {
            return Err("mcp-telemetry mounted twice from one meter".into());
        };
        let task = tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                meter.report(&event);
            }
        });
        // A late connection from a withdrawn tree must not be reported against
        // the tree that replaced it.
        let _ = ctx.effect(move || task.abort());
        Ok(())
    }
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
        // Under this row's name, so a host narrowing the catalog can write
        // either `mcp__github__*` or `mcp-host:mcp__github__*`
        // (`atomcode_harness::seams::ToolPolicy`). A tool the policy keeps out
        // is not in the catalog afterwards, so it does not go on the published
        // list either — `names` is what this row will withdraw later.
        if toolbox.register_from("mcp-host", adapter).is_ok()
            && toolbox.get(&name).is_some()
            && !names.contains(&name)
        {
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

        // A tree mounted over servers that are already up — a remount on the same
        // parts, which an undo or a restored snapshot is — offers their tools
        // before the row returns, from what the registry last listed. Left to the
        // task below, the first request after the remount could go out before it
        // ran, with no MCP tools at all. The task's live listing then reconciles.
        publish_mcp(&toolbox, &registry, registry.listed_tools(), &shared, true).await;

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
                            Some(atomcode_capabilities::mcp::McpConnectEvent::Connected { name, .. }) => {
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
        // The task and the tools leave with the row — unless another row has taken
        // over the same parts' publication since (a remount mounts the new tree
        // before it drops this one). The name list is shared and by then holds the
        // new tree's tools: draining it here would leave them registered there but
        // unlisted, and `withdraw_mcp_tools`, which unregisters by this list, would
        // then leave revoked tools in front of the model.
        let _ = ctx.effect(move || {
            task.abort();
            let still_mounted = toolbox_slot
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &toolbox));
            if !still_mounted {
                return;
            }
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
                if !matches!(
                    committed.event,
                    SessionEvent::Compacted { .. } | SessionEvent::MessagesRewritten { .. }
                ) {
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
/// The product's compaction is the kernel's strategy, decided against the log:
/// under pressure, old tool output is folded in place and the conversation's
/// words kept (`read_file` exempt, the active turn whole); past 0.78 of the
/// window, older turns are summarized by the conversation's own model — the
/// last summary updated rather than rewritten, the first request and about a
/// quarter of the window of recent turns kept; on an overflow, stub → truncate
/// → summarize, splitting a turn too long to keep whole. A `/compact` the
/// person asks for is that summary, steered by their focus. Every summary is
/// billed to the session through the side provider, and every way it can fail
/// falls back to something model-free.
pub(crate) struct CompactionCodingPlugin(pub(crate) atomcode_review::SharedReviewProvider);

#[derive(serde::Deserialize)]
struct CompactionCodingRow {
    #[serde(default = "default_compaction_threshold")]
    threshold: f32,
}

fn default_compaction_threshold() -> f32 {
    0.75
}

struct CodingCompaction {
    provider: atomcode_review::SharedReviewProvider,
}

impl CodingCompaction {
    /// Resolved per call: `/model` and a logout swap the provider under it.
    fn strategy(&self) -> (atomcode_capabilities::compaction::OverflowCompaction, bool) {
        let provider = self.provider.read().ok().and_then(|slot| slot.clone());
        let writes = provider.is_some();
        (
            atomcode_capabilities::compaction::OverflowCompaction::new(
                atomcode_capabilities::compaction::StubCompaction::default(),
                provider,
            ),
            writes,
        )
    }
}

#[async_trait]
impl atomcode_harness::seams::Compaction for CodingCompaction {
    fn describe(&self) -> String {
        "fold old tool output under pressure; past 0.78 of the window, or on request, have the \
         conversation's model summarize older turns; on an overflow, stub, truncate, then summarize"
            .into()
    }

    fn calls_model(
        &self,
        log: &atomcode_harness::session::SessionLog,
        ask: &atomcode_harness::seams::CompactionAsk,
    ) -> bool {
        let (strategy, writes) = self.strategy();
        writes
            && atomcode_harness::plugins::compaction::strategy_would_summarize(&strategy, log, ask)
    }

    async fn compact(
        &self,
        log: &atomcode_harness::session::SessionLog,
        ask: &atomcode_harness::seams::CompactionAsk,
    ) -> Option<atomcode_harness::seams::CompactionDecision> {
        let (strategy, _) = self.strategy();
        atomcode_harness::plugins::compaction::decide_with_strategy(&strategy, log, ask).await
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
            }))
            .map_err(|e| e.to_string())?;
        atomcode_harness::plugins::loop_policy::mount_compaction_trigger(ctx, row.threshold);
        Ok(())
    }
}

// ---- the runtime's own capabilities, as commands a person runs ---------------
//
// goal, loop, the local-context queue and the policy intervention are
// capabilities, not host controls: they belong to a row, and a front end reaches
// them through the command catalog like any other row's command
// (`docs/adr/0021` §3, §10). The two policy errors are decided here rather than
// in the host contract (§8) — whether anything is waiting, and whether what a
// person typed is one of its choices, are this row's judgements.

/// `capability-commands`: goal, loop, queue and policy, in the catalog.
pub(crate) struct CapabilityCommandsPlugin(pub(crate) Arc<dyn crate::runtime::RuntimeCommands>);

#[async_trait]
impl Plugin for CapabilityCommandsPlugin {
    fn name(&self) -> &'static str {
        "capability-commands"
    }
    fn uses(&self) -> &'static [&'static str] {
        &["commands"]
    }
    fn description(&self) -> &'static str {
        "goal, loop, the local-context queue, the policy intervention and `worktree`, as commands"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        for command in [
            Arc::new(GoalCommand(self.0.clone()))
                as Arc<dyn atomcode_harness::commands::CatalogCommand>,
            Arc::new(LoopCommand(self.0.clone())),
            Arc::new(QueueCommand(self.0.clone())),
            Arc::new(PolicyCommand(self.0.clone())),
            Arc::new(WorktreeCommand(self.0.clone())),
        ] {
            atomcode_harness::commands::register(ctx, command)?;
        }
        Ok(())
    }
}

/// A command on the conversation as a whole — never on one member: a member
/// runs in the same runtime, and starting a goal "on" it would be starting one
/// on the conversation under another name.
fn on_the_session(name: &str, usage: Option<&str>, summary: &str) -> CommandDescription {
    CommandDescription {
        name: name.into(),
        usage: usage.map(str::to_string),
        summary: summary.into(),
        target: atomcode_kernel::agent::CommandTarget::Session,
    }
}

/// Only for the conversation itself. A delegated agent is driven by its lead,
/// and these drive the runtime.
fn the_conversation_itself(agent: &atomcode_harness::agent::Agent) -> bool {
    agent.parent().is_none()
}

/// `worktree <名字> [基准] | list | done | cleanup <名字> [--force]`: a branch of
/// one's own with a checkout of its own, and the step that takes you into it.
///
/// **A catalog command rather than a host command** (`docs/adr/0021` §3, the
/// same place goal and loop live). Host control is a *neutral* contract — a
/// front end asks it for things that mean something to any host, a coding
/// runtime or a daemon or a test — and `git worktree` means nothing to a host
/// that is not driving a repository.
///
/// **Making the checkout and going into it are one gesture**, which is the
/// thing this command got wrong the first time: it said where it had made one
/// and left the person to type `/cd`. The step into a quieter tree is the whole
/// point of asking, and the runtime already owns that transition
/// ([`crate::runtime::RuntimeCommands::change_directory`], `docs/adr/0001`) —
/// so `done` is not a remembered path either, it is the repository's own main
/// checkout, which survives restarts where a slot in a front end's memory does
/// not.
struct WorktreeCommand(Arc<dyn crate::runtime::RuntimeCommands>);

#[async_trait]
impl atomcode_harness::commands::CatalogCommand for WorktreeCommand {
    fn describe(&self) -> CommandDescription {
        on_the_session(
            "worktree",
            Some("[create] <名字> [基准] | list | done | cleanup <名字> [--force]"),
            "开一个自己的分支与 checkout 并进去干活;`list` 看有哪些,`done` 回主检出,`cleanup` 清掉",
        )
    }
    fn offered_for(&self, agent: &atomcode_harness::agent::Agent) -> bool {
        the_conversation_itself(agent)
    }
    async fn run(
        &self,
        agent: Arc<atomcode_harness::agent::Agent>,
        args: &str,
    ) -> Result<String, String> {
        let root = agent
            .ctx()
            .service::<atomcode_harness::seams::FsSvc>()
            .map(|fs| fs.root())
            .ok_or_else(|| "这个会话没有工作区".to_string())?;
        run_worktree(&root, args, &self.0).await
    }
}

/// The command's own body, apart from the agent it is reached through.
///
/// Split out because what it does is about a **repository and the runtime**, and
/// neither is the agent: the tests below drive it against a real repository and
/// a runtime that records where it was sent, which is the only way to tell
/// "made a worktree and went there" from "made a worktree and said so".
async fn run_worktree(
    root: &std::path::Path,
    args: &str,
    runtime: &Arc<dyn crate::runtime::RuntimeCommands>,
) -> Result<String, String> {
    let parts: Vec<&str> = args.split_whitespace().collect();
    // The three words a person types instead of a name. A branch named `list`
    // is therefore unreachable — tuix read it the same way, and the alternative
    // is a command whose subcommands depend on the repository.
    match parts.first().copied() {
        None => Err(WORKTREE_USAGE.into()),
        Some("list") => list_worktrees(root),
        Some("done") => {
            let at = main_checkout(root)?;
            runtime.change_directory(at.clone()).await?;
            Ok(format!("回到 {} 干活", at.display()))
        }
        Some("cleanup") => {
            let name = parts
                .get(1)
                .ok_or_else(|| "要一个名字:`/worktree cleanup 试一下`".to_string())?;
            let force = parts
                .get(2)
                .is_some_and(|flag| matches!(*flag, "--force" | "-f"));
            cleanup(root, name, force, runtime).await
        }
        // `create` 是上一代前端的写法,也是人会自己想到的写法。不认它的后果
        // 不是报错而是**建一个叫 `create` 的分支并切进去**,后面那个名字变成
        // 基准、再后面的静默丢掉 —— 一条看起来成功了的命令。
        Some(_) => {
            let Some((name, base)) = worktree_target(&parts) else {
                return Err("要一个名字:`/worktree create 试一下`".into());
            };
            let at = worktree(root, name, base)?;
            runtime.change_directory(at.clone()).await?;
            Ok(format!("在 worktree `{name}` 里干活:{}", at.display()))
        }
    }
}

/// 哪个名字、以哪里为基准 —— 带不带 `create` 都一样。
///
/// `create` 是上一代前端的写法,也是人会自己想到的写法。不认它的后果
/// 不是报错,是把它当成名字去建一个叫 `create` 的分支并切进去,后面那个
/// 名字变成基准、再后面的静默丢掉。
fn worktree_target<'a>(parts: &[&'a str]) -> Option<(&'a str, Option<&'a str>)> {
    let rest = match parts.first().copied() {
        Some("create") => &parts[1..],
        _ => parts,
    };
    Some((*rest.first()?, rest.get(1).copied()))
}

const WORKTREE_USAGE: &str =
    "用法:`/worktree [create] <名字> [基准]` 开一个自己的 checkout 并进去 · \
     `/worktree list` 看有哪些 · `/worktree done` 回主检出 · \
     `/worktree cleanup <名字> [--force]` 清掉";

/// What `list` answers: every worktree git has, the person's own place marked.
///
/// Read from git rather than from disk, and for the reason `worktree` itself
/// reads registration: a directory git has forgotten is not somewhere anyone can
/// work.
fn list_worktrees(root: &std::path::Path) -> Result<String, String> {
    let here = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let mut text = String::new();
    for checkout in git_worktrees(root)? {
        let same = std::fs::canonicalize(&checkout.path)
            .map(|at| at == here)
            .unwrap_or(false);
        text.push_str(&format!(
            "  {} {:<16} {}{}\n",
            if same { "●" } else { "○" },
            checkout.branch.as_deref().unwrap_or("(detached)"),
            checkout.path.display(),
            if same { " ← 当前" } else { "" },
        ));
    }
    if text.is_empty() {
        return Ok("这个仓库没有 worktree".into());
    }
    Ok(format!("worktree:\n{text}"))
}

/// Remove the checkout `name` git has, and say where the person ended up when it
/// was the one they were standing in.
///
/// Standing in the directory being removed is the case that needs care: git
/// refuses to remove the worktree it is run from, so the person is moved to the
/// main checkout through the runtime *first*, and only then is the checkout
/// taken away. Doing it in the other order answers "cleaned up" about a
/// checkout that is still there.
async fn cleanup(
    root: &std::path::Path,
    name: &str,
    force: bool,
    runtime: &Arc<dyn crate::runtime::RuntimeCommands>,
) -> Result<String, String> {
    let checkout = git_worktree_path(root, name)?
        .ok_or_else(|| format!("git 没有 `{name}` 这个 worktree;`/worktree list` 看有哪些"))?;
    let here = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let removing_this_one = std::fs::canonicalize(&checkout)
        .map(|at| at == here)
        .unwrap_or(false);
    let mut moved = None;
    if removing_this_one {
        let main = main_checkout(root)?;
        runtime.change_directory(main.clone()).await?;
        moved = Some(main);
    }
    remove_worktree(root, name, force)?;
    Ok(match moved {
        Some(main) => format!("清掉 worktree `{name}`,回到 {} 干活", main.display()),
        None => format!("清掉 worktree `{name}`"),
    })
}

/// Take the checkout `name` away, or say why not.
///
/// A checkout with uncommitted work is refused by git unless forced, and that
/// refusal is the answer rather than a failure: the person is told what is in
/// the way and how to say otherwise, instead of losing work to a tidy-up.
fn remove_worktree(root: &std::path::Path, name: &str, force: bool) -> Result<(), String> {
    let checkout = git_worktree_path(root, name)?
        .ok_or_else(|| format!("git 没有 `{name}` 这个 worktree;`/worktree list` 看有哪些"))?;
    let path_arg = checkout.display().to_string();
    let mut args = vec!["worktree", "remove"];
    if force {
        args.push("--force");
    }
    args.push(&path_arg);
    // Run from the repository, never from the checkout being removed: git
    // refuses to remove the one it is standing in, and a person cleaning up
    // the checkout they are *in* has already been moved out by `cleanup`.
    let repository = main_checkout(root).unwrap_or_else(|_| root.to_path_buf());
    if let Err(error) = git_out(&repository, &args) {
        let dirty = !force
            && ["untracked", "modified", "changes"]
                .iter()
                .any(|word| error.contains(word));
        return Err(if dirty {
            format!("`{name}` 里还有没提交的改动;要清掉就 `/worktree cleanup {name} --force`")
        } else {
            format!("清不掉 `{name}`:{error}")
        });
    }
    Ok(())
}

/// One worktree git has registered: what branch is checked out there, and where.
struct Checkout {
    path: std::path::PathBuf,
    branch: Option<String>,
}

/// Every worktree git has, in git's own order — the main checkout first.
///
/// The order is load-bearing rather than incidental: it is what makes
/// [`main_checkout`] a fact from the repository instead of a path this file
/// guessed, and the main checkout is the one place `/worktree done` can always
/// go back to.
fn git_worktrees(root: &std::path::Path) -> Result<Vec<Checkout>, String> {
    let listing = git_out(root, &["worktree", "list", "--porcelain"])?;
    let mut out: Vec<Checkout> = Vec::new();
    let mut path: Option<std::path::PathBuf> = None;
    let mut branch: Option<String> = None;
    let flush = |path: &mut Option<std::path::PathBuf>,
                 branch: &mut Option<String>,
                 out: &mut Vec<Checkout>| {
        if let Some(path) = path.take() {
            out.push(Checkout {
                path,
                branch: branch.take(),
            });
        }
    };
    for line in listing.lines() {
        if let Some(at) = line.strip_prefix("worktree ") {
            flush(&mut path, &mut branch, &mut out);
            path = Some(std::path::PathBuf::from(at));
        } else if let Some(name) = line.strip_prefix("branch refs/heads/") {
            branch = Some(name.trim().to_string());
        } else if line.is_empty() {
            flush(&mut path, &mut branch, &mut out);
        }
    }
    flush(&mut path, &mut branch, &mut out);
    Ok(out)
}

/// The repository's own main checkout — where `done` goes back to.
fn main_checkout(root: &std::path::Path) -> Result<std::path::PathBuf, String> {
    git_worktrees(root)?
        .into_iter()
        .next()
        .map(|checkout| checkout.path)
        .ok_or_else(|| "git 没说主检出在哪".to_string())
}

/// Make (or find) the worktree `name` under `root`, and say where it is.
///
/// `.worktrees/<name>` inside the repository, which is where this repository
/// already puts them — a worktree beside the checkout instead would land in
/// whatever directory happens to be the parent, which on a machine with several
/// checkouts is somebody else's.
///
/// An existing one is *found*, not an error: `/worktree x` twice is a person
/// going back to what they opened, and refusing the second would make the
/// command something you have to remember whether you already ran.
///
/// **"Found" means git has it checked out, not that the directory is there**,
/// and the difference is the whole reason this asks git: a `name` directory
/// left behind by a `rm -rf` of a checkout git still has registered is not a
/// worktree, and answering `/cd` with it sends the person somewhere git does
/// not know about. [`git_worktree_path`] is the registration, and its answer
/// is the only one taken.
///
/// **An existing branch of that name is taken over, never reset** — and that
/// is why the branch decides the flags rather than a flag deciding the branch.
/// `git worktree add -B <name>` *moves the branch* to HEAD (git says so:
/// `resetting branch 'x'; was at <sha>`), quietly dropping whatever it was
/// pointing at, which for a name a person already used is their work. Without
/// a branch switch an existing one is checked out where it stands, and only a
/// name nobody has used gets `-b`.
fn worktree(
    root: &std::path::Path,
    name: &str,
    base: Option<&str>,
) -> Result<std::path::PathBuf, String> {
    // A name, not a path: it becomes both a directory under `.worktrees` and a
    // branch, and a `..` in it would put the checkout outside the repository.
    if name.is_empty()
        || name.contains(['/', '\\'])
        || name.starts_with('-')
        || name.starts_with('.')
    {
        return Err(format!(
            "`{name}` 不能当 worktree 的名字:要一个不带路径分隔符的名字"
        ));
    }
    // `FsSvc::root()` is the working directory when the world is unfenced, so
    // this can be a subdirectory: `git -C` finds the repository from it, but
    // `.worktrees` has to hang off the *repository*, or `/worktree x` means
    // something different depending on where in the tree the person is.
    let root = match git_out(root, &["rev-parse", "--show-toplevel"]) {
        Ok(top) if !top.trim().is_empty() => std::path::PathBuf::from(top.trim()),
        // No repository, or no git. The failure is the next command's to
        // report, in git's own words, rather than a guess made here.
        _ => root.to_path_buf(),
    };
    let at = root.join(".worktrees").join(name);

    if let Some(existing) = git_worktree_path(&root, name)? {
        return Ok(existing);
    }
    // Left over from a checkout git no longer has: `add` would refuse the path
    // and the person would be told "already exists" about something they
    // cannot see. Only a directory git has forgotten is cleared.
    if at.is_dir() {
        std::fs::remove_dir_all(&at)
            .map_err(|e| format!("{} 是上次剩下的目录,但清不掉:{e}", at.display()))?;
    }

    // Two shapes, because `-b` takes the branch name as its own argument:
    // `git worktree add -b <name> <path> [<base>]` for a name nobody has used,
    // and `git worktree add <path> <name>` to check out an existing branch
    // where it stands. One ordering for both reads the path as a branch (or a
    // branch as the start-point) and git refuses it, which is what it did here.
    //
    // `base` is where a *new* branch starts, and it is only meaningful there: an
    // existing branch is checked out where it already points, which is the
    // "taken over, never reset" rule above. A base named for an existing branch
    // is therefore ignored rather than silently moving it.
    let at_arg = at.display().to_string();
    let mut args = vec!["worktree", "add"];
    let branch_exists = git_out(
        &root,
        &["rev-parse", "--verify", &format!("refs/heads/{name}")],
    )
    .map(|sha| !sha.trim().is_empty())
    .unwrap_or(false);
    if branch_exists {
        args.push(&at_arg);
        args.push(name);
    } else {
        args.push("-b");
        args.push(name);
        args.push(&at_arg);
        if let Some(base) = base.filter(|b| !b.trim().is_empty()) {
            args.push(base);
        }
    }
    git_out(&root, &args).map_err(|e| format!("git 拒绝了:{e}"))?;

    // `.worktrees` inside the repository is what shows up in the person's own
    // `git status` as `?? .worktrees/` the moment this command does its job.
    // The repository's own ignore rules come first; only when none of them
    // says anything does the local exclude get the line — local to this
    // checkout, so it travels with neither the history nor anybody else.
    let _ = exclude_worktrees_dir(&root);

    match git_worktree_path(&root, name)? {
        Some(checked_out) => Ok(checked_out),
        // `add` reported success and git still has no worktree of that name:
        // say so rather than hand back a path on the strength of an exit
        // status. Nothing here is allowed to answer with a directory it has
        // not confirmed.
        None => Err(format!(
            "git 说建好了,但 `git worktree list` 里没有 `{name}`:{}",
            at.display()
        )),
    }
}

/// Where git has `name` checked out, if git has it at all — the registration,
/// not the directory. `git worktree list --porcelain` is the one that settles
/// it (a leftover directory keeps its own name registered nowhere).
///
/// The path is git's own answer rather than the one this file computed, which
/// is also what makes it the right one: on macOS a `/var` tempdir and git's
/// `/private/var` are the same place spelled two ways, and the answer `/cd`
/// gets should be the spelling git itself resolves.
fn git_worktree_path(
    root: &std::path::Path,
    name: &str,
) -> Result<Option<std::path::PathBuf>, String> {
    let listing = git_out(root, &["worktree", "list", "--porcelain"])?;
    let mut at: Option<std::path::PathBuf> = None;
    for line in listing.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            at = Some(std::path::PathBuf::from(path));
        } else if let Some(branch) = line.strip_prefix("branch refs/heads/") {
            if branch.trim() == name {
                return Ok(at);
            }
        } else if line.is_empty() {
            // `worktree <path>` always precedes its `branch` line, so a blank
            // line ends one entry; a worktree with none never matches `name`.
            at = None;
        }
    }
    Ok(None)
}

/// Keep `.worktrees/` out of the person's own `git status`, without touching
/// anyone's tracked files: the local exclude when the ignore rules are silent,
/// and nothing at all when they are not.
fn exclude_worktrees_dir(root: &std::path::Path) -> Result<(), String> {
    // `check-ignore` over the *contents* answers for the directory an entry
    // would have to name. Exit 0 is "some rule covers it" (the repository's,
    // the person's, or a line already written below); exit 1 is "no rule
    // does", and anything else is git being unable to say.
    let probe = root.join(".worktrees").join(".probe");
    let covered = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["check-ignore", "-q"])
        .arg(&probe)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    if matches!(covered.map(|s| s.code()), Ok(Some(0)) | Ok(None)) {
        return Ok(());
    }

    let git_path = |arg: &str| -> Result<std::path::PathBuf, String> {
        let out = git_out(root, &["rev-parse", "--git-path", arg])?;
        let path = std::path::PathBuf::from(out.trim());
        Ok(if path.is_absolute() {
            path
        } else {
            root.join(path)
        })
    };
    let exclude = git_path("info/exclude")?;
    if let Some(parent) = exclude.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return Ok(());
        }
    }
    let mut existing = std::fs::read_to_string(&exclude).unwrap_or_default();
    if existing
        .lines()
        .any(|line| matches!(line.trim(), ".worktrees/" | ".worktrees" | "/.worktrees/"))
    {
        return Ok(());
    }
    if !existing.is_empty() && !existing.ends_with('\n') {
        existing.push('\n');
    }
    existing.push_str(".worktrees/\n");
    std::fs::write(&exclude, existing).map_err(|e| format!("写不了 {}:{e}", exclude.display()))
}

/// Run one git command in `root` and hand back its stdout, or git's own words
/// on failure. `-C` rather than a directory on the child: the same call works
/// for the repository root and for the subdirectory `FsSvc::root()` may be.
fn git_out(root: &std::path::Path, args: &[&str]) -> Result<String, String> {
    let mut git = std::process::Command::new("git");
    git.arg("-C")
        .arg(root)
        .args(args)
        .stdin(std::process::Stdio::null());
    atomcode_capabilities::process_utils::suppress_console_window_sync(&mut git);
    let out = git.output().map_err(|e| format!("起不了 git:{e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// `worklog [日期]`: the day's work, recapped across every project.
///
/// **A catalog command, and here for the reason `/worktree` is** — but with one
/// more thing behind it. The substance already existed twice over:
/// [`atomcode_capabilities::session::collect_day_turns`] gathers a local day's
/// completed turns from the *whole* session store (every project, not this one),
/// and [`atomcode_capabilities::session::build_worklog_prompt`] pre-computes the
/// durations and flags so the model only has to fill the template. What was
/// missing was a registration: the classic front end carried this command in its
/// own table, and the row-assembled screen reads the command catalog instead, so
/// on that screen the command had simply never existed
/// (`docs/plans/2026-09-18-tui-panels-and-commands-inventory.md` B1-5, and
/// `docs/adr/0021` §10 for why the row that owns a capability registers it).
///
/// Registered only when this runtime keeps a session store — the row is where a
/// `session-store` exists as much as where a `session` does. A day recap is
/// *about* the store, and one that had no store to read would inject a
/// confident "there is no work" over a history it never looked at.
///
/// **A person reads the result as their own turn.** `agent.send` queues it with
/// [`MessageOrigin::User`], so the turn it starts appears in the transcript the
/// way everything they typed does, and `undo` reaches it
/// (`docs/adr/0024` — the log is the authority, and this must be in it).
/// `init`: write (or improve) the instruction file this project's agents read.
///
/// **A catalog command, and here for the reason `/worklog` is.** The substance
/// already existed — [`crate::build_init_prompt`] picks the built-in prompt for
/// the configured language and appends the person's own requirements from
/// `init_prompt_file` — and what was missing was a registration: the classic
/// front end carried this command in its own table, and the row-assembled
/// screen reads the command catalog, so on that screen `/init` had simply never
/// existed (`docs/plans/2026-09-18-tui-panels-and-commands-inventory.md` B1-4).
///
/// The configuration is read **when the command runs**, not when the row
/// mounts, for the reason the settings panel holds a path rather than a loaded
/// `Config`: a file edited behind the screen's back is whatever the file says.
pub(crate) struct InitPlugin {
    pub(crate) language: Option<atomcode_config::locale::Locale>,
    /// The settings file this runtime was configured from, when it was. `None`
    /// means there is nothing to read, so the built-in prompt stands.
    pub(crate) config_file: Option<std::path::PathBuf>,
}

#[async_trait]
impl Plugin for InitPlugin {
    fn name(&self) -> &'static str {
        "init"
    }
    fn uses(&self) -> &'static [&'static str] {
        &["commands"]
    }
    fn description(&self) -> &'static str {
        "`/init`: have the model read this repository and write the instruction file its agents load"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        atomcode_harness::commands::register(
            ctx,
            Arc::new(InitCommand {
                language: self.language,
                config_file: self.config_file.clone(),
            }),
        )
    }
}

struct InitCommand {
    language: Option<atomcode_config::locale::Locale>,
    config_file: Option<std::path::PathBuf>,
}

/// The prompt `/init` hands the model, from the configuration as it is now.
///
/// Split out so the whole decision is testable without a tree or a model. A
/// configuration that will not parse is not an error: the built-in prompt is
/// what this command is for, and refusing to run because an unrelated key is
/// malformed would be refusing the thing that works.
fn init_prompt_from(
    language: Option<atomcode_config::locale::Locale>,
    config_file: Option<&std::path::Path>,
) -> Result<String, String> {
    use atomcode_config::locale::Locale;
    let locale = language.unwrap_or(Locale::En);
    let custom = config_file
        .and_then(|path| atomcode_config::config::Config::load(path).ok())
        .and_then(|config| config.init_prompt_file);
    crate::build_init_prompt(locale, custom.as_deref())
}

#[async_trait]
impl atomcode_harness::commands::CatalogCommand for InitCommand {
    fn describe(&self) -> CommandDescription {
        on_the_session(
            "init",
            None,
            "让模型读一遍这个仓库,把 AGENTS.md 写出来或改好",
        )
    }
    fn offered_for(&self, agent: &atomcode_harness::agent::Agent) -> bool {
        the_conversation_itself(agent)
    }
    async fn run(
        &self,
        agent: Arc<atomcode_harness::agent::Agent>,
        _args: &str,
    ) -> Result<String, String> {
        let prompt = init_prompt_from(self.language, self.config_file.as_deref())?;
        let english = matches!(
            self.language,
            Some(atomcode_config::locale::Locale::En) | None
        );
        agent.send(prompt);
        Ok(if english {
            "Reading the repository to write its instruction file.".to_string()
        } else {
            "正在读这个仓库,准备写它的说明文件。".to_string()
        })
    }
}

pub(crate) struct WorklogPlugin {
    /// The locale the template is written in. Carried in rather than read from
    /// the process-wide i18n cache: a command is registered by a row, and a row
    /// decides from its own configuration which language it speaks.
    pub(crate) language: Option<atomcode_config::locale::Locale>,
}

#[async_trait]
impl Plugin for WorklogPlugin {
    fn name(&self) -> &'static str {
        "worklog"
    }
    fn uses(&self) -> &'static [&'static str] {
        &["commands"]
    }
    fn description(&self) -> &'static str {
        "`/worklog`: one local day's completed turns across every project, as a recap the model fills in"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        atomcode_harness::commands::register(
            ctx,
            Arc::new(WorklogCommand {
                language: self.language,
            }),
        )
    }
}

struct WorklogCommand {
    language: Option<atomcode_config::locale::Locale>,
}

/// What this command hands the model, and the day it covers — split out so the
/// whole decision is testable without a store, a tree or a model.
enum WorklogPrompt {
    /// The day's turns, as text. Always non-empty; the empty day's text is the
    /// template's own "there is no work" instruction, deliberately.
    Prompt(String),
    /// The argument is not a date. What to say back instead.
    Unusable(String),
}

fn worklog_prompt_at(
    arg: &str,
    today: chrono::NaiveDate,
    sessions_root: &std::path::Path,
    english: bool,
) -> WorklogPrompt {
    let Some(date) = atomcode_capabilities::session::resolve_worklog_date(arg, today) else {
        return WorklogPrompt::Unusable(if english {
            "Usage: /worklog [date]  (today / yesterday / 8/27 / 2026-08-27)".to_string()
        } else {
            "用法:/worklog [日期](默认今天;支持 today / yesterday / 8/27 / 2026-08-27)".to_string()
        });
    };
    let (after_ms, before_ms) = atomcode_capabilities::session::local_day_window_ms(date);
    let turns =
        atomcode_capabilities::session::collect_day_turns(sessions_root, after_ms, before_ms);
    // The label is the one a person would write themselves (`8/27`), and it goes
    // into the heading the model sees — not a machine date.
    let label = date.format("%-m/%-d").to_string();
    WorklogPrompt::Prompt(atomcode_capabilities::session::build_worklog_prompt(
        &label, &turns, english,
    ))
}

#[async_trait]
impl atomcode_harness::commands::CatalogCommand for WorklogCommand {
    fn describe(&self) -> CommandDescription {
        on_the_session(
            "worklog",
            Some("[今天|昨天|8/27]"),
            "跨所有项目翻一天的会话记录,做成一份工作日报",
        )
    }
    fn offered_for(&self, agent: &atomcode_harness::agent::Agent) -> bool {
        the_conversation_itself(agent)
    }
    async fn run(
        &self,
        agent: Arc<atomcode_harness::agent::Agent>,
        args: &str,
    ) -> Result<String, String> {
        // The closed label is because the locale may have no opinion: a config
        // with no `language` key follows the conversation, and the model writes
        // its reply in whatever language the person is using either way.
        let english = matches!(
            self.language,
            Some(atomcode_config::locale::Locale::En) | None
        );
        match worklog_prompt_at(
            args,
            chrono::Local::now().date_naive(),
            &SessionManager::sessions_root(),
            english,
        ) {
            WorklogPrompt::Unusable(usage) => return Err(usage),
            WorklogPrompt::Prompt(prompt) => {
                agent.send(prompt);
                Ok(if english {
                    "Put the recap of that day in the conversation.".to_string()
                } else {
                    "把那天的工作复盘放进对话了。".to_string()
                })
            }
        }
    }
}

/// 「收工」的几种说法。
///
/// `/goal` 与 `/loop` 共用一份:一个人想停下自主循环的时候,不该还要先想起
/// 这里用的是哪个词。这几个词没有一个可能是真的目标或真的每轮任务 ——
/// 谁也不会把「达成 cancel」当成目标 —— 所以认它们不会吃掉一条真指令。
fn means_stop(arg: &str) -> bool {
    matches!(arg, "stop" | "off" | "clear" | "cancel" | "reset" | "none")
}

/// 「怎么用」的几种说法。
///
/// 和 [`means_stop`] 一样两条命令共用,而理由更硬一层:不认它的话,
/// `/goal help` 会**把「help」当成一个目标条件开始干**,`/loop help` 会
/// 每轮都做一件叫 help 的事。一个字面上在问「怎么用」的输入,
/// 绝不能反而把东西跑起来 —— 这和打错一个参数就静默切换面板是同一类事故。
///
/// 这几个写法没有一个可能是真条件或真任务:没人会把「达成 --help」当成目标。
fn means_help(arg: &str) -> bool {
    matches!(arg, "help" | "?" | "-h" | "--help")
}

/// 「它现在怎么样了」的几种说法。
///
/// 和 [`means_help`] 同一类事故,只是更难看见:`/goal status` 会
/// **把「status」当成一个目标条件开始干**,`/loop status` 会每轮都
/// 做一件叫 status 的事 —— 而且因为两者看起来都像「开始了」,人不会
/// 立刻发现自己请来的不是一份报告。
///
/// 答得出来的东西本来就一直画在状态行上。真要让这条命令自己报一份,
/// 得给 `RuntimeCommands` 加一个进度读法 —— 那是另一件事;这里先把
/// 「问一句反而把东西跑起来」堵掉。
fn means_status(arg: &str) -> bool {
    matches!(arg, "status" | "state" | "progress" | "状态")
}

struct GoalCommand(Arc<dyn crate::runtime::RuntimeCommands>);

#[async_trait]
impl atomcode_harness::commands::CatalogCommand for GoalCommand {
    fn describe(&self) -> CommandDescription {
        on_the_session(
            "goal",
            Some("<要达成的条件> | stop | pause"),
            "自己干到条件成立为止;`stop` 收工,`pause` 先搁着。",
        )
    }
    fn offered_for(&self, agent: &atomcode_harness::agent::Agent) -> bool {
        the_conversation_itself(agent)
    }
    async fn run(
        &self,
        _agent: Arc<atomcode_harness::agent::Agent>,
        args: &str,
    ) -> Result<String, String> {
        run_goal(&self.0, args).await
    }
}

/// `/goal` 的全部实体。
///
/// 提成函数的理由同 [`run_worktree`]:命令本身用不着 agent,而判据要证明的
/// 恰恰是「**没有**去叫运行时」——`/goal help` 不许把目标跑起来。
/// 走 `CatalogCommand::run` 的话要先造一个真 agent,那是在判别的东西之外。
async fn run_goal(
    runtime: &Arc<dyn crate::runtime::RuntimeCommands>,
    args: &str,
) -> Result<String, String> {
    {
        match args.trim() {
            "" => Err("要一个条件:达成什么才算完。".into()),
            // 问「怎么用」的绝不能把东西跑起来,见 `means_help`。
            word if means_help(word) => Ok("/goal <要达成的条件> —— 自己干到它成立为止。\n\
                 /goal stop(或 off/clear/cancel/reset/none)—— 收工。\n\
                 /goal pause —— 先搁着,接着说话就继续。\n\
                 干到哪一轮了,状态行一直在说。"
                .into()),
            // 收工的几种说法都认。一个人想停下自主循环的时候,不该还要先想起
            // 这里用的是哪个词 —— 而这几个词没有一个可能是真条件:谁也不会把
            // 「达成 cancel」当成目标。
            word if means_status(word) => {
                Ok("干到哪一轮了,状态行一直在说。要收工就 `/goal stop`。".into())
            }
            word if means_stop(word) => {
                runtime.stop_goal().await?;
                Ok("目标停了。".into())
            }
            "pause" => {
                runtime.pause_goal().await?;
                Ok("目标先搁着,还可以接着干。".into())
            }
            condition => {
                runtime.start_goal(condition.to_string()).await?;
                Ok(format!("开始干,直到:{condition}"))
            }
        }
    }
}

struct LoopCommand(Arc<dyn crate::runtime::RuntimeCommands>);

#[async_trait]
impl atomcode_harness::commands::CatalogCommand for LoopCommand {
    fn describe(&self) -> CommandDescription {
        on_the_session(
            "loop",
            Some("<每轮要做的事> | stop"),
            "一遍遍地做同一件事,直到 `stop`。",
        )
    }
    fn offered_for(&self, agent: &atomcode_harness::agent::Agent) -> bool {
        the_conversation_itself(agent)
    }
    async fn run(
        &self,
        _agent: Arc<atomcode_harness::agent::Agent>,
        args: &str,
    ) -> Result<String, String> {
        run_loop(&self.0, args).await
    }
}

/// 固定间隔的上下界,秒。
///
/// 下界 10 秒:比这更密的不是「一遍遍地做」,是一个烧账号的循环 —— 每一轮都是
/// 一次完整的模型请求。上界一天:再长就不是循环了,是定时任务,该由外面的
/// scheduler 管,而这个进程不保证还活着(重启不恢复)。
const LOOP_EVERY_MIN: u32 = 10;
const LOOP_EVERY_MAX: u32 = 86_400;

/// `/loop 5m <每轮要做的事>` 的前半:第一个词是不是一个间隔。
///
/// `30s` / `5m` / `1h`,越界与不认识的写法都是 `None` —— 那时它就是这句话的
/// 第一个词,而不是一个被吃掉的间隔。
///
/// 后缀用 `strip_suffix` 而不是按字节切:每轮要做的事经常是中文,
/// `split_at(len-1)` 会切在字符中间(`gates/tui-string-slice.sh` 记着这个仓库
/// 为它死过四次)。
fn every_seconds(word: &str) -> Option<u32> {
    let (digits, per) = if let Some(d) = word.strip_suffix('s') {
        (d, 1)
    } else if let Some(d) = word.strip_suffix('m') {
        (d, 60)
    } else if let Some(d) = word.strip_suffix('h') {
        (d, 3600)
    } else {
        return None;
    };
    let secs = digits.parse::<u32>().ok()?.checked_mul(per)?;
    (LOOP_EVERY_MIN..=LOOP_EVERY_MAX)
        .contains(&secs)
        .then_some(secs)
}

/// `/loop` 的全部实体。提成函数的理由同 [`run_goal`]。
async fn run_loop(
    runtime: &Arc<dyn crate::runtime::RuntimeCommands>,
    args: &str,
) -> Result<String, String> {
    {
        match args.trim() {
            "" => Err("要一句话:每轮做什么。".into()),
            // 同 `/goal`:问「怎么用」的绝不能把东西跑起来。
            word if means_help(word) => Ok("/loop <每轮要做的事> —— 一遍遍地做,直到收工;\n\
                 \u{20}   下一轮什么时候开始,由模型自己安排(schedule_wakeup)。\n\
                 /loop 5m <每轮要做的事> —— 改成固定间隔(30s / 5m / 1h,10 秒到一天)。\n\
                 /loop stop(或 off/clear/cancel/reset/none)—— 收工。\n\
                 跑到第几轮了,状态行一直在说。重启不恢复。"
                .into()),
            word if means_status(word) => {
                Ok("跑到第几轮了,状态行一直在说。要收工就 `/loop stop`。".into())
            }
            // 同 `/goal`:收工的几种说法都认,理由也一样。
            word if means_stop(word) => {
                runtime.stop_loop().await?;
                Ok("循环停了。".into())
            }
            rest => {
                let (first, tail) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
                let (prompt, every) = match every_seconds(first) {
                    Some(secs) => (tail.trim(), Some(secs)),
                    None => (rest, None),
                };
                if prompt.is_empty() {
                    return Err(format!("`{first}` 是多久,可是每轮做什么?"));
                }
                // 循环一条 `/loop` 就是循环这个循环。它不会是谁想要的,
                // 而它的代价是每一轮都再开一个。
                if prompt.split_whitespace().next() == Some("/loop") {
                    return Err("`/loop` 不能循环它自己。".into());
                }
                runtime.start_loop(prompt.to_string(), every).await?;
                Ok(match every {
                    None => format!("每轮都做:{prompt}"),
                    Some(secs) => format!("每 {} 做一次:{prompt}", spoken_every(secs)),
                })
            }
        }
    }
}

/// 一个间隔,按人写它的方式说回去。
fn spoken_every(secs: u32) -> String {
    match secs {
        s if s % 3600 == 0 => format!("{} 小时", s / 3600),
        s if s % 60 == 0 => format!("{} 分钟", s / 60),
        s => format!("{s} 秒"),
    }
}

struct QueueCommand(Arc<dyn crate::runtime::RuntimeCommands>);

#[async_trait]
impl atomcode_harness::commands::CatalogCommand for QueueCommand {
    fn describe(&self) -> CommandDescription {
        on_the_session(
            "queue",
            Some("<要先说的话>"),
            "排在下一轮前面的话 —— 现在不打断,下一轮模型先看到它。",
        )
    }
    fn offered_for(&self, agent: &atomcode_harness::agent::Agent) -> bool {
        the_conversation_itself(agent)
    }
    async fn run(
        &self,
        _agent: Arc<atomcode_harness::agent::Agent>,
        args: &str,
    ) -> Result<String, String> {
        let text = args.trim();
        if text.is_empty() {
            return Err("要有话可排。".into());
        }
        self.0.queue_local_context(text.to_string()).await?;
        Ok("排好了,下一轮先说这个。".into())
    }
}

struct PolicyCommand(Arc<dyn crate::runtime::RuntimeCommands>);

impl PolicyCommand {
    /// What a person types for each way out, and what it means.
    fn action(word: &str) -> Option<atomcode_kernel::event::PolicyRecoveryAction> {
        use atomcode_kernel::event::PolicyRecoveryAction as A;
        match word {
            "done" => Some(A::CompleteExternally),
            "skip" => Some(A::SkipStep),
            "how" => Some(A::ViewSafeInstructions),
            "end" => Some(A::EndTask),
            _ => None,
        }
    }

    fn word(action: &atomcode_kernel::event::PolicyRecoveryAction) -> &'static str {
        use atomcode_kernel::event::PolicyRecoveryAction as A;
        match action {
            A::CompleteExternally => "done",
            A::SkipStep => "skip",
            A::ViewSafeInstructions => "how",
            A::EndTask => "end",
            // A way out added since: named by nothing a person can type, which
            // is better than naming it as one of these.
            _ => "?",
        }
    }
}

#[async_trait]
impl atomcode_harness::commands::CatalogCommand for PolicyCommand {
    fn describe(&self) -> CommandDescription {
        on_the_session(
            "policy",
            Some("done | skip | how | end"),
            "卡在策略边界上时,说怎么往下走;不带参数就是问有哪些走法。",
        )
    }
    fn offered_for(&self, agent: &atomcode_harness::agent::Agent) -> bool {
        the_conversation_itself(agent)
    }
    async fn run(
        &self,
        _agent: Arc<atomcode_harness::agent::Agent>,
        args: &str,
    ) -> Result<String, String> {
        // Both judgements are this row's (`docs/adr/0021` §8): whether anything
        // is waiting, and whether this is one of its ways out.
        let pending = self
            .0
            .pending_policy()
            .await
            .ok_or_else(|| "现在没有卡住的策略边界要你决定。".to_string())?;
        let choices = pending
            .actions
            .iter()
            .map(Self::word)
            .collect::<Vec<_>>()
            .join(" | ");
        let word = args.trim();
        if word.is_empty() {
            return Ok(format!("可以怎么走:{choices}"));
        }
        let action = Self::action(word)
            .filter(|action| pending.actions.contains(action))
            .ok_or_else(|| format!("`{word}` 不是这次的走法;可以选:{choices}"))?;
        self.0.resolve_policy(pending.id, action).await?;
        Ok(format!("按 `{word}` 往下走。"))
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use std::sync::Arc;

    /// What a person wrote themselves reaches the model, and a configuration
    /// that will not parse still gets them the built-in prompt.
    ///
    /// The second half is the one worth pinning: refusing to run `/init`
    /// because some unrelated key in the file is malformed would refuse the
    /// thing that works, and the thing that works is most of what this command
    /// is for.
    #[test]
    fn the_init_prompt_carries_what_this_machine_was_configured_with() {
        use atomcode_config::locale::Locale;
        let dir = tempfile::tempdir().unwrap();

        let builtin = super::init_prompt_from(Some(Locale::En), None).expect("the built-in one");
        assert!(builtin.contains("AGENTS.md"), "{builtin}");

        let mine = dir.path().join("mine.md");
        std::fs::write(&mine, "ALWAYS-MENTION-THE-NPU-QUANTIZER\n").unwrap();
        let config = dir.path().join("config.toml");
        std::fs::write(
            &config,
            format!("init_prompt_file = {:?}\n", mine.to_string_lossy()),
        )
        .unwrap();
        let with_mine =
            super::init_prompt_from(Some(Locale::En), Some(&config)).expect("with the extra");
        assert!(
            with_mine.contains("ALWAYS-MENTION-THE-NPU-QUANTIZER"),
            "{with_mine}"
        );
        assert!(
            with_mine.contains("AGENTS.md"),
            "and still the built-in one"
        );

        let broken = dir.path().join("broken.toml");
        std::fs::write(&broken, "this is not toml =\n").unwrap();
        assert_eq!(
            super::init_prompt_from(Some(Locale::En), Some(&broken)).expect("still answers"),
            builtin,
            "an unreadable configuration leaves the built-in prompt standing"
        );

        // The language is the row's, not the process's.
        let zh = super::init_prompt_from(Some(Locale::ZhCn), None).expect("zh");
        assert_ne!(zh, builtin);
    }
    use super::worktree;
    use super::{worklog_prompt_at, WorklogPrompt};

    /// Run git in `dir` and insist it worked: every claim below is about what
    /// git did, so a silent failure would make the assertion meaningless.
    fn git(dir: &std::path::Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {args:?} in {}: {}",
            dir.display(),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A repository with one commit — what a worktree is made in, and the only
    /// kind of place these claims are true of.
    ///
    /// The root is canonicalized because git's answers are: on macOS a `/var`
    /// tempdir and git's `/private/var` are one place spelled two ways, and
    /// every path compared below is one of git's.
    fn a_repository() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().canonicalize().expect("canonical root");
        git(&root, &["init", "-q"]);
        git(&root, &["config", "user.email", "t@example.com"]);
        git(&root, &["config", "user.name", "t"]);
        std::fs::write(root.join("a.txt"), "a").expect("write");
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-qm", "first"]);
        (dir, root)
    }

    /// A worktree name becomes both a directory and a branch, so it is checked
    /// before either: a name with a separator in it would put the checkout
    /// outside the repository it belongs to.
    ///
    /// Against a real repository, because what this is really asserting is that
    /// git made one — a check that only looked at the string would pass while
    /// the command did nothing.
    #[test]
    fn a_worktree_is_made_under_the_repository_and_only_under_it() {
        let (_dir, root) = a_repository();

        let at = worktree(&root, "try-it", None).expect("a worktree");
        assert_eq!(at, root.join(".worktrees").join("try-it"));
        assert!(at.join("a.txt").is_file(), "git checked the tree out");
        assert!(
            git(&root, &["worktree", "list", "--porcelain"]).contains("branch refs/heads/try-it"),
            "git has it registered, not just on disk"
        );

        // Asking again finds the one that is there rather than refusing: going
        // back to what you opened must not depend on remembering that you did.
        assert_eq!(worktree(&root, "try-it", None).expect("again"), at);

        for bad in ["", "../escape", "a/b", "-x", ".hidden"] {
            let refused = worktree(&root, bad, None);
            assert!(refused.is_err(), "`{bad}` must be refused: {refused:?}");
        }
        assert!(
            !root
                .parent()
                .map(|p| p.join("escape").exists())
                .unwrap_or(false),
            "nothing was made outside the repository"
        );
    }

    /// The working directory is not always the repository root, and a worktree
    /// belongs to the repository: asked from a subdirectory, it must still land
    /// under the root rather than nest itself where the person happens to be.
    #[test]
    fn the_repository_owns_the_name_not_the_directory_you_are_in() {
        let (_dir, root) = a_repository();
        let nested = root.join("crates").join("inside");
        std::fs::create_dir_all(&nested).expect("mkdir");

        let at = worktree(&nested, "from-here", None).expect("a worktree");
        assert_eq!(at, root.join(".worktrees").join("from-here"));
        assert!(
            !nested.join(".worktrees").exists(),
            "nothing was made around the directory the person was in"
        );
    }

    /// A name a person already used is their **work**, not a slot to reclaim.
    ///
    /// `git worktree add -B <name>` moves the branch to HEAD (git's own words:
    /// `resetting branch 'x'; was at <sha>`), which is how this command would
    /// quietly drop the commits a branch was carrying. The checkout must have
    /// the branch's own commit, and the branch must still point at it.
    #[test]
    fn an_existing_branch_is_taken_over_never_reset() {
        let (_dir, root) = a_repository();
        let base = git(&root, &["rev-parse", "--abbrev-ref", "HEAD"]);

        git(&root, &["checkout", "-q", "-b", "mine"]);
        std::fs::write(root.join("b.txt"), "b").expect("write");
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-qm", "the work on mine"]);
        let theirs = git(&root, &["rev-parse", "mine"]);
        git(&root, &["checkout", "-q", &base]);
        assert_ne!(
            git(&root, &["rev-parse", "HEAD"]),
            theirs,
            "the fixture has a base the branch is ahead of"
        );

        let at = worktree(&root, "mine", None).expect("the branch is taken over");
        assert_eq!(
            git(&at, &["rev-parse", "HEAD"]),
            theirs,
            "the checkout is the branch's own commit, not the base it was reset to"
        );
        assert!(
            at.join("b.txt").is_file(),
            "the branch's work came with the checkout"
        );
        assert_eq!(
            git(&root, &["rev-parse", "mine"]),
            theirs,
            "the branch still points at its own work"
        );
    }

    /// A directory git does not know is not a worktree.
    ///
    /// "The directory is there" used to be the whole test, and answering `/cd`
    /// on it sends the person somewhere git has no checkout — the leftover of a
    /// removed checkout adopted as if it were one. A real worktree gets made.
    #[test]
    fn a_directory_git_does_not_know_is_not_a_worktree() {
        let (_dir, root) = a_repository();
        let leftover = root.join(".worktrees").join("ghost");
        std::fs::create_dir_all(leftover.join("junk")).expect("mkdir");
        std::fs::write(leftover.join("junk").join("x"), "x").expect("write");

        let at = worktree(&root, "ghost", None).expect("a worktree, not the leftover");
        assert_eq!(at, leftover);
        assert!(at.join("a.txt").is_file(), "git checked the tree out");
        assert!(
            !at.join("junk").exists(),
            "the leftover was cleared rather than adopted"
        );
        assert!(
            git(&root, &["worktree", "list", "--porcelain"]).contains("branch refs/heads/ghost"),
            "and git knows about it"
        );
    }

    /// `/worktree x` must not show up in the person's own `git status` — the
    /// checkout it makes lives inside their repository, and an untracked
    /// `.worktrees/` is the command dirtying the tree it was asked to isolate.
    #[test]
    fn the_persons_own_git_status_is_left_clean() {
        let (_dir, root) = a_repository();
        assert_eq!(git(&root, &["status", "--porcelain"]), "");

        worktree(&root, "tidy", None).expect("a worktree");
        assert_eq!(
            git(&root, &["status", "--porcelain"]),
            "",
            "the person's checkout is as clean as it was"
        );
    }

    /// The main checkout is the repository's own first answer, not a path this
    /// file keeps in a slot — a front end that remembered the way back would
    /// lose it on the next restart, and the way back is a fact about the
    /// repository that outlives the process.
    #[test]
    fn the_way_back_is_the_repositorys_own_main_checkout() {
        let (_dir, root) = a_repository();
        assert_eq!(
            super::main_checkout(&root).expect("a main checkout"),
            root,
            "with no worktrees, the main checkout is the repository itself"
        );

        // Made from inside a worktree rather than from the root: `done` has to
        // work from wherever the person is, which is the case that would break
        // if the answer were `root` as it was passed in.
        let at = worktree(&root, "out-there", None).expect("a worktree");
        assert_eq!(
            super::main_checkout(&at).expect("a main checkout"),
            root,
            "asked from the worktree, the answer is still the main checkout"
        );
    }

    /// `list` is what git has, and it marks the one the person is standing in.
    ///
    /// Read from git rather than from disk: a directory a person left behind is
    /// not a place anyone can work, and listing it would offer them a `/cd` into
    /// something git does not have.
    #[test]
    fn list_shows_what_git_has_and_where_the_person_is() {
        let (_dir, root) = a_repository();
        let at = worktree(&root, "over-here", None).expect("a worktree");

        let from_root = super::list_worktrees(&root).expect("the list");
        assert!(
            from_root.contains("over-here") && from_root.contains("← 当前"),
            "the main checkout is marked: {from_root}"
        );

        let from_inside = super::list_worktrees(&at).expect("the list");
        let current = from_inside
            .lines()
            .find(|line| line.contains("← 当前"))
            .unwrap_or_else(|| panic!("a current one: {from_inside}"));
        assert!(
            current.contains("over-here") && current.contains(&at.display().to_string()),
            "the worktree the person is in is the marked one: {from_inside}"
        );
    }

    /// A base names where a *new* branch starts — and only there.
    ///
    /// An existing branch is checked out where it stands (the rule
    /// [`worktree`] states above), so a base pointing at one must not move it:
    /// that is the same "taken over, never reset" failure in a different
    /// spelling, and it would drop the commits the branch was carrying.
    #[test]
    fn a_base_is_where_a_new_branch_starts_and_nowhere_else() {
        let (_dir, root) = a_repository();
        git(&root, &["checkout", "-q", "-b", "elsewhere"]);
        std::fs::write(root.join("b.txt"), "b").expect("write");
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-qm", "work on elsewhere"]);
        let tip = git(&root, &["rev-parse", "elsewhere"]);
        let main = git(&root, &["rev-parse", "HEAD~1"]);

        // A new name with a base: the branch starts at the base.
        let fresh = worktree(&root, "sprout", Some("HEAD~1")).expect("a worktree");
        assert_eq!(
            git(&fresh, &["rev-parse", "HEAD"]),
            main,
            "the new branch starts where the base says"
        );

        // An existing branch with a base: the branch is where it stands, and
        // the base is not a way to move it.
        let taken = worktree(&root, "elsewhere", Some("HEAD~1")).expect("taken over");
        assert_eq!(
            git(&taken, &["rev-parse", "HEAD"]),
            tip,
            "the existing branch was not reset to the base"
        );
        assert_eq!(
            git(&root, &["rev-parse", "elsewhere"]),
            tip,
            "and it still points at its own work"
        );
    }

    /// A checkout with uncommitted work is refused, and the person is told how
    /// to say otherwise — rather than losing it to a tidy-up they asked for.
    #[test]
    fn a_dirty_checkout_is_refused_until_forced() {
        let (_dir, root) = a_repository();
        let at = worktree(&root, "busy", None).expect("a worktree");
        std::fs::write(at.join("scratch.txt"), "not committed").expect("write");

        let refused = super::remove_worktree(&root, "busy", false)
            .expect_err("a dirty checkout is not removed");
        assert!(
            refused.contains("busy") && refused.contains("--force"),
            "it says what is in the way and how to say otherwise: {refused}"
        );
        assert!(at.is_dir(), "and the checkout is still there");

        super::remove_worktree(&root, "busy", true).expect("forced");
        assert!(!at.exists(), "forced, it is gone");
    }

    /// 停下自主循环的几种说法都认,而真的目标不会被当成「停」。
    ///
    /// 这条判据的反面才是它存在的理由:名单要是宽到吃掉一条真指令,
    /// `/goal 清理 cancel 分支` 就会变成「把目标停掉」,而人看到的是一句
    /// 「目标停了」——一个说了反话的回执。
    #[test]
    fn every_way_of_saying_stop_is_understood_and_nothing_else_is() {
        for word in ["stop", "off", "clear", "cancel", "reset", "none"] {
            assert!(super::means_stop(word), "{word} 该被当成收工");
        }
        for real in [
            "",
            "pause",
            "把测试跑绿",
            "cancel 掉那个订单接口的重试",
            "stop the bleeding in the parser",
        ] {
            assert!(!super::means_stop(real), "{real:?} 是一条真指令,不是收工");
        }
    }

    /// 问「怎么用」的绝不能把东西跑起来。
    ///
    /// 这条判据的反面同样是它存在的理由,而且已经发生过:不认 `help` 的时候,
    /// `/goal help` 会**开始朝着一个叫「help」的条件干活**,回执还写着
    /// 「开始干,直到:help」—— 人问了一句怎么用,得到的是一个跑起来的自主循环。
    #[test]
    fn asking_how_to_use_it_never_starts_it() {
        for word in ["help", "?", "-h", "--help"] {
            assert!(super::means_help(word), "{word} 是在问怎么用");
        }
        for real in [
            "",
            "stop",
            "把 --help 的输出补全",
            "写一份 help 文档",
            "helper 类拆出来",
        ] {
            assert!(
                !super::means_help(real),
                "{real:?} 是一条真指令,不是在问用法"
            );
        }
    }

    /// 接线:`/goal help` 与 `/loop help` 真的走到底,而且**没有**叫运行时。
    ///
    /// 上面那条只钉了 `means_help` 这个函数 —— 命令的 `match` 里不写这一臂,
    /// 它照样全绿。这条钉的是那一臂:`Pointed` 的 `start_goal`/`start_loop`
    /// 是 `unreachable!`,所以「问用法反而跑起来」会当场炸,而不是悄悄发生。
    #[tokio::test]
    async fn goal_and_loop_answer_a_help_without_starting_anything() {
        let runtime: Arc<dyn crate::runtime::RuntimeCommands> = pointed();
        for word in ["help", "?", "-h", "--help"] {
            let said = super::run_goal(&runtime, word).await.expect(word);
            assert!(said.contains("/goal"), "它说的是用法:{said}");
            let said = super::run_loop(&runtime, word).await.expect(word);
            assert!(said.contains("/loop"), "它说的是用法:{said}");
        }
        // 而一条真指令仍然照常往下走(这里就是 `unreachable!` 的那一条,
        // 所以只断言它确实到了运行时那一步)。
        let panicked = std::panic::AssertUnwindSafe(super::run_goal(&runtime, "把测试跑绿"));
        assert!(
            futures::FutureExt::catch_unwind(panicked).await.is_err(),
            "一条真条件必须走到运行时去"
        );
    }

    /// `/worktree create <名字> [基准]` 建的是那个名字,不是一个叫
    /// `create` 的分支。
    ///
    /// 不认这个词的后果不是报错而是**静默走错**:`create` 当成名字、
    /// `fix-bug` 当成基准、`main` 直接丢掉,然后切进一个叫 `create` 的
    /// checkout 里 —— 一条看起来成功了的命令。上一代前端认它,人也会
    /// 自己想到这么写。
    ///
    /// 只钉解析:真建一个 worktree 要一个仓库和一次 `git worktree add`。
    #[test]
    fn worktree_create_names_the_worktree_not_the_word() {
        assert_eq!(
            super::worktree_target(&["create", "fix-bug", "main"]),
            Some(("fix-bug", Some("main")))
        );
        assert_eq!(
            super::worktree_target(&["create", "fix-bug"]),
            Some(("fix-bug", None))
        );
        // 不写 `create` 的老写法照旧管用。
        assert_eq!(
            super::worktree_target(&["fix-bug", "main"]),
            Some(("fix-bug", Some("main")))
        );
        // `create` 后面什么都没有 —— 不是“建一个叫 create 的”。
        assert_eq!(super::worktree_target(&["create"]), None);
    }

    /// 问「现在怎么样了」也不能把东西跑起来。
    ///
    /// 和 `help` 同一类事故,只是更难看见:`/goal status` 会把「status」
    /// 当成目标条件开始干,`/loop status` 会每轮做一件叫 status 的事 ——
    /// 而两者看起来都像「开始了」,人不会立刻发现请来的不是一份报告。
    ///
    /// `Pointed` 的 `start_goal`/`start_loop` 是 `unreachable!`,所以「问一句反而
    /// 跑起来」在这里是当场炸,不是静悄悄发生。
    #[tokio::test]
    async fn goal_and_loop_answer_a_status_without_starting_anything() {
        let runtime: Arc<dyn crate::runtime::RuntimeCommands> = pointed();
        for word in ["status", "state", "progress", "状态"] {
            let said = super::run_goal(&runtime, word).await.expect(word);
            assert!(said.contains("/goal stop"), "它说的是怎么看、怎么停:{said}");
            let said = super::run_loop(&runtime, word).await.expect(word);
            assert!(said.contains("/loop stop"), "{said}");
        }
        // 而一句真的以 status 开头的条件仍然是条件:名单是整词,
        // 不是前缀 —— 否则「status 里不再有错」这样的目标就永远开不了。
        let panicked = std::panic::AssertUnwindSafe(super::run_goal(&runtime, "status 里不再有错"));
        assert!(
            futures::FutureExt::catch_unwind(panicked).await.is_err(),
            "一句真条件必须走到运行时去"
        );
    }

    /// `/loop 5m <每轮要做的事>` 的解析,包括它**不**该认的那些。
    ///
    /// 反面才是理由:名单要是宽到吃掉第一个词,`/loop 5m 之内把测试跑绿` 里的
    /// 「5m」就成了间隔,而人写的是一句话的开头。所以只认 `30s/5m/1h` 这三种
    /// 后缀、只认纯数字、还要落在 10 秒到一天之间 —— 越界的写法退回去当词。
    #[test]
    fn an_interval_is_read_only_when_it_really_is_one() {
        assert_eq!(super::every_seconds("30s"), Some(30));
        assert_eq!(super::every_seconds("5m"), Some(300));
        assert_eq!(super::every_seconds("1h"), Some(3600));
        for word in [
            "5",    // 没有单位
            "m",    // 没有数字
            "5x",   // 不认识的单位
            "-5m",  // 负数
            "5s",   // 低于下界:每 5 秒一次是在烧账号
            "48h",  // 高于上界:那是定时任务
            "每天", // 不是这个写法
            "5米",  // 多字节后缀,按字节切会切在字符中间
        ] {
            assert_eq!(super::every_seconds(word), None, "{word:?} 不是间隔");
        }
    }

    /// 人说了间隔,那间隔就是节奏 —— 模型这一轮要的不算。
    ///
    /// 这条是这个功能的全部意思:一个被告知「每五分钟一次」的循环,
    /// 要是模型能把它拽到三十秒,那这个节奏就不是人定的。反过来,
    /// 没说间隔的时候仍然是模型自己安排(它不要,循环就结束)。
    #[test]
    fn the_cadence_a_person_set_wins_over_what_the_model_asked_for() {
        use crate::controllers::{LoopState, WakeupRequest};
        let asked = WakeupRequest {
            delay_seconds: 30,
            prompt: "别的事".into(),
            reason: "模型自己要的".into(),
        };

        let paced = LoopState::new(1, "看一眼 CI".into(), 0).every(Some(300));
        let next = paced.next_round(Some(asked.clone())).expect("还有下一轮");
        assert_eq!(next.delay_seconds, 300, "人定的间隔");
        assert_eq!(next.prompt, "看一眼 CI", "每轮做的还是那句话");
        // 而且模型一次都不要的时候,下一轮照样来 —— 没有间隔的循环这时就结束了。
        assert!(paced.next_round(None).is_some(), "节奏不靠模型开口");

        let self_paced = LoopState::new(2, "看一眼 CI".into(), 0);
        assert_eq!(
            self_paced.next_round(Some(asked)).map(|w| w.delay_seconds),
            Some(30),
            "没说间隔时,模型自己安排"
        );
        assert!(
            self_paced.next_round(None).is_none(),
            "而它不要下一轮,循环就到此为止"
        );
    }

    /// 接线:间隔真的传到运行时,而不只是被解析出来。
    #[tokio::test]
    async fn a_loop_with_a_cadence_passes_it_to_the_runtime() {
        let runtime = std::sync::Arc::new(Looped::default());
        let dynamic: Arc<dyn crate::runtime::RuntimeCommands> = runtime.clone();

        super::run_loop(&dynamic, "5m 看一眼 CI")
            .await
            .expect("起了");
        super::run_loop(&dynamic, "看一眼 CI").await.expect("起了");
        assert_eq!(
            *runtime.started.lock().unwrap(),
            vec![
                ("看一眼 CI".to_string(), Some(300)),
                ("看一眼 CI".to_string(), None),
            ],
            "带间隔的把间隔带过去了,不带的仍然是模型自己安排"
        );

        // 间隔后面没话,和循环它自己,都不许起。
        assert!(super::run_loop(&dynamic, "5m").await.is_err());
        assert!(super::run_loop(&dynamic, "/loop 看一眼").await.is_err());
        assert_eq!(runtime.started.lock().unwrap().len(), 2, "两条都没起");
    }

    /// 驱动循环决定「下一轮什么时候开始」时,走的是 `LoopState::next_round`。
    ///
    /// **读源码,而不是跑一次。** 要看见固定间隔真的把下一轮开起来,得等满一个
    /// 间隔,而下界是 10 秒 —— 那条判据判的是时钟走没走,不是这个决定对不对,
    /// 而决定本身上面那条已经钉死了。这条钉的是唯一还没钉住的东西:
    /// 那个决定有没有被用上。
    ///
    /// 会判红的改法正是当初的写法:回到 `if let Some(wakeup) = pending_wakeup.take()`
    /// —— 那样固定间隔会被解析、被存进 `LoopState`,然后在模型不开口的那一轮
    /// 静静地什么都不做,循环就结束了。人看到的是「每 5 分钟做一次」之后跑了一轮。
    #[test]
    fn the_driver_loop_asks_the_loop_when_the_next_round_is_due() {
        let source = include_str!("runtime.rs");
        assert!(
            source.contains("state.next_round(pending_wakeup.take())"),
            "回合结束时要问 LoopState 下一轮什么时候开始"
        );
        assert!(
            !source.contains("if let Some(wakeup) = pending_wakeup.take()"),
            "不能绕过 next_round 直接拿模型要的那个 —— 人定的节奏会被跳过"
        );
    }

    /// `git` 那一档**不经过会话快照**,所以「这个会话不做工作区快照」
    /// 永远不是它没有答案的理由。
    ///
    /// **读源码,而不是跑一次。** 要跑它得起一个真运行时、真仓库,那是在判
    /// git 装没装;这里唯一还没钉住的东西是那个分支在**问快照之前**。
    /// 会判红的改法正是没有它的样子:没有工作区快照的会话里 `/diff git`
    /// 会被拒绝,而它本来只需要一个 git。
    #[test]
    fn the_checkout_scope_answers_without_a_session_snapshot() {
        let source = include_str!("runtime.rs");
        // 只看 `/diff` 那一条命令的处理臂:`snapshot_hook()` 在别的命令里也用,
        // 从整份文件里 find 第一个会量到无关的那一处。
        let arm = source
            .find("Some(CodingRuntimeControl::WorkspaceChanges {")
            .expect("那条命令要在");
        let arm = &source[arm..];
        let at = arm
            .find("if scope == WorkspaceScope::Git {")
            .expect("git 那一档要在");
        let snapshot = arm
            .find("let Some(hook) = runtime.parts.snapshot_hook() else {")
            .expect("快照那一档要在");
        assert!(
            at < snapshot,
            "git 档要排在问快照之前,否则没有快照的会话答不了它"
        );
    }

    /// 只记下 `/loop` 拿什么起过的运行时。
    #[derive(Default)]
    struct Looped {
        started: std::sync::Mutex<Vec<(String, Option<u32>)>>,
    }

    #[async_trait]
    impl crate::runtime::RuntimeCommands for Looped {
        async fn start_loop(&self, prompt: String, every: Option<u32>) -> Result<(), String> {
            self.started.lock().unwrap().push((prompt, every));
            Ok(())
        }
        async fn start_goal(&self, _: String) -> Result<(), String> {
            unreachable!("not this command")
        }
        async fn stop_goal(&self) -> Result<(), String> {
            unreachable!("not this command")
        }
        async fn pause_goal(&self) -> Result<(), String> {
            unreachable!("not this command")
        }
        async fn stop_loop(&self) -> Result<(), String> {
            unreachable!("not this command")
        }
        async fn queue_local_context(&self, _: String) -> Result<(), String> {
            unreachable!("not this command")
        }
        async fn pending_policy(&self) -> Option<atomcode_kernel::event::PolicyIntervention> {
            None
        }
        async fn resolve_policy(
            &self,
            _: u64,
            _: atomcode_kernel::event::PolicyRecoveryAction,
        ) -> Result<(), String> {
            unreachable!("not this command")
        }
        async fn change_directory(&self, _: std::path::PathBuf) -> Result<(), String> {
            unreachable!("not this command")
        }
    }

    /// A runtime that only remembers where it was pointed.
    ///
    /// The whole claim of `/worktree` is that it *goes* somewhere, and where it
    /// went is exactly what a real runtime's generation and working directory
    /// would say — but reaching a real one means the whole driver loop, which
    /// these claims are not about. What a stub can settle is the part that was
    /// broken: whether the command asked to be moved at all.
    #[derive(Default)]
    struct Pointed {
        at: std::sync::Mutex<Vec<std::path::PathBuf>>,
    }

    #[async_trait]
    impl crate::runtime::RuntimeCommands for Pointed {
        async fn start_goal(&self, _: String) -> Result<(), String> {
            unreachable!("not this command")
        }
        async fn stop_goal(&self) -> Result<(), String> {
            unreachable!("not this command")
        }
        async fn pause_goal(&self) -> Result<(), String> {
            unreachable!("not this command")
        }
        async fn start_loop(&self, _: String, _: Option<u32>) -> Result<(), String> {
            unreachable!("not this command")
        }
        async fn stop_loop(&self) -> Result<(), String> {
            unreachable!("not this command")
        }
        async fn queue_local_context(&self, _: String) -> Result<(), String> {
            unreachable!("not this command")
        }
        async fn pending_policy(&self) -> Option<atomcode_kernel::event::PolicyIntervention> {
            None
        }
        async fn resolve_policy(
            &self,
            _: u64,
            _: atomcode_kernel::event::PolicyRecoveryAction,
        ) -> Result<(), String> {
            unreachable!("not this command")
        }
        async fn change_directory(&self, directory: std::path::PathBuf) -> Result<(), String> {
            self.at.lock().unwrap().push(directory);
            Ok(())
        }
    }

    fn pointed() -> std::sync::Arc<Pointed> {
        std::sync::Arc::new(Pointed::default())
    }

    fn where_it_went(runtime: &std::sync::Arc<Pointed>) -> Vec<std::path::PathBuf> {
        runtime.at.lock().unwrap().clone()
    }

    /// **Making a checkout and going into it is one gesture.** The command used
    /// to say where it had made one and leave the person to type `/cd`, so what
    /// is pinned here is the step: a name makes the worktree *and* asks the
    /// runtime to move there.
    #[tokio::test]
    async fn a_name_makes_a_worktree_and_goes_there() {
        let (_dir, root) = a_repository();
        let runtime = pointed();
        let runtime_dyn: Arc<dyn crate::runtime::RuntimeCommands> = runtime.clone();

        let said = super::run_worktree(&root, "scratch-box", &runtime_dyn)
            .await
            .expect("making one");

        let at = root.join(".worktrees").join("scratch-box");
        assert_eq!(
            where_it_went(&runtime),
            vec![at.clone()],
            "it asked to be moved into the checkout it made"
        );
        assert!(
            at.join("a.txt").is_file(),
            "and the checkout is a real one: git put the tree in it"
        );
        assert!(said.contains("scratch-box"), "it says which one: {said}");
    }

    /// `done` goes back to the repository's own main checkout — asked of git,
    /// not remembered, so it still works after a restart.
    #[tokio::test]
    async fn done_goes_back_to_the_main_checkout() {
        let (_dir, root) = a_repository();
        let at = worktree(&root, "somewhere-else", None).expect("a worktree");
        let runtime = pointed();
        let runtime_dyn: Arc<dyn crate::runtime::RuntimeCommands> = runtime.clone();

        super::run_worktree(&at, "done", &runtime_dyn)
            .await
            .expect("going back");

        assert_eq!(
            where_it_went(&runtime),
            vec![root.clone()],
            "asked from inside a worktree, `done` still points at the main checkout"
        );
    }

    /// `cleanup` of the checkout the person is standing in moves them out
    /// **first**: git refuses to remove the worktree it is run from, so doing it
    /// in the other order would answer "cleaned up" about a checkout still there.
    #[tokio::test]
    async fn cleaning_up_the_current_checkout_moves_out_first() {
        let (_dir, root) = a_repository();
        let at = worktree(&root, "from-here", None).expect("a worktree");
        let runtime = pointed();
        let runtime_dyn: Arc<dyn crate::runtime::RuntimeCommands> = runtime.clone();

        let said = super::run_worktree(&at, "cleanup from-here", &runtime_dyn)
            .await
            .expect("cleaning up");

        assert_eq!(
            where_it_went(&runtime).first(),
            Some(&root),
            "it moved out to the main checkout"
        );
        assert!(!at.exists(), "and only then was the checkout gone: {said}");
    }

    /// A checkout nobody is standing in is removed where it is, and the person
    /// is not moved for it.
    #[tokio::test]
    async fn cleaning_up_another_checkout_moves_nobody() {
        let (_dir, root) = a_repository();
        let at = worktree(&root, "over-there", None).expect("a worktree");
        let runtime = pointed();
        let runtime_dyn: Arc<dyn crate::runtime::RuntimeCommands> = runtime.clone();

        super::run_worktree(&root, "cleanup over-there", &runtime_dyn)
            .await
            .expect("cleaning up");

        assert!(
            where_it_went(&runtime).is_empty(),
            "where the person is did not change"
        );
        assert!(!at.exists(), "and the checkout is gone");
    }

    /// No argument says how to ask, rather than guessing a name — the same
    /// shape every other catalog command answers with.
    #[tokio::test]
    async fn no_argument_says_how_to_ask() {
        let (_dir, root) = a_repository();
        let runtime: Arc<dyn crate::runtime::RuntimeCommands> = pointed();

        let refused = super::run_worktree(&root, "", &runtime)
            .await
            .expect_err("nothing to do without a name");

        assert!(
            refused.contains("worktree"),
            "it names the command: {refused}"
        );
    }

    /// The repository's own ignore rules come first: when they already cover
    /// `.worktrees`, nothing is added — least of all to a tracked file.
    #[test]
    fn a_repository_that_already_ignores_it_is_left_alone() {
        let (_dir, root) = a_repository();
        std::fs::write(root.join(".gitignore"), ".worktrees/\n").expect("write");
        git(&root, &["add", ".gitignore"]);
        git(&root, &["commit", "-qm", "ignore the worktrees"]);
        let exclude = || {
            std::fs::read_to_string(root.join(".git").join("info").join("exclude"))
                .unwrap_or_default()
        };
        let before = exclude();

        worktree(&root, "already", None).expect("a worktree");

        assert_eq!(exclude(), before, "the local exclude was not touched");
        assert_eq!(
            git(&root, &["status", "--porcelain"]),
            "",
            "and the tree is still clean"
        );
    }

    /// The day an argument names, and what the model is told about it.
    ///
    /// The assertion that matters is on the *data*: next-midnight's turn is out
    /// and a turn inside the window is in, which is the whole reason the window
    /// is half-open and computed here rather than in the prompt.
    #[test]
    fn a_recap_is_built_from_the_day_it_names_and_no_other() {
        use atomcode_capabilities::session::events::storage_id;
        use atomcode_capabilities::session::{SessionManager, SessionMeta, StorageOwner};
        use atomcode_kernel::session::{LoggedEvent, SessionEvent, SessionHeader};

        let day = chrono::NaiveDate::from_ymd_opt(2026, 8, 27).expect("a date");
        let (after, before) = atomcode_capabilities::session::local_day_window_ms(day);
        let dir = tempfile::tempdir().expect("tempdir");

        // A real event session — the only shape the reader accepts as a session,
        // and the shape production writes.
        let session = "1756100000000-4242";
        let bucket = "0123456789abcdef";
        let manager = SessionManager::with_root(dir.path().join(bucket));
        let lease = manager.acquire_lease(session).expect("lease");
        let mut meta = SessionMeta::new(session, "/w/atomcode", 0);
        meta.owner = StorageOwner::Native;
        // The index is the cheap pre-filter a day recap reads before it opens any
        // log — `SnapshotHook` keeps it stamped as the session runs, so a session
        // is only looked at when its lifetime overlaps the day asked for. This one
        // spans all three days, which is what lets the assertion below be about
        // the *turn* timestamps rather than about the catalog.
        meta.created_at = after - 100_000;
        meta.updated_at = before + 100_000;
        manager
            .create_event_session(&lease, &SessionHeader::new(session), &meta)
            .expect("create");

        // One turn per day: the day before the window, inside it, and the next
        // midnight exactly (which is the first instant of the following day).
        let one_turn = |turn: u64, at: i64, said: &str| {
            [
                LoggedEvent {
                    seq: 0,
                    at: at as u64,
                    event: SessionEvent::TurnStart { turn },
                },
                LoggedEvent {
                    seq: 0,
                    at: at as u64,
                    event: SessionEvent::UserMessage {
                        turn,
                        text: said.to_string(),
                        images: Vec::new(),
                    },
                },
                LoggedEvent {
                    seq: 0,
                    at: at as u64,
                    event: SessionEvent::AssistantMessage {
                        turn,
                        round: 1,
                        text: "done".into(),
                        reasoning: String::new(),
                        tool_calls: Vec::new(),
                        reasoning_blocks: Vec::new(),
                        meta: None,
                    },
                },
                LoggedEvent {
                    seq: 0,
                    at: at as u64,
                    event: SessionEvent::TurnEnd {
                        turn,
                        stop: atomcode_kernel::event::StopReason::Stopped,
                        error: None,
                    },
                },
            ]
        };
        let mut facts: Vec<LoggedEvent> = Vec::new();
        for (turn, at, said) in [
            (1u64, after - 60_000, "yesterday's work"),
            (2, after + 60_000, "today's work"),
            (3, before, "tomorrow's work"),
        ] {
            facts.extend(one_turn(turn, at, said));
        }
        // Sequence numbers as the store requires them: strictly increasing.
        for (seq, logged) in facts.iter_mut().enumerate() {
            logged.seq = (seq + 1) as u64;
        }
        manager.append_events(&lease, &facts).expect("append");

        let WorklogPrompt::Prompt(prompt) = worklog_prompt_at("8/27", day, dir.path(), false)
        else {
            panic!("a usable date builds a prompt");
        };
        assert!(
            prompt.contains("today's work"),
            "the day's turn is in: {prompt}"
        );
        for absent in ["yesterday's work", "tomorrow's work"] {
            assert!(
                !prompt.contains(absent),
                "`{absent}` is outside the window: {prompt}"
            );
        }
        assert_eq!(storage_id(session), session, "the id a log is stored under");
    }

    #[test]
    fn an_argument_that_is_not_a_date_comes_back_as_usage() {
        let day = chrono::NaiveDate::from_ymd_opt(2026, 8, 27).expect("a date");
        let root = tempfile::tempdir().expect("tempdir");
        for unparseable in ["last tuesday", "13/45", "2026"] {
            let said = worklog_prompt_at(unparseable, day, root.path(), false);
            assert!(
                matches!(said, WorklogPrompt::Unusable(_)),
                "`{unparseable}` is refused rather than recapped"
            );
        }
        // And the words are the person's language, like the template is.
        let WorklogPrompt::Unusable(zh) = worklog_prompt_at("nope", day, root.path(), false) else {
            panic!("usage");
        };
        let WorklogPrompt::Unusable(en) = worklog_prompt_at("nope", day, root.path(), true) else {
            panic!("usage");
        };
        assert!(zh.contains("用法") && en.contains("Usage"), "{zh} / {en}");
    }
}
