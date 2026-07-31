//! Telemetry re-entry for the neutral kernel.
//!
//! The kernel emits NO telemetry by design. It exposes two seams the project's
//! observability rides on, and this module supplies an adapter for each:
//!   - [`TelemetryHook`] — TURN-level ([`LifecycleHooks`], the docs' "telemetry
//!     HOME"): one `LlmChat` per LLM round.
//!   - [`ToolTelemetryMiddleware`] — TOOL-level ([`ToolMiddleware`]): one `ToolCall`
//!     per executed tool.
//!
//! Both are registered by [`assemble`](crate::assemble)/[`prepare`](crate::prepare)
//! ONLY when `CodingAgentConfig::telemetry` is set, so a telemetry-free embedder
//! keeps a zero-telemetry kernel.
//!
//! LlmChat fidelity: token totals (input/output/cached), duration, context window,
//! tool-call count and messages-count are EXACT (from `MessageMeta` + the round's
//! request). `tool_def_tokens` is estimated (bytes/4, as the legacy path does). The
//! per-zone breakdown (`system_tokens` / `message_tokens` / `tool_result_tokens`) is
//! not computed by the kernel and is reported as 0 rather than guessed.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Instant;

use async_trait::async_trait;
use atomcode_kernel::event::StopReason;
use atomcode_kernel::hook::{LifecycleHooks, TurnCtx};
use atomcode_kernel::message::{Conversation, Message};
use atomcode_kernel::middleware::{AfterOutcome, BeforeOutcome, ToolMiddleware};
use atomcode_kernel::provider::{ChatOptions, LlmProvider};
use atomcode_kernel::request::RequestCtx;
use atomcode_kernel::stream::{ProviderError, StreamEvent, TokenUsage};
use atomcode_kernel::tool::{Tool, ToolCall, ToolDef, ToolResult};
use atomcode_telemetry::{CurrentContext, Event, LlmErrorKind, Telemetry, ToolErrorKind};
use futures::stream::{BoxStream, StreamExt};

/// Estimate the tokens a single outgoing message contributes — the byte/4
/// heuristic the legacy `Message::estimate_tokens` uses, adapted to the kernel's
/// FLAT message shape (text + tool_calls + reasoning + images). Approximate by
/// design: the EXACT prompt total comes from the provider's usage report; this only
/// apportions it across zones for the LlmChat breakdown.
fn estimate_message_tokens(m: &Message) -> u32 {
    use atomcode_kernel::message::Role;
    // Images dominate when present (vision ≈ 1600 tok each), mirroring legacy.
    if !m.images.is_empty() {
        return ((m.text.len() / 4).max(1) + m.images.len() * 1600 + 4) as u32;
    }
    let byte_count = if m.role == Role::Tool {
        m.text.len() + 10 // tool result + small wrapper overhead
    } else if !m.tool_calls.is_empty() {
        let calls: usize = m
            .tool_calls
            .iter()
            .map(|tc| tc.name.len() + tc.arguments.len() + 20)
            .sum();
        m.text.len() + calls + m.reasoning.as_ref().map_or(0, |r| r.len())
    } else {
        m.text.len()
    };
    ((byte_count / 4).max(1) + 4) as u32
}

/// Scale per-zone estimates so they sum to EXACTLY `target` (the rounding residual
/// lands on the largest zone). Returns the raw estimates when there's nothing to
/// scale to (`target == 0` or all-zero estimates).
fn scale_to(target: u32, est: [u32; 4]) -> (u32, u32, u32, u32) {
    let sum: u32 = est.iter().sum();
    if target == 0 || sum == 0 {
        return (est[0], est[1], est[2], est[3]);
    }
    let scale = target as f64 / sum as f64;
    let mut out = [0u32; 4];
    for i in 0..4 {
        out[i] = (est[i] as f64 * scale).round() as u32;
    }
    let residual = target as i64 - out.iter().map(|&x| x as i64).sum::<i64>();
    if residual != 0 {
        let max_i = (0..4).max_by_key(|&i| out[i]).unwrap_or(0);
        out[max_i] = (out[max_i] as i64 + residual).max(0) as u32;
    }
    (out[0], out[1], out[2], out[3])
}

/// Split the EXACT prompt `total` into (system, message, tool_result, tool_def).
/// `anchored` is the message zone's REAL-measured portion (assistant tokens from
/// usage reports) — it is preserved untouched; only the UNMEASURED remainder
/// (`total - anchored`) is distributed across the byte/4 `est` zones. The four always
/// sum to `total`. (`est` = [system, message(non-anchored), tool_result, tool_def].)
fn apportion(total: u32, anchored: u32, est: [u32; 4]) -> (u32, u32, u32, u32) {
    if total == 0 {
        // Failed round (no usage): raw estimates, the anchor folded into the message zone.
        return (est[0], est[1].saturating_add(anchored), est[2], est[3]);
    }
    if anchored >= total || est.iter().sum::<u32>() == 0 {
        // The real anchor alone meets the budget (or nothing to estimate): the whole
        // total is the message zone. Keeps the sum exact. (Effectively unreachable —
        // assistant history can't exceed a prompt that contains it plus system/tools.)
        return (0, total, 0, 0);
    }
    let (s, msg, tr, td) = scale_to(total - anchored, est);
    (s, msg + anchored, tr, td)
}

