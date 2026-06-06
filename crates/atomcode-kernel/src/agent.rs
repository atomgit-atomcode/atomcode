use crate::event::{AgentCommand, AgentEvent};
use crate::hook::{HookChain, LifecycleHooks, TurnCtx};
use crate::message::{Conversation, Message, MessageMeta, SessionSnapshot, SNAPSHOT_VERSION};
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

/// Default kernel cap on a single tool result's `content` byte length.
///
/// 256 KiB, matched to production's per-tool-response byte budget
/// (`atomcode-core` `crates/atomcode-core/src/tool/read.rs` `MAX_BYTES_PER_RESPONSE
/// = 256 * 1024`), which is explicitly sized for AtomCode's bigger-context models.
/// A mounted third-party tool may not self-cap, so the kernel applies this
/// CENTRAL backstop regardless of any per-tool limit. `0` disables the cap
/// (UNBOUNDED) — see `AgentBuilder::max_tool_result_bytes` — but the default is
/// bounded.
pub const DEFAULT_MAX_TOOL_RESULT_BYTES: usize = 256 * 1024;

/// Enforce the kernel's tool-result size cap on `result.content`, IN PLACE.
///
/// Contract:
/// * `max == 0` → UNBOUNDED: returns without touching the content.
/// * `content.len() <= max` (byte length) → untouched, no marker.
/// * `content.len() > max` → TRUNCATE the body to the largest UTF-8 char
///   boundary `<= max` (never splits a multi-byte char → never panics), then
///   APPEND a neutral marker `\n…[truncated: N of M bytes elided by kernel cap]`
///   where `M` is the original byte length and `N = M - kept` is the elided
///   count. The marker counts ON TOP of the cap, so the final stored length is
///   `kept (<= max) + marker.len()` — i.e. it may slightly exceed `max` by the
///   marker; this is intentional and keeps the math reported in the marker exact.
///
/// DETERMINISTIC: same content + same cap → byte-identical output, so the cap
/// never breaks the append-only wire-prefix (prefix-cache) invariant.
fn cap_tool_result(result: &mut ToolResult, max: usize) {
    if max == 0 {
        return; // unbounded
    }
    let total = result.content.len();
    if total <= max {
        return; // under cap: untouched
    }
    // Back off to the largest UTF-8 char boundary <= max so we never split a
    // multi-byte char. `is_char_boundary(0)` is always true, so this terminates.
    let mut keep = max;
    while keep > 0 && !result.content.is_char_boundary(keep) {
        keep -= 1;
    }
    let elided = total - keep;
    result.content.truncate(keep);
    result
        .content
        .push_str(&format!("\n…[truncated: {elided} of {total} bytes elided by kernel cap]"));
}

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
    /// When set, the session SEEDS its conversation from this snapshot's messages
    /// instead of `Conversation::new()` + persona (resume path).
    resume: Option<SessionSnapshot>,
    /// Byte cap on a single tool result's `content` (the kernel's only built-in
    /// safety at this altitude; see `cap_tool_result`). `0` = unbounded.
    max_tool_result_bytes: usize,
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
            resume: self.resume,
            max_tool_result_bytes: self.max_tool_result_bytes,
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
    resume: Option<SessionSnapshot>,
    max_tool_result_bytes: usize,
}

