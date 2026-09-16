//! Turn policy: the things a loop usually hardcodes.
//!
//! A round budget, a wall-clock deadline, provider retry, history compaction and
//! a no-progress guard are five separate opinions with five separate lifetimes.
//! In a builder-assembled agent they are five branches in one function. Here each
//! is a row, and the loop's own code says only "ask, then obey".

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use atomcode_kernel::message::Message;
use atomcode_kernel::tool::ToolResult;
use atomcode_plexus::{Context, Listener, Next, Plugin, Waterfall};
use serde::Deserialize;
use serde_json::Value;

use crate::events::{
    AgentRequest, ModelRequest, ModelResponse, RequestError, ToolBatch, ToolExec, ToolsExecute,
    ToolsExecuteBatch, TurnProgress, TurnStopping,
};
use crate::seams::{Compaction, CompactionDecision, CompactionSvc, SessionSvc, StopReason};
use crate::session::InjectionOrigin;
use crate::session::SessionEvent;

fn parse<T: for<'de> Deserialize<'de> + Default>(config: &Value) -> Result<T, String> {
    if config.is_null() {
        return Ok(T::default());
    }
    serde_json::from_value(config.clone()).map_err(|e| format!("bad config: {e}"))
}

// ---- round budget -------------------------------------------------------

#[derive(Debug, Deserialize)]
struct RoundCapRow {
    /// Rounds allowed in one turn. **`0` means no limit**, which is the
    /// convention the coding engine already uses for the same knob
    /// (`parts.rs` / `assemble.rs`: `if cfg.max_rounds != 0 { builder.max_rounds(…) }`)
    /// — so a caller mapping its own config onto this row does not have to
    /// remember to translate, and a session that was unlimited before is still
    /// unlimited after.
    ///
    /// This matters more than it looks. `rounds` counts *completed* steps and the
    /// check is `rounds >= max_rounds`, so a literal reading of `0` stops the
    /// turn before its first request — a session that can do nothing at all. An
    /// unlimited engine assembled onto this row without the special case would
    /// be bricked, not merely capped.
    #[serde(default = "default_rounds")]
    max_rounds: u32,
    /// Stop the turn after this many seconds. `0` disables the deadline.
    #[serde(default)]
    max_seconds: u64,
    /// Ask the person before the budget cuts a turn off: going on grants another
    /// `max_rounds`, anything else — a stop, no answer, nobody to ask — ends it
    /// at the cap. For a front end that draws the question; off, the budget is a
    /// fuse.
    #[serde(default)]
    checkpoint: bool,
}

impl Default for RoundCapRow {
    fn default() -> Self {
        Self {
            max_rounds: default_rounds(),
            max_seconds: 0,
            checkpoint: false,
        }
    }
}

fn default_rounds() -> u32 {
    24
}

struct RoundCap {
    max_rounds: u32,
    max_seconds: u64,
    /// Where the person is asked, when the row asks at all.
    checkpoint: Option<Context>,
    /// Rounds granted past the budget, per session, for the turn they were
    /// granted in.
    granted: Mutex<HashMap<String, (u64, u32)>>,
}

impl RoundCap {
    fn fuse(max_rounds: u32, max_seconds: u64) -> Self {
        Self {
            max_rounds,
            max_seconds,
            checkpoint: None,
            granted: Mutex::new(HashMap::new()),
        }
    }

    /// Put the budget to the person driving this agent. `Some` is the verdict;
    /// `None` means go on.
    async fn ask(&self, ctx: &Context, progress: &TurnProgress, cap: u32) -> Option<StopReason> {
        let scoped = crate::agent::scoped(ctx);
        let Some(log) = scoped.service::<SessionSvc>() else {
            return Some(StopReason::MaxRounds);
        };
        // A delegated child has nobody to ask, and a budget nobody can extend
        // is a fuse.
        let Some(requester) = ctx
            .service::<crate::seams::ToolDriverSvc>()
            .and_then(|driver| driver.requester(log.id()))
        else {
            return Some(StopReason::MaxRounds);
        };
        let answer = requester
            .request(
                atomcode_kernel::event::ROUND_CAP_CHECKPOINT_KIND,
                serde_json::json!({
                    "round": progress.rounds,
                    "cap": cap,
                    // What going on grants, so the front end can say "N more".
                    "base": self.max_rounds,
                }),
            )
            .await;
        if answer.get("continue").and_then(Value::as_bool) == Some(true) {
            let mut granted = self.granted.lock().expect("round grants poisoned");
            let entry = granted
                .entry(log.id().to_string())
                .or_insert((progress.turn, 0));
            entry.1 = entry.1.saturating_add(self.max_rounds);
            return None;
        }
        // No answer because the person pressed stop: the turn was cancelled,
        // not cut off.
        let cancelled = ctx
            .service::<crate::seams::AgentsSvc>()
            .and_then(|agents| agents.by_session(log.id()))
            .is_some_and(|agent| agent.cancelled());
        Some(if cancelled {
            StopReason::Cancelled
        } else {
            StopReason::MaxRounds
        })
    }

