//! `TodoHook` — injects the current todo list as an ephemeral `<system-reminder>` at
//! the TAIL of every request, so the model always sees current progress even after the
//! originating todowrite result is compacted away. Cache-safe: tail-only, per-request
//! clone (never stored) — mirrors PlanModeGate / StatusReminderHook.

use async_trait::async_trait;
use atomcode_capabilities::reminder::system_reminder;
use atomcode_capabilities::tools::todo::{
    derive_current_todos, render_todos_numbered, TodoItem, TodoStatus,
};
use atomcode_kernel::hook::{LifecycleHooks, TurnCtx};
use atomcode_kernel::message::{Conversation, Message, Role};

/// Injected when the model tries to STOP while the task list still has open items — the
/// residual weak-model gap after incremental `todo` updates land: it does the last item's work
/// (e.g. the closing summary) then ends WITHOUT marking it completed. Mirrors
/// `VerifyCadenceHook`'s `offer_continuation` cadence; nudges at most ONCE per real-user turn
/// (and the kernel `max_continuations` fuse bounds it), so it can never spin.
const TODO_COMPLETION_NUDGE: &str = "Before you finish: the task list still has open items. \
If you have actually completed them, mark each one done now with `todowrite` \
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
        m.role == Role::User
            && m.synthetic
            && m.text.trim_start().starts_with(TODO_COMPLETION_NUDGE)
    })
}

/// True iff the model actively MANAGED the task list this turn (a `todo`/`todowrite` call after
/// the last real-user message). We only nudge when it did — so a stop where the model is asking
/// the user something unrelated to a STALE list from an earlier turn isn't hijacked into a
/// continuation. Mirrors `VerifyCadenceHook`'s narrow "only right after an edit" scoping.
fn managed_todos_this_turn(convo: &Conversation) -> bool {
    let start = current_real_user_start(convo);
    convo.messages[start..].iter().any(|m| {
        m.tool_calls
            .iter()
            .any(|c| c.name == "todo" || c.name == "todowrite")
    })
}