impl RunningAgent {
    async fn session_loop(self, mut cmd_rx: UnboundedReceiver<AgentCommand>) {
        let mut convo = match &self.resume {
            // RESUME: seed from the saved snapshot's messages. Those already
            // include the persona/system message, so we do NOT re-add persona.
            Some(snap) if snap.version == SNAPSHOT_VERSION => {
                // Carry the snapshot's `cache_epoch` so a resume restores the same
                // prefix generation (defaults to 0 for v1 snapshots via serde).
                let mut c = Conversation { messages: snap.messages.clone(), cache_epoch: snap.cache_epoch };
                // An externally-supplied or mid-turn-persisted snapshot may end in a
                // DANGLING assistant tool_call (a tool_use with no tool_result). Seeding
                // it verbatim would make the first resumed request an API-invalid payload.
                // backfill is append-only + idempotent → a no-op for well-formed snapshots,
                // a repair for malformed ones. (See backfill_cancelled_tool_results.)
                c.backfill_cancelled_tool_results();
                c
            }
            // FORWARD-COMPAT SEAM: a snapshot from an unknown (newer/older) kernel
            // version cannot be safely interpreted. Surface it and start EMPTY
            // rather than panic or silently misread bytes. (When/if the schema
            // bumps, a migration would live here.)
            Some(snap) => {
                self.rt.emit(AgentEvent::Error {
                    message: format!(
                        "unsupported snapshot version {} (kernel supports {})",
                        snap.version, SNAPSHOT_VERSION
                    ),
                });
                Conversation::new()
            }
            // FRESH: new conversation + persona injection point. Empty persona by
            // default → neutral kernel.
            None => {
                let mut c = Conversation::new();
                if !self.persona.is_empty() {
                    c.push(Message::system(self.persona.clone()));
                }
                c
            }
        };
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
                    self.rt.emit(AgentEvent::Snapshot {
                        snapshot: SessionSnapshot::from_conversation(&convo),
                    });
                }
                AgentCommand::SendMessage { mut text } => {
                    if let Err(reason) = self.hooks.user_prompt_submit(&mut text).await {
                        self.rt.emit(AgentEvent::Error { message: format!("prompt rejected: {reason}") });
                        self.rt.emit(AgentEvent::TurnComplete);
                        continue;
                    }
                    convo.push(Message::user(text));
                    // Per-turn cancellation token: Cancel fires it; run_turn polls
                    // it at the stream, between tools, and inside execute. A CLONE
                    // also rides into each ToolContext so cooperative tools can bail.
                    let turn_token = tokio_util::sync::CancellationToken::new();
                    // Drive the turn while STILL servicing commands (Respond/Cancel/Shutdown)
                    // so a middleware blocked on approval can be answered out-of-band.
                    let mut turn = Box::pin(self.run_turn(&mut convo, turn_token.clone()));
                    let mut shutdown = false;
                    loop {
                        tokio::select! {
                            _ = &mut turn => break,
                            maybe = cmd_rx.recv() => match maybe {
                                Some(AgentCommand::Respond { id, value }) => self.rt.resolve(id, value),
                                Some(AgentCommand::Shutdown) => { shutdown = true; break; }
                                Some(AgentCommand::Cancel) => turn_token.cancel(),
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

    async fn run_turn(&self, convo: &mut Conversation, cancel: tokio_util::sync::CancellationToken) {
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
            loop {
                // MID-STREAM cancel checkpoint: cancellation stops stream
                // consumption immediately. Carried from production runner.rs:420.
                // Cancel fires BEFORE any assistant message is built → there is
                // nothing dangling to backfill: just emit Cancelled + TurnComplete
                // and return (no bogus partial-success assistant message).
                let ev = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        self.rt.emit(AgentEvent::Cancelled);
                        self.rt.emit(AgentEvent::TurnComplete);
                        return;
                    }
                    ev = stream.next() => match ev {
                        Some(ev) => ev,
                        None => break,
                    },
                };
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
            // ── Per-batch dedup state (claim 21 / A1 gap ⑨) ──
            // `result_ids` = call_ids that have ALREADY produced a result THIS
            // batch (real, stub, or blocked). `seen_calls` = `(name, arguments)`
            // pairs that already EXECUTED this batch. Both reset per assistant
            // message (per `pending_calls` loop), matching production's in-batch
            // `is_dup` scope (runner.rs:917-942) — duplicates ACROSS turns are a
            // separate concern (production's cross-turn loop_guard), out of scope
            // for the kernel here.
            let mut result_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
            let mut seen_calls: std::collections::HashSet<(String, String)> =
                std::collections::HashSet::new();
            for mut call in pending_calls {
                // BETWEEN-TOOLS cancel checkpoint: do not dispatch any remaining
                // tool_call once cancelled. Carried from production runner.rs:916.
                // The skipped calls (this one + the rest) are paired with synthetic
                // "(cancelled)" results by backfill on the cancel path below.
                if cancel.is_cancelled() {
                    convo.backfill_cancelled_tool_results();
                    self.rt.emit(AgentEvent::Cancelled);
                    self.rt.emit(AgentEvent::TurnComplete);
                    return;
                }

                // ── DUPLICATE TOOL-CALL DEDUP GATE ──
                // Some (esp. thinking-mode / weak) models emit the SAME tool_call
                // multiple times in ONE assistant message. The dedup KEY is the
                // ORIGINAL `(call.name, call.arguments)`, captured HERE — BEFORE the
                // ToolMiddleware `before` chain (below) may rewrite `call.arguments`.
                // Rationale: two calls the MODEL emitted identically are duplicates
                // regardless of what middleware would later do to them; keying on
                // post-middleware args could spuriously merge two model-distinct
                // calls (if a rewrite collapses them) or fail to catch a true dup
                // (if a rewrite is non-deterministic).
                let dedup_key = (call.name.clone(), call.arguments.clone());

                // (1) SAME call_id (mode A — the load-bearing API-validity fix):
                // a second result for an already-resulted id would push TWO
                // tool_result messages for one tool_use id → an illegal payload on
                // the next request (each tool_use id must map to EXACTLY ONE
                // tool_result). SKIP it ENTIRELY: no execute, no push, no events.
                // The first occurrence's result already covers this id, so there is
                // nothing dangling for backfill to repair either.
                if result_ids.contains(&call.id) {
                    continue;
                }

                // (2) SAME (name, arguments) with a NEW id (mode B — carry
                // production runner.rs:933-942): do NOT re-execute. Push a stub
                // result so this distinct id STILL gets exactly one result (parity
                // → API-valid), emit its ToolResult, record the id, and continue.
                if seen_calls.contains(&dedup_key) {
                    let result = ToolResult {
                        call_id: call.id.clone(),
                        content: "[duplicate call — identical tool and arguments to an earlier \
                                  call this turn; result already returned above]"
                            .to_string(),
                        is_error: false,
                    };
                    result_ids.insert(call.id.clone());
                    self.rt.emit(AgentEvent::ToolResult { result: result.clone() });
                    convo.push(Message::tool_result(&result.call_id, &result.content, result.is_error));
                    continue;
                }

                // Whether the tool's `execute` ACTUALLY ran (not unknown-tool, not
                // blocked-by-middleware). Gates whether we record `(name,args)` into
                // the seen-executed set for mode-B dedup (see record block below).
                let mut executed = false;
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
                            executed = true;
                            self.rt.emit(AgentEvent::ToolStarted { call: call.clone() });
                            let ctx = ToolContext {
                                working_dir: std::env::current_dir().unwrap_or_default(),
                                cancel: cancel.clone(),
                            };
                            // INSIDE-EXECUTE backstop: poll cancel while the tool
                            // future runs so a long tool is interrupted mid-flight.
                            // DEVIATES from production runner.rs:1431 (a FAIR select)
                            // by being `biased` execute-first: a tool that already
                            // completed deterministically keeps its real result,
                            // rather than losing a coin-flip to the cancel branch.
                            // Cooperative tools that poll ctx.cancel win this race and
                            // clean up properly. A tool still PENDING when cancel fires
                            // is dropped as a backstop — its side effects (if any) are
                            // unknown, so the synthetic result says so (see ToolContext
                            // doc: drop stops polling, it is NOT resource cleanup).
                            let mut r = tokio::select! {
                                biased;
                                r = tool.execute(&call.arguments, &ctx) => r,
                                _ = cancel.cancelled() => ToolResult {
                                    call_id: call.id.clone(),
                                    content: "(cancelled — side effects unknown)".into(),
                                    is_error: true,
                                },
                            };
                            r.call_id = call.id.clone();
                            r
                        }
                    }
                };
                // ToolMiddleware after-chain: transform / observe the result.
                // Middleware sees the RAW (uncapped) result.
                for mw in &self.middlewares {
                    mw.after(&mut result).await;
                }
                // KERNEL TOOL-RESULT SIZE CAP — the kernel's only built-in safety
                // at this altitude (it cannot sandbox). Applied AFTER the
                // after-chain and BEFORE the push+emit, so the stored history, the
                // model (next round), and the driver all see the CAPPED result —
                // keeping context bounded and history growth predictable
                // (deterministic → prefix-cache safe). The tiny `(cancelled)`/error
                // stubs never reach the cap, so they pass through untouched.
                cap_tool_result(&mut result, self.max_tool_result_bytes);
                if result.is_error {
                    self.hooks.on_error(&result.content).await;
                }
                self.rt.emit(AgentEvent::ToolResult { result: result.clone() });
                convo.push(Message::tool_result(&result.call_id, &result.content, result.is_error));

                // (3) Record this id as "resulted" so a later SAME-id call (mode A)
                // is skipped. Recorded for EVERY path that produces a result —
                // including an unknown-tool error and a middleware-`blocked:` error
                // (each still pushed exactly one tool_result for `call.id`, so a
                // later same-id call would create the API-invalid duplicate we must
                // skip). Record `(name, arguments)` (the ORIGINAL key captured at
                // the top, before any middleware rewrite) only when the tool
                // ACTUALLY ran — i.e. not for unknown-tool / blocked cases — so a
                // later distinct id that the model intends to RETRY a previously
                // failed/blocked call is not mistaken for a no-op duplicate.
                result_ids.insert(call.id.clone());
                if executed {
                    seen_calls.insert(dedup_key);
                }
            }
        }
    }
}