    /// This session's budget for this turn: the configured one plus whatever
    /// the person granted during it.
    fn cap_for(&self, ctx: &Context, turn: u64) -> u32 {
        let Some(log) = crate::agent::scoped(ctx).service::<SessionSvc>() else {
            return self.max_rounds;
        };
        let mut granted = self.granted.lock().expect("round grants poisoned");
        match granted.get(log.id()) {
            Some((granted_turn, extra)) if *granted_turn == turn => {
                self.max_rounds.saturating_add(*extra)
            }
            Some(_) => {
                granted.remove(log.id());
                self.max_rounds
            }
            None => self.max_rounds,
        }
    }
}

#[async_trait]
impl Listener<TurnStopping> for RoundCap {
    async fn call(&self, progress: &TurnProgress) -> Option<StopReason> {
        // A turn ending on its own is not cut off by a budget it happened to
        // reach on its last round.
        if !progress.continuing {
            return None;
        }
        // `0` is "no limit", not "stop now" — see `RoundCapRow::max_rounds`.
        // Spelled as a guard rather than by never mounting the listener, so the
        // row can still carry a deadline (`max_seconds`) with the round budget
        // switched off.
        if self.max_rounds > 0 {
            let cap = match &self.checkpoint {
                Some(ctx) => self.cap_for(ctx, progress.turn),
                None => self.max_rounds,
            };
            if progress.rounds >= cap {
                match &self.checkpoint {
                    Some(ctx) => {
                        if let Some(stop) = self.ask(ctx, progress, cap).await {
                            return Some(stop);
                        }
                    }
                    None => return Some(StopReason::MaxRounds),
                }
            }
        }
        if self.max_seconds > 0 && progress.elapsed.as_secs() >= self.max_seconds {
            return Some(StopReason::StoppedByPolicy);
        }
        None
    }
}

pub struct RoundCapPlugin;

#[async_trait]
impl Plugin for RoundCapPlugin {
    fn name(&self) -> &'static str {
        "round-cap"
    }
    fn uses(&self) -> &'static [&'static str] {
        // Only with `checkpoint`, and only to ask: whoever drives the agent, and
        // whether it was that person who stopped the turn.
        &["tool-driver", "agents"]
    }
    fn description(&self) -> &'static str {
        "end a turn after a round budget or a wall-clock deadline"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        let row: RoundCapRow = parse(config)?;
        let mut cap = RoundCap::fuse(row.max_rounds, row.max_seconds);
        if row.checkpoint {
            cap.checkpoint = Some(ctx.clone());
        }
        let _ = ctx.on_serial::<TurnStopping>(Arc::new(cap));
        Ok(())
    }
}

// ---- provider retry -----------------------------------------------------

#[derive(Debug, Deserialize)]
struct RetryRow {
    #[serde(default = "default_attempts")]
    attempts: u32,
    /// First wait; each retry doubles it.
    #[serde(default = "default_backoff")]
    backoff_ms: u64,
    /// Ceiling on the doubling, so a generous `attempts` never turns into a
    /// minutes-long wait.
    #[serde(default = "default_cap")]
    cap_ms: u64,
}

impl Default for RetryRow {
    fn default() -> Self {
        Self {
            attempts: default_attempts(),
            backoff_ms: default_backoff(),
            cap_ms: default_cap(),
        }
    }
}

fn default_attempts() -> u32 {
    3
}

fn default_backoff() -> u64 {
    3000
}

fn default_cap() -> u64 {
    30_000
}

/// The longest a server `Retry-After` may hold a retry. A hostile or
/// misconfigured hint must not park the turn for minutes; our own schedule
/// would not have.
const RETRY_AFTER_CAP: Duration = Duration::from_secs(60);

/// How long to wait before retry number `attempt` (1-based).
///
/// A server `Retry-After` is authoritative — the gateway is saying exactly when
/// to come back — clamped to `[1s, RETRY_AFTER_CAP]`: zero would busy-spin, and
/// the ceiling keeps a bad hint from stalling the turn. Without a hint the wait
/// doubles from `base` and stops at `cap`, so a sustained-but-transient gateway
/// failure (a relay's momentary "no upstream available") gets a real window to
/// clear instead of three quick pokes.
pub(crate) fn retry_backoff(
    attempt: u32,
    base: Duration,
    cap: Duration,
    retry_after: Option<Duration>,
) -> Duration {
    if let Some(hint) = retry_after {
        return hint.clamp(Duration::from_secs(1), RETRY_AFTER_CAP);
    }
    // Bounded shift: a large configured `attempts` must not overflow `1 << n`.
    let shift = attempt.saturating_sub(1).min(20);
    base.saturating_mul(1u32 << shift).min(cap)
}

