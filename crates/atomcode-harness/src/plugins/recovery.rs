//! Recovery policies: what to do when a model request fails in a way that
//! trying again *differently* would fix.
//!
//! Each is a listener on `agent/request`, and each knows exactly one failure
//! mode. They stack in a fixed order because the order is load-bearing:
//!
//! ```text
//! rate-limit  →  overflow  →  retry  →  [ the provider ]
//!  waits out      compacts     backs off
//! ```
//!
//! A rate limit must be handled outermost: waiting is the only thing that helps,
//! and compacting or backing off underneath it just burns the budget faster.
//! Overflow must be handled before a plain retry, because retrying an
//! over-window prompt unchanged is guaranteed to fail the same way.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use atomcode_kernel::message::Message;
use atomcode_plexus::{Context, Next, Plugin, Waterfall};
use serde::Deserialize;
use serde_json::Value;

use crate::events::{AgentRequest, ModelRequest, ModelResponse, RequestError};
use crate::seams::{CompactionSvc, SessionSvc};
use crate::session::{NoticeKind, SessionEvent};

fn parse<T: for<'de> Deserialize<'de> + Default>(config: &Value) -> Result<T, String> {
    if config.is_null() {
        return Ok(T::default());
    }
    serde_json::from_value(config.clone()).map_err(|e| format!("bad config: {e}"))
}

// ---- rate limiting ------------------------------------------------------

#[derive(Debug, Deserialize)]
struct RateLimitRow {
    /// How many times to wait out a 429 before giving the turn back.
    #[serde(default = "default_waits")]
    max_waits: u32,
    /// A 429 whose window resets further out than this is not worth blocking
    /// on: the turn ends and the caller decides when to come back.
    #[serde(default = "default_ceiling")]
    max_wait_secs: u64,
    /// Used when the response carried no `Retry-After`.
    #[serde(default = "default_fallback")]
    fallback_secs: u64,
}

impl Default for RateLimitRow {
    fn default() -> Self {
        Self {
            max_waits: default_waits(),
            max_wait_secs: default_ceiling(),
            fallback_secs: default_fallback(),
        }
    }
}

fn default_waits() -> u32 {
    5
}

/// Beyond two minutes, waiting stops being self-healing and starts being a
/// hang. Mirrors the kernel's `RATE_LIMIT_AUTO_WAIT_SECS`.
fn default_ceiling() -> u64 {
    120
}

fn default_fallback() -> u64 {
    5
}

/// Tell a person, through the log.
///
/// Not `eprintln!`: in a full-screen UI stderr corrupts the display it was
/// meant to inform, and a state that only exists on stderr cannot be rendered
/// by a panel, replayed on resume, or constructed in a test.
fn notice(ctx: &Context, notice: crate::session::NoticeKind, detail: String) {
    if let Some(session) = ctx.service::<SessionSvc>() {
        crate::session::commit(
            ctx,
            &session,
            crate::session::SessionEvent::Notice {
                turn: session.current_turn(),
                notice,
                detail,
            },
        );
    }
}

struct RateLimit {
    ctx: Context,
    max_waits: u32,
    max_wait: Duration,
    fallback: Duration,
}

#[async_trait]
impl Waterfall<AgentRequest> for RateLimit {
    async fn handle(
        &self,
        req: &mut ModelRequest,
        next: Next<'_, AgentRequest>,
    ) -> Result<ModelResponse, RequestError> {
        let mut waits = 0;
        loop {
            match next.run(req).await {
                Ok(response) => return Ok(response),
                Err(error) if error.is_rate_limited() && waits < self.max_waits => {
                    // A real `Retry-After` is authoritative; a guess is only a
                    // fallback. Waiting less than the server asked for is how a
                    // client gets its limit extended.
                    let wait = error.retry_after.unwrap_or(self.fallback);
                    if wait > self.max_wait {
                        // The window resets further out than this turn should
                        // block for. Hand it back rather than hang.
                        return Err(RequestError {
                            message: format!(
                                "{} (resets in {}s — longer than this harness waits)",
                                error.message,
                                wait.as_secs()
                            ),
                            ..error
                        });
                    }
                    waits += 1;
                    notice(
                        &self.ctx,
                        NoticeKind::RateLimited,
                        format!(
                            "rate limited; waiting {}s ({waits}/{})",
                            wait.as_secs(),
                            self.max_waits
                        ),
                    );
                    tokio::time::sleep(wait).await;
                }
                Err(error) => return Err(error),
            }
        }
    }
}