/// Map a provider/stream error message onto a telemetry `LlmErrorKind`. Mirrors the
/// legacy `classify_llm_error` (which lives in core, off-limits here).
fn classify_llm_error(reason: &str) -> LlmErrorKind {
    let r = reason.to_lowercase();
    if r.contains("401") || r.contains("403") || r.contains("unauthorized") || r.contains("auth") {
        LlmErrorKind::AuthError
    } else if r.contains("429") || r.contains("rate") || r.contains("throttl") {
        LlmErrorKind::RateLimited
    } else if r.contains("500") || r.contains("502") || r.contains("503") {
        LlmErrorKind::ServerError
    } else if r.contains("stream timeout") || r.contains("no event for") {
        LlmErrorKind::StreamTimeout
    } else if r.contains("decode") || r.contains("mid-flight") || r.contains("terminated") {
        LlmErrorKind::StreamInterrupted
    } else if r.contains("context") || r.contains("max_tokens") || r.contains("token limit") {
        LlmErrorKind::ContextOverflow
    } else if r.contains("connect")
        || r.contains("dns")
        || r.contains("network")
        || r.contains("timeout")
    {
        LlmErrorKind::NetworkError
    } else {
        LlmErrorKind::Other
    }
}

/// Shared attribution + emit path for the telemetry adapters. Fixes the
/// provider/host/model envelope at assembly; both adapters track within its scope.
#[derive(Clone)]
struct Attribution {
    telemetry: Arc<Telemetry>,
    provider: Option<String>,
    provider_host: Option<String>,
    model: String,
    /// The agent's session id, so every event groups under THIS session instead of
    /// falling back to the process launch id. `None` for a Disabled-session agent or
    /// an id that isn't a uuid.
    session_id: Option<uuid::Uuid>,
    /// Logical origin tag (e.g. `"code_review"`) → `Envelope.surface`. `None` = the
    /// primary agent loop. Set via [`MeteredProvider::with_surface`].
    surface: Option<String>,
}

impl Attribution {
    /// `vendor` is the provider family (e.g. `"openai"`); `base_url` lets the
    /// telemetry envelope resolve a stable provider host. `session_id` is the agent's
    /// session id string (parsed to a uuid for correlation).
    fn new(
        telemetry: Arc<Telemetry>,
        vendor: impl Into<String>,
        base_url: &str,
        model: impl Into<String>,
        session_id: Option<&str>,
    ) -> Self {
        let vendor = vendor.into();
        let provider_host = atomcode_telemetry::resolve_provider_host(&vendor, Some(base_url));
        Self {
            telemetry,
            provider: Some(vendor),
            provider_host,
            model: model.into(),
            session_id: session_id.and_then(|s| uuid::Uuid::parse_str(s).ok()),
            surface: None,
        }
    }

    fn scope_ctx(&self) -> CurrentContext {
        CurrentContext {
            provider: self.provider.clone(),
            provider_host: self.provider_host.clone(),
            model: Some(self.model.clone()),
            session_id: self.session_id,
            surface: self.surface.clone(),
            ..Default::default()
        }
    }

    async fn emit(&self, event: Event) {
        let tel = self.telemetry.clone();
        CurrentContext::scope(self.scope_ctx(), || async move {
            tel.track(event);
        })
        .await;
    }
}

/// Emits `Event::LlmChat` per round. Per-round figures come from the response's
/// `MessageMeta`; `messages_count` / `tool_def_tokens` come from the matching
/// `on_request`.
pub struct TelemetryHook {
    attr: Attribution,
    /// Per-zone prompt token estimates captured at `on_request` and read by the
    /// matching `on_model_response` (same round; one round at a time, so plain
    /// atomics suffice). The EXACT prompt TOTAL still comes from the usage report —
    /// these only break it down by zone for the LlmChat fields.
    last_messages_count: AtomicU32,
    last_system_tokens: AtomicU32,
    /// byte/4 estimate of the message zone EXCLUDING real-anchored assistant tokens
    /// (user messages + any assistant message missing a real count).
    last_message_tokens: AtomicU32,
    /// REAL assistant tokens (sum of stored `meta.tokens.completion`) — the message
    /// zone's measured portion, preserved (never re-scaled), per the compaction
    /// subsystem's "trust real usage, estimate only the rest" principle.
    last_anchored_tokens: AtomicU32,
    last_tool_result_tokens: AtomicU32,
    last_tool_def_tokens: AtomicU32,
    /// Last error observed via `on_error`, consumed by `turn_complete` to classify a
    /// failed turn. (`on_error` also fires for tool errors, but `turn_complete` only
    /// reads it on a PROVIDER terminal reason — so tool errors never become LlmChat.)
    last_error: StdMutex<Option<String>>,
}

