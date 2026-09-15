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
//! | `pre_request_options`, `on_request` | `agent/request`, before delegating |
//! | `on_model_response` | `agent/request`, after a successful response |
//! | `offer_continuation` | `agent/request`, after a response that ends the round; queued as a harness message |
//! | `on_error` | `agent/request`, after a failed request |
//! | `turn_complete` | `turn/finishing` — awaited, before the driver learns the turn ended |
//!
//! Not bridged, and why: `pre_request` rewrites the request after the log was
//! checked, which would put content in front of the model that the log cannot
//! explain; `on_text_delta` / `on_reasoning_delta` mutate a stream the harness
//! commits verbatim; `on_rate_limit` belongs to the retry policy; `session_end`
//! has no harness moment. A hook that needs one of these gets a row of its own.
//!
//! Only the conversation's own agent is served. A delegated child runs its turns
//! through the same events, and a snapshot writer that heard them would file the
//! child's work under the parent's session.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use async_trait::async_trait;
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
#[derive(Clone, Debug, Default)]
pub struct SessionSeed {
    /// The native binding's id. `None` for a sessionless runtime, which lets the
    /// tree mint one.
    pub id: Option<String>,
    /// The stored conversation to continue from.
    pub snapshot: Option<SessionSnapshot>,
}

/// `session-native`: the runtime's session, handed to the tree.
pub(crate) struct SessionNativePlugin(pub(crate) Arc<SessionSeed>);

#[async_trait]
impl Plugin for SessionNativePlugin {
    fn name(&self) -> &'static str {
        "session-native"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["session-defaults"]
    }
    fn description(&self) -> &'static str {
        "the coding runtime's native session: its id, and its stored conversation as the seed"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
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
    pub(crate) fn rows(&self) -> String {
        self.names()
            .iter()
            .map(|name| {
                format!(
                    "[[insert]]\nid = {}\nname = \"kernel-hooks\"\nconfig = {{ hook = {} }}\n\n",
                    atomcode_harness::bundle::toml_string(&format!("kernel-hooks-{name}")),
                    atomcode_harness::bundle::toml_string(name),
                )
            })
            .collect()
    }
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
        let reason = atomcode_harness::plugins::handle::stop_reason(outcome.stop);
        let ctx = self.turn_ctx(&agent, outcome.turn, outcome.rounds);
        self.hook.turn_complete(&convo, &reason, &ctx).await;
        None
    }
}