pub struct AgentBuilder {
    provider: Option<Arc<dyn LlmProvider>>,
    tools: Option<MountedTools>,
    persona: String,
    middlewares: Vec<Arc<dyn ToolMiddleware>>,
    /// Composable lifecycle hooks, accumulated in REGISTRATION ORDER. `.build()`
    /// wraps this Vec in a `HookChain` (which fans out per the documented contract);
    /// an empty Vec yields an empty `HookChain` that behaves exactly like `NoopHooks`.
    hooks: Vec<Arc<dyn LifecycleHooks>>,
    max_rounds: Option<u32>,
    resume: Option<SessionSnapshot>,
    max_tool_result_bytes: usize,
}

impl Default for AgentBuilder {
    fn default() -> Self {
        Self {
            provider: None,
            tools: None,
            persona: String::new(),
            middlewares: Vec::new(),
            hooks: Vec::new(),
            max_rounds: None,
            resume: None,
            // BOUNDED by default — a mounted tool's content cannot blow the
            // context window / OOM the host unless the embedder opts into `0`.
            max_tool_result_bytes: DEFAULT_MAX_TOOL_RESULT_BYTES,
        }
    }
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
    /// Register a `ToolMiddleware`. Middlewares run in REGISTRATION ORDER — the
    /// `before` chain forward (first-registered runs first) and the `after` chain
    /// likewise. This order is LOAD-BEARING: e.g. an approval middleware that
    /// round-trips the user MUST be registered BEFORE a redaction middleware that
    /// rewrites args, or the user approves bytes different from what executes.
    pub fn middleware(mut self, m: Arc<dyn ToolMiddleware>) -> Self {
        self.middlewares.push(m);
        self
    }
    /// Append a lifecycle hook. Hooks COMPOSE: many may be registered and they fan
    /// out per the `HookChain` contract (run in registration order; `turn_end`
    /// first-`Some` wins; `user_prompt_submit` short-circuits on the first block).
    pub fn hook(mut self, h: Arc<dyn LifecycleHooks>) -> Self {
        self.hooks.push(h);
        self
    }
    /// Back-compat alias for `hook` (APPENDS — does not replace). Existing single-
    /// hook call sites keep working; for the single-hook case `HookChain` is a
    /// transparent passthrough.
    pub fn hooks(self, h: Arc<dyn LifecycleHooks>) -> Self {
        self.hook(h)
    }
    /// Hard cap on LLM rounds per turn (safety fuse; None = unlimited).
    pub fn max_rounds(mut self, n: u32) -> Self {
        self.max_rounds = Some(n);
        self
    }
    /// Byte cap on a SINGLE tool result's `content`. This is the kernel's ONLY
    /// built-in safety mechanism for mounted tools (it cannot sandbox — see the
    /// trust-model contract on `crate::tool`). A result whose content exceeds `n`
    /// bytes is truncated on a UTF-8 char boundary with a marker before it reaches
    /// the model, the stored history, or the driver — bounding context growth.
    /// Defaults to [`DEFAULT_MAX_TOOL_RESULT_BYTES`] (256 KiB). `0` DISABLES the
    /// cap (UNBOUNDED) — only do this if every mounted tool self-caps.
    pub fn max_tool_result_bytes(mut self, n: usize) -> Self {
        self.max_tool_result_bytes = n;
        self
    }
    /// RESUME a persisted session: SEED the conversation from `snapshot.messages`
    /// instead of `Conversation::new()` + persona. The saved messages already
    /// carry the persona/system message, so persona is NOT re-injected on resume.
    /// History continues append-only across the resume boundary → the provider's
    /// prefix cache survives. A snapshot whose `version` the kernel does not
    /// support yields an `AgentEvent::Error` and an empty start (see
    /// `session_loop`'s forward-compat seam).
    pub fn resume(mut self, snapshot: SessionSnapshot) -> Self {
        self.resume = Some(snapshot);
        self
    }
    pub fn build(self) -> Agent {
        Agent {
            provider: self.provider.expect("provider is required"),
            tools: self.tools.expect("tools are required"),
            persona: self.persona,
            middlewares: self.middlewares,
            // Wrap the registered hooks in a HookChain (single `Arc<dyn
            // LifecycleHooks>`); an empty Vec → an empty chain == NoopHooks. The
            // run-loop call sites are unchanged — they still call one hook object.
            hooks: Arc::new(HookChain::new(self.hooks)),
            max_rounds: self.max_rounds,
            resume: self.resume,
            max_tool_result_bytes: self.max_tool_result_bytes,
        }
    }
}

