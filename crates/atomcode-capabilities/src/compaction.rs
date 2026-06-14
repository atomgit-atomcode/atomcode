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

use async_trait::async_trait;
use atomcode_kernel::message::{
    CompactionPlan, CompactionStrategy, CompactionView, Message, Role,
};

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