impl TelemetryHook {
    pub fn new(
        telemetry: Arc<Telemetry>,
        vendor: impl Into<String>,
        base_url: &str,
        model: impl Into<String>,
        session_id: Option<&str>,
    ) -> Self {
        Self {
            attr: Attribution::new(telemetry, vendor, base_url, model, session_id),
            last_messages_count: AtomicU32::new(0),
            last_system_tokens: AtomicU32::new(0),
            last_message_tokens: AtomicU32::new(0),
            last_anchored_tokens: AtomicU32::new(0),
            last_tool_result_tokens: AtomicU32::new(0),
            last_tool_def_tokens: AtomicU32::new(0),
            last_error: StdMutex::new(None),
        }
    }
}

#[async_trait]
impl LifecycleHooks for TelemetryHook {
    async fn on_request(
        &self,
        messages: &[Message],
        tools: &[ToolDef],
        _options: &ChatOptions,
        _ctx: &TurnCtx,
    ) {
        use atomcode_kernel::message::Role;
        self.last_messages_count
            .store(messages.len() as u32, Ordering::Relaxed);

        // Apportion the prompt across zones. system / user / tool-result are byte/4
        // estimates; an ASSISTANT message instead contributes its REAL token count
        // (the provider's `meta.tokens.completion` recorded when it was generated) —
        // anchoring the measured portion instead of re-estimating it.
        let mut system = 0u32;
        let mut msg = 0u32;
        let mut anchored = 0u32;
        let mut tool_result = 0u32;
        for m in messages {
            match m.role {
                Role::System => system += estimate_message_tokens(m),
                Role::Tool => tool_result += estimate_message_tokens(m),
                Role::User => msg += estimate_message_tokens(m),
                Role::Assistant => {
                    match m
                        .meta
                        .as_ref()
                        .map(|mm| mm.tokens.completion)
                        .filter(|&c| c > 0)
                    {
                        Some(real) => anchored += real,
                        None => msg += estimate_message_tokens(m),
                    }
                }
            }
        }
        // tool_def: name + description + serialized params, ~4 chars/token + overhead.
        let tool_def: u32 = tools
            .iter()
            .map(|t| {
                ((t.name.len() + t.description.len() + t.parameters.to_string().len()) / 4 + 4)
                    as u32
            })
            .sum();

        self.last_system_tokens.store(system, Ordering::Relaxed);
        self.last_message_tokens.store(msg, Ordering::Relaxed);
        self.last_anchored_tokens.store(anchored, Ordering::Relaxed);
        self.last_tool_result_tokens
            .store(tool_result, Ordering::Relaxed);
        self.last_tool_def_tokens.store(tool_def, Ordering::Relaxed);
    }

    async fn on_model_response(&self, response: &mut Message) {
        // No meta (e.g. a synthesized/empty response) ⇒ nothing to report.
        let Some(m) = response.meta.as_ref() else {
            return;
        };

        // Apportion the EXACT prompt total across zones: the byte/4 estimates only
        // give the RELATIVE split (the provider's true tokenizer is unknown), but the
        // absolute total IS known (the usage report). Scaling the estimates to sum to
        // that total anchors the breakdown to ground truth — strictly more precise
        // than a raw heuristic that can drift 0.7–1.3× from the real total.
        let (system_tokens, message_tokens, tool_result_tokens, tool_def_tokens) = apportion(
            m.tokens.prompt,
            self.last_anchored_tokens.load(Ordering::Relaxed),
            [
                self.last_system_tokens.load(Ordering::Relaxed),
                self.last_message_tokens.load(Ordering::Relaxed),
                self.last_tool_result_tokens.load(Ordering::Relaxed),
                self.last_tool_def_tokens.load(Ordering::Relaxed),
            ],
        );

        let event = Event::LlmChat {
            duration_ms: m.elapsed_ms as u32,
            tool_calls_count: response.tool_calls.len() as u32,
            input_tokens: m.tokens.prompt,
            output_tokens: m.tokens.completion,
            cached_tokens: m.tokens.cached,
            had_error: false,
            context_window: m.ctx_window,
            system_tokens,
            tool_def_tokens,
            tool_result_tokens,
            message_tokens,
            messages_count: self.last_messages_count.load(Ordering::Relaxed),
            error_kind: None,
            error_data: None,
        };
        self.attr.emit(event).await;
    }

    async fn on_error(&self, error: &str) {
        // Remember the latest error; `turn_complete` decides if it was the cause of
        // a PROVIDER-level turn failure (tool errors terminate normally → ignored).
        if let Ok(mut g) = self.last_error.lock() {
            *g = Some(error.to_string());
        }
    }

