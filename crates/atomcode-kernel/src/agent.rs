use crate::event::{AgentCommand, AgentEvent};
use crate::hook::{LifecycleHooks, NoopHooks, TurnCtx};
use crate::message::{Conversation, Message, MessageMeta};
use crate::middleware::ToolMiddleware;
use crate::provider::LlmProvider;
use crate::request::RequestCtx;
use crate::stream::{StreamEvent, TokenUsage};
use crate::tool::{MountedTools, ToolContext, ToolResult};
use futures::StreamExt;
use serde_json::Value;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

/// Bidirectional session handle: send AgentCommand, receive AgentEvent.
pub struct AgentHandle {
    pub commands: UnboundedSender<AgentCommand>,
    pub events: UnboundedReceiver<AgentEvent>,
    pub task: tokio::task::JoinHandle<()>,
}

/// Aggregated result for one-shot/batch drivers.
#[derive(Default, Debug)]
pub struct Outcome {
    pub text: String,
    pub tool_results: Vec<ToolResult>,
}

/// Auto-response policy for the one-shot adapter (no human in the loop).
#[derive(Clone, Copy)]
pub enum AutoRespond {
    AllowAll,
    DenyAll,
}

impl AutoRespond {
    fn decide(&self, _kind: &str, _payload: &Value) -> Value {
        match self {
            AutoRespond::AllowAll => serde_json::json!({ "decision": "allow" }),
            AutoRespond::DenyAll => serde_json::json!({ "decision": "deny" }),
        }
    }
}

pub struct Agent {
    provider: Arc<dyn LlmProvider>,
    tools: MountedTools,
    persona: String,
    middlewares: Vec<Arc<dyn ToolMiddleware>>,
    hooks: Arc<dyn LifecycleHooks>,
    max_rounds: Option<u32>,
}

impl Agent {
    pub fn builder() -> AgentBuilder {
        AgentBuilder::default()
    }

    /// Long-lived bidirectional session. The driver owns the returned handle.
    pub fn spawn(self) -> AgentHandle {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (ev_tx, ev_rx) = mpsc::unbounded_channel();
        let running = RunningAgent {
            provider: self.provider,
            tools: self.tools,
            persona: self.persona,
            middlewares: self.middlewares,
            hooks: self.hooks,
            rt: RequestCtx::new(ev_tx),
            max_rounds: self.max_rounds,
        };
        let task = tokio::spawn(running.session_loop(cmd_rx));
        AgentHandle { commands: cmd_tx, events: ev_rx, task }
    }

    /// One-shot adapter for batch/CI/CodeReview: send one message, auto-answer
    /// Requests per policy, aggregate events into a structured Outcome, then let
    /// the session tear down (so session_end runs).
    pub async fn run_to_completion(self, input: impl Into<String>, policy: AutoRespond) -> Outcome {
        let mut handle = self.spawn();
        let _ = handle.commands.send(AgentCommand::SendMessage { text: input.into() });
        let mut outcome = Outcome::default();
        while let Some(ev) = handle.events.recv().await {
            match ev {
                AgentEvent::TextDelta(t) => outcome.text.push_str(&t),
                AgentEvent::ToolResult { result } => outcome.tool_results.push(result),
                AgentEvent::Request { id, kind, payload } => {
                    let value = policy.decide(&kind, &payload);
                    let _ = handle.commands.send(AgentCommand::Respond { id, value });
                }
                AgentEvent::TurnComplete => {
                    let _ = handle.commands.send(AgentCommand::Shutdown);
                    break;
                }
                _ => {}
            }
        }
        let _ = handle.task.await;
        outcome
    }
}

struct RunningAgent {
    provider: Arc<dyn LlmProvider>,
    tools: MountedTools,
    persona: String,
    middlewares: Vec<Arc<dyn ToolMiddleware>>,
    hooks: Arc<dyn LifecycleHooks>,
    rt: RequestCtx,
    max_rounds: Option<u32>,
}

