//! History compaction written by the utility model.
//!
//! `compaction-tail` (in `loop_policy.rs`) folds the history away by *listing*
//! what happened, model-free. That is the right default — a compactor that needs
//! a model call cannot run when the provider is the thing that is failing — but a
//! written summary carries far more of the session's substance for the same
//! number of tokens.
//!
//! This row is the other half of the same seam. It folds the *same* span (the
//! boundary is `settled_span`, shared with the model-free strategy so the two
//! cannot disagree about what was compacted) and asks `llm-utility` to rewrite
//! the digest as a summary. Every way the model can fail to help — no utility
//! provider mounted, the call errors, it times out, it answers nothing — falls
//! back to the model-free text, so a compaction always produces a summary.
//!
//! Opt-in, by patch: one `[[remove]]` of `compaction-tail` and one `[[insert]]`
//! of `compaction-summary` (plus a provider for `llm-utility`). One seam has one
//! provider, so this replaces that row rather than joining it.
//!
//! Also here, for any strategy to use: the first rung of an overflow — long tool
//! output shown as a stub — and [`decide_with_strategy`], which runs a kernel
//! [`CompactionStrategy`] against the log, so a policy written for a
//! conversation of messages decides a tree's compaction unchanged.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use atomcode_kernel::message::{
    CompactTrigger, CompactionStrategy, CompactionView, Conversation, Message, Role,
};
use atomcode_kernel::provider::ChatOptions;
use atomcode_kernel::stream::StreamEvent;
use atomcode_plexus::{Context, Plugin};
use futures::StreamExt;
use serde_json::Value;

use crate::seams::{Compaction, CompactionAsk, CompactionDecision, CompactionSvc, LlmUtilitySvc};
use crate::session::{LoggedEvent, Provenance, RewrittenText, SessionEvent, SessionLog};

use super::loop_policy;

fn parse<T: for<'de> serde::Deserialize<'de> + Default>(config: &Value) -> Result<T, String> {
    if config.is_null() {
        return Ok(T::default());
    }
    serde_json::from_value(config.clone()).map_err(|e| format!("bad config: {e}"))
}

const SUMMARY_SYSTEM: &str = "You compress the earlier part of an engineering session into a \
short handover note for whoever continues it. Keep what a continuation needs: what was asked, \
decisions made, files and commands that mattered, and anything still unresolved. When the notes \
open with an earlier summary, update it: keep what is still true and fold in what came after. \
Drop pleasantries and repetition. Write plainly, in the language of the session, with no preamble \
and no closing offer to help.";

/// A model-written summary, with the model-free list as its floor.
struct SummarisingCompaction {
    ctx: Context,
    keep_turns: u64,
    max_tokens: u32,
    timeout: Duration,
}

impl SummarisingCompaction {
    /// Ask the utility model to summarize `digest`. `None` on any failure, so the
    /// caller falls back — a summary is never worth failing a turn over.
    async fn ask(&self, digest: &str) -> Option<String> {
        // Resolved per call, so a patch that swaps the utility model applies to
        // the next compaction without remounting this row.
        let provider = self.ctx.service::<LlmUtilitySvc>()?;
        let prompt = vec![
            Message::system(SUMMARY_SYSTEM),
            Message::user(digest.to_string()),
        ];
        let options = ChatOptions {
            max_tokens: Some(self.max_tokens),
            ..ChatOptions::default()
        };
        let call = async {
            let mut stream = provider.chat_stream(&prompt, &[], &options).await.ok()?;
            let mut out = String::new();
            while let Some(event) = stream.next().await {
                if let StreamEvent::TextDelta(text) = event {
                    out.push_str(&text);
                }
            }
            let trimmed = out.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        };
        tokio::time::timeout(self.timeout, call)
            .await
            .ok()
            .flatten()
    }
}

#[async_trait]
impl Compaction for SummarisingCompaction {
    fn describe(&self) -> String {
        format!(
            "keep the last {} turn(s), summarize the rest with the utility model",
            self.keep_turns
        )
    }

    fn calls_model(&self, log: &SessionLog, ask: &CompactionAsk) -> bool {
        self.ctx.service::<LlmUtilitySvc>().is_some()
            && stub_on_overflow(log, ask).is_none()
            && loop_policy::settled_span(log, self.keep_turns).is_some()
    }

