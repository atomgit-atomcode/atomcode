//! Cache-friendly history compaction — the v2 port of core's `collapse_committed`.
//!
//! Stubs OLD tool results in place (full output → a one-line summary), keeping the
//! ACTIVE turn full and exempting `read_file`. It is the POLICY only: the kernel owns
//! every invariant (sacred floor, net-loss guard, tool-call/result pairing repair,
//! `cache_epoch` bump, and turn-boundary-only triggering) in
//! [`Conversation::apply_plan`](atomcode_kernel::message::Conversation::apply_plan) plus
//! the task-boundary auto trigger. This strategy never mutates the conversation; it only
//! proposes a [`CompactionPlan`] of in-place rewrites.
//!
//! Why this is cache-friendly (the whole point — see core's
//! `2026-06-09-cache-friendly-compaction-design`): the stub is COMMITTED to history and
//! MONOTONIC. An already-stubbed result (`text.len() <= MIN_COLLAPSE_SIZE`) yields no
//! rewrite, so a re-run is a noop (the kernel's net-loss guard refuses it, epoch unchanged).
//! Each turn therefore breaks the provider prefix cache at most ONCE, at the TAIL (the
//! turn that just went stale), then freezes — instead of the old ephemeral `microcompact`
//! that re-derived stubs every render and flipped historical bytes full↔stub,炸 the
//! prefix repeatedly. Below the trigger threshold nothing is stubbed at all → short
//! sessions stay full-fidelity and purely append-only (perfect cache).

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use atomcode_kernel::message::{
    CompactTrigger, CompactionPlan, CompactionStrategy, CompactionView, Message, Role,
};
use atomcode_kernel::provider::{ChatOptions, LlmProvider};
use atomcode_kernel::stream::StreamEvent;
use futures::StreamExt;

/// Tool results at or below this byte size are left alone. A produced stub is far under
/// this, which is what makes the rewrite MONOTONIC: re-running never re-stubs a stub.
/// Mirrors core's `MIN_COLLAPSE_SIZE`.
pub const MIN_COLLAPSE_SIZE: usize = 500;

/// Cache-friendly stub compaction (port of core's `collapse_committed` /
/// `compact_old_tool_results_in_place`).
pub struct StubCompaction {
    /// Keep this many most-recent turns FULL (`1` = only the active turn). A "turn"
    /// begins at a non-synthetic [`Role::User`] message.
    keep_recent_turns: usize,
    /// Never stub `read_file` results — compacting them makes the model "falsely
    /// confident" and re-edit the same file (core's 5–7 atomgr finding); keeping them
    /// preserves line-number context for edit mode.
    exempt_read_file: bool,
}

impl Default for StubCompaction {
    /// The normal-path policy from core: keep only the active turn full, exempt read_file.
    fn default() -> Self {
        Self { keep_recent_turns: 1, exempt_read_file: true }
    }
}

impl StubCompaction {
    pub fn new(keep_recent_turns: usize, exempt_read_file: bool) -> Self {
        Self { keep_recent_turns, exempt_read_file }
    }
}

#[async_trait]
impl CompactionStrategy for StubCompaction {
    async fn plan(&self, view: &CompactionView<'_>) -> CompactionPlan {
        let msgs = view.messages;
        let boundary = active_turn_start(msgs, self.keep_recent_turns);
        if boundary <= view.sacred_floor {
            return CompactionPlan::noop(); // nothing older than the kept window
        }
        let id_to_tool = call_id_to_tool(msgs);
        let mut rewrites: Vec<(usize, String)> = Vec::new();
        for (i, m) in msgs.iter().enumerate().take(boundary) {
            if i < view.sacred_floor {
                continue; // protected prefix (kernel re-enforces; skip early)
            }
            if m.role != Role::Tool || m.text.len() <= MIN_COLLAPSE_SIZE {
                continue; // not a (big enough) tool result; already-small = idempotent
            }
            let tool = m
                .tool_call_id
                .as_deref()
                .and_then(|id| id_to_tool.get(id))
                .map(String::as_str)
                .unwrap_or("tool");
            if self.exempt_read_file && tool == "read_file" {
                continue;
            }
            // Tool RESULT success = NOT is_error.
            rewrites.push((i, build_compact_stub(tool, &m.text, !m.is_error)));
        }
        if rewrites.is_empty() {
            return CompactionPlan::noop();
        }
        // Pure in-place stubbing: no drain, no summary, no resume note.
        CompactionPlan { drain_from: 0, drain_to: 0, summary: None, rewrites, resume_note: None }
    }
}

/// Tool results below this size are left alone under OVERFLOW (smaller than the normal
/// `MIN_COLLAPSE_SIZE` — overflow is more aggressive). A produced stub is well under this,
/// so re-running never re-stubs a stub (monotonic / idempotent).
const AGGRESSIVE_STUB_MIN: usize = 160;

/// Marker appended to a hard-truncated message body. Presence of this substring makes
/// truncation idempotent (a re-run skips an already-truncated message).
const TRUNCATE_MARKER: &str = "\n[truncated: showing ";

/// Utilization high-water mark at which the AUTO task-boundary trigger escalates
/// from the cache-friendly stub-only policy to a real drain+summarize (the same
/// plan as a manual `/compact`). Below it, Auto only folds old tool results — cheap
/// and prompt-cache-preserving. At/above it, gentle stubbing is no longer enough to
/// keep context in check, so Auto summarizes old turns (accepting the one-time
/// cache-prefix rewrite) instead of letting context climb until overflow. Auto only
/// fires at all once `compact_threshold` (default 0.7) is crossed; this is the
/// SECOND, higher gate that decides stub-vs-summarize.
const AUTO_DRAIN_UTILIZATION: f32 = 0.85;

/// Hard ceiling on a generated summary. A runaway model could emit an enormous summary that
/// still passes `apply_plan`'s net-loss guard (it's smaller than the drained span) yet bloats
/// the wire on every subsequent turn (#747). We truncate the accumulated summary at this many
/// bytes — the HARD guarantee, independent of whether the provider honors `max_tokens`. A real
/// span summary is a paragraph, far under this; 64 KiB only catches pathological runaways.
const MAX_SUMMARY_BYTES: usize = 64 * 1024;
/// Soft cap forwarded to the summary provider (≈ `MAX_SUMMARY_BYTES`/4). Stops a runaway
/// generation early; the byte cap above is the backstop if the gateway ignores it.
const MAX_SUMMARY_TOKENS: u32 = 16_000;

/// System prompt for the anchored compaction summary LLM call.
const SUMMARY_SYSTEM_PROMPT: &str =
    "You are an anchored context summarization assistant for a coding session. Summarize \
     only the conversation history you are given. If a <previous-summary> block is present, \
     treat it as the current anchored summary and UPDATE it: preserve still-true details, \
     remove stale ones, and merge in new facts — do not rewrite unchanged sections from \
     scratch. Always output the exact section structure requested, keeping every section \
     (write \"(none)\" when empty). Do not answer the conversation. Do not mention that you \
     are summarizing or compacting. Respond in the same language as the conversation.";