struct Retry {
    ctx: Context,
    attempts: u32,
    base: Duration,
    cap: Duration,
}

#[async_trait]
impl Waterfall<AgentRequest> for Retry {
    async fn handle(
        &self,
        req: &mut ModelRequest,
        next: Next<'_, AgentRequest>,
    ) -> Result<ModelResponse, RequestError> {
        let mut attempt = 1;
        loop {
            // `Next` is `Copy`, so delegating again genuinely re-runs the
            // request rather than replaying a stale answer.
            match next.run(req).await {
                Ok(response) => return Ok(response),
                Err(error) => {
                    // The provider already classified this. Deciding again from
                    // the message text is how a handler ends up retrying an
                    // auth failure or giving up on a transient one.
                    // An empty answer to a nudge the harness wrote is an answer:
                    // the model read the note and had nothing to add. It arrives
                    // marked retryable like any empty response, and retrying it
                    // spends the budget on a round that was already finished.
                    let nothing_to_add = error.empty_response && req.answering_a_nudge;
                    let worth_retrying = !error.is_fatal()
                        && !nothing_to_add
                        && (error.retryable || error.empty_response)
                        // A rate limit and an overflow each have a dedicated
                        // handler that knows how to make the next attempt
                        // different; a blind retry here would just burn budget.
                        && !error.is_rate_limited()
                        && !error.context_overflow;
                    if attempt >= self.attempts || !worth_retrying {
                        return Err(error);
                    }
                    let wait = retry_backoff(attempt, self.base, self.cap, error.retry_after);
                    // A silent re-issue reads as "nothing happened" to a person
                    // watching a stalled turn. Logged, so every front end can
                    // show it and a replay can explain the gap.
                    super::recovery::notice(
                        &self.ctx,
                        crate::session::NoticeKind::ProviderRetry,
                        format!(
                            "{}; retrying in {}s ({attempt}/{})",
                            error.message,
                            wait.as_secs(),
                            self.attempts.saturating_sub(1)
                        ),
                    );
                    tokio::time::sleep(wait).await;
                    attempt += 1;
                }
            }
        }
    }
}

pub struct RetryPlugin;

#[async_trait]
impl Plugin for RetryPlugin {
    fn name(&self) -> &'static str {
        "llm-retry"
    }
    fn description(&self) -> &'static str {
        "retry a transient provider failure with doubling backoff, honouring a real Retry-After"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        let row: RetryRow = parse(config)?;
        let _ = ctx.on_waterfall::<AgentRequest>(
            Arc::new(Retry {
                ctx: ctx.clone(),
                attempts: row.attempts,
                base: Duration::from_millis(row.backoff_ms),
                cap: Duration::from_millis(row.cap_ms),
            }),
            true,
        );
        Ok(())
    }
}

// ---- compaction ---------------------------------------------------------

#[derive(Debug, Deserialize)]
pub(crate) struct CompactionRow {
    /// Compact once the last request's prompt tokens exceed this fraction of
    /// the model's window.
    #[serde(default = "default_threshold")]
    pub(crate) threshold: f32,
    /// Turns to keep verbatim below the summary.
    #[serde(default = "default_keep")]
    pub(crate) keep_turns: u64,
}

impl Default for CompactionRow {
    fn default() -> Self {
        Self {
            threshold: default_threshold(),
            keep_turns: default_keep(),
        }
    }
}

fn default_threshold() -> f32 {
    0.75
}

fn default_keep() -> u64 {
    2
}

/// The span a compaction folds away, and what it is made of.
///
/// Shared by the model-free and the model-written strategy so the two agree on
/// *what* is compacted and on the material a summary is built from — the
/// boundary is a policy decision, and a second copy of it would be a second
/// answer to the same question.
///
/// Built from what the model already sees of the span, not from the raw log: an
/// earlier summary stands in for everything it replaced, and is carried rather
/// than rebuilt from the events under it. Rebuilding is what dropped a written
/// summary the moment a model-free fold ran after it.
pub struct CompactedSpan {
    pub through: crate::session::SeqNo,
    /// The summaries the model already had for this span, oldest first, verbatim.
    pub prior: Vec<String>,
    /// What was asked since the last fold, one line each, oldest first.
    pub prompts: Vec<String>,
    /// The tools used since the last fold, sorted, once each.
    pub tools: Vec<String>,
}

