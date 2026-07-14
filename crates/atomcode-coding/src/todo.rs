//! `TodoHook` — injects the current todo list as an ephemeral `<system-reminder>` at
//! the TAIL of every request, so the model always sees current progress even after the
//! originating todowrite result is compacted away. Cache-safe: tail-only, per-request
//! clone (never stored) — mirrors PlanModeGate / StatusReminderHook.

use async_trait::async_trait;
use atomcode_capabilities::reminder::system_reminder;
use atomcode_capabilities::tools::todo::{derive_current_todos, render_todos_numbered, TodoStatus};
use atomcode_kernel::hook::{LifecycleHooks, TurnCtx};
use atomcode_kernel::message::{Conversation, Message, Role};

/// Injected when the model tries to STOP while the task list still has open items — the
/// residual weak-model gap after incremental `todo` updates land: it does the last item's work
/// (e.g. the closing summary) then ends WITHOUT marking it completed. Mirrors
/// `VerifyCadenceHook`'s `offer_continuation` cadence; nudges at most ONCE per real-user turn
/// (and the kernel `max_continuations` fuse bounds it), so it can never spin.
const TODO_COMPLETION_NUDGE: &str = "Before you finish: the task list still has open items. \
If you have actually completed them, mark each one done now with `todo` \
(`{\"action\":\"update\",\"id\":<id>,\"status\":\"completed\"}`). If some are NOT done, keep working \
through them. Only stop with open items if you genuinely need approval/input, are stuck, or the \
request is ambiguous — in that case say so briefly.";

pub struct TodoHook;

/// Index of the current real-user turn's start (last non-synthetic user message).
fn current_real_user_start(convo: &Conversation) -> usize {
    convo
        .messages
        .iter()
        .rposition(|m| m.role == Role::User && !m.synthetic)
        .unwrap_or(0)
}

/// True iff the completion nudge was already injected in the CURRENT real-user turn — so we
/// nudge at most once; if the model stops again with open items, we let it end.
fn completion_nudge_already_present(convo: &Conversation) -> bool {
    let start = current_real_user_start(convo);
    convo.messages[start..].iter().any(|m| {
        m.role == Role::User && m.synthetic && m.text.trim_start().starts_with(TODO_COMPLETION_NUDGE)
    })
}

#[async_trait]
impl LifecycleHooks for TodoHook {
    async fn pre_request(&self, messages: &mut Vec<Message>, _ctx: &TurnCtx) {
        let todos = derive_current_todos(messages);
        if todos.is_empty() {
            return;
        }
        // ASCII-safe body (the model doesn't need glyph prettiness; the TUI renders
        // the pretty version). Tail-append so the cached prefix is preserved.
        let body = format!(
            "Current task list (each line is `#<id> <task>`) — keep it accurate and finish it:\n\
- The MOMENT you START an item: `todo` with `{{\"action\":\"update\",\"id\":<id>,\"status\":\"in_progress\"}}`.\n\
- The MOMENT you FINISH an item: `todo` with `{{\"action\":\"update\",\"id\":<id>,\"status\":\"completed\"}}` (do not leave a done item showing incomplete).\n\
- Update ONE item at a time with `todo` — do NOT resend the whole list with `todowrite` (that is only for the initial plan or a full re-plan).\n\
- Do NOT stop, summarize, or hand back while ANY item is still pending or in_progress — keep working through them, unless you truly need approval, are genuinely stuck, or the request is ambiguous.\n\
{}",
            render_todos_numbered(&todos, false)
        );
        messages.push(Message::user(system_reminder(&body)));
    }