    async fn turn_complete(&self, _convo: &Conversation, reason: &StopReason, _ctx: &TurnCtx) {
        let last = self.last_error.lock().ok().and_then(|mut g| g.take());
        // A round that produced a model response already emitted its LlmChat. Emit an
        // extra had_error one ONLY for an LLM/provider terminal failure — not normal
        // stop, cancel, or the round/continuation fuses (those aren't LLM errors).
        let kind = match reason {
            StopReason::ProviderError => last
                .as_deref()
                .map(classify_llm_error)
                .unwrap_or(LlmErrorKind::Other),
            StopReason::Timeout => LlmErrorKind::StreamTimeout,
            _ => return,
        };
        let event = Event::LlmChat {
            duration_ms: 0,
            tool_calls_count: 0,
            input_tokens: 0,
            output_tokens: 0,
            cached_tokens: 0,
            had_error: true,
            context_window: 0,
            system_tokens: 0,
            tool_def_tokens: 0,
            tool_result_tokens: 0,
            message_tokens: 0,
            messages_count: self.last_messages_count.load(Ordering::Relaxed),
            error_kind: Some(kind),
            error_data: last.map(|e| e.chars().take(200).collect()),
        };
        self.attr.emit(event).await;
    }
}

/// Emits `Event::ToolCall` per executed tool. `before` stamps the start (keyed by
/// call id, so parallel batches don't collide); `after` computes the duration and
/// emits. Observation-only — never blocks or rewrites — so it registers AFTER the
/// approval middleware without touching the approve-what-runs contract.
pub struct ToolTelemetryMiddleware {
    attr: Attribution,
    /// call_id → (tool name, start). `ToolResult` carries no name, so it's stamped
    /// in `before` and looked up in `after`.
    inflight: StdMutex<HashMap<String, (String, Instant)>>,
}

impl ToolTelemetryMiddleware {
    pub fn new(
        telemetry: Arc<Telemetry>,
        vendor: impl Into<String>,
        base_url: &str,
        model: impl Into<String>,
        session_id: Option<&str>,
    ) -> Self {
        Self {
            attr: Attribution::new(telemetry, vendor, base_url, model, session_id),
            inflight: StdMutex::new(HashMap::new()),
        }
    }
}

#[async_trait]
impl ToolMiddleware for ToolTelemetryMiddleware {
    async fn before(
        &self,
        call: &mut ToolCall,
        _tool: &Arc<dyn Tool>,
        _rt: &RequestCtx,
    ) -> BeforeOutcome {
        if let Ok(mut m) = self.inflight.lock() {
            m.insert(call.id.clone(), (call.name.clone(), Instant::now()));
        }
        BeforeOutcome::Proceed
    }

    async fn after(&self, result: &mut ToolResult) -> AfterOutcome {
        // No stamp ⇒ this middleware's `before` never ran (a prior middleware blocked
        // the call); nothing to attribute.
        let Some((name, started)) = self
            .inflight
            .lock()
            .ok()
            .and_then(|mut m| m.remove(&result.call_id))
        else {
            return AfterOutcome::Proceed;
        };
        let success = !result.is_error;
        // A middleware block (e.g. approval deny) yields a deterministic
        // "blocked: …" content. (Unknown/unmounted tools never reach the middleware
        // `before` chain — the kernel builds their error result directly — so there's
        // no NotFound case to classify here.)
        let error_kind = if success {
            None
        } else if result.content.starts_with("blocked:") {
            Some(ToolErrorKind::DeniedByUser)
        } else {
            Some(ToolErrorKind::ExecutionFailed)
        };
        let event = Event::ToolCall {
            name,
            success,
            duration_ms: started.elapsed().as_millis() as u32,
            error_kind,
            error_data: None,
        };
        self.attr.emit(event).await;
        AfterOutcome::Proceed
    }
}

/// An [`LlmProvider`] decorator that records one [`Event::LlmChat`] for each call made
/// THROUGH it. Its job: meter LLM calls that run OUTSIDE the instrumented host agent
/// loop, where the turn-level [`TelemetryHook`] (which fires on `on_request` /
/// `on_model_response`) never sees them and their token spend would be invisible. Three
/// such call sites wrap their provider with this:
///   - the [`OverflowCompaction`] tier-2 summary call (history summarization — the v2
///     port of core's `run_llm_summary` telemetry, `c58427a3`);
///   - the in-session `code_review` sub-agent (assembled in [`assemble`](crate::assemble));
///   - the standalone `atomcodex review` agent (wrapped by the clix driver).
///
/// The latter two run their own kernel loop with no telemetry hooks of their own.
///
/// The emitted event is a plain `LlmChat` (the telemetry `Event` enum has no
/// sub-agent-specific variant) carrying the call's REAL token usage (folded from the
/// stream's [`StreamEvent::Usage`] events) and wall-clock duration. The per-zone
/// breakdown is folded into the message zone (these out-of-loop calls have no
/// separately-measured system/tool zones) so zones still sum to the prompt total.
/// Wrapping is opt-in at assembly — only when a telemetry sink is configured — so a
/// telemetry-free embedder keeps a zero-telemetry kernel AND an undecorated provider.
///
/// [`OverflowCompaction`]: atomcode_capabilities::compaction::OverflowCompaction
pub struct MeteredProvider {
    inner: Arc<dyn LlmProvider>,
    attr: Arc<Attribution>,
}