    async fn compact(&self, log: &SessionLog, ask: &CompactionAsk) -> Option<CompactionDecision> {
        if let Some(stubbed) = stub_on_overflow(log, ask) {
            return Some(stubbed);
        }
        let span = loop_policy::settled_span(log, self.keep_turns)?;
        let summary = match self.ask(&span.digest()).await {
            Some(written) => format!("=== EARLIER IN THIS SESSION ===\n{written}\n"),
            // The floor. Same text the model-free row would have produced, so a
            // provider that is down degrades the *quality* of the summary and
            // nothing else.
            None => loop_policy::listed_summary(&span),
        };
        Some(CompactionDecision::fold(span.through, summary))
    }
}

#[derive(Debug, serde::Deserialize)]
struct SummaryRow {
    #[serde(default = "default_keep")]
    keep_turns: u64,
    #[serde(default = "default_tokens")]
    max_tokens: u32,
    #[serde(default = "default_timeout")]
    timeout_secs: u64,
    /// When to compact, shared with the model-free row so swapping the strategy
    /// does not change the pressure that triggers it.
    #[serde(default = "default_threshold")]
    threshold: f32,
}

fn default_keep() -> u64 {
    2
}
fn default_tokens() -> u32 {
    1024
}
fn default_timeout() -> u64 {
    60
}
fn default_threshold() -> f32 {
    0.75
}

impl Default for SummaryRow {
    fn default() -> Self {
        Self {
            keep_turns: default_keep(),
            max_tokens: default_tokens(),
            timeout_secs: default_timeout(),
            threshold: default_threshold(),
        }
    }
}

pub struct CompactionSummaryPlugin;

#[async_trait]
impl Plugin for CompactionSummaryPlugin {
    fn name(&self) -> &'static str {
        "compaction-summary"
    }
    fn uses(&self) -> &'static [&'static str] {
        // `compaction` because it fills that slot; `llm` for the context window
        // the trigger measures against; `llm-utility` for the summary itself.
        &["compaction", "llm", "llm-utility"]
    }
    fn provides(&self) -> &'static [&'static str] {
        &["compaction"]
    }
    fn description(&self) -> &'static str {
        "history compaction summarized by the utility model, falling back to the model-free list"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        let row: SummaryRow = parse(config)?;
        let _ = ctx
            .provide::<CompactionSvc>(Arc::new(SummarisingCompaction {
                ctx: ctx.clone(),
                keep_turns: row.keep_turns,
                max_tokens: row.max_tokens,
                timeout: Duration::from_secs(row.timeout_secs),
            }))
            .map_err(|e| e.to_string())?;
        // The same trigger the model-free row mounts. Without it a swapped-in
        // strategy would be one that never fires.
        loop_policy::mount_compaction_trigger(ctx, row.threshold);
        Ok(())
    }
}

// ---- the first rung of an overflow --------------------------------------

/// Tool output at or below this many bytes is left as it is. A stub is far
/// shorter, which is what keeps stubbing from ever stubbing a stub.
const WORTH_STUBBING: usize = 500;

/// On an overflow, long tool output shown as a one-line stub.
///
/// The cheapest thing that shrinks a request without losing what was asked or
/// decided — and the only thing that helps a turn that overflowed on its own
/// output, which has nothing *settled* for a fold to take. `None` for any other
/// trigger, or when every long result already is a stub.
pub fn stub_on_overflow(log: &SessionLog, ask: &CompactionAsk) -> Option<CompactionDecision> {
    if !matches!(ask.trigger, CompactTrigger::Overflow { .. }) {
        return None;
    }
    let traced = crate::session::derive_traced(&log.events());
    let names: std::collections::HashMap<&str, &str> = traced
        .iter()
        .flat_map(|t| t.message.tool_calls.iter())
        .map(|call| (call.id.as_str(), call.name.as_str()))
        .collect();
    let rewrites: Vec<RewrittenText> = traced
        .iter()
        .filter(|t| t.message.role == Role::Tool && t.message.text.len() > WORTH_STUBBING)
        .filter_map(|t| match t.source {
            Provenance::Event(seq) => Some(RewrittenText {
                seq,
                text: atomcode_capabilities::compaction::build_compact_stub(
                    t.message
                        .tool_call_id
                        .as_deref()
                        .and_then(|id| names.get(id).copied())
                        .unwrap_or("tool"),
                    &t.message.text,
                    !t.message.is_error,
                ),
            }),
            _ => None,
        })
        .collect();
    (!rewrites.is_empty()).then(|| CompactionDecision {
        rewrites,
        ..CompactionDecision::default()
    })
}

