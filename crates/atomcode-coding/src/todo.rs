//! `TodoHook` — injects the current todo list as an ephemeral `<system-reminder>` at
//! the TAIL of every request, so the model always sees current progress even after the
//! originating todowrite result is compacted away. Cache-safe: tail-only, per-request
//! clone (never stored) — mirrors PlanModeGate / StatusReminderHook.

use async_trait::async_trait;
use atomcode_capabilities::reminder::system_reminder;
use atomcode_capabilities::tools::todo::{derive_current_todos, render_todos_numbered};
use atomcode_kernel::hook::{LifecycleHooks, TurnCtx};
use atomcode_kernel::message::Message;

pub struct TodoHook;

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
}