impl MeteredProvider {
    pub fn new(
        inner: Arc<dyn LlmProvider>,
        telemetry: Arc<Telemetry>,
        vendor: impl Into<String>,
        base_url: &str,
        model: impl Into<String>,
        session_id: Option<&str>,
    ) -> Self {
        Self {
            inner,
            attr: Arc::new(Attribution::new(
                telemetry, vendor, base_url, model, session_id,
            )),
        }
    }

    /// Tag every event this provider emits with a `surface` (e.g. `"code_review"`), so
    /// the backend can attribute an out-of-loop sub-agent's token spend to its feature
    /// rather than commingling it with the host session's primary-loop rounds.
    pub fn with_surface(self, surface: impl Into<String>) -> Self {
        let mut attr = (*self.attr).clone();
        attr.surface = Some(surface.into());
        Self {
            inner: self.inner,
            attr: Arc::new(attr),
        }
    }
}

#[async_trait]
impl LlmProvider for MeteredProvider {
    fn model_name(&self) -> &str {
        self.inner.model_name()
    }
    fn context_window(&self) -> u32 {
        self.inner.context_window()
    }
    fn bind_session_id(&self, session_id: &str) {
        self.inner.bind_session_id(session_id);
    }
    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &[ToolDef],
        options: &ChatOptions,
    ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
        let start = Instant::now();
        let ctx_window = self.inner.context_window();
        let messages_count = messages.len() as u32;
        let inner_stream = self.inner.chat_stream(messages, tools, options).await?;
        let attr = self.attr.clone();
        // Proxy the inner stream verbatim, folding Usage events; when it ends, emit ONE
        // LlmChat with the accumulated usage. `unfold`'s step closure is async, so the
        // terminal emit can await — no extra task, the consumer's drain drives it.
        let stream = futures::stream::unfold(
            (
                inner_stream,
                TokenUsage::default(),
                false,
                Some((attr, start, messages_count, ctx_window)),
            ),
            |(mut s, mut usage, had_error, mut pending)| async move {
                match s.next().await {
                    Some(ev) => {
                        let had_error = match &ev {
                            StreamEvent::Usage(u) => {
                                usage.merge_max(*u);
                                had_error
                            }
                            StreamEvent::Error(_) => true,
                            _ => had_error,
                        };
                        Some((ev, (s, usage, had_error, pending)))
                    }
                    None => {
                        if let Some((attr, start, messages_count, ctx_window)) = pending.take() {
                            attr.emit(Event::LlmChat {
                                duration_ms: start.elapsed().as_millis() as u32,
                                tool_calls_count: 0,
                                input_tokens: usage.prompt,
                                output_tokens: usage.completion,
                                cached_tokens: usage.cached,
                                had_error,
                                context_window: ctx_window,
                                system_tokens: 0,
                                tool_def_tokens: 0,
                                tool_result_tokens: 0,
                                // Whole prompt is the (summarizer) message zone; zones sum to total.
                                message_tokens: usage.prompt,
                                messages_count,
                                error_kind: had_error.then_some(LlmErrorKind::Other),
                                error_data: None,
                            })
                            .await;
                        }
                        None
                    }
                }
            },
        );
        Ok(Box::pin(stream))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_kernel::message::MessageMeta;

    #[tokio::test]
    async fn emits_one_llm_chat_per_response_with_attribution() {
        let (tel, captured) = Telemetry::in_memory("test".into());
        let hook = TelemetryHook::new(
            tel,
            "openai",
            "https://api.example.com/v1",
            "deepseek-v4",
            None,
        );

        hook.on_request(
            &[Message::user("hi"), Message::user("there")],
            &[],
            &ChatOptions::default(),
            &TurnCtx::default(),
        )
        .await;

        let mut resp = Message::assistant("answer", vec![]);
        resp.meta = Some(MessageMeta {
            tokens: TokenUsage {
                prompt: 100,
                completion: 20,
                cached: 8,
            },
            elapsed_ms: 1234,
            ctx_window: 128_000,
            ..Default::default()
        });
        hook.on_model_response(&mut resp).await;

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let records = captured.lock().await;
        assert_eq!(records.len(), 1, "exactly one LlmChat per response");
        match &records[0].event {
            Event::LlmChat {
                input_tokens,
                output_tokens,
                cached_tokens,
                duration_ms,
                context_window,
                messages_count,
                ..
            } => {
                assert_eq!(*input_tokens, 100);
                assert_eq!(*output_tokens, 20);
                assert_eq!(*cached_tokens, 8);
                assert_eq!(*duration_ms, 1234);
                assert_eq!(*context_window, 128_000);
                assert_eq!(*messages_count, 2, "captured from on_request");
            }
            other => panic!("expected LlmChat, got {other:?}"),
        }
        assert_eq!(records[0].envelope.provider.as_deref(), Some("openai"));
        assert_eq!(records[0].envelope.model.as_deref(), Some("deepseek-v4"));
    }