impl CompactedSpan {
    /// The span as plain text: the earlier summary, then what was asked and used
    /// since. No header and no closing instruction — each strategy wraps it in
    /// its own words.
    pub fn digest(&self) -> String {
        let mut out = String::new();
        for prior in &self.prior {
            out.push_str(prior.trim());
            out.push_str("\n\n");
        }
        if !self.prompts.is_empty() {
            if !self.prior.is_empty() {
                out.push_str("Asked since:\n");
            }
            for prompt in &self.prompts {
                out.push_str("- ");
                out.push_str(prompt);
                out.push('\n');
            }
        }
        if !self.tools.is_empty() {
            out.push_str(&format!("Tools used: {}\n", self.tools.join(", ")));
        }
        out
    }
}

const LISTED_HEADER: &str =
    "=== EARLIER IN THIS SESSION ===\nThese turns were compacted. What was asked:\n";
const LISTED_FOOTER: &str =
    "Ask again for any detail you need from before this point rather than assuming it.\n";
const TOOLS_LINE: &str = "Tools used: ";
/// Requests a listed summary names. Past this the oldest are counted, not listed:
/// a list that grows with every prompt of a long session is its own pressure.
const LISTED_MAX: usize = 40;

/// A listed summary taken apart: what was written before the list, the requests
/// it names, how many it had already stopped naming, and the tools.
fn split_listed(text: &str) -> (&str, Vec<String>, usize, Vec<String>) {
    let Some(at) = text.find(LISTED_HEADER) else {
        return (text.trim(), Vec::new(), 0, Vec::new());
    };
    let list = &text[at + LISTED_HEADER.len()..];
    let list = list.split(LISTED_FOOTER).next().unwrap_or(list);
    let (mut asked, mut omitted, mut tools) = (Vec::new(), 0, Vec::new());
    for line in list.lines() {
        if let Some(named) = line.strip_prefix(TOOLS_LINE) {
            tools.extend(named.split(", ").map(str::to_string));
        } else if let Some(count) = line
            .strip_prefix("- (")
            .and_then(|rest| rest.strip_suffix(" earlier requests not listed)"))
            .and_then(|n| n.parse::<usize>().ok())
        {
            omitted += count;
        } else if let Some(prompt) = line.strip_prefix("- ") {
            asked.push(prompt.to_string());
        }
    }
    (text[..at].trim(), asked, omitted, tools)
}

/// The model-free summary of a span: whatever earlier summary it carries, then
/// what was asked and the tools used. The floor every strategy falls back to.
///
/// An earlier *listed* summary is merged into one list rather than nested under
/// a second header; anything written is kept above it, word for word.
pub fn listed_summary(span: &CompactedSpan) -> String {
    let mut written = Vec::new();
    let mut asked = Vec::new();
    let mut omitted = 0;
    let mut tools = std::collections::BTreeSet::new();
    for prior in &span.prior {
        let (text, named, skipped, used) = split_listed(prior);
        if !text.is_empty() {
            written.push(text);
        }
        asked.extend(named);
        omitted += skipped;
        tools.extend(used);
    }
    asked.extend(span.prompts.iter().cloned());
    tools.extend(span.tools.iter().cloned());
    if asked.len() > LISTED_MAX {
        omitted += asked.len() - LISTED_MAX;
        asked.drain(..asked.len() - LISTED_MAX);
    }

    let mut out = String::new();
    for text in written {
        out.push_str(text);
        out.push_str("\n\n");
    }
    out.push_str(LISTED_HEADER);
    if omitted > 0 {
        out.push_str(&format!("- ({omitted} earlier requests not listed)\n"));
    }
    for prompt in &asked {
        out.push_str("- ");
        out.push_str(prompt);
        out.push('\n');
    }
    if !tools.is_empty() {
        let tools: Vec<_> = tools.into_iter().collect();
        out.push_str(TOOLS_LINE);
        out.push_str(&tools.join(", "));
        out.push('\n');
    }
    out.push_str(LISTED_FOOTER);
    out
}

