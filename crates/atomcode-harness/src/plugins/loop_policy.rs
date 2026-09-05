//! Turn policy: the things a loop usually hardcodes.
//!
//! A round budget, a wall-clock deadline, provider retry, history compaction and
//! a no-progress guard are five separate opinions with five separate lifetimes.
//! In a builder-assembled agent they are five branches in one function. Here each
//! is a row, and the loop's own code says only "ask, then obey".

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use atomcode_kernel::message::Message;
use atomcode_kernel::tool::ToolResult;
use atomcode_plexus::{Context, Listener, Next, Plugin, Waterfall};
use serde::Deserialize;
use serde_json::Value;

use crate::events::{
    AgentRequest, ModelRequest, ModelResponse, ToolExec, ToolsExecute, TurnProgress, TurnStopping,
};
use crate::seams::{Compaction, CompactionDecision, CompactionSvc, SessionSvc, StopReason};
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
    #[serde(default = "default_rounds")]
    max_rounds: u32,
    /// Stop the turn after this many seconds. `0` disables the deadline.
    #[serde(default)]
    max_seconds: u64,
}

impl Default for RoundCapRow {
    fn default() -> Self {
        Self {
            max_rounds: default_rounds(),
            max_seconds: 0,
        }
    }
}

fn default_rounds() -> u32 {
    24
}

struct RoundCap {
    max_rounds: u32,
    max_seconds: u64,
}

#[async_trait]
impl Listener<TurnStopping> for RoundCap {
    async fn call(&self, progress: &TurnProgress) -> Option<StopReason> {
        if progress.rounds >= self.max_rounds {
            return Some(StopReason::MaxRounds);
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
    fn description(&self) -> &'static str {
        "end a turn after a round budget or a wall-clock deadline"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        let row: RoundCapRow = parse(config)?;
        let _ = ctx.on_serial::<TurnStopping>(Arc::new(RoundCap {
            max_rounds: row.max_rounds,
            max_seconds: row.max_seconds,
        }));
        Ok(())
    }
}

// ---- provider retry -----------------------------------------------------

#[derive(Debug, Deserialize)]
struct RetryRow {
    #[serde(default = "default_attempts")]
    attempts: u32,
    #[serde(default = "default_backoff")]
    backoff_ms: u64,
}

impl Default for RetryRow {
    fn default() -> Self {
        Self {
            attempts: default_attempts(),
            backoff_ms: default_backoff(),
        }
    }
}

fn default_attempts() -> u32 {
    3
}

fn default_backoff() -> u64 {
    1000
}

struct Retry {
    attempts: u32,
    backoff_ms: u64,
}

/// Whether an error is worth trying again. Auth failures and bad requests are
/// not: retrying them burns time and, on a paid endpoint, money.
fn is_transient(message: &str) -> bool {
    let lowered = message.to_lowercase();
    const FATAL: &[&str] = &[
        "401",
        "403",
        "invalid api key",
        "unauthorized",
        "insufficient balance",
        "400",
        "model not found",
    ];
    if FATAL.iter().any(|marker| lowered.contains(marker)) {
        return false;
    }
    const TRANSIENT: &[&str] = &[
        "429",
        "500",
        "502",
        "503",
        "504",
        "timeout",
        "timed out",
        "connection",
        "reset",
        "eof",
        "stream",
    ];
    TRANSIENT.iter().any(|marker| lowered.contains(marker))
}

#[async_trait]
impl Waterfall<AgentRequest> for Retry {
    async fn handle(
        &self,
        req: &mut ModelRequest,
        next: Next<'_, AgentRequest>,
    ) -> Result<ModelResponse, String> {
        let mut attempt = 1;
        loop {
            // `Next` is `Copy`, so delegating again genuinely re-runs the
            // request rather than replaying a stale answer.
            match next.run(req).await {
                Ok(response) => return Ok(response),
                Err(message) => {
                    if attempt >= self.attempts || !is_transient(&message) {
                        return Err(message);
                    }
                    let wait = self.backoff_ms * 2u64.pow(attempt - 1);
                    tokio::time::sleep(std::time::Duration::from_millis(wait)).await;
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
        "retry a transient provider failure with backoff"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        let row: RetryRow = parse(config)?;
        let _ = ctx.on_waterfall::<AgentRequest>(
            Arc::new(Retry {
                attempts: row.attempts,
                backoff_ms: row.backoff_ms,
            }),
            true,
        );
        Ok(())
    }
}

// ---- compaction ---------------------------------------------------------

#[derive(Debug, Deserialize)]
struct CompactionRow {
    /// Compact once the last request's prompt tokens exceed this fraction of
    /// the model's window.
    #[serde(default = "default_threshold")]
    threshold: f32,
    /// Turns to keep verbatim below the summary.
    #[serde(default = "default_keep")]
    keep_turns: u64,
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
        let events = log.events();
        let current = log.current_turn();
        let cutoff = current.saturating_sub(self.keep_turns);
        if cutoff == 0 {
            return None;
        }
        let boundary = events
            .iter()
            .filter(|e| e.event.turn() <= cutoff)
            .map(|e| e.seq)
            .max()?;

        let mut prompts = Vec::new();
        let mut tools = Vec::new();
        for logged in events.iter().filter(|e| e.seq <= boundary) {
            match &logged.event {
                SessionEvent::UserMessage { text, .. } => prompts.push(text.clone()),
                SessionEvent::AssistantMessage { tool_calls, .. } => {
                    tools.extend(tool_calls.iter().map(|c| c.name.clone()));
                }
                _ => {}
            }
        }
        if prompts.is_empty() {
            return None;
        }
        tools.sort();
        tools.dedup();
        let mut summary = String::from(
            "=== EARLIER IN THIS SESSION ===\nThese turns were compacted. What was asked:\n",
        );
        for prompt in &prompts {
            summary.push_str("- ");
            summary.push_str(&truncate(prompt, 200));
            summary.push('\n');
        }
        if !tools.is_empty() {
            summary.push_str(&format!("Tools used: {}\n", tools.join(", ")));
        }
        summary.push_str(
            "Ask again for any detail you need from before this point rather than assuming it.\n",
        );
        Some(CompactionDecision {
            through: boundary,
            summary,
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
    ) -> Result<ModelResponse, String> {
        let (Some(session), Some(compaction)) = (
            self.ctx.service::<SessionSvc>(),
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
            session.append(SessionEvent::Compacted {
                turn: session.current_turn(),
                through: decision.through,
                summary: decision.summary,
            });
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
    fn inject(&self) -> &'static [&'static str] {
        &["sessions"]
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
        let _ = ctx.on_waterfall::<AgentRequest>(
            Arc::new(CompactBeforeRequest {
                ctx: ctx.clone(),
                threshold: row.threshold,
            }),
            false,
        );
        Ok(())
    }
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