pub struct RateLimitPlugin;

#[async_trait]
impl Plugin for RateLimitPlugin {
    fn name(&self) -> &'static str {
        "llm-rate-limit"
    }
    fn description(&self) -> &'static str {
        "wait out a 429, honouring a real Retry-After"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        let row: RateLimitRow = parse(config)?;
        // Prepended: waiting has to happen outside every other recovery, or
        // they each burn an attempt against a limit that has not lifted.
        let _ = ctx.on_waterfall::<AgentRequest>(
            Arc::new(RateLimit {
                ctx: ctx.clone(),
                max_waits: row.max_waits,
                max_wait: Duration::from_secs(row.max_wait_secs),
                fallback: Duration::from_secs(row.fallback_secs),
            }),
            true,
        );
        Ok(())
    }
}

// ---- context overflow ---------------------------------------------------

#[derive(Debug, Deserialize)]
struct OverflowRow {
    /// Compact-and-retry passes before surfacing the overflow. Each pass cuts
    /// more, so a bounded ladder either fits or proves it cannot.
    #[serde(default = "default_attempts")]
    max_attempts: u32,
}

impl Default for OverflowRow {
    fn default() -> Self {
        Self {
            max_attempts: default_attempts(),
        }
    }
}

fn default_attempts() -> u32 {
    3
}

/// Compact and retry when the history no longer fits.
///
/// Distinct from the threshold-driven compaction, which acts on *predicted*
/// pressure before a request. This one acts on the provider's own verdict after
/// the fact, which is the only signal that is never wrong — and the only one
/// available when the window is unknown or the token estimate was off.
struct OverflowLadder {
    ctx: Context,
    max_attempts: u32,
}

#[async_trait]
impl Waterfall<AgentRequest> for OverflowLadder {
    async fn handle(
        &self,
        req: &mut ModelRequest,
        next: Next<'_, AgentRequest>,
    ) -> Result<ModelResponse, RequestError> {
        let mut attempt = 0;
        loop {
            match next.run(req).await {
                Ok(response) => return Ok(response),
                Err(error) if error.context_overflow && attempt < self.max_attempts => {
                    let (Some(session), Some(compaction)) = (
                        self.ctx.service::<SessionSvc>(),
                        self.ctx.service::<CompactionSvc>(),
                    ) else {
                        // Nothing can shrink the history, so retrying would
                        // fail identically. Say why rather than spin.
                        return Err(RequestError {
                            message: format!(
                                "{} (no compaction provider is mounted, so this cannot recover)",
                                error.message
                            ),
                            ..error
                        });
                    };
                    let Some(decision) = compaction.compact(&session).await else {
                        return Err(RequestError {
                            message: format!(
                                "{} (nothing further can be compacted)",
                                error.message
                            ),
                            ..error
                        });
                    };
                    crate::session::commit(
                        &self.ctx,
                        &session,
                        SessionEvent::Compacted {
                            turn: session.current_turn(),
                            through: decision.through,
                            summary: decision.summary,
                        },
                    );
                    // Re-project: the retry has to carry the compacted history,
                    // and it still has to be exactly what the log says.
                    let mut messages: Vec<Message> = req
                        .messages
                        .iter()
                        .take_while(|m| {
                            m.role == atomcode_kernel::message::Role::System && !m.synthetic
                        })
                        .cloned()
                        .collect();
                    messages.extend(session.derive_messages());
                    req.messages = messages;
                    attempt += 1;
                    notice(
                        &self.ctx,
                        NoticeKind::OverflowCompacted,
                        format!(
                            "context overflow; compacted and retrying ({attempt}/{})",
                            self.max_attempts
                        ),
                    );
                }
                Err(error) => return Err(error),
            }
        }
    }
}

pub struct OverflowPlugin;