/// The mid-work "reconcile your pointer" anchor prepended to the per-request reminder.
/// Weak models DRIFT: they leave `in_progress` on a task they already finished or moved
/// past (e.g. still on #4 while actually editing #6's code), or work with nothing marked
/// in_progress at all. The full numbered list is already injected below, but the
/// in_progress status is just a `[~]` glyph buried in it — low salience for weak models.
/// This surfaces the current pointer as an explicit imperative every turn so the model
/// re-confronts it BEFORE acting. Deterministic: reads only the derived state, never
/// guesses which task the model "should" be on.
/// - An `in_progress` task → name its `#<id>` + title and force a reconcile.
/// - No `in_progress` but open (pending) items remain → tell it to mark what it's on.
/// - Otherwise (all completed) → `None` (nothing to reconcile; don't add noise).
/// `id` is the 1-based position, matching `render_todos_numbered`.
fn todo_anchor_line(todos: &[TodoItem]) -> Option<String> {
    if let Some(i) = todos
        .iter()
        .position(|t| t.status == TodoStatus::InProgress)
    {
        return Some(format!(
            ">> You are currently ON task #{} \"{}\". Before your NEXT action, reconcile: if it \
is actually DONE, mark it completed now (`{{\"action\":\"update\",\"id\":{},\"status\":\"completed\"}}`); \
if you have moved on to a DIFFERENT task, switch in_progress to THAT id FIRST. Do not leave \
in_progress pointing at a task you are no longer working on.",
            i + 1,
            todos[i].content,
            i + 1
        ));
    }
    if todos.iter().any(|t| t.status == TodoStatus::Pending) {
        return Some(
            ">> NOTHING is in_progress but tasks remain. Before you act, mark the task you are \
actually working on as in_progress (`{\"action\":\"update\",\"id\":<id>,\"status\":\"in_progress\"}`)."
                .to_string(),
        );
    }
    None
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
        // The anchor line (mid-work drift backstop) leads, so the current in_progress
        // pointer is the first thing the model sees — above the list and the rules.
        let anchor = todo_anchor_line(&todos)
            .map(|a| format!("{a}\n\n"))
            .unwrap_or_default();
        let body = format!(
            "{anchor}Current task list (each line is `#<id> <task>`) — keep it accurate and finish it:\n\
- The MOMENT you START an item: `todowrite` with `{{\"action\":\"update\",\"id\":<id>,\"status\":\"in_progress\"}}`.\n\
- The MOMENT you FINISH an item: `todowrite` with `{{\"action\":\"update\",\"id\":<id>,\"status\":\"completed\"}}` (do not leave a done item showing incomplete).\n\
- Update ONE item at a time (the `{{\"action\":...}}` shape) — do NOT resend the whole `todos` list for a single status change (the full list is only for the initial plan or a full re-plan).\n\
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
        if !has_open || !managed_todos_this_turn(convo) || completion_nudge_already_present(convo) {
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
        Message::assistant(
            "",
            vec![ToolCall {
                id: "1".into(),
                name: "todowrite".into(),
                arguments: args.into(),
            }],
        )
    }

    // ---- mid-work drift backstop: the anchor line ------------------------------------------

    fn item(content: &str, status: TodoStatus) -> TodoItem {
        TodoItem {
            content: content.into(),
            status,
        }
    }

    #[test]
    fn anchor_names_in_progress_id_and_title() {
        // #2 is in_progress → anchor must name that exact id + title and force a reconcile.
        let todos = vec![
            item("first", TodoStatus::Completed),
            item("do the thing", TodoStatus::InProgress),
            item("later", TodoStatus::Pending),
        ];
        let a = todo_anchor_line(&todos).expect("in_progress → anchor");
        assert!(a.contains("#2"), "must name the 1-based id: {a}");
        assert!(a.contains("do the thing"), "must name the title: {a}");
        assert!(
            a.contains("reconcile") && a.contains("moved on"),
            "must force reconcile: {a}"
        );
    }

    #[test]
    fn anchor_when_nothing_in_progress_but_open_items_remain() {
        // No in_progress, but a pending item exists → tell the model to mark what it's on.
        let todos = vec![
            item("first", TodoStatus::Completed),
            item("second", TodoStatus::Pending),
        ];
        let a = todo_anchor_line(&todos).expect("open + no in_progress → anchor");
        assert!(a.contains("NOTHING is in_progress"), "{a}");
        assert!(a.contains("in_progress"), "must tell it to mark one: {a}");
    }

    #[test]
    fn no_anchor_when_all_completed() {
        // Everything done → nothing to reconcile; don't add noise.
        let todos = vec![
            item("a", TodoStatus::Completed),
            item("b", TodoStatus::Completed),
        ];
        assert!(todo_anchor_line(&todos).is_none());
    }

    #[tokio::test]
    async fn pre_request_prepends_anchor_for_in_progress() {
        let mut msgs = vec![
            Message::user("do it"),
            todowrite_msg(r#"{"todos":[{"content":"step one","status":"in_progress"}]}"#),
        ];
        TodoHook.pre_request(&mut msgs, &TurnCtx::default()).await;
        let last = &msgs[msgs.len() - 1];
        assert!(
            last.text.contains("currently ON task #1"),
            "anchor must lead: {}",
            last.text
        );
        assert!(
            last.text.contains("step one"),
            "anchor must name the task: {}",
            last.text
        );
        // The anchor precedes the list body.
        let anchor_at = last.text.find("currently ON task").unwrap();
        let list_at = last.text.find("Current task list").unwrap();
        assert!(
            anchor_at < list_at,
            "anchor must come before the list: {}",
            last.text
        );
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
            todowrite_msg(
                r#"{"todos":[{"content":"a","status":"completed"},{"content":"b","status":"in_progress"}]}"#,
            ),
            Message::assistant("here is the summary…", vec![]),
        ]);
        assert!(
            TodoHook.offer_continuation(&convo).await.is_some(),
            "open item on stop must nudge"
        );
    }

    #[tokio::test]
    async fn no_nudge_when_all_completed() {
        let convo = convo_of(vec![
            Message::user("do it"),
            todowrite_msg(
                r#"{"todos":[{"content":"a","status":"completed"},{"content":"b","status":"completed"}]}"#,
            ),
            Message::assistant("all done", vec![]),
        ]);
        assert!(
            TodoHook.offer_continuation(&convo).await.is_none(),
            "all completed → let it stop"
        );
    }

    #[tokio::test]
    async fn no_nudge_when_no_todos() {
        let convo = convo_of(vec![
            Message::user("hi"),
            Message::assistant("hi there", vec![]),
        ]);
        assert!(TodoHook.offer_continuation(&convo).await.is_none());
    }

    #[tokio::test]
    async fn no_nudge_when_list_untouched_this_turn() {
        // An open item lingers from a PRIOR turn, but this turn the model only answered a
        // question (no todo/todowrite call) → don't hijack the stop into a continuation.
        let convo = convo_of(vec![
            Message::user("plan it"),
            todowrite_msg(r#"{"todos":[{"content":"a","status":"in_progress"}]}"#),
            Message::assistant("planned", vec![]),
            Message::user("what does foo do?"),
            Message::assistant("foo does X.", vec![]),
        ]);
        assert!(
            TodoHook.offer_continuation(&convo).await.is_none(),
            "a stale open list not touched this turn must not force a continuation"
        );
    }

    #[tokio::test]
    async fn nudges_at_most_once_per_turn() {
        let mut convo = convo_of(vec![
            Message::user("do it"),
            todowrite_msg(r#"{"todos":[{"content":"a","status":"in_progress"}]}"#),
            Message::assistant("summary", vec![]),
        ]);
        assert!(
            TodoHook.offer_continuation(&convo).await.is_some(),
            "first stop nudges"
        );
        // Kernel injected the nudge as a synthetic user message; model stops again without closing.
        convo
            .messages
            .push(Message::synthetic_user(TODO_COMPLETION_NUDGE));
        convo
            .messages
            .push(Message::assistant("still open", vec![]));
        assert!(
            TodoHook.offer_continuation(&convo).await.is_none(),
            "already nudged this turn → let it stop (no spin)"
        );
    }
}
