//! Edit-then-verify discipline — the coding self-correction loop.
//!
//! When the model stops (no more tool calls) having EDITED code but not run a successful
//! build/check afterward, we inject a one-shot nudge to verify before finishing. This is
//! the kernel `offer_continuation` seam: `Some(text)` continues the turn with a synthetic user
//! message; `None` lets it stop. The kernel's `max_continuations` fuse bounds
//! the loop, and our own state nudges ONCE per edit-batch so we never spin.
//!
//! Language-agnostic: detection keys only on tool NAMES (edit_file / write_file / bash),
//! never on cargo/npm/etc. The nudge text lists `cargo check` / `tsc --noEmit` only as
//! examples.

use async_trait::async_trait;
use atomcode_kernel::hook::LifecycleHooks;
use atomcode_kernel::message::{Conversation, Role};
use std::collections::HashMap;
use std::sync::Mutex;

const NUDGE: &str = "You made code edits but have not verified them. Run a fast check \
(`cargo check`, `tsc --noEmit`, or the equivalent for this project) to catch errors \
before finishing. Do NOT start a long-running process (dev server, watcher, full build).";

/// `offer_continuation` hook implementing the edit-then-verify cadence. Holds a small amount of
/// interior state so it nudges at most once per unverified edit.
#[derive(Default)]
pub struct VerifyCadenceHook {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// The tool_call_id of the last successful edit we ALREADY nudged for. Lets a fresh
    /// edit (different id) re-trigger, while a repeated stop on the SAME unverified edit
    /// does not nudge twice (which would loop with no model agency to stop it).
    nudged_for: Option<String>,
}

impl VerifyCadenceHook {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Scan the conversation: returns the tool_call_id of the most recent successful edit
/// IF it has no successful `bash` after it (i.e. unverified), else `None`.
fn unverified_edit_id(convo: &Conversation) -> Option<String> {
    // Tool-call ids are assigned by the assistant message that precedes the matching
    // tool-result message, so a single forward pass can resolve a result's tool name.
    let mut names: HashMap<&str, &str> = HashMap::new();
    let mut last_edit_id: Option<String> = None;
    let mut bash_after_edit = false;

    for msg in &convo.messages {
        match msg.role {
            Role::Assistant => {
                for tc in &msg.tool_calls {
                    names.insert(tc.id.as_str(), tc.name.as_str());
                }
            }
            Role::Tool => {
                if msg.is_error {
                    continue;
                }
                let Some(id) = msg.tool_call_id.as_deref() else {
                    continue;
                };
                match names.get(id).copied() {
                    Some("edit_file") | Some("write_file") => {
                        last_edit_id = Some(id.to_string());
                        bash_after_edit = false;
                    }
                    Some("bash") => bash_after_edit = true,
                    _ => {}
                }
            }
            _ => {}
        }
    }

    match last_edit_id {
        Some(id) if !bash_after_edit => Some(id),
        _ => None,
    }
}

#[async_trait]
impl LifecycleHooks for VerifyCadenceHook {
    /// The hook instance is REUSED across respawns (it lives in `CodingParts`), but
    /// `nudged_for` is per-CONVERSATION state keyed by tool_call_id — and providers
    /// with sequential per-conversation ids (`call_0`, `call_1`, …) would collide a
    /// FRESH conversation's first edit with the old one's last nudge, wrongly
    /// suppressing it once. A fresh session start resets; a resume keeps the state
    /// (same conversation → an already-nudged edit must stay nudged).
    async fn session_start(&self, _convo: &mut Conversation, resumed: bool) {
        if !resumed {
            self.state.lock().unwrap_or_else(|e| e.into_inner()).nudged_for = None;
        }
    }

    async fn offer_continuation(&self, convo: &Conversation) -> Option<String> {
        let edit_id = unverified_edit_id(convo)?;
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.nudged_for.as_deref() == Some(edit_id.as_str()) {
            return None; // already nudged for this exact edit — let the turn stop.
        }
        state.nudged_for = Some(edit_id);
        Some(NUDGE.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_kernel::message::Message;
    use atomcode_kernel::tool::ToolCall;

    fn assistant_call(id: &str, name: &str) -> Message {
        Message::assistant("", vec![ToolCall { id: id.into(), name: name.into(), arguments: "{}".into() }])
    }
    fn tool_result(id: &str, is_error: bool) -> Message {
        Message::tool_result(id, "ok", is_error)
    }

    async fn nudge_of(msgs: Vec<Message>) -> (VerifyCadenceHook, Option<String>) {
        let mut convo = Conversation::new();
        convo.messages = msgs;
        let hook = VerifyCadenceHook::new();
        let r = hook.offer_continuation(&convo).await;
        (hook, r)
    }

    #[tokio::test]
    async fn edit_without_build_nudges_once() {
        let msgs = vec![assistant_call("e1", "edit_file"), tool_result("e1", false)];
        let (hook, first) = nudge_of(msgs.clone()).await;
        assert!(first.is_some(), "unverified edit must nudge");
        // Calling again on the SAME conversation must NOT nudge twice.
        let mut convo = Conversation::new();
        convo.messages = msgs;
        assert!(hook.offer_continuation(&convo).await.is_none(), "must not nudge twice for the same edit");
    }

    #[tokio::test]
    async fn edit_then_successful_build_does_not_nudge() {
        let msgs = vec![
            assistant_call("e1", "edit_file"),
            tool_result("e1", false),
            assistant_call("b1", "bash"),
            tool_result("b1", false),
        ];
        assert!(nudge_of(msgs).await.1.is_none(), "verified edit must not nudge");
    }

    #[tokio::test]
    async fn failed_build_after_edit_still_nudges() {
        // A bash that ERRORED does not count as verification.
        let msgs = vec![
            assistant_call("e1", "edit_file"),
            tool_result("e1", false),
            assistant_call("b1", "bash"),
            tool_result("b1", true),
        ];
        assert!(nudge_of(msgs).await.1.is_some(), "errored bash is not verification");
    }

    #[tokio::test]
    async fn write_file_counts_as_edit() {
        let msgs = vec![assistant_call("w1", "write_file"), tool_result("w1", false)];
        assert!(nudge_of(msgs).await.1.is_some());
    }

    #[tokio::test]
    async fn no_edits_does_not_nudge() {
        let msgs = vec![assistant_call("r1", "read_file"), tool_result("r1", false)];
        assert!(nudge_of(msgs).await.1.is_none());
    }

    #[tokio::test]
    async fn build_then_edit_is_unverified() {
        // bash BEFORE the edit does not verify the later edit.
        let msgs = vec![
            assistant_call("b1", "bash"),
            tool_result("b1", false),
            assistant_call("e1", "edit_file"),
            tool_result("e1", false),
        ];
        assert!(nudge_of(msgs).await.1.is_some(), "build must come AFTER the edit");
    }

    #[tokio::test]
    async fn fresh_edit_after_nudge_retriggers() {
        let hook = VerifyCadenceHook::new();
        let mut convo = Conversation::new();
        convo.messages = vec![assistant_call("e1", "edit_file"), tool_result("e1", false)];
        assert!(hook.offer_continuation(&convo).await.is_some(), "first edit nudges");
        assert!(hook.offer_continuation(&convo).await.is_none(), "same edit, no second nudge");
        // A NEW edit (different id) appears → nudge again.
        convo.messages.push(assistant_call("e2", "edit_file"));
        convo.messages.push(tool_result("e2", false));
        assert!(hook.offer_continuation(&convo).await.is_some(), "a fresh unverified edit re-triggers");
    }
}