/// The span a compaction would fold away at this depth. `None` when every turn
/// still fits inside `keep_turns`, when the last fold already reaches as far —
/// there is nothing new under it, and folding it again only rewrites the same
/// cut — or when nothing was asked or used since.
pub fn settled_span(log: &crate::session::SessionLog, keep_turns: u64) -> Option<CompactedSpan> {
    let events = log.events();
    let cutoff = log.current_turn().saturating_sub(keep_turns);
    if cutoff == 0 {
        return None;
    }
    let boundary = events
        .iter()
        .filter(|e| e.event.turn() <= cutoff)
        .map(|e| e.seq)
        .max()?;
    let (folded, summary) = events
        .iter()
        .rev()
        .find_map(|e| match &e.event {
            SessionEvent::Compacted {
                through, summary, ..
            } => Some((*through, Some(summary.clone()))),
            _ => None,
        })
        .unwrap_or((0, None));
    let undone: std::collections::HashSet<u64> = events
        .iter()
        .filter_map(|e| match e.event {
            SessionEvent::Interrupted { turn, undone: true } => Some(turn),
            _ => None,
        })
        .collect();

    let mut prior: Vec<String> = summary.into_iter().collect();
    let mut prompts = Vec::new();
    let mut tools = Vec::new();
    for logged in events
        .iter()
        .filter(|e| e.seq > folded && e.seq <= boundary)
    {
        match &logged.event {
            // A summary a resumed session was seeded with.
            SessionEvent::Injected {
                text,
                origin: InjectionOrigin::CompactionSummary,
                ..
            } => prior.push(text.clone()),
            SessionEvent::UserMessage { turn, text, .. } if !undone.contains(turn) => {
                prompts.push(truncate(text, 200));
            }
            SessionEvent::AssistantMessage {
                turn, tool_calls, ..
            } if !undone.contains(turn) => {
                tools.extend(tool_calls.iter().map(|c| c.name.clone()));
            }
            _ => {}
        }
    }
    if prompts.is_empty() && tools.is_empty() {
        return None;
    }
    tools.sort();
    tools.dedup();
    Some(CompactedSpan {
        through: boundary,
        prior,
        prompts,
        tools,
    })
}

/// Summarize by listing what happened rather than by asking a model.
///
/// Model-free on purpose: a compactor that needs a model call cannot run when
/// the provider is the thing that is failing, and it makes every compaction a
/// billable, latency-bearing event.
struct TailCompaction {
    keep_turns: u64,
}

#[async_trait]
impl Compaction for TailCompaction {
    fn describe(&self) -> String {
        format!(
            "keep the last {} turn(s), summarize the rest",
            self.keep_turns
        )
    }

    async fn compact(&self, log: &crate::session::SessionLog) -> Option<CompactionDecision> {
        let span = settled_span(log, self.keep_turns)?;
        Some(CompactionDecision {
            through: span.through,
            summary: listed_summary(&span),
        })
    }
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.replace('\n', " ");
    }
    text.chars()
        .take(max)
        .collect::<String>()
        .replace('\n', " ")
        + "…"
}

/// Runs before the request goes out: if the last round's usage crossed the
/// threshold, ask the compaction provider for a boundary and log it. The
/// resulting prompt is smaller *and* still fully derived from the log.
struct CompactBeforeRequest {
    ctx: Context,
    threshold: f32,
}

#[async_trait]
impl Waterfall<AgentRequest> for CompactBeforeRequest {
    async fn handle(
        &self,
        req: &mut ModelRequest,
        next: Next<'_, AgentRequest>,
    ) -> Result<ModelResponse, RequestError> {
        let (Some(session), Some(compaction)) = (
            crate::agent::scoped(&self.ctx).service::<SessionSvc>(),
            self.ctx.service::<CompactionSvc>(),
        ) else {
            return next.run(req).await;
        };

        // Context pressure is measured from what the provider last reported,
        // not estimated from the text: the provider is the authority on its own
        // tokenizer.
        let window = self
            .ctx
            .service::<crate::seams::LlmSvc>()
            .map(|p| p.context_window())
            .unwrap_or(0);
        let used = last_prompt_tokens(&session);
        if window == 0 || used == 0 || (used as f32) < (window as f32 * self.threshold) {
            return next.run(req).await;
        }

        if let Some(decision) = compaction.compact(&session).await {
            crate::session::apply_compaction(&self.ctx, &session, decision);
            // Re-project: the request must carry the compacted history, and it
            // must still be exactly what the log says.
            let mut messages: Vec<Message> = req
                .messages
                .iter()
                .take_while(|m| m.role == atomcode_kernel::message::Role::System && !m.synthetic)
                .cloned()
                .collect();
            messages.extend(session.derive_messages());
            req.messages = messages;
        }
        next.run(req).await
    }
}

fn last_prompt_tokens(session: &crate::session::SessionLog) -> u32 {
    session
        .events()
        .iter()
        .rev()
        .find_map(|e| match &e.event {
            SessionEvent::Usage { usage, .. } => Some(usage.prompt),
            _ => None,
        })
        .unwrap_or(0)
}

pub struct CompactionPlugin;