impl RunningAgent {
    async fn session_loop(self, mut cmd_rx: UnboundedReceiver<AgentCommand>) {
        let mut convo = Conversation::new();
        // Persona is an INJECTION POINT. Empty by default → neutral kernel.
        if !self.persona.is_empty() {
            convo.push(Message::system(self.persona.clone()));
        }
        self.hooks.session_start(&mut convo).await;
        loop {
            let cmd = match cmd_rx.recv().await {
                Some(c) => c,
                None => break,
            };
            match cmd {
                AgentCommand::Shutdown => break,
                AgentCommand::Cancel => {}
                AgentCommand::Respond { id, value } => self.rt.resolve(id, value),
                AgentCommand::Snapshot => {
                    let messages = convo
                        .messages
                        .iter()
                        .map(|m| crate::event::MessageSnapshot {
                            role: format!("{:?}", m.role),
                            text: m.text.clone(),
                            meta: m.meta.clone(),
                        })
                        .collect();
                    self.rt.emit(AgentEvent::Snapshot { messages });
                }
                AgentCommand::SendMessage { mut text } => {
                    if let Err(reason) = self.hooks.user_prompt_submit(&mut text).await {
                        self.rt.emit(AgentEvent::Error { message: format!("prompt rejected: {reason}") });
                        self.rt.emit(AgentEvent::TurnComplete);
                        continue;
                    }
                    convo.push(Message::user(text));
                    // Drive the turn while STILL servicing commands (Respond/Cancel/Shutdown)
                    // so a middleware blocked on approval can be answered out-of-band.
                    let mut turn = Box::pin(self.run_turn(&mut convo));
                    let mut shutdown = false;
                    loop {
                        tokio::select! {
                            _ = &mut turn => break,
                            maybe = cmd_rx.recv() => match maybe {
                                Some(AgentCommand::Respond { id, value }) => self.rt.resolve(id, value),
                                Some(AgentCommand::Shutdown) => { shutdown = true; break; }
                                Some(AgentCommand::Cancel) => {}
                                Some(AgentCommand::Snapshot) => {}
                                Some(AgentCommand::SendMessage { .. }) => {}
                                None => { shutdown = true; break; }
                            }
                        }
                    }
                    if shutdown {
                        break;
                    }
                }
            }
        }
        self.hooks.session_end(&convo).await;
    }