/// Wraps [`StubCompaction`] with a hard-OVERFLOW escalation ladder. `Auto`/`Manual`
/// delegate to the inner gentle policy verbatim (normal path unchanged). `Overflow`
/// escalates by `attempt`: 0 = aggressive stub, 1 = hard-truncate oversized messages,
/// 2 = drain old turns into one LLM summary (plain drain when `summary_provider` is None).
///
/// Off the normal path: only the kernel's overflow-retry loop constructs
/// [`CompactTrigger::Overflow`], and only after a real provider rejection — pressure never
/// reaches these tiers.
pub struct OverflowCompaction {
    inner: StubCompaction,
    /// Provider used ONLY by tier 2 to summarize the drained span. `None` ⇒ tier 2 plain-drains.
    summary_provider: Option<Arc<dyn LlmProvider>>,
}

impl OverflowCompaction {
    pub fn new(inner: StubCompaction, summary_provider: Option<Arc<dyn LlmProvider>>) -> Self {
        Self { inner, summary_provider }
    }

    /// Aggressive stub of every tool result in `[from, to)` over `AGGRESSIVE_STUB_MIN`
    /// (no read_file exemption). Monotonic: an already-stubbed result is left alone.
    fn aggressive_stub_rewrites(msgs: &[Message], from: usize, to: usize) -> Vec<(usize, String)> {
        let id_to_tool = call_id_to_tool(msgs);
        let mut out = Vec::new();
        for (i, m) in msgs.iter().enumerate().take(to).skip(from) {
            if m.role != Role::Tool || m.text.len() <= AGGRESSIVE_STUB_MIN {
                continue;
            }
            let tool = m
                .tool_call_id
                .as_deref()
                .and_then(|id| id_to_tool.get(id))
                .map(String::as_str)
                .unwrap_or("tool");
            out.push((i, build_compact_stub(tool, &m.text, !m.is_error)));
        }
        out
    }

    /// Hard-truncate any single message in `[from, len)` whose text exceeds `budget_chars`.
    /// Idempotent via `TRUNCATE_MARKER`. Char-based (CJK-safe).
    fn truncate_rewrites(msgs: &[Message], from: usize, budget_chars: usize) -> Vec<(usize, String)> {
        let mut out = Vec::new();
        for (i, m) in msgs.iter().enumerate().skip(from) {
            if m.text.contains(TRUNCATE_MARKER) {
                continue; // already truncated
            }
            let total = m.text.chars().count();
            if total <= budget_chars {
                continue;
            }
            let head: String = m.text.chars().take(budget_chars).collect();
            out.push((i, format!("{head}{TRUNCATE_MARKER}{budget_chars} of {total} chars]")));
        }
        out
    }

    async fn overflow_plan(&self, view: &CompactionView<'_>, attempt: u8) -> CompactionPlan {
        let msgs = view.messages;
        let floor = view.sacred_floor;
        match attempt {
            0 => {
                let rewrites = Self::aggressive_stub_rewrites(msgs, floor, msgs.len());
                if rewrites.is_empty() {
                    return CompactionPlan::noop();
                }
                CompactionPlan { drain_from: 0, drain_to: 0, summary: None, rewrites, resume_note: None }
            }
            1 => {
                // ~2 chars/token lower bound; min floor so tiny windows don't over-truncate.
                let budget = (view.ctx_window as usize).saturating_mul(2).max(8_000);
                let rewrites = Self::truncate_rewrites(msgs, floor, budget);
                if rewrites.is_empty() {
                    return CompactionPlan::noop();
                }
                CompactionPlan { drain_from: 0, drain_to: 0, summary: None, rewrites, resume_note: None }
            }
            _ => {
                let drain_to = active_turn_start(msgs, 1).max(floor);
                if drain_to <= floor {
                    return CompactionPlan::noop(); // nothing older than the active turn
                }
                let rewrites = Self::aggressive_stub_rewrites(msgs, drain_to, msgs.len());
                if !span_has_non_anchor(&msgs[floor..drain_to]) {
                    // Only a prior anchor is drainable — don't re-drain/summarize it; still
                    // apply the aggressive stub rewrites to the kept span.
                    if rewrites.is_empty() {
                        return CompactionPlan::noop();
                    }
                    return CompactionPlan { drain_from: 0, drain_to: 0, summary: None, rewrites, resume_note: None };
                }
                let summary = self.summarize(&msgs[floor..drain_to], None).await;
                CompactionPlan { drain_from: floor, drain_to, summary, rewrites, resume_note: None }
            }
        }
    }

    /// `/compact [focus]` — a USER-requested compaction (off both the normal stub path and
    /// the overflow ladder). Drains everything older than the active turn into ONE LLM
    /// summary, keeping the active turn intact. This is the v1-parity behavior: plain
    /// `/compact` (no focus) summarizes just like a focused one — a non-empty `focus` only
    /// STEERS the summary toward a topic, it does not gate the drain. No provider ⇒
    /// plain-drain (the net-loss guard refuses it if it wouldn't shrink). `rewrites` is
    /// empty: the old span is replaced by the summary, the kept span is left untouched (less
    /// aggressive than overflow, which also stubs the kept span). Falls back to the gentle
    /// inner stub policy only when there is nothing older than the active turn to drain.
    async fn manual_plan(&self, view: &CompactionView<'_>, focus: Option<&str>) -> CompactionPlan {
        let msgs = view.messages;
        let floor = view.sacred_floor;
        let drain_to = active_turn_start(msgs, 1).max(floor);
        if drain_to <= floor || !span_has_non_anchor(&msgs[floor..drain_to]) {
            // Nothing older than the active turn, OR the only drainable content is a prior
            // anchor (re-summarizing it alone is wasteful and only degrades it) — fall back
            // to the gentle stub policy.
            return self.inner.plan(view).await;
        }
        let summary = self.summarize(&msgs[floor..drain_to], focus).await;
        CompactionPlan { drain_from: floor, drain_to, summary, rewrites: Vec::new(), resume_note: None }
    }

    /// Summarize a drained span into one paragraph via the configured provider. `None` if
    /// no provider, the call errors, or the result is empty (caller then plain-drains).
    /// One-shot, no tools; operates on the already-stubbed span so its input is small.
    /// `focus` (from `/compact <focus>`) steers the summary toward a topic when present.
    async fn summarize(&self, span: &[Message], focus: Option<&str>) -> Option<String> {
        let provider = self.summary_provider.as_ref()?;
        // Split: the prior anchor (if any) is the UPDATE base; everything else is the new
        // transcript to fold in. The anchor is NEVER re-rendered as transcript (erosion fix).
        let previous_anchor = find_prior_anchor(span);
        let new_events: Vec<Message> =
            span.iter().filter(|m| !is_anchor_message(m)).cloned().collect();
        let transcript = render_transcript(&new_events);
        let prompt = vec![
            Message::system(SUMMARY_SYSTEM_PROMPT),
            Message::user(build_summary_prompt(previous_anchor, &transcript, focus)),
        ];
        // Soft-cap the generation (`max_tokens`); the byte loop below is the hard backstop.
        let opts = ChatOptions { max_tokens: Some(MAX_SUMMARY_TOKENS), ..ChatOptions::default() };
        let mut stream = provider.chat_stream(&prompt, &[], &opts).await.ok()?;
        let mut out = String::new();
        // Reserve room for the sentinel + newline prepended below, so the FINAL summary
        // (sentinel + body) still respects MAX_SUMMARY_BYTES (the #747 hard guarantee).
        let body_budget = MAX_SUMMARY_BYTES.saturating_sub(ANCHOR_SENTINEL.len() + 1);
        while let Some(ev) = stream.next().await {
            if let StreamEvent::TextDelta(t) = ev {
                let remaining = body_budget.saturating_sub(out.len());
                if t.len() <= remaining {
                    out.push_str(&t);
                } else {
                    let mut cut = remaining;
                    while !t.is_char_boundary(cut) {
                        cut -= 1; // is_char_boundary(0) == true, so this terminates
                    }
                    out.push_str(&t[..cut]);
                    break;
                }
            }
        }
        // Strip any leading sentinel the model may have echoed, then stamp exactly ONE so the
        // NEXT compaction recognizes this as the anchor.
        let body = out.trim();
        let body = body.strip_prefix(ANCHOR_SENTINEL).map(str::trim_start).unwrap_or(body);
        if body.is_empty() {
            None
        } else {
            Some(format!("{ANCHOR_SENTINEL}\n{body}"))
        }
    }
}