#[async_trait]
impl Plugin for CompactionPlugin {
    fn name(&self) -> &'static str {
        "compaction-tail"
    }
    fn uses(&self) -> &'static [&'static str] {
        // It reads its own slot back through the context (so a later patch can
        // replace the strategy under it) and asks the model adapter for the
        // context window it is compacting against.
        &["compaction", "llm"]
    }
    fn provides(&self) -> &'static [&'static str] {
        &["compaction"]
    }
    fn description(&self) -> &'static str {
        "model-free history compaction: keep recent turns, summarize the rest"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        let row: CompactionRow = parse(config)?;
        let _ = ctx
            .provide::<CompactionSvc>(Arc::new(TailCompaction {
                keep_turns: row.keep_turns,
            }))
            .map_err(|e| e.to_string())?;
        mount_compaction_trigger(ctx, row.threshold);
        Ok(())
    }
}

/// Mount the automatic-compaction trigger.
///
/// *When* to compact is one decision, and it lives here rather than inside a
/// strategy: a deployment that swaps the strategy for a model-written one still
/// wants compaction to fire at the same pressure. Both rows call this, and a
/// replacement strategy that forgot to would be a strategy that never runs.
pub fn mount_compaction_trigger(ctx: &Context, threshold: f32) {
    let _ = ctx.on_waterfall::<AgentRequest>(
        Arc::new(CompactBeforeRequest {
            ctx: ctx.clone(),
            threshold,
        }),
        false,
    );
}

// ---- tool-loop guard ----------------------------------------------------

#[derive(Debug, Deserialize)]
struct GuardRow {
    /// Warn the model after this many identical calls with identical results.
    #[serde(default = "default_warn")]
    warn_after: u32,
    /// End the turn after this many.
    #[serde(default = "default_stop")]
    stop_after: u32,
}

impl Default for GuardRow {
    fn default() -> Self {
        Self {
            warn_after: default_warn(),
            stop_after: default_stop(),
        }
    }
}

fn default_warn() -> u32 {
    3
}

fn default_stop() -> u32 {
    4
}

#[derive(Default)]
struct LoopState {
    /// (call signature, result) -> consecutive repeats.
    counts: HashMap<String, u32>,
    tripped: bool,
}

struct ToolLoopGuard {
    state: Arc<Mutex<LoopState>>,
    warn_after: u32,
    stop_after: u32,
}

#[async_trait]
impl Waterfall<ToolsExecute> for ToolLoopGuard {
    async fn handle(&self, exec: &mut ToolExec, next: Next<'_, ToolsExecute>) -> ToolResult {
        let signature = format!("{}::{}", exec.call.name, exec.call.arguments);
        let mut result = next.run(exec).await;
        let key = format!("{signature}=>{}", result.content);

        let mut state = self.state.lock().expect("loop guard poisoned");
        // Only consecutive repetition counts: a call that made progress in
        // between is not a loop, and clearing here keeps it that way.
        state.counts.retain(|k, _| k == &key);
        let count = state.counts.entry(key).or_insert(0);
        *count += 1;
        let count = *count;
        if count >= self.stop_after {
            state.tripped = true;
            result.is_error = true;
            result.content = format!(
                "[tool-loop guard] `{}` returned the same result {count} times. Stopping: \
                 repeating it will not make progress.",
                exec.call.name
            );
        } else if count >= self.warn_after {
            result.content.push_str(&format!(
                "\n\n[tool-loop guard] This is call {count} with the same arguments and the same \
                 result. Change approach rather than repeating it."
            ));
        }
        result
    }
}

#[async_trait]
impl Listener<TurnStopping> for ToolLoopGuard {
    async fn call(&self, _progress: &TurnProgress) -> Option<StopReason> {
        let mut state = self.state.lock().expect("loop guard poisoned");
        if state.tripped {
            state.tripped = false;
            state.counts.clear();
            return Some(StopReason::ToolLoopDetected);
        }
        None
    }
}

/// The coarse fuse: the same calls, round after round, whatever they returned.
///
/// The exact guard above needs the *results* to match too, which is the right
/// bar for "this is definitely stuck". It misses the case that actually burns a
/// budget: a call whose output varies slightly every time — a timestamp, a
/// counter, a directory listing that keeps changing — issued identically for
/// round after round. Nothing matches, so nothing trips, and the turn runs to
/// its cap.
///
/// So this one keys on the call signature alone, with a higher threshold: nudge
/// first, and only stop if the nudge does not change anything.
struct RepeatFuse {
    ctx: Context,
    nudge_at: u32,
    stop_at: u32,
    state: Mutex<RepeatState>,
}

#[derive(Default)]
struct RepeatState {
    signature: String,
    rounds: u32,
    nudged: bool,
}

const REPEAT_NUDGE: &str = "\
You have issued the SAME tool call with the SAME arguments several rounds in a row. Stop \
repeating it and change your approach. If you are trying to ask the user something, do not \
print it with a shell command — end your turn with a plain-text question. If the task is done, \
reply with a short summary and no tool calls. If you are blocked, say what you need.";