    #[tokio::test]
    async fn breaks_down_prompt_zones() {
        use atomcode_kernel::message::Role;
        let (tel, captured) = Telemetry::in_memory("test".into());
        let hook = TelemetryHook::new(tel, "openai", "https://x/v1", "m", None);

        let mut sys = Message::user("you are a helpful coding agent");
        sys.role = Role::System;
        let user = Message::user("hello there");
        let tool_res = Message::tool_result("c1", "a chunk of tool output text", false);
        let tools = vec![ToolDef {
            name: "bash".into(),
            description: "run a shell command".into(),
            parameters: serde_json::json!({"type": "object"}),
        }];
        hook.on_request(
            &[sys, user, tool_res],
            &tools,
            &ChatOptions::default(),
            &TurnCtx::default(),
        )
        .await;

        let mut resp = Message::assistant("ok", vec![]);
        resp.meta = Some(MessageMeta {
            tokens: TokenUsage {
                prompt: 50,
                completion: 5,
                cached: 0,
            },
            ctx_window: 1000,
            ..Default::default()
        });
        hook.on_model_response(&mut resp).await;

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let records = captured.lock().await;
        match &records[0].event {
            Event::LlmChat {
                input_tokens,
                system_tokens,
                message_tokens,
                tool_result_tokens,
                tool_def_tokens,
                messages_count,
                ..
            } => {
                assert!(*system_tokens > 0, "system zone");
                assert!(*message_tokens > 0, "user/assistant zone");
                assert!(*tool_result_tokens > 0, "tool-result zone");
                assert!(*tool_def_tokens > 0, "tool-def zone");
                assert_eq!(*messages_count, 3);
                // The precision property: zones sum to the EXACT prompt total.
                assert_eq!(
                    system_tokens + message_tokens + tool_result_tokens + tool_def_tokens,
                    *input_tokens,
                    "zones must sum to the provider-reported prompt total"
                );
            }
            other => panic!("expected LlmChat, got {other:?}"),
        }
    }

    #[test]
    fn apportion_anchors_real_and_sums_to_total() {
        // No anchor: pure scale of the byte/4 estimates to the exact total.
        let (a, b, c, d) = apportion(100, 0, [10, 20, 30, 40]);
        assert_eq!(a + b + c + d, 100);
        assert!(
            d > a && d > b && d > c,
            "largest estimate keeps the largest share"
        );

        // With an anchor: the REAL portion is preserved untouched in the message zone,
        // only the remainder (60) is distributed across the estimates; sum stays exact.
        let (s, msg, tr, td) = apportion(100, 40, [10, 0, 10, 10]);
        assert_eq!(s + msg + tr + td, 100, "zones sum to the exact total");
        assert!(msg >= 40, "real anchor is preserved in the message zone");

        // Failed round (total 0): estimates pass through, anchor folds into message.
        assert_eq!(apportion(0, 5, [1, 2, 3, 4]), (1, 7, 3, 4));
    }