// ---- a kernel strategy, deciding against the log ------------------------

/// The conversation a kernel strategy is shown, and where each message came from.
struct StrategyView {
    messages: Vec<Message>,
    sources: Vec<Provenance>,
    /// The protected head: through the first real request.
    floor: usize,
}

impl StrategyView {
    /// The projection, laid out the way a kernel conversation keeps it.
    ///
    /// A strategy protects the first request and looks for the summary it wrote
    /// last just below it, as a synthetic user message. The log projects that
    /// summary as a note at the head, so it moves under the first request. `None`
    /// when nothing was ever asked.
    fn of(events: &[LoggedEvent]) -> Option<Self> {
        let (summaries, rest): (Vec<_>, Vec<_>) = crate::session::derive_traced(events)
            .into_iter()
            .partition(|t| matches!(t.source, Provenance::Summary(_)));
        let first = rest
            .iter()
            .position(|t| t.message.role == Role::User && !t.message.synthetic)?;
        let mut ordered = Vec::with_capacity(rest.len() + summaries.len());
        let mut rest = rest.into_iter();
        ordered.extend(rest.by_ref().take(first + 1));
        ordered.extend(summaries.into_iter().map(|mut summary| {
            summary.message.role = Role::User;
            summary
        }));
        ordered.extend(rest);
        let (messages, sources): (Vec<_>, Vec<_>) =
            ordered.into_iter().map(|t| (t.message, t.source)).unzip();
        let floor = Conversation {
            messages: messages.clone(),
            cache_epoch: 0,
        }
        .sacred_floor();
        Some(Self {
            messages,
            sources,
            floor,
        })
    }

    fn kernel_view(&self, ask: &CompactionAsk) -> CompactionView<'_> {
        CompactionView {
            messages: &self.messages,
            trigger: ask.trigger.clone(),
            ctx_window: ask.window,
            used_tokens: ask.used_tokens,
            utilization: if ask.window == 0 {
                0.0
            } else {
                ask.used_tokens as f32 / ask.window as f32
            },
            sacred_floor: self.floor,
        }
    }
}

/// Whether `strategy` would write a summary for this ask — the slow path.
pub fn strategy_would_summarize(
    strategy: &dyn CompactionStrategy,
    log: &SessionLog,
    ask: &CompactionAsk,
) -> bool {
    StrategyView::of(&log.events())
        .is_some_and(|view| strategy.will_summarize(&view.kernel_view(ask)))
}