/// Order-independent signature of one round's calls.
///
/// Call ids are excluded on purpose: providers commonly mint a fresh id for an
/// otherwise identical retry, and keying on them would make every repeat look
/// like a new call.
fn round_signature(calls: &[atomcode_kernel::tool::ToolCall]) -> String {
    let mut parts: Vec<String> = calls
        .iter()
        .map(|call| format!("{}\u{0}{}", call.name, call.arguments))
        .collect();
    parts.sort();
    parts.join("\u{1}")
}

#[async_trait]
impl Waterfall<ToolsExecuteBatch> for RepeatFuse {
    async fn handle(
        &self,
        batch: &mut ToolBatch,
        next: Next<'_, ToolsExecuteBatch>,
    ) -> Vec<ToolResult> {
        // A round with no tool calls is not a repeat of anything — it is the
        // model talking. Counting it made three plain answers in a row look
        // like a loop.
        if batch.calls.is_empty() {
            return next.run(batch).await;
        }
        let signature = round_signature(&batch.calls);
        let rounds = {
            let mut state = self.state.lock().expect("repeat fuse poisoned");
            if state.signature == signature {
                state.rounds += 1;
            } else {
                state.signature = signature;
                state.rounds = 1;
                state.nudged = false;
            }
            state.rounds
        };

        let results = next.run(batch).await;

        if rounds >= self.nudge_at {
            let mut state = self.state.lock().expect("repeat fuse poisoned");
            if !state.nudged {
                state.nudged = true;
                drop(state);
                // Logged as a fact with provenance, like every other thing the
                // harness tells the model on its own initiative.
                if let Some(session) = crate::agent::scoped(&self.ctx).service::<SessionSvc>() {
                    crate::session::commit(
                        &self.ctx,
                        &session,
                        SessionEvent::Injected {
                            turn: session.current_turn(),
                            text: REPEAT_NUDGE.to_string(),
                            origin: InjectionOrigin::Continuation,
                        },
                    );
                }
            }
        }
        results
    }
}

#[async_trait]
impl Listener<TurnStopping> for RepeatFuse {
    async fn call(&self, _progress: &TurnProgress) -> Option<StopReason> {
        let mut state = self.state.lock().expect("repeat fuse poisoned");
        if state.rounds >= self.stop_at {
            *state = RepeatState::default();
            return Some(StopReason::ToolLoopDetected);
        }
        None
    }
}

pub struct RepeatFusePlugin;

#[async_trait]
impl Plugin for RepeatFusePlugin {
    fn name(&self) -> &'static str {
        "repeat-fuse"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tools"]
    }
    fn description(&self) -> &'static str {
        "nudge, then stop, when the same calls repeat regardless of what they return"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        #[derive(Deserialize)]
        struct Row {
            #[serde(default = "default_nudge_at")]
            nudge_at: u32,
            #[serde(default = "default_stop_at")]
            stop_at: u32,
        }
        fn default_nudge_at() -> u32 {
            3
        }
        fn default_stop_at() -> u32 {
            6
        }
        let (nudge_at, stop_at) = if config.is_null() {
            (default_nudge_at(), default_stop_at())
        } else {
            let row: Row =
                serde_json::from_value(config.clone()).map_err(|e| format!("bad config: {e}"))?;
            (row.nudge_at, row.stop_at)
        };
        if nudge_at >= stop_at {
            return Err("nudge_at must be lower than stop_at".into());
        }
        let fuse = Arc::new(RepeatFuse {
            ctx: ctx.clone(),
            nudge_at,
            stop_at,
            state: Mutex::new(RepeatState::default()),
        });
        let _ = ctx.on_waterfall::<ToolsExecuteBatch>(fuse.clone(), false);
        let _ = ctx.on_serial::<TurnStopping>(fuse);
        Ok(())
    }
}

pub struct ToolLoopGuardPlugin;

#[async_trait]
impl Plugin for ToolLoopGuardPlugin {
    fn name(&self) -> &'static str {
        "tool-loop-guard"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tools"]
    }
    fn description(&self) -> &'static str {
        "warn, then stop, when the same call keeps returning the same result"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        let row: GuardRow = parse(config)?;
        if row.warn_after >= row.stop_after {
            return Err("warn_after must be lower than stop_after".into());
        }
        let guard = Arc::new(ToolLoopGuard {
            state: Arc::new(Mutex::new(LoopState::default())),
            warn_after: row.warn_after,
            stop_after: row.stop_after,
        });
        // The same object on two seams: it observes executions and it answers
        // the stopping question. One owner, one piece of state.
        let _ = ctx.on_waterfall::<ToolsExecute>(guard.clone(), false);
        let _ = ctx.on_serial::<TurnStopping>(guard);
        Ok(())
    }
}