    async fn run_turn(&self, convo: &mut Conversation) {
        self.hooks.turn_start(convo).await;
        self.rt.emit(AgentEvent::TurnStarted);
        let defs = self.tools.defs();
        let mut round: u32 = 0;
        loop {
            round += 1;
            // Hard cap (safety fuse): stop before exceeding max_rounds.
            if let Some(max) = self.max_rounds {
                if round > max {
                    self.rt.emit(AgentEvent::Error { message: format!("max rounds ({max}) reached") });
                    self.rt.emit(AgentEvent::TurnComplete);
                    return;
                }
            }
            let turn_ctx = TurnCtx { round, max_rounds: self.max_rounds };
            let start = Instant::now();
            let mut messages = convo.messages.clone();
            self.hooks.pre_request(&mut messages, &turn_ctx).await;
            // A failed OPEN cleanly fails the turn — no bogus assistant message,
            // no empty-success illusion.
            let mut stream = match self.provider.chat_stream(&messages, &defs).await {
                Ok(s) => s,
                Err(e) => {
                    self.hooks.on_error(&e.message).await;
                    self.rt.emit(AgentEvent::Error { message: e.message });
                    self.rt.emit(AgentEvent::TurnComplete);
                    return;
                }
            };
            let mut assistant_text = String::new();
            let mut pending_calls = Vec::new();
            let mut usage = TokenUsage::default();
            let mut truncated = false;
            while let Some(ev) = stream.next().await {
                match ev {
                    StreamEvent::TextDelta(t) => {
                        assistant_text.push_str(&t);
                        self.rt.emit(AgentEvent::TextDelta(t));
                    }
                    StreamEvent::Reasoning(t) => self.rt.emit(AgentEvent::Reasoning(t)),
                    StreamEvent::ToolCall(c) => pending_calls.push(c),
                    StreamEvent::Usage(u) => usage = u,
                    // A mid-stream error CLEANLY FAILS the turn: surface it and end —
                    // do NOT fall through to a fake empty-success completion.
                    StreamEvent::Error(e) => {
                        self.hooks.on_error(&e.message).await;
                        self.rt.emit(AgentEvent::Error { message: e.message });
                        self.rt.emit(AgentEvent::TurnComplete);
                        return;
                    }
                    StreamEvent::Done { truncated: t } => {
                        truncated = t;
                        break;
                    }
                }
            }
            // Truncation is observable via a Warning; the round still finishes
            // normally (continuation is a separate follow-up task).
            if truncated {
                self.rt.emit(AgentEvent::Warning(
                    "response truncated: finish_reason=length".into(),
                ));
            }
            let ctx_window = self.provider.context_window();
            let used_tokens = usage.prompt;
            let utilization = if ctx_window > 0 {
                used_tokens as f32 / ctx_window as f32
            } else {
                0.0
            };
            let meta = MessageMeta {
                tokens: usage,
                elapsed_ms: start.elapsed().as_millis() as u64,
                ctx_window,
                used_tokens,
                utilization,
                round,
            };
            let mut assistant_msg = Message::assistant(assistant_text.clone(), pending_calls.clone());
            assistant_msg.meta = Some(meta);
            self.hooks.on_model_response(&mut assistant_msg).await;
            self.rt.emit(AgentEvent::Usage(assistant_msg.meta.clone().unwrap_or_default()));
            // Fix #5: the hook may have transformed the response (e.g. dropped a tool
            // call) — re-derive the calls to execute from the (possibly edited) message
            // so a dropped call is NOT executed.
            let pending_calls = assistant_msg.tool_calls.clone();
            convo.push(assistant_msg);
            if pending_calls.is_empty() {
                if let Some(reminder) = self.hooks.turn_end(convo).await {
                    convo.push(Message::user(reminder));
                    continue;
                }
                self.rt.emit(AgentEvent::TurnComplete);
                return;
            }
            for mut call in pending_calls {
                let mut result = match self.tools.get(&call.name) {
                    None => ToolResult {
                        call_id: call.id.clone(),
                        content: format!("unknown or unmounted tool: {}", call.name),
                        is_error: true,
                    },
                    Some(tool) => {
                        // ToolMiddleware before-chain: may rewrite the call (&mut),
                        // round-trip via rt (approval), or block via Err. Runs after
                        // lookup; ToolStarted fires only for a tool that executes
                        // (no ghost row for blocked tools).
                        let mut blocked: Option<String> = None;
                        for mw in &self.middlewares {
                            if let Err(reason) = mw.before(&mut call, &tool, &self.rt).await {
                                blocked = Some(reason);
                                break;
                            }
                        }
                        if let Some(reason) = blocked {
                            ToolResult {
                                call_id: call.id.clone(),
                                content: format!("blocked: {reason}"),
                                is_error: true,
                            }
                        } else {
                            self.rt.emit(AgentEvent::ToolStarted { call: call.clone() });
                            let ctx = ToolContext { working_dir: std::env::current_dir().unwrap_or_default() };
                            let mut r = tool.execute(&call.arguments, &ctx).await;
                            r.call_id = call.id.clone();
                            r
                        }
                    }
                };
                // ToolMiddleware after-chain: transform / observe the result.
                for mw in &self.middlewares {
                    mw.after(&mut result).await;
                }
                if result.is_error {
                    self.hooks.on_error(&result.content).await;
                }
                self.rt.emit(AgentEvent::ToolResult { result: result.clone() });
                convo.push(Message::tool_result(&result.call_id, &result.content, result.is_error));
            }
        }
    }
}

#[derive(Default)]
pub struct AgentBuilder {
    provider: Option<Arc<dyn LlmProvider>>,
    tools: Option<MountedTools>,
    persona: String,
    middlewares: Vec<Arc<dyn ToolMiddleware>>,
    hooks: Option<Arc<dyn LifecycleHooks>>,
    max_rounds: Option<u32>,
}

impl AgentBuilder {
    pub fn provider(mut self, p: Arc<dyn LlmProvider>) -> Self {
        self.provider = Some(p);
        self
    }
    pub fn tools(mut self, t: MountedTools) -> Self {
        self.tools = Some(t);
        self
    }
    pub fn persona(mut self, s: impl Into<String>) -> Self {
        self.persona = s.into();
        self
    }
    pub fn middleware(mut self, m: Arc<dyn ToolMiddleware>) -> Self {
        self.middlewares.push(m);
        self
    }
    pub fn hooks(mut self, h: Arc<dyn LifecycleHooks>) -> Self {
        self.hooks = Some(h);
        self
    }
    /// Hard cap on LLM rounds per turn (safety fuse; None = unlimited).
    pub fn max_rounds(mut self, n: u32) -> Self {
        self.max_rounds = Some(n);
        self
    }
    pub fn build(self) -> Agent {
        Agent {
            provider: self.provider.expect("provider is required"),
            tools: self.tools.expect("tools are required"),
            persona: self.persona,
            middlewares: self.middlewares,
            hooks: self.hooks.unwrap_or_else(|| Arc::new(NoopHooks)),
            max_rounds: self.max_rounds,
        }
    }
}