/// Run a kernel compaction strategy against the log, and say what it decided in
/// the log's terms.
///
/// The kernel applied a strategy's plan under invariants it owned; the same
/// invariants hold here, against what the model would see:
///
/// - the protected head is never drained or rewritten;
/// - a cut never leaves a tool result whose call it removed;
/// - a plan that does not leave the conversation strictly smaller is refused,
///   so a summary longer than what it replaces never lands.
///
/// A drain without a written summary (no provider, a call that failed) is folded
/// under the model-free list rather than dropped outright.
pub async fn decide_with_strategy(
    strategy: &dyn CompactionStrategy,
    log: &SessionLog,
    ask: &CompactionAsk,
) -> Option<CompactionDecision> {
    let events = log.events();
    let view = StrategyView::of(&events)?;
    let plan = strategy.plan(&view.kernel_view(ask)).await;

    let len = view.messages.len();
    let floor = view.floor.min(len);
    let drain_from = plan.drain_from.max(floor).min(len);
    let drain_to = plan.drain_to.min(len).max(drain_from);
    let mut decision = CompactionDecision {
        note: plan.resume_note,
        ..CompactionDecision::default()
    };

    if drain_to > drain_from {
        let seqs = |range: std::ops::Range<usize>| {
            view.sources[range]
                .iter()
                .filter(|source| !matches!(source, Provenance::Summary(_)))
                .map(Provenance::seq)
                .collect::<Vec<_>>()
        };
        let from = seqs(0..drain_from).into_iter().max().unwrap_or(0);
        let through = match seqs(drain_to..len).into_iter().min() {
            Some(first_kept) => first_kept - 1,
            None => events.last().map(|e| e.seq).unwrap_or(0),
        };
        if let Some(through) = keep_calls_with_results(&events, from, through) {
            decision.from = from;
            decision.through = through;
            decision.summary = plan.summary.unwrap_or_else(|| {
                // The summaries go back to the note they are in the log, so the
                // list carries a model-free one too — it has no sentinel to be
                // recognized by.
                let drained: Vec<Message> = (drain_from..drain_to)
                    .map(|index| {
                        let mut message = view.messages[index].clone();
                        if matches!(view.sources[index], Provenance::Summary(_)) {
                            message.role = Role::System;
                        }
                        message
                    })
                    .collect();
                loop_policy::listed_summary(&loop_policy::CompactedSpan::from_messages(
                    through, &drained,
                ))
            });
        }
    }

    for (index, text) in plan.rewrites {
        if index < floor || index >= len || (drain_from..drain_to).contains(&index) {
            continue;
        }
        if let Provenance::Event(seq) = view.sources[index] {
            if seq > decision.from && seq <= decision.through {
                continue;
            }
            decision.rewrites.push(RewrittenText { seq, text });
        }
    }

    if decision.is_empty() {
        return None;
    }
    let before = wire_bytes(&crate::session::derive_messages(&events));
    let after = wire_bytes(&crate::session::derive_messages(&with_decision(
        &events, &decision,
    )));
    (after < before).then_some(decision)
}

/// Where a cut can go without leaving a result whose call it removed. `None`
/// when no cut above `from` can.
fn keep_calls_with_results(
    events: &[LoggedEvent],
    from: crate::session::SeqNo,
    mut through: crate::session::SeqNo,
) -> Option<crate::session::SeqNo> {
    loop {
        if through <= from {
            return None;
        }
        let trial = CompactionDecision {
            through,
            from,
            summary: String::new(),
            ..CompactionDecision::default()
        };
        let projected = crate::session::derive_messages(&with_decision(events, &trial));
        let mut called = std::collections::HashSet::new();
        let orphan = projected.iter().find_map(|message| {
            called.extend(message.tool_calls.iter().map(|c| c.id.as_str()));
            match (&message.role, message.tool_call_id.as_deref()) {
                (Role::Tool, Some(id)) if !called.contains(id) => Some(id.to_string()),
                _ => None,
            }
        });
        let Some(orphan) = orphan else {
            return Some(through);
        };
        let asked = events.iter().find_map(|logged| match &logged.event {
            SessionEvent::AssistantMessage { tool_calls, .. }
                if tool_calls.iter().any(|c| c.id == orphan) =>
            {
                Some(logged.seq)
            }
            _ => None,
        })?;
        through = asked - 1;
    }
}

/// The log as it would be with `decision` committed.
fn with_decision(events: &[LoggedEvent], decision: &CompactionDecision) -> Vec<LoggedEvent> {
    let mut out = events.to_vec();
    let mut seq = events.last().map(|e| e.seq).unwrap_or(0);
    if !decision.rewrites.is_empty() {
        seq += 1;
        out.push(LoggedEvent {
            seq,
            event: SessionEvent::MessagesRewritten {
                turn: 0,
                texts: decision.rewrites.clone(),
            },
        });
    }
    if decision.through != 0 {
        seq += 1;
        out.push(LoggedEvent {
            seq,
            event: SessionEvent::Compacted {
                turn: 0,
                through: decision.through,
                summary: decision.summary.clone(),
                from: decision.from,
            },
        });
    }
    out
}

/// The kernel's size proxy: the bytes that ride the wire for each message.
/// Tool-call arguments count, or dropping a call-heavy message would not
/// register as a reduction.
fn wire_bytes(messages: &[Message]) -> usize {
    messages
        .iter()
        .map(|m| {
            m.text.len()
                + m.reasoning.as_ref().map_or(0, String::len)
                + m.tool_calls
                    .iter()
                    .map(|c| c.id.len() + c.name.len() + c.arguments.len())
                    .sum::<usize>()
                + m.tool_call_id.as_ref().map_or(0, String::len)
        })
        .sum()
}