/// Render a message span as a compact role-tagged transcript for the summarizer.
fn render_transcript(span: &[Message]) -> String {
    let mut s = String::new();
    for m in span {
        let role = match m.role {
            Role::System => "SYSTEM",
            Role::User => "USER",
            Role::Assistant => "ASSISTANT",
            Role::Tool => "TOOL",
        };
        s.push_str(role);
        s.push_str(": ");
        s.push_str(&m.text);
        s.push('\n');
    }
    s
}

/// Sentinel first line stamped on every anchored compaction summary. Used to find the
/// prior anchor in a drained span. Bumping the version invalidates older anchors (they
/// are simply treated as plain history → re-summarized once, which is safe).
pub(crate) const ANCHOR_SENTINEL: &str = "<!-- atomcode:anchor v1 -->";

/// True iff `m` is an anchored compaction summary: a kernel-injected (`synthetic`)
/// user-role message whose text starts with [`ANCHOR_SENTINEL`].
fn is_anchor_message(m: &Message) -> bool {
    m.role == Role::User && m.synthetic && m.text.starts_with(ANCHOR_SENTINEL)
}

/// The body of the LAST anchor in `span` (sentinel stripped + trimmed), or `None` if the
/// span has no anchor OR the anchor body is empty (an empty body must NOT drive an UPDATE).
fn find_prior_anchor(span: &[Message]) -> Option<&str> {
    span.iter()
        .rev()
        .find(|m| is_anchor_message(m))
        .map(|m| m.text.strip_prefix(ANCHOR_SENTINEL).unwrap_or(&m.text).trim())
        .filter(|s| !s.is_empty())
}

/// True iff `span` has at least one NON-anchor message (i.e. real content to summarize).
/// When false, the only drainable thing is a prior anchor — re-summarizing it alone is
/// wasteful and would only degrade it, so callers must NOT drain/summarize.
fn span_has_non_anchor(span: &[Message]) -> bool {
    span.iter().any(|m| !is_anchor_message(m))
}

/// Fixed Markdown structure for the anchored summary. English section headers; bullet
/// CONTENT follows the conversation's language (instructed in the system prompt).
const SUMMARY_TEMPLATE: &str = "\
Output exactly this Markdown structure, in this order, every section present:

## Goal
- [single-sentence task summary, or (none)]

## Constraints & Preferences
- [user constraints/preferences/specs, or (none)]

## Progress
### Done
- [completed work, or (none)]
### In Progress
- [current work, or (none)]
### Blocked
- [blockers, or (none)]

## Key Decisions
- [decision and why, or (none)]

## Next Steps
- [ordered next actions, or (none)]

## Critical Context
- [important technical facts, errors, open questions, or (none)]

## Relevant Files
- [path: why it matters, or (none)]

Rules: terse bullets, never prose paragraphs. Preserve exact file paths, commands, error \
strings, and identifiers. Keep every section even when empty.";

/// Build the USER-message text for the summary call. With a `previous_anchor` it instructs
/// an UPDATE and embeds the prior anchor as a `<previous-summary>` block; otherwise a fresh
/// CREATE. `focus` appends a steer. The new `transcript` (events to fold in) is appended last.
fn build_summary_prompt(previous_anchor: Option<&str>, transcript: &str, focus: Option<&str>) -> String {
    let mut p = String::new();
    match previous_anchor {
        Some(anchor) => {
            p.push_str(
                "Update the anchored summary below using the new conversation history. \
                 Preserve the decisions, constraints, and file paths it records, removing \
                 only details the new history supersedes; merge in new facts.\n\
                 <previous-summary>\n",
            );
            p.push_str(anchor);
            p.push_str("\n</previous-summary>\n\n");
        }
        None => p.push_str("Create a new anchored summary from the conversation history.\n\n"),
    }
    p.push_str(SUMMARY_TEMPLATE);
    if let Some(f) = focus.map(str::trim).filter(|f| !f.is_empty()) {
        p.push_str(&format!("\n\nPay special attention to anything related to: {f}."));
    }
    p.push_str("\n\nConversation history to summarize:\n\n");
    p.push_str(transcript);
    p
}

#[async_trait]
impl CompactionStrategy for OverflowCompaction {
    async fn plan(&self, view: &CompactionView<'_>) -> CompactionPlan {
        match &view.trigger {
            CompactTrigger::Overflow { attempt } => self.overflow_plan(view, *attempt).await,
            // Any user-typed `/compact` drains old turns into an LLM summary — matching v1,
            // where plain `/compact` is a real summarize, not just tool-output stubbing. A
            // non-empty focus only STEERS the summary; it no longer gates the drain.
            CompactTrigger::Manual { focus } => self.manual_plan(view, focus.as_deref()).await,
            // AUTO: cache-friendly stub at moderate pressure, but once utilization
            // crosses `AUTO_DRAIN_UTILIZATION` the gentle stub can't keep context in
            // check, so escalate to the SAME drain+summarize as `/compact` (falls back
            // to the stub when there's nothing older than the active turn to drain).
            // This is why auto-compaction now actually reduces context instead of only
            // nibbling tool results — the prior behavior forced users to /compact by hand.
            // The `auto_drain_would_help` guard prevents thrashing: if the bulk is the
            // sacred-floor-protected first message / active turn (e.g. one over-window
            // paste), summarizing the small drainable remainder can't cut pressure, so
            // an LLM summary + cache-bust every turn would be pure waste — stay on stub.
            CompactTrigger::Auto { .. }
                if view.utilization >= AUTO_DRAIN_UTILIZATION && auto_drain_would_help(view) =>
            {
                self.manual_plan(view, None).await
            }
            _ => self.inner.plan(view).await,
        }
    }

    /// True only when `plan` will DRAIN old turns into a summary (the slow path) — for
    /// a manual `/compact`, overflow tier 2, or an AUTO trigger that crossed
    /// `AUTO_DRAIN_UTILIZATION` AND whose drain would actually reduce pressure
    /// (`auto_drain_would_help`). When there is nothing older than the active turn to
    /// drain it falls back to the fast inner stub (drain ≤ floor) and is NOT announced.
    /// CHEAP: just the same `active_turn_start` scan `manual_plan`/`overflow_plan` use.
    fn will_summarize(&self, view: &CompactionView<'_>) -> bool {
        let floor = view.sacred_floor;
        let drains = active_turn_start(view.messages, 1).max(floor) > floor;
        match &view.trigger {
            CompactTrigger::Manual { .. } => drains,
            CompactTrigger::Overflow { attempt } => *attempt >= 2 && drains,
            CompactTrigger::Auto { .. } => {
                view.utilization >= AUTO_DRAIN_UTILIZATION && drains && auto_drain_would_help(view)
            }
        }
    }
}