#[cfg(test)]
mod retry_backoff_tests {
    use super::retry_backoff;
    use std::time::Duration;

    const BASE: Duration = Duration::from_secs(3);
    const CAP: Duration = Duration::from_secs(30);

    #[test]
    fn doubling_backoff_grows_and_stops_at_the_cap() {
        // 1-based attempt → 3, 6, 12, 24, then pinned at the cap.
        assert_eq!(retry_backoff(1, BASE, CAP, None), Duration::from_secs(3));
        assert_eq!(retry_backoff(2, BASE, CAP, None), Duration::from_secs(6));
        assert_eq!(retry_backoff(3, BASE, CAP, None), Duration::from_secs(12));
        assert_eq!(retry_backoff(4, BASE, CAP, None), Duration::from_secs(24));
        assert_eq!(retry_backoff(5, BASE, CAP, None), CAP);
        assert_eq!(retry_backoff(6, BASE, CAP, None), CAP);
        // A large configured count must neither overflow the shift nor pass the cap.
        assert_eq!(retry_backoff(100, BASE, CAP, None), CAP);
    }

    #[test]
    fn a_retry_after_hint_wins_and_is_clamped() {
        // The server's word beats the schedule…
        assert_eq!(
            retry_backoff(1, BASE, CAP, Some(Duration::from_secs(20))),
            Duration::from_secs(20)
        );
        // …a zero hint is floored so the client never busy-spins…
        assert_eq!(
            retry_backoff(3, BASE, CAP, Some(Duration::ZERO)),
            Duration::from_secs(1)
        );
        // …and a hostile hint cannot park the turn for minutes.
        assert_eq!(
            retry_backoff(1, BASE, CAP, Some(Duration::from_secs(6000))),
            Duration::from_secs(60)
        );
    }
}

/// 轮次预算的语义。单独一个模块，因为 `retry_backoff_tests` 只 import 了它自己
/// 那一件东西，而这个判据要 `RoundCap` 与 `TurnProgress`。
#[cfg(test)]
mod round_budget_tests {
    use super::{RoundCap, TurnProgress, TurnStopping};
    use crate::seams::StopReason;
    use atomcode_plexus::Listener;
    use std::time::Duration;

    /// **`0` 是「不限」，不是「立刻停」。**
    ///
    /// coding 引擎对同一个旋钮就是这么定义的（`parts.rs` / `assemble.rs`:
    /// `if cfg.max_rounds != 0 { builder.max_rounds(…) }`）。而这里的判断是
    /// `rounds >= max_rounds`，`rounds` 数的是**已完成**的步数 —— 所以照字面读
    /// `0` 会在回合的第一个请求之前就把它停掉：一个什么也做不了的会话。
    ///
    /// 这不是理论风险，是把「原引擎不限轮次」的配置映射过来时的必经之路：忘了
    /// 特判就把会话废掉，而不是仅仅收紧上限。所以判据钉住两个方向：`0` 必须**永
    /// 不**因轮次而停，正数必须在**恰好**那么多轮时停（不能因为加了 `> 0` 而把
    /// 正数路径也改宽）。
    #[tokio::test]
    async fn zero_rounds_means_unlimited_and_a_positive_cap_still_fires() {
        let at = |rounds: u32| TurnProgress {
            turn: 1,
            rounds,
            tool_calls: 0,
            used_tokens: 0,
            elapsed: Duration::ZERO,
            continuing: true,
        };
        let unlimited = RoundCap::fuse(0, 0);
        // Swept rather than checked at one value: "stopped at round 0" and
        // "stopped at round 10 000" are the two ways an unlimited budget can
        // still be wrong, and only one of them is near the boundary.
        for rounds in [0u32, 1, 2, 24, 25, 10_000] {
            assert_eq!(
                unlimited.call(&at(rounds)).await,
                None,
                "`max_rounds = 0` must never stop a turn for its round count, \
                 but it did at {rounds} rounds"
            );
        }

        let capped = RoundCap::fuse(5, 0);
        assert_eq!(capped.call(&at(4)).await, None, "one under the budget");
        assert_eq!(
            capped.call(&at(5)).await,
            Some(StopReason::MaxRounds),
            "the budget is inclusive: the sixth round must not start"
        );

        // And a deadline still works with the round budget switched off — the
        // pair is why this is a guard in `call` rather than a row that is simply
        // not mounted when `0`.
        let deadline_only = RoundCap::fuse(0, 30);
        let mut late = at(3);
        late.elapsed = Duration::from_secs(31);
        assert_eq!(
            deadline_only.call(&late).await,
            Some(StopReason::StoppedByPolicy),
            "an unlimited round budget must not disable the deadline"
        );
    }
}