    /// The model wants to stop. If the task list still has OPEN items (pending or in_progress),
    /// inject a one-shot nudge to close them out (or keep working) and continue the turn — the
    /// residual gap where a weak model finishes the last item's work but forgets the final
    /// `todo update`. Fires at most once per real-user turn; `None` otherwise lets it stop.
    async fn offer_continuation(&self, convo: &Conversation) -> Option<String> {
        let todos = derive_current_todos(&convo.messages);
        let has_open = todos.iter().any(|t| t.status != TodoStatus::Completed);
        if !has_open || completion_nudge_already_present(convo) {
            return None;
        }
        Some(TODO_COMPLETION_NUDGE.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_kernel::hook::{LifecycleHooks, TurnCtx};
    use atomcode_kernel::message::{Message, Role};
    use atomcode_kernel::tool::ToolCall;

    fn todowrite_msg(args: &str) -> Message {
        Message::assistant("", vec![ToolCall { id: "1".into(), name: "todowrite".into(), arguments: args.into() }])
    }

    #[tokio::test]
    async fn injects_reminder_when_list_present() {
        let mut msgs = vec![
            Message::user("do the thing"),
            todowrite_msg(r#"{"todos":[{"content":"step one","status":"in_progress"}]}"#),
        ];
        let before = msgs.len();
        TodoHook.pre_request(&mut msgs, &TurnCtx::default()).await;
        assert_eq!(msgs.len(), before + 1, "one reminder appended");
        let last = &msgs[msgs.len() - 1];
        assert_eq!(last.role, Role::User);
        assert!(last.text.contains("system-reminder"), "{}", last.text);
        assert!(last.text.contains("step one"), "{}", last.text);
    }

    #[tokio::test]
    async fn no_injection_when_no_list() {
        let mut msgs = vec![Message::user("hi"), Message::assistant("hello", vec![])];
        let before = msgs.len();
        TodoHook.pre_request(&mut msgs, &TurnCtx::default()).await;
        assert_eq!(msgs.len(), before, "empty list → no injection");
    }

    // ---- offer_continuation: close out the last item ---------------------------------------

    fn convo_of(msgs: Vec<Message>) -> Conversation {
        let mut c = Conversation::new();
        c.messages = msgs;
        c
    }

    #[tokio::test]
    async fn nudges_to_close_out_open_items_on_stop() {
        // The reported gap: the model produced its final summary but left an item open.
        let convo = convo_of(vec![
            Message::user("do the audit"),
            todowrite_msg(r#"{"todos":[{"content":"a","status":"completed"},{"content":"b","status":"in_progress"}]}"#),
            Message::assistant("here is the summary…", vec![]),
        ]);
        assert!(TodoHook.offer_continuation(&convo).await.is_some(), "open item on stop must nudge");
    }

    #[tokio::test]
    async fn no_nudge_when_all_completed() {
        let convo = convo_of(vec![
            Message::user("do it"),
            todowrite_msg(r#"{"todos":[{"content":"a","status":"completed"},{"content":"b","status":"completed"}]}"#),
            Message::assistant("all done", vec![]),
        ]);
        assert!(TodoHook.offer_continuation(&convo).await.is_none(), "all completed → let it stop");
    }

    #[tokio::test]
    async fn no_nudge_when_no_todos() {
        let convo = convo_of(vec![Message::user("hi"), Message::assistant("hi there", vec![])]);
        assert!(TodoHook.offer_continuation(&convo).await.is_none());
    }

    #[tokio::test]
    async fn nudges_at_most_once_per_turn() {
        let mut convo = convo_of(vec![
            Message::user("do it"),
            todowrite_msg(r#"{"todos":[{"content":"a","status":"in_progress"}]}"#),
            Message::assistant("summary", vec![]),
        ]);
        assert!(TodoHook.offer_continuation(&convo).await.is_some(), "first stop nudges");
        // Kernel injected the nudge as a synthetic user message; model stops again without closing.
        convo.messages.push(Message::synthetic_user(TODO_COMPLETION_NUDGE));
        convo.messages.push(Message::assistant("still open", vec![]));
        assert!(
            TodoHook.offer_continuation(&convo).await.is_none(),
            "already nudged this turn → let it stop (no spin)"
        );
    }
}