/// Anti-thrash guard for AUTO escalation: would draining the old (post-sacred-floor,
/// pre-active-turn) span plausibly bring utilization back BELOW the high-water mark?
/// Estimates the tokens that would REMAIN after the drain (the protected prefix + the
/// active turn) by their byte share of the provider-recorded `used_tokens`. If that
/// remainder alone still exceeds `ctx_window * AUTO_DRAIN_UTILIZATION`, summarizing the
/// middle can't help — so an over-window single paste (un-drainable, in the sacred
/// floor) does NOT trigger a futile LLM summary + cache-bust on every turn. Returns
/// `true` (allow escalation) when there is no basis to estimate (window/usage unknown),
/// matching the pre-guard behavior for the normal long-session case.
fn auto_drain_would_help(view: &CompactionView<'_>) -> bool {
    let floor = view.sacred_floor;
    let drain_to = active_turn_start(view.messages, 1).max(floor);
    if drain_to <= floor {
        return false; // nothing drainable
    }
    if view.ctx_window == 0 || view.used_tokens == 0 {
        return true; // no basis to estimate — allow (long-session default)
    }
    let total_bytes: usize = view.messages.iter().map(|m| m.text.len()).sum();
    if total_bytes == 0 {
        return false;
    }
    let drainable_bytes: usize =
        view.messages[floor..drain_to].iter().map(|m| m.text.len()).sum();
    let remaining_bytes = total_bytes.saturating_sub(drainable_bytes);
    // Token estimate of what survives the drain, by byte share of the recorded usage.
    let remaining_tokens =
        (remaining_bytes as u64).saturating_mul(view.used_tokens as u64) / total_bytes as u64;
    let mark = (view.ctx_window as f32 * AUTO_DRAIN_UTILIZATION) as u64;
    remaining_tokens < mark
}

/// Index at which the kept window (the `keep_recent_turns` most-recent turns) begins;
/// everything before it is "old" and eligible to stub. A turn starts at a NON-synthetic
/// [`Role::User`] message (synthetic users are kernel-injected summaries / resume notes,
/// not real turns). Returns `0` when there are not strictly MORE than `keep_recent_turns`
/// real turns — i.e. nothing is old yet (mirrors core's `turns.len() <= keep_recent_turns`).
fn active_turn_start(msgs: &[Message], keep_recent_turns: usize) -> usize {
    if keep_recent_turns == 0 {
        return msgs.len();
    }
    let starts: Vec<usize> = msgs
        .iter()
        .enumerate()
        .filter(|(_, m)| m.role == Role::User && !m.synthetic)
        .map(|(i, _)| i)
        .collect();
    if starts.len() <= keep_recent_turns {
        return 0; // not enough turns to have anything older than the kept window
    }
    starts[starts.len() - keep_recent_turns]
}

/// Map each tool-call id → the tool NAME the model used, harvested from the assistant
/// messages' own `tool_calls` (zero hardcoded tool knowledge). Unknown ids default to
/// `"tool"` at the call site.
fn call_id_to_tool(msgs: &[Message]) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for m in msgs {
        if m.role == Role::Assistant {
            for tc in &m.tool_calls {
                map.insert(tc.id.clone(), tc.name.clone());
            }
        }
    }
    map
}