    #[tokio::test]
    async fn anchors_real_assistant_tokens_in_breakdown() {
        use atomcode_kernel::message::Role;
        let (tel, captured) = Telemetry::in_memory("test".into());
        let hook = TelemetryHook::new(tel, "openai", "https://x/v1", "m", None);

        let mut sys = Message::user("system persona");
        sys.role = Role::System;
        // A past assistant reply carrying a REAL provider completion-token count.
        let mut past = Message::assistant("a prior reply", vec![]);
        past.meta = Some(MessageMeta {
            tokens: TokenUsage {
                prompt: 0,
                completion: 30,
                cached: 0,
            },
            ..Default::default()
        });
        let user = Message::user("next question");
        hook.on_request(
            &[sys, past, user],
            &[],
            &ChatOptions::default(),
            &TurnCtx::default(),
        )
        .await;

        let mut resp = Message::assistant("ok", vec![]);
        resp.meta = Some(MessageMeta {
            tokens: TokenUsage {
                prompt: 100,
                completion: 5,
                cached: 0,
            },
            ctx_window: 1000,
            ..Default::default()
        });
        hook.on_model_response(&mut resp).await;

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let records = captured.lock().await;
        match &records[0].event {
            Event::LlmChat {
                input_tokens,
                system_tokens,
                message_tokens,
                tool_result_tokens,
                tool_def_tokens,
                ..
            } => {
                assert_eq!(*input_tokens, 100);
                assert!(*system_tokens > 0, "system zone scaled from estimate");
                assert!(
                    *message_tokens >= 30,
                    "real assistant tokens preserved, not scaled away"
                );
                assert_eq!(
                    system_tokens + message_tokens + tool_result_tokens + tool_def_tokens,
                    100,
                    "zones still sum to the exact total"
                );
            }
            other => panic!("expected LlmChat, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn no_meta_emits_nothing() {
        let (tel, captured) = Telemetry::in_memory("test".into());
        let hook = TelemetryHook::new(tel, "openai", "https://x/v1", "m", None);
        let mut resp = Message::assistant("hi", vec![]); // meta = None
        hook.on_model_response(&mut resp).await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(captured.lock().await.is_empty());
    }

    #[tokio::test]
    async fn provider_error_turn_emits_had_error_llm_chat() {
        let (tel, captured) = Telemetry::in_memory("test".into());
        let hook = TelemetryHook::new(tel, "openai", "https://x/v1", "m", None);

        hook.on_error("HTTP 429: rate limited").await;
        hook.turn_complete(
            &Conversation::default(),
            &StopReason::ProviderError,
            &TurnCtx::default(),
        )
        .await;

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let records = captured.lock().await;
        assert_eq!(records.len(), 1);
        match &records[0].event {
            Event::LlmChat {
                had_error,
                error_kind,
                ..
            } => {
                assert!(*had_error);
                assert!(matches!(error_kind, Some(LlmErrorKind::RateLimited)));
            }
            other => panic!("expected had_error LlmChat, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn normal_stop_emits_no_extra_llm_chat() {
        let (tel, captured) = Telemetry::in_memory("test".into());
        let hook = TelemetryHook::new(tel, "openai", "https://x/v1", "m", None);
        // A tool error fired on_error, but the turn stopped normally → no LlmChat.
        hook.on_error("tool failed").await;
        hook.turn_complete(
            &Conversation::default(),
            &StopReason::Stopped,
            &TurnCtx::default(),
        )
        .await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(captured.lock().await.is_empty());
    }

    #[tokio::test]
    async fn denied_tool_emits_tool_call_denied() {
        use atomcode_kernel::testkit::EchoTool;
        let (tel, captured) = Telemetry::in_memory("test".into());
        let mw = ToolTelemetryMiddleware::new(tel, "openai", "https://x/v1", "m", None);

        let tool: Arc<dyn Tool> = Arc::new(EchoTool);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let rt = RequestCtx::new(tx, None);
        let mut call = ToolCall {
            id: "c9".into(),
            name: "bash".into(),
            arguments: "{}".into(),
        };
        let _ = mw.before(&mut call, &tool, &rt).await;
        // Approval denied upstream → the kernel hands `after` a "blocked: …" result.
        let mut result = ToolResult {
            call_id: "c9".into(),
            content: "blocked: user denied".into(),
            is_error: true,
            images: vec![],
        };
        mw.after(&mut result).await;

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let records = captured.lock().await;
        assert_eq!(records.len(), 1);
        match &records[0].event {
            Event::ToolCall {
                name,
                success,
                error_kind,
                ..
            } => {
                assert_eq!(name, "bash");
                assert!(!success);
                assert!(matches!(error_kind, Some(ToolErrorKind::DeniedByUser)));
            }
            other => panic!("expected denied ToolCall, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn tool_middleware_emits_tool_call() {
        use atomcode_kernel::testkit::EchoTool;
        let (tel, captured) = Telemetry::in_memory("test".into());
        let mw = ToolTelemetryMiddleware::new(tel, "openai", "https://x/v1", "m", None);

        let tool: Arc<dyn Tool> = Arc::new(EchoTool);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let rt = RequestCtx::new(tx, None);
        let mut call = ToolCall {
            id: "c1".into(),
            name: "bash".into(),
            arguments: "{}".into(),
        };
        let _ = mw.before(&mut call, &tool, &rt).await;
        let mut result = ToolResult {
            call_id: "c1".into(),
            content: "ok".into(),
            is_error: false,
            images: vec![],
        };
        mw.after(&mut result).await;

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let records = captured.lock().await;
        assert_eq!(records.len(), 1);
        match &records[0].event {
            Event::ToolCall { name, success, .. } => {
                assert_eq!(name, "bash");
                assert!(*success);
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
        assert_eq!(records[0].envelope.model.as_deref(), Some("m"));
    }

    /// A canned summary provider: emits text, a usage report, then Done (or an Error when
    /// `fail`), so the decorator has real usage to fold and a terminal to emit on.
    struct CannedSummary {
        fail: bool,
    }
    #[async_trait]
    impl LlmProvider for CannedSummary {
        fn model_name(&self) -> &str {
            "summ"
        }
        async fn chat_stream(
            &self,
            _: &[Message],
            _: &[ToolDef],
            _: &ChatOptions,
        ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
            let mut evs = vec![
                StreamEvent::TextDelta("summary text".into()),
                StreamEvent::Usage(TokenUsage {
                    prompt: 300,
                    completion: 40,
                    cached: 0,
                }),
            ];
            if self.fail {
                evs.push(StreamEvent::Error(ProviderError {
                    message: "boom".into(),
                    ..Default::default()
                }));
            } else {
                evs.push(StreamEvent::Done { truncated: false });
            }
            Ok(Box::pin(futures::stream::iter(evs)))
        }
    }

    #[tokio::test]
    async fn compaction_provider_records_summary_llm_chat() {
        let (tel, captured) = Telemetry::in_memory("test".into());
        let dec = MeteredProvider::new(
            Arc::new(CannedSummary { fail: false }),
            tel,
            "openai",
            "https://x/v1",
            "deepseek-v4",
            None,
        );
        let mut stream = dec
            .chat_stream(
                &[Message::user("summarize this")],
                &[],
                &ChatOptions::default(),
            )
            .await
            .unwrap();
        // Drain — the terminal LlmChat emit fires when the proxied stream ends.
        let mut text = String::new();
        while let Some(ev) = stream.next().await {
            if let StreamEvent::TextDelta(t) = ev {
                text.push_str(&t);
            }
        }
        assert_eq!(text, "summary text", "stream proxied verbatim");

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let records = captured.lock().await;
        assert_eq!(records.len(), 1, "exactly one LlmChat for the summary call");
        match &records[0].event {
            Event::LlmChat {
                input_tokens,
                output_tokens,
                had_error,
                message_tokens,
                ..
            } => {
                assert_eq!(*input_tokens, 300, "folded from StreamEvent::Usage");
                assert_eq!(*output_tokens, 40);
                assert!(!*had_error);
                assert_eq!(*message_tokens, 300, "prompt folded into the message zone");
            }
            other => panic!("expected LlmChat, got {other:?}"),
        }
        assert_eq!(records[0].envelope.model.as_deref(), Some("deepseek-v4"));
    }

    // A MeteredProvider tagged `with_surface` stamps every emitted event's envelope
    // with that surface, so the backend can attribute an out-of-loop sub-agent's spend
    // (e.g. the code_review tool) instead of commingling it with the host session.
    #[tokio::test]
    async fn metered_provider_tags_surface() {
        // Tagged → surface flows to the envelope.
        let (tel, captured) = Telemetry::in_memory("test".into());
        let dec = MeteredProvider::new(
            Arc::new(CannedSummary { fail: false }),
            tel,
            "openai",
            "https://x/v1",
            "deepseek-v4",
            None,
        )
        .with_surface("code_review");
        let mut stream = dec
            .chat_stream(
                &[Message::user("review this")],
                &[],
                &ChatOptions::default(),
            )
            .await
            .unwrap();
        while stream.next().await.is_some() {}
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let records = captured.lock().await;
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].envelope.surface.as_deref(),
            Some("code_review"),
            "a with_surface provider must stamp envelope.surface"
        );

        // Untagged → surface stays None (the primary-loop default).
        let (tel2, captured2) = Telemetry::in_memory("test".into());
        let plain = MeteredProvider::new(
            Arc::new(CannedSummary { fail: false }),
            tel2,
            "openai",
            "https://x/v1",
            "deepseek-v4",
            None,
        );
        let mut s2 = plain
            .chat_stream(&[Message::user("hi")], &[], &ChatOptions::default())
            .await
            .unwrap();
        while s2.next().await.is_some() {}
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let records2 = captured2.lock().await;
        assert_eq!(records2.len(), 1);
        assert_eq!(
            records2[0].envelope.surface, None,
            "an untagged provider must leave envelope.surface None"
        );
    }

    #[tokio::test]
    async fn compaction_provider_marks_had_error_on_stream_error() {
        let (tel, captured) = Telemetry::in_memory("test".into());
        let dec = MeteredProvider::new(
            Arc::new(CannedSummary { fail: true }),
            tel,
            "openai",
            "https://x/v1",
            "m",
            None,
        );
        let mut stream = dec
            .chat_stream(&[Message::user("x")], &[], &ChatOptions::default())
            .await
            .unwrap();
        while stream.next().await.is_some() {}

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let records = captured.lock().await;
        assert_eq!(records.len(), 1);
        match &records[0].event {
            Event::LlmChat {
                had_error,
                error_kind,
                ..
            } => {
                assert!(*had_error, "stream Error → had_error");
                assert!(error_kind.is_some());
            }
            other => panic!("expected LlmChat, got {other:?}"),
        }
    }
}