#[async_trait]
impl Plugin for OverflowPlugin {
    fn name(&self) -> &'static str {
        "compaction-overflow"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["sessions"]
    }
    fn uses(&self) -> &'static [&'static str] {
        &["compaction"]
    }
    fn description(&self) -> &'static str {
        "compact and retry when the provider says the history no longer fits"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        let row: OverflowRow = parse(config)?;
        let _ = ctx.on_waterfall::<AgentRequest>(
            Arc::new(OverflowLadder {
                ctx: ctx.clone(),
                max_attempts: row.max_attempts,
            }),
            false,
        );
        Ok(())
    }
}

// ---- liveness -----------------------------------------------------------

#[derive(Debug, Deserialize)]
struct TimeoutRow {
    /// A whole request that takes longer than this is treated as hung.
    #[serde(default = "default_request_secs")]
    request_secs: u64,
}

impl Default for TimeoutRow {
    fn default() -> Self {
        Self {
            request_secs: default_request_secs(),
        }
    }
}

fn default_request_secs() -> u64 {
    600
}

/// A ceiling on one model request.
///
/// The provider adapter has its own byte-idle watchdog, which catches a stream
/// that stops producing. This catches the other shape: a stream that keeps
/// producing forever. Without it a turn can hang with no way back except
/// killing the process.
struct RequestTimeout {
    limit: Duration,
}

#[async_trait]
impl Waterfall<AgentRequest> for RequestTimeout {
    async fn handle(
        &self,
        req: &mut ModelRequest,
        next: Next<'_, AgentRequest>,
    ) -> Result<ModelResponse, RequestError> {
        match tokio::time::timeout(self.limit, next.run(req)).await {
            Ok(result) => result,
            Err(_) => Err(RequestError {
                retryable: true,
                ..RequestError::message(format!(
                    "the model request exceeded {}s",
                    self.limit.as_secs()
                ))
            }),
        }
    }
}

pub struct RequestTimeoutPlugin;

#[async_trait]
impl Plugin for RequestTimeoutPlugin {
    fn name(&self) -> &'static str {
        "llm-request-timeout"
    }
    fn description(&self) -> &'static str {
        "bound one model request so a turn cannot hang forever"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        let row: TimeoutRow = parse(config)?;
        // Outermost, so the ceiling covers the waits and retries below it too —
        // a bound that each retry resets is not a bound.
        let _ = ctx.on_waterfall::<AgentRequest>(
            Arc::new(RequestTimeout {
                limit: Duration::from_secs(row.request_secs),
            }),
            true,
        );
        Ok(())
    }
}

// ---- reasoning hygiene --------------------------------------------------

/// Placeholders some adapters emit when a thinking model produced no usable
/// reasoning. They are not content, and storing them means echoing them back
/// next turn as though the model had said them.
const FILLER: &[&str] = &[
    "·",
    "(no reasoning detected)",
    "(no reasoning recorded)",
    "no reasoning detected",
    "no reasoning recorded",
];

/// Strips filler from the reasoning channel.
///
/// It matters more than it looks: reasoning is echoed back to a thinking model
/// on the next request, so a placeholder that survives becomes part of the
/// conversation the model reasons about — and it costs prefix-cacheable tokens
/// to say nothing.
struct ReasoningFilter;

#[async_trait]
impl Waterfall<AgentRequest> for ReasoningFilter {
    async fn handle(
        &self,
        req: &mut ModelRequest,
        next: Next<'_, AgentRequest>,
    ) -> Result<ModelResponse, RequestError> {
        let mut response = next.run(req).await?;
        if response.reasoning.is_empty() {
            return Ok(response);
        }
        let mut cleaned = response.reasoning.clone();
        for marker in FILLER {
            if cleaned.contains(marker) {
                cleaned = cleaned.replace(marker, "");
            }
        }
        let cleaned = cleaned.trim();
        response.reasoning = if cleaned.is_empty() {
            String::new()
        } else {
            cleaned.to_string()
        };
        Ok(response)
    }
}

pub struct ReasoningFilterPlugin;

#[async_trait]
impl Plugin for ReasoningFilterPlugin {
    fn name(&self) -> &'static str {
        "reasoning-filter"
    }
    fn description(&self) -> &'static str {
        "drop placeholder reasoning before it is stored and echoed back"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let _ = ctx.on_waterfall::<AgentRequest>(Arc::new(ReasoningFilter), false);
        Ok(())
    }
}