/// The one-line stub a stubbed tool result is replaced with. Byte-for-byte port of core's
/// `build_compact_stub`: `[<tool> ok|FAILED: N lines, first: <≤80 chars>]`. For a bash
/// result whose first line is the `[elapsed: …]` metadata prefix, the SECOND line is used
/// so `first:` surfaces real output, not the exit-code banner.
pub fn build_compact_stub(tool_name: &str, output: &str, success: bool) -> String {
    let line_count = output.lines().count();
    let first_line: String = {
        let mut iter = output.lines();
        let l1 = iter.next().unwrap_or("(empty)");
        let chosen = if l1.starts_with("[elapsed:") { iter.next().unwrap_or(l1) } else { l1 };
        chosen.chars().take(80).collect()
    };
    let status = if success { "ok" } else { "FAILED" };
    format!("[{tool_name} {status}: {line_count} lines, first: {first_line}]")
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_kernel::message::{CompactTrigger, Conversation, Message};
    use atomcode_kernel::tool::ToolCall;

    fn big(s: &str) -> String {
        // Force output over MIN_COLLAPSE_SIZE so it is eligible to stub.
        format!("{s}\n{}", "x".repeat(MIN_COLLAPSE_SIZE))
    }

    fn asst_call(id: &str, name: &str) -> Message {
        Message::assistant("", vec![ToolCall { id: id.into(), name: name.into(), arguments: "{}".into() }])
    }

    fn view<'a>(msgs: &'a [Message], sacred_floor: usize) -> CompactionView<'a> {
        CompactionView {
            messages: msgs,
            trigger: CompactTrigger::Auto { utilization: 0.8 },
            ctx_window: 1000,
            used_tokens: 800,
            utilization: 0.8,
            sacred_floor,
        }
    }

    fn overflow_view<'a>(msgs: &'a [Message], floor: usize, attempt: u8, ctx_window: u32) -> CompactionView<'a> {
        CompactionView {
            messages: msgs,
            trigger: CompactTrigger::Overflow { attempt },
            ctx_window,
            used_tokens: ctx_window,
            utilization: 1.0,
            sacred_floor: floor,
        }
    }

    fn manual_view<'a>(msgs: &'a [Message], floor: usize, focus: Option<&str>) -> CompactionView<'a> {
        CompactionView {
            messages: msgs,
            trigger: CompactTrigger::Manual { focus: focus.map(str::to_string) },
            ctx_window: 8000,
            used_tokens: 100,
            utilization: 0.0125,
            sacred_floor: floor,
        }
    }

    #[test]
    fn stub_format_matches_core() {
        assert_eq!(build_compact_stub("bash", "line1\nline2\nline3", true), "[bash ok: 3 lines, first: line1]");
        assert_eq!(build_compact_stub("grep", "boom", false), "[grep FAILED: 1 lines, first: boom]");
        assert_eq!(build_compact_stub("bash", "", true), "[bash ok: 0 lines, first: (empty)]");
        // [elapsed: …] banner is skipped so `first:` shows real output.
        assert_eq!(
            build_compact_stub("bash", "[elapsed: 2s, exit: 0]\nreal output here", true),
            "[bash ok: 2 lines, first: real output here]"
        );
    }

    #[tokio::test]
    async fn stubs_old_keeps_active_exempts_read_file() {
        // sys, user1, asst(calls bash+read_file), tool(bash big), tool(read_file big),
        // user2 (active turn), asst(calls grep), tool(grep big)
        let msgs = vec![
            Message::system("persona"),
            Message::user("first"),
            asst_call("b1", "bash").also(asst_call("r1", "read_file")),
            Message::tool_result("b1", &big("bash out"), false),
            Message::tool_result("r1", &big("file contents"), false),
            Message::user("second"),
            asst_call("g1", "grep"),
            Message::tool_result("g1", &big("grep out"), false),
        ];
        let mut conv = Conversation::new();
        conv.messages = msgs;
        let floor = conv.sacred_floor();

        let plan = StubCompaction::default().plan(&view(&conv.messages, floor)).await;
        // Only the OLD bash result is stubbed: read_file exempt, grep is in the active turn.
        let report = conv.apply_plan(plan, floor);
        assert!(report.committed, "a real reduction must commit");
        assert!(conv.messages[3].text.starts_with("[bash "), "old bash → stub: {:?}", conv.messages[3].text);
        assert!(conv.messages[4].text.len() > MIN_COLLAPSE_SIZE, "read_file exempt → still full");
        assert!(conv.messages[7].text.len() > MIN_COLLAPSE_SIZE, "active-turn grep → still full");
    }

    #[tokio::test]
    async fn idempotent_second_run_is_noop() {
        let msgs = vec![
            Message::system("persona"),
            Message::user("first"),
            asst_call("b1", "bash"),
            Message::tool_result("b1", &big("bash out"), false),
            Message::user("second"),
            asst_call("g1", "grep"),
            Message::tool_result("g1", &big("grep out"), false),
        ];
        let mut conv = Conversation::new();
        conv.messages = msgs;
        let floor = conv.sacred_floor();

        let p1 = StubCompaction::default().plan(&view(&conv.messages, floor)).await;
        let r1 = conv.apply_plan(p1, floor);
        assert!(r1.committed);
        let epoch = conv.cache_epoch;

        // Re-plan on the now-stubbed history → nothing left to stub → noop, no epoch bump.
        let p2 = StubCompaction::default().plan(&view(&conv.messages, floor)).await;
        assert!(p2.is_noop(), "already-stubbed history must produce a noop plan");
        let r2 = conv.apply_plan(p2, floor);
        assert!(!r2.committed, "noop must not commit");
        assert_eq!(conv.cache_epoch, epoch, "cache epoch must not move on a noop");
    }

    #[tokio::test]
    async fn single_turn_is_noop() {
        // Only one real turn → nothing is "old".
        let msgs = vec![
            Message::system("persona"),
            Message::user("only"),
            asst_call("b1", "bash"),
            Message::tool_result("b1", &big("bash out"), false),
        ];
        let mut conv = Conversation::new();
        conv.messages = msgs;
        let floor = conv.sacred_floor();
        let plan = StubCompaction::default().plan(&view(&conv.messages, floor)).await;
        assert!(plan.is_noop(), "a single-turn history has nothing older than the active turn");
    }

    #[tokio::test]
    async fn overflow_tier0_stubs_all_tool_results_even_read_file() {
        // Aggressive: read_file is NOT exempt under overflow, and active-turn results stub too.
        let msgs = vec![
            Message::system("persona"),
            Message::user("u1"),
            asst_call("r1", "read_file"),
            Message::tool_result("r1", &big("file body"), false),
        ];
        let mut conv = Conversation::new();
        conv.messages = msgs;
        let floor = conv.sacred_floor();
        let plan = OverflowCompaction::new(StubCompaction::default(), None)
            .plan(&overflow_view(&conv.messages, floor, 0, 8000))
            .await;
        let report = conv.apply_plan(plan, floor);
        assert!(report.committed, "tier 0 must stub the read_file result under overflow");
        assert!(
            conv.messages[3].text.starts_with("[read_file "),
            "read_file stubbed: {:?}",
            conv.messages[3].text
        );
    }

    #[tokio::test]
    async fn overflow_tier1_truncates_oversized_message() {
        let huge = "x".repeat(50_000);
        let msgs = vec![
            Message::system("persona"),
            Message::user("u1"),
            Message::assistant("a1", vec![]),
            Message::user(huge.as_str()), // an oversized later message (e.g. a giant paste)
        ];
        let mut conv = Conversation::new();
        conv.messages = msgs;
        let floor = conv.sacred_floor();
        let plan = OverflowCompaction::new(StubCompaction::default(), None)
            .plan(&overflow_view(&conv.messages, floor, 1, 1000))
            .await;
        let report = conv.apply_plan(plan, floor);
        assert!(report.committed, "tier 1 must truncate the oversized message");
        assert!(
            conv.messages.last().unwrap().text.contains("[truncated: showing "),
            "marker present"
        );
        assert!(conv.messages.last().unwrap().text.len() < huge.len(), "shrunk");
    }

    #[tokio::test]
    async fn overflow_tier1_is_noop_when_nothing_oversized() {
        let msgs = vec![
            Message::system("p"),
            Message::user("small"),
            Message::assistant("tiny", vec![]),
        ];
        let mut conv = Conversation::new();
        conv.messages = msgs;
        let floor = conv.sacred_floor();
        let plan = OverflowCompaction::new(StubCompaction::default(), None)
            .plan(&overflow_view(&conv.messages, floor, 1, 1_000_000))
            .await;
        assert!(plan.is_noop(), "no message exceeds budget → noop");
    }

    #[tokio::test]
    async fn overflow_delegates_normal_triggers_to_inner() {
        // Auto/Manual must behave EXACTLY like StubCompaction (normal path unchanged).
        let msgs = vec![
            Message::system("p"),
            Message::user("u1"),
            asst_call("b1", "bash"),
            Message::tool_result("b1", &big("out"), false),
            Message::user("u2"),
            asst_call("g1", "grep"),
            Message::tool_result("g1", &big("o2"), false),
        ];
        let mut a = Conversation::new();
        a.messages = msgs.clone();
        let mut b = Conversation::new();
        b.messages = msgs;
        let floor = a.sacred_floor();
        let pa = OverflowCompaction::new(StubCompaction::default(), None)
            .plan(&view(&a.messages, floor))
            .await;
        let pb = StubCompaction::default().plan(&view(&b.messages, floor)).await;
        assert_eq!(pa.rewrites, pb.rewrites, "Auto trigger must match inner StubCompaction byte-for-byte");
    }

    struct CannedSummaryProvider;
    #[async_trait]
    impl LlmProvider for CannedSummaryProvider {
        fn model_name(&self) -> &str {
            "canned"
        }
        async fn chat_stream(
            &self,
            _: &[Message],
            _: &[atomcode_kernel::tool::ToolDef],
            _: &ChatOptions,
        ) -> Result<futures::stream::BoxStream<'static, StreamEvent>, atomcode_kernel::stream::ProviderError>
        {
            Ok(Box::pin(futures::stream::iter(vec![
                StreamEvent::TextDelta("PRIOR CONTEXT SUMMARY".into()),
                StreamEvent::Done { truncated: false },
            ])))
        }
    }

    /// Streams ~96 KiB of a 3-byte CJK char so the summary blows past `MAX_SUMMARY_BYTES`,
    /// exercising BOTH the byte ceiling and char-boundary-safe truncation (96 KiB is not a
    /// multiple of 3, so the cut lands mid-char and must be walked back).
    struct HugeSummaryProvider {
        /// Records the `max_tokens` the summarizer forwarded, so the test can assert the SOFT
        /// cap is actually sent (the byte hard-cap would otherwise mask a dropped max_tokens).
        max_tokens_seen: std::sync::Arc<std::sync::atomic::AtomicU32>,
    }
    #[async_trait]
    impl LlmProvider for HugeSummaryProvider {
        fn model_name(&self) -> &str {
            "huge"
        }
        async fn chat_stream(
            &self,
            _: &[Message],
            _: &[atomcode_kernel::tool::ToolDef],
            opts: &ChatOptions,
        ) -> Result<futures::stream::BoxStream<'static, StreamEvent>, atomcode_kernel::stream::ProviderError>
        {
            self.max_tokens_seen
                .store(opts.max_tokens.unwrap_or(0), std::sync::atomic::Ordering::SeqCst);
            let chunk = "你".repeat(8 * 1024); // 24 KiB per delta
            Ok(Box::pin(futures::stream::iter(vec![
                StreamEvent::TextDelta(chunk.clone()),
                StreamEvent::TextDelta(chunk.clone()),
                StreamEvent::TextDelta(chunk.clone()),
                StreamEvent::TextDelta(chunk),
                StreamEvent::Done { truncated: false },
            ])))
        }
    }

    #[tokio::test]
    async fn summary_is_capped_at_max_bytes() {
        let msgs = vec![
            Message::system("p"),
            Message::user("u1"),
            Message::assistant(big("a1"), vec![]),
            Message::user(big("u2")),
            Message::assistant(big("a2"), vec![]),
            Message::user("u3-active"),
        ];
        let mut conv = Conversation::new();
        conv.messages = msgs;
        let floor = conv.sacred_floor();
        let seen = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let strat = OverflowCompaction::new(
            StubCompaction::default(),
            Some(Arc::new(HugeSummaryProvider { max_tokens_seen: seen.clone() })),
        );
        let plan = strat.plan(&overflow_view(&conv.messages, floor, 2, 8000)).await;
        let summary = plan.summary.expect("tier 2 produces a summary");
        // Hard byte ceiling enforced (provider streamed ~96 KiB). The total is
        // sentinel + "\n" + body, all ≤ MAX_SUMMARY_BYTES.
        assert!(
            summary.len() <= MAX_SUMMARY_BYTES,
            "summary {} bytes exceeds cap {MAX_SUMMARY_BYTES}",
            summary.len()
        );
        // The body budget is MAX_SUMMARY_BYTES - ANCHOR_SENTINEL.len() - 1 (for "\n").
        // The total fills essentially to MAX_SUMMARY_BYTES (body truncated at a char
        // boundary just at the budget, then the sentinel + "\n" are prepended). The
        // lower bound is still well within 4 bytes of the cap.
        assert!(summary.len() > MAX_SUMMARY_BYTES - 4, "should truncate near the cap, got {}", summary.len());
        // Output is anchor-prefixed.
        assert!(summary.starts_with(ANCHOR_SENTINEL), "summary must carry the sentinel");
        // Char-boundary-safe: the body (after sentinel + newline) is intact and all 你.
        let body_part = summary.strip_prefix(ANCHOR_SENTINEL).unwrap().trim_start_matches('\n');
        assert!(body_part.chars().all(|c| c == '你'), "truncation must not corrupt multibyte chars");
        // SOFT cap was actually forwarded (not masked by the byte hard-cap / dropped like the
        // historical v2 max_tokens regression).
        assert_eq!(
            seen.load(std::sync::atomic::Ordering::SeqCst),
            MAX_SUMMARY_TOKENS,
            "summarize() must forward max_tokens as the soft cap"
        );
    }

    #[tokio::test]
    async fn overflow_tier2_drains_old_turns_into_llm_summary() {
        // The drained span must be BIGGER than the summary, or apply_plan's net-loss guard
        // (correctly) refuses — so the old turns carry bulk, as in a real overflow.
        let msgs = vec![
            Message::system("p"),
            Message::user("u1"),
            Message::assistant(big("a1"), vec![]),
            Message::user(big("u2")),
            Message::assistant(big("a2"), vec![]),
            Message::user("u3-active"),
        ];
        let mut conv = Conversation::new();
        conv.messages = msgs;
        let floor = conv.sacred_floor();
        let strat = OverflowCompaction::new(StubCompaction::default(), Some(Arc::new(CannedSummaryProvider)));
        let plan = strat.plan(&overflow_view(&conv.messages, floor, 2, 8000)).await;
        let s = plan.summary.as_deref().expect("tier 2 attaches the LLM summary");
        assert!(s.starts_with(ANCHOR_SENTINEL) && s.contains("PRIOR CONTEXT SUMMARY"));
        assert!(plan.drain_from == floor && plan.drain_to > floor, "drains the old span");
        let report = conv.apply_plan(plan, floor);
        assert!(report.committed);
        // The drained span is replaced by ONE synthetic_user anchor message.
        assert!(conv.messages.iter().any(|m| is_anchor_message(m) && m.text.contains("PRIOR CONTEXT SUMMARY")));
    }

    #[tokio::test]
    async fn manual_focus_drains_old_into_focused_summary() {
        // `/compact <focus>`: old turns (carrying bulk) drain into ONE focused LLM summary;
        // the active turn stays intact. Span must out-weigh the summary or the net-loss
        // guard refuses — so the old turns are big(), as in real usage.
        let msgs = vec![
            Message::system("p"),
            Message::user("u1"),
            Message::assistant(big("a1"), vec![]),
            Message::user(big("u2")),
            Message::assistant(big("a2"), vec![]),
            Message::user("u3-active"),
        ];
        let mut conv = Conversation::new();
        conv.messages = msgs;
        let floor = conv.sacred_floor();
        let strat =
            OverflowCompaction::new(StubCompaction::default(), Some(Arc::new(CannedSummaryProvider)));
        let plan = strat.plan(&manual_view(&conv.messages, floor, Some("the auth refactor"))).await;
        let s = plan.summary.as_deref().expect("focused manual compaction summarizes");
        assert!(s.starts_with(ANCHOR_SENTINEL) && s.contains("PRIOR CONTEXT SUMMARY"));
        assert!(plan.drain_from == floor && plan.drain_to > floor, "drains the old span");
        assert!(plan.rewrites.is_empty(), "manual focus keeps the active span untouched");
        let report = conv.apply_plan(plan, floor);
        assert!(report.committed);
        assert!(conv.messages.iter().any(|m| is_anchor_message(m) && m.text.contains("PRIOR CONTEXT SUMMARY")));
    }

    #[tokio::test]
    async fn manual_plain_and_blank_focus_drain_and_summarize() {
        // v1 parity: plain `/compact` (None) and a blank focus ("   ") now drain old turns
        // into an LLM summary just like a focused /compact — they are NOT stub-only. A focus
        // would only steer the summary; its absence does not gate the drain.
        let msgs = vec![
            Message::system("p"),
            Message::user("u1"),
            Message::assistant(big("a1"), vec![]),
            Message::user(big("u2")),
            Message::assistant(big("a2"), vec![]),
            Message::user("u3-active"),
        ];
        let mut conv = Conversation::new();
        conv.messages = msgs;
        let floor = conv.sacred_floor();
        let strat =
            OverflowCompaction::new(StubCompaction::default(), Some(Arc::new(CannedSummaryProvider)));
        for focus in [None, Some("   ")] {
            let plan = strat.plan(&manual_view(&conv.messages, floor, focus)).await;
            let s = plan.summary.as_deref().expect(&format!("plain/blank manual summarizes (focus={focus:?})"));
            assert!(s.starts_with(ANCHOR_SENTINEL) && s.contains("PRIOR CONTEXT SUMMARY"));
            assert!(plan.drain_from == floor && plan.drain_to > floor, "drains the old span");
            assert!(plan.rewrites.is_empty(), "manual keeps the active span untouched");
        }
    }

    #[tokio::test]
    async fn manual_plain_falls_back_to_stub_when_nothing_older_than_active_turn() {
        // A genuinely short conversation (nothing older than the active turn to drain) still
        // delegates to the gentle inner stub policy — no spurious empty/summary drain.
        let msgs = vec![
            Message::system("p"),
            Message::user("u1-active"),
            asst_call("b1", "bash"),
            Message::tool_result("b1", &big("out"), false),
        ];
        let mut conv = Conversation::new();
        conv.messages = msgs;
        let floor = conv.sacred_floor();
        let strat =
            OverflowCompaction::new(StubCompaction::default(), Some(Arc::new(CannedSummaryProvider)));
        let stub = StubCompaction::default().plan(&view(&conv.messages, floor)).await;
        let plan = strat.plan(&manual_view(&conv.messages, floor, None)).await;
        assert!(plan.summary.is_none(), "no drain/summary when nothing is older than active turn");
        assert!(plan.drain_to == 0, "no drain on fallback");
        assert_eq!(plan.rewrites, stub.rewrites, "falls back to inner stub byte-for-byte");
    }

    fn auto_view<'a>(msgs: &'a [Message], floor: usize, utilization: f32) -> CompactionView<'a> {
        CompactionView {
            messages: msgs,
            trigger: CompactTrigger::Auto { utilization },
            ctx_window: 1000,
            used_tokens: (1000.0 * utilization) as u32,
            utilization,
            sacred_floor: floor,
        }
    }

    /// The user's core ask: auto-compaction must ACTUALLY reduce context (like
    /// `/compact`) once pressure is high, not just nibble old tool results.
    #[tokio::test]
    async fn auto_at_high_utilization_drains_and_summarizes_like_manual() {
        let msgs = vec![
            Message::system("p"),
            Message::user("u1"),
            Message::assistant(big("a1"), vec![]),
            Message::user(big("u2")),
            Message::assistant(big("a2"), vec![]),
            Message::user("u3-active"),
        ];
        let mut conv = Conversation::new();
        conv.messages = msgs;
        let floor = conv.sacred_floor();
        let strat =
            OverflowCompaction::new(StubCompaction::default(), Some(Arc::new(CannedSummaryProvider)));
        let v = auto_view(&conv.messages, floor, 0.95);
        assert!(strat.will_summarize(&v), "auto at high pressure must announce a summarize");
        let plan = strat.plan(&v).await;
        let s = plan.summary.as_deref().expect("auto-high drains old turns into an LLM summary, like manual /compact");
        assert!(s.starts_with(ANCHOR_SENTINEL) && s.contains("PRIOR CONTEXT SUMMARY"));
        assert!(plan.drain_from == floor && plan.drain_to > floor, "auto-high drains the old span");
    }

    /// Below the high-water mark, Auto keeps the cache-friendly stub-only policy
    /// (no history rewrite, no LLM summary).
    #[tokio::test]
    async fn auto_at_moderate_utilization_stays_gentle_stub() {
        let msgs = vec![
            Message::system("p"),
            Message::user("u1"),
            asst_call("b1", "bash"),
            Message::tool_result("b1", &big("bash out"), false),
            Message::user("u2-active"),
        ];
        let mut conv = Conversation::new();
        conv.messages = msgs;
        let floor = conv.sacred_floor();
        let strat =
            OverflowCompaction::new(StubCompaction::default(), Some(Arc::new(CannedSummaryProvider)));
        let v = auto_view(&conv.messages, floor, 0.8);
        assert!(!strat.will_summarize(&v), "moderate auto must NOT announce a summarize");
        let plan = strat.plan(&v).await;
        assert!(plan.summary.is_none(), "moderate auto = no LLM summary");
        assert_eq!(plan.drain_to, 0, "moderate auto = no drain (stub only)");
    }

    /// Anti-thrash: when the bulk is the sacred-floor-protected FIRST user message
    /// (the over-window-paste case), draining the small remainder can't bring
    /// utilization down — so Auto must NOT pay an LLM summary + cache-bust every
    /// turn. It stays on the cheap stub even though utilization is high.
    #[tokio::test]
    async fn auto_does_not_summarize_when_bulk_is_protected_first_message() {
        let msgs = vec![
            Message::system("p"),
            // Huge first paste — protected by sacred_floor, un-drainable.
            Message::user(&"x".repeat(5000)),
            Message::assistant("a1", vec![]),
            Message::user("u2"),
            Message::assistant("a2", vec![]),
            Message::user("u3-active"),
        ];
        let mut conv = Conversation::new();
        conv.messages = msgs;
        let floor = conv.sacred_floor();
        let strat =
            OverflowCompaction::new(StubCompaction::default(), Some(Arc::new(CannedSummaryProvider)));
        let v = auto_view(&conv.messages, floor, 0.95);
        assert!(
            !strat.will_summarize(&v),
            "must NOT summarize when the bulk is un-drainable (anti-thrash)"
        );
        let plan = strat.plan(&v).await;
        assert!(plan.summary.is_none(), "no LLM summary when draining can't reduce pressure");
    }

    #[tokio::test]
    async fn overflow_tier2_plain_drains_without_provider() {
        let msgs = vec![
            Message::system("p"),
            Message::user("u1"),
            Message::assistant("a1", vec![]),
            Message::user("u2-active"),
        ];
        let mut conv = Conversation::new();
        conv.messages = msgs;
        let floor = conv.sacred_floor();
        let plan = OverflowCompaction::new(StubCompaction::default(), None)
            .plan(&overflow_view(&conv.messages, floor, 2, 8000))
            .await;
        assert!(plan.summary.is_none(), "no provider → plain drain");
        assert!(plan.drain_to > floor);
    }

    #[test]
    fn will_summarize_announces_only_drain_paths() {
        // Multi-turn history: a manual `/compact` WILL drain old turns → announce.
        let multi = vec![
            Message::system("p"),
            Message::user("u1"),
            Message::assistant(big("a1"), vec![]),
            Message::user(big("u2")),
            Message::assistant(big("a2"), vec![]),
            Message::user("u3-active"),
        ];
        let mut conv = Conversation::new();
        conv.messages = multi;
        let floor = conv.sacred_floor();
        let strat = OverflowCompaction::new(StubCompaction::default(), None);

        assert!(
            strat.will_summarize(&manual_view(&conv.messages, floor, None)),
            "manual /compact with >1 real turn drains → announce"
        );
        // Auto never announces (it only does the fast in-place stub).
        assert!(
            !strat.will_summarize(&view(&conv.messages, floor)),
            "auto trigger never announces a summary"
        );
        // Overflow tier 0/1 (stub/truncate) stay silent; only tier 2 drains.
        assert!(!strat.will_summarize(&overflow_view(&conv.messages, floor, 0, 8000)));
        assert!(!strat.will_summarize(&overflow_view(&conv.messages, floor, 1, 8000)));
        assert!(strat.will_summarize(&overflow_view(&conv.messages, floor, 2, 8000)));

        // Short history (≤1 real turn): a manual `/compact` falls back to the fast inner
        // stub (a no-op here) → NO announce, so no spurious "compacting…" line.
        let short = vec![
            Message::system("p"),
            Message::user("u1-active"),
            asst_call("b1", "bash"),
            Message::tool_result("b1", &big("out"), false),
        ];
        let mut sc = Conversation::new();
        sc.messages = short;
        let sfloor = sc.sacred_floor();
        assert!(
            !strat.will_summarize(&manual_view(&sc.messages, sfloor, None)),
            "short manual /compact is a no-op → must NOT announce"
        );
    }

    #[test]
    fn find_prior_anchor_matches_only_sentineled_synthetic_user() {
        let body = "## Goal\n- ship it";
        let anchor = Message::synthetic_user(format!("{ANCHOR_SENTINEL}\n{body}"));
        // A non-synthetic message that happens to contain the sentinel is NOT an anchor.
        let mut decoy = Message::user(format!("{ANCHOR_SENTINEL}\nnot an anchor"));
        decoy.synthetic = false;
        // A synthetic_user WITHOUT the sentinel (e.g. a resume note) is NOT an anchor.
        let resume_note = Message::synthetic_user("just resuming".to_string());

        // Directly exercise the predicate's rejections — `find_prior_anchor` below stops at
        // the real anchor, so without these the negative cases would never be evaluated and
        // a too-loose predicate (dropping the synthetic or sentinel guard) would pass.
        assert!(is_anchor_message(&anchor), "the real anchor matches");
        assert!(!is_anchor_message(&decoy), "non-synthetic with sentinel must NOT match");
        assert!(!is_anchor_message(&resume_note), "synthetic without sentinel must NOT match");

        let span = vec![
            Message::user("u1"),
            resume_note,
            decoy,
            anchor,
            Message::assistant("a1", vec![]),
        ];
        assert_eq!(find_prior_anchor(&span), Some(body));

        let none = vec![Message::user("u1"), Message::assistant("a1", vec![])];
        assert_eq!(find_prior_anchor(&none), None);
    }

    #[test]
    fn build_summary_prompt_update_vs_create_and_focus() {
        // UPDATE path: prior anchor present → instruct update + embed <previous-summary>.
        let p = build_summary_prompt(Some("## Goal\n- old goal"), "USER: hi", Some("auth"));
        assert!(p.contains("<previous-summary>"), "must embed the prior anchor block");
        assert!(p.contains("## Goal\n- old goal"), "prior anchor body is carried in");
        assert!(p.to_lowercase().contains("update"), "must instruct an update");
        assert!(p.contains("auth"), "focus steer is appended");
        assert!(p.contains("USER: hi"), "new transcript is included");
        // Every template section is requested.
        for s in ["## Goal", "## Constraints & Preferences", "## Progress", "### Done",
                  "### In Progress", "### Blocked", "## Key Decisions", "## Next Steps",
                  "## Critical Context", "## Relevant Files"] {
            assert!(p.contains(s), "template must request section {s}");
        }

        // CREATE path: no prior anchor → no <previous-summary>, instruct create.
        let c = build_summary_prompt(None, "USER: hi", None);
        assert!(!c.contains("<previous-summary>"), "no prior block when no anchor");
        assert!(c.to_lowercase().contains("create"), "must instruct a fresh create");
    }

    use std::sync::Mutex;

    struct CapturingSummaryProvider {
        seen: Arc<Mutex<Vec<Message>>>,
    }
    #[async_trait]
    impl LlmProvider for CapturingSummaryProvider {
        fn model_name(&self) -> &str { "capturing" }
        async fn chat_stream(
            &self,
            messages: &[Message],
            _: &[atomcode_kernel::tool::ToolDef],
            _: &ChatOptions,
        ) -> Result<futures::stream::BoxStream<'static, StreamEvent>, atomcode_kernel::stream::ProviderError> {
            *self.seen.lock().unwrap() = messages.to_vec();
            Ok(Box::pin(futures::stream::iter(vec![
                StreamEvent::TextDelta("## Goal\n- updated".into()),
                StreamEvent::Done { truncated: false },
            ])))
        }
    }

    #[tokio::test]
    async fn summarize_updates_prior_anchor_and_does_not_re_render_it() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let strat = OverflowCompaction::new(
            StubCompaction::default(),
            Some(Arc::new(CapturingSummaryProvider { seen: seen.clone() })),
        );
        // Span: a prior anchor (sentinel synthetic_user) followed by new events.
        let span = vec![
            Message::synthetic_user(format!("{ANCHOR_SENTINEL}\n## Goal\n- OLD GOAL")),
            Message::user("please also handle errors"),
            Message::assistant("done", vec![]),
        ];
        let out = strat.summarize(&span, Some("errors")).await.expect("returns a summary");

        // Output is anchored.
        assert!(out.starts_with(ANCHOR_SENTINEL), "output carries the sentinel: {out:?}");

        // The provider's user prompt embeds the prior anchor as <previous-summary>, and the
        // prior anchor is NOT re-rendered into the transcript (it appears once, in the block).
        let msgs = seen.lock().unwrap().clone();
        let user_prompt = &msgs.last().unwrap().text;
        assert!(user_prompt.contains("<previous-summary>"));
        assert!(user_prompt.contains("OLD GOAL"), "prior anchor body carried as the base");
        assert_eq!(user_prompt.matches("OLD GOAL").count(), 1, "anchor not duplicated into transcript");
        assert!(!user_prompt.contains(ANCHOR_SENTINEL), "sentinel line itself is stripped from the base");
        assert!(user_prompt.contains("please also handle errors"), "new events are folded in");
        assert!(user_prompt.contains("errors"), "focus steer present");
    }

    #[tokio::test]
    async fn successive_compactions_keep_exactly_one_anchor() {
        // Big old turns so the net-loss guard commits both compactions.
        let mut conv = Conversation::new();
        conv.messages = vec![
            Message::system("p"),
            Message::user(big("u1")),
            Message::assistant(big("a1"), vec![]),
            Message::user("u-active"),
        ];
        let floor = conv.sacred_floor();
        let strat = OverflowCompaction::new(StubCompaction::default(), Some(Arc::new(CannedSummaryProvider)));

        // Compaction #1: creates the first anchor.
        let p1 = strat.plan(&manual_view(&conv.messages, floor, None)).await;
        assert!(conv.apply_plan(p1, floor).committed);
        assert_eq!(conv.messages.iter().filter(|m| is_anchor_message(m)).count(), 1, "one anchor after #1");

        // Add more bulk, then Compaction #2: must UPDATE (drain old anchor, insert new) — still ONE.
        conv.messages.push(Message::assistant(big("a2"), vec![]));
        conv.messages.push(Message::user("u-active-2"));
        let p2 = strat.plan(&manual_view(&conv.messages, floor, None)).await;
        assert!(conv.apply_plan(p2, floor).committed);
        assert_eq!(conv.messages.iter().filter(|m| is_anchor_message(m)).count(), 1, "still one anchor after #2");
    }
}

/// Tiny test helper to chain two assistant tool calls into one message (a real assistant
/// turn can call several tools at once). Test-only.
#[cfg(test)]
trait AlsoCalls {
    fn also(self, other: Message) -> Message;
}
#[cfg(test)]
impl AlsoCalls for Message {
    fn also(mut self, other: Message) -> Message {
        self.tool_calls.extend(other.tool_calls);
        self
    }
}