#[cfg(test)]
mod cap_tests {
    use super::*;
    use crate::tool::ToolResult;

    fn res(content: &str) -> ToolResult {
        ToolResult { call_id: "c1".into(), content: content.into(), is_error: false }
    }

    #[test]
    fn caps_oversized_result_on_char_boundary() {
        let original = "a".repeat(1000);
        let mut r = res(&original);
        cap_tool_result(&mut r, 100);
        // The marker is present.
        assert!(r.content.contains("[truncated:"), "must carry a truncation marker: {}", r.content);
        // The kept body (everything before the marker) is a valid byte prefix of
        // the original — deterministic, append-only-safe truncation.
        let body = r.content.split('\n').next().unwrap();
        assert!(body.len() <= 100, "kept body must be <= cap; got {}", body.len());
        assert!(original.as_bytes().starts_with(body.as_bytes()), "kept body must be a prefix of the original");
        // Marker reports the right elided byte count: M=1000, kept=100 → 900.
        assert!(r.content.contains("900 of 1000 bytes"), "marker math wrong: {}", r.content);
    }

    #[test]
    fn does_not_touch_small_result() {
        let mut r = res("small output");
        cap_tool_result(&mut r, 65536);
        assert_eq!(r.content, "small output", "content under cap must be byte-identical");
        assert!(!r.content.contains("truncated"), "no marker on an un-capped result");
    }