// ---- partial streams ----------------------------------------------------

#[derive(Debug, Deserialize)]
struct StreamRow {
    /// How many times a turn may preserve a broken stream's output and carry
    /// on. Low on purpose: a stream that keeps breaking mid-flight is a signal
    /// about the connection, not something to paper over indefinitely.
    #[serde(default = "default_recoveries")]
    max_recoveries: u32,
}

impl Default for StreamRow {
    fn default() -> Self {
        Self {
            max_recoveries: default_recoveries(),
        }
    }
}

fn default_recoveries() -> u32 {
    1
}

const RESUME_NUDGE: &str = "\
The previous response was cut off mid-stream. The preserved assistant message and any \
interrupted tool results above are authoritative — continue from that saved progress. Do not \
repeat tool calls that already completed, and do not restart the task.";

/// Keeps what a broken stream produced, instead of discarding it.
///
/// The failure this addresses is quiet: a stream drops after the model has
/// written half an answer and issued two tool calls. Treating that as a plain
/// error throws all of it away, and the retry pays for the same tokens again —
/// or worse, re-runs side-effecting calls that already happened.
///
/// So the partial output is committed as a real assistant message, a nudge
/// explains the situation, and the turn continues from there. Bounded, because
/// a connection that keeps breaking is not something to keep absorbing.
struct StreamRecovery {
    ctx: Context,
    max_recoveries: u32,
    used: std::sync::atomic::AtomicU32,
}

#[async_trait]
impl Waterfall<AgentRequest> for StreamRecovery {
    async fn handle(
        &self,
        req: &mut ModelRequest,
        next: Next<'_, AgentRequest>,
    ) -> Result<ModelResponse, RequestError> {
        match next.run(req).await {
            Ok(response) => Ok(response),
            Err(error) => {
                let Some(partial) = error.partial.clone() else {
                    return Err(error);
                };
                let used = self.used.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                if used > self.max_recoveries {
                    return Err(error);
                }
                let Some(session) = self.ctx.service::<SessionSvc>() else {
                    return Err(error);
                };

                let turn = session.current_turn();
                // Committed, not appended: a preserved message that never
                // reaches the store would be missing from a resumed session
                // while the model still believed it had said it.
                crate::session::commit(
                    &self.ctx,
                    &session,
                    SessionEvent::AssistantMessage {
                        turn,
                        round: req.round,
                        text: partial.text.clone(),
                        reasoning: partial.reasoning.clone(),
                        // Tool calls are deliberately dropped: a call that was
                        // still streaming has partial arguments, and one that
                        // finished has no result recorded — either way the
                        // model must re-issue it rather than have the harness
                        // guess. The nudge says so.
                        tool_calls: Vec::new(),
                    },
                );
                crate::session::commit(
                    &self.ctx,
                    &session,
                    SessionEvent::Injected {
                        turn,
                        text: RESUME_NUDGE.to_string(),
                        origin: crate::session::InjectionOrigin::Continuation,
                    },
                );
                notice(
                    &self.ctx,
                    NoticeKind::StreamRecovered,
                    format!(
                        "stream broke after partial output; preserved it and continuing \
                         ({used}/{})",
                        self.max_recoveries
                    ),
                );

                // Re-project so the retry carries the preserved message.
                let mut messages: Vec<Message> = req
                    .messages
                    .iter()
                    .take_while(|m| {
                        m.role == atomcode_kernel::message::Role::System && !m.synthetic
                    })
                    .cloned()
                    .collect();
                messages.extend(session.derive_messages());
                req.messages = messages;
                next.run(req).await
            }
        }
    }
}

pub struct StreamRecoveryPlugin;

#[async_trait]
impl Plugin for StreamRecoveryPlugin {
    fn name(&self) -> &'static str {
        "llm-stream-recovery"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["sessions"]
    }
    fn description(&self) -> &'static str {
        "keep what a broken stream produced and continue from it"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        let row: StreamRow = parse(config)?;
        let _ = ctx.on_waterfall::<AgentRequest>(
            Arc::new(StreamRecovery {
                ctx: ctx.clone(),
                max_recoveries: row.max_recoveries,
                used: std::sync::atomic::AtomicU32::new(0),
            }),
            false,
        );
        Ok(())
    }
}