    #[test]
    fn cap_respects_multibyte_utf8_boundary() {
        // '世' is 3 bytes; '🦀' is 4 bytes. Build a string whose byte length far
        // exceeds the cap, then pick caps that land MID-CHAR.
        let s = "世".repeat(100); // 300 bytes
        let mut r = res(&s);
        // cap=100 → 100 is NOT a multiple of 3, so the naive byte slice would split
        // a '世'. Must back off to the nearest <= 100 boundary (99).
        cap_tool_result(&mut r, 100);
        let body = r.content.split('\n').next().unwrap();
        assert!(body.len() <= 100, "body must be <= cap");
        // Valid UTF-8 prefix → re-validates and is a prefix of original.
        assert!(std::str::from_utf8(body.as_bytes()).is_ok(), "kept body must be valid UTF-8");
        assert!(s.as_bytes().starts_with(body.as_bytes()), "kept body must be a prefix of the original");
        assert_eq!(body.len() % 3, 0, "must truncate on a '世' (3-byte) boundary, not mid-char");

        // Now a 4-byte char with a cap that lands mid-char → must not panic and
        // must stay a valid prefix.
        let crabs = "🦀".repeat(50); // 200 bytes
        let mut r2 = res(&crabs);
        cap_tool_result(&mut r2, 50); // 50 % 4 != 0 → mid-char
        let body2 = r2.content.split('\n').next().unwrap();
        assert!(std::str::from_utf8(body2.as_bytes()).is_ok(), "valid UTF-8");
        assert_eq!(body2.len() % 4, 0, "must truncate on a '🦀' (4-byte) boundary");
        assert!(body2.len() <= 50);
    }

    #[test]
    fn unbounded_cap_zero_never_truncates() {
        let huge = "x".repeat(5_000_000);
        let mut r = res(&huge);
        cap_tool_result(&mut r, 0);
        assert_eq!(r.content.len(), 5_000_000, "cap=0 means unbounded — no truncation");
    }

    #[test]
    fn cap_is_deterministic() {
        let original = "δ".repeat(1000); // 2-byte chars
        let mut a = res(&original);
        let mut b = res(&original);
        cap_tool_result(&mut a, 333);
        cap_tool_result(&mut b, 333);
        assert_eq!(a.content, b.content, "same content + same cap must yield byte-identical truncation");
    }
}
