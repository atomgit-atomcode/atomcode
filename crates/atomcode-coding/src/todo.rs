//! `TodoHook` — injects the current todo list as an ephemeral `<system-reminder>` at
//! the TAIL of every request, so the model always sees current progress even after the
//! originating todowrite result is compacted away. Cache-safe: tail-only, per-request
//! clone (never stored) — mirrors PlanModeGate / StatusReminderHook.

use async_trait::async_trait;
use atomcode_capabilities::reminder::synthetic_system_reminder;
use atomcode_capabilities::session::manager::{SessionManager, TodoSidecarItem};
use atomcode_capabilities::tools::todo::{
    derive_current_todos, is_todo_plan, render_todos_numbered, TodoItem, TodoStatus,
};
use atomcode_kernel::event::StopReason;
use atomcode_kernel::hook::{LifecycleHooks, TurnCtx};
use atomcode_kernel::message::{Conversation, Message, Role};
use atomcode_kernel::provider::{ChatOptions, ToolChoice};

use atomcode_config::config::TodoEagerness;

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

pub struct TodoHook {
    /// Project root for locating the session todo sidecar
    /// (`<session_root>/<project_bucket>/<session_id>.todos.json`). `None` in
    /// tests / headless drivers: sidecar persistence is skipped and the hook
    /// stays transcript-derived only (matches the pre-sidecar behavior).
    working_dir: Option<std::path::PathBuf>,
}

impl TodoHook {
    pub fn new(working_dir: impl Into<std::path::PathBuf>) -> Self {
        Self {
            working_dir: Some(working_dir.into()),
        }
    }
}

impl Default for TodoHook {
    fn default() -> Self {
        Self { working_dir: None }
    }
}

/// High-recency todo activation policy. Unlike `TodoHook`, this only acts on
/// round one of a real user turn and only while no structured list exists.
pub struct TodoEagerHook {
    eagerness: TodoEagerness,
    /// `auto` keeps ordinary models judgment-based. For DeepSeek (a weak model
    /// that under-uses soft reminders) a high-confidence feature/refactor request
    /// upgrades the reminder from the soft tier to the FIRM tier — it no longer
    /// forces the tool choice (that was unsupported by DeepSeek V4 and regressed
    /// efficiency on small tasks; only `always` hard-forces).
    force_complex_for_weak_model: bool,
}

impl TodoEagerHook {
    pub fn new(model: &str, provider_type: &str, configured: TodoEagerness) -> Self {
        let normalized = model.to_ascii_lowercase().replace(['_', ' '], "-");
        let is_deepseek_v4_flash = normalized.contains("deepseek")
            && normalized.contains("v4")
            && normalized.contains("flash");
        let mut force_complex_for_weak_model =
            configured == TodoEagerness::Auto && is_deepseek_v4_flash;
        let mut eagerness = match configured {
            TodoEagerness::Auto if is_deepseek_v4_flash => TodoEagerness::Preferred,
            TodoEagerness::Auto => TodoEagerness::Auto,
            other => other,
        };
        if eagerness == TodoEagerness::Always && provider_type.eq_ignore_ascii_case("ollama") {
            eprintln!(
                "[todo] eager=always is unsupported by provider type ollama; using preferred"
            );
            eagerness = TodoEagerness::Preferred;
        }
        // Ollama's adapter cannot express a forced tool choice. Keep the
        // high-recency reminder, but never promise enforcement the provider drops.
        if provider_type.eq_ignore_ascii_case("ollama") {
            force_complex_for_weak_model = false;
        }
        Self {
            eagerness,
            force_complex_for_weak_model,
        }
    }

    fn should_activate(&self, messages: &[Message], ctx: &TurnCtx) -> bool {
        let todos = derive_current_todos(messages);
        ctx.round == 1
            && self.eagerness != TodoEagerness::Auto
            && todos
                .iter()
                .all(|todo| todo.status == TodoStatus::Completed)
    }

    /// The explicit `always` policy is the ONLY one that hard-forces the tool
    /// choice (`todowrite` first). The DeepSeek weak-model path deliberately does
    /// not: its hard tool_choice was unsupported by DeepSeek V4 and dropped by the
    /// provider, and forcing a plan on small tasks regressed turns/tokens/wall
    /// clock for no measured quality gain — so it only firms up the text nudge.
    fn should_hard_force(&self, messages: &[Message], ctx: &TurnCtx) -> bool {
        self.should_activate(messages, ctx) && self.eagerness == TodoEagerness::Always
    }
}

/// Word-boundary-aware substring test for ASCII signals; plain substring for
/// non-ASCII ones. Word boundaries only model English morphology — an ASCII
/// signal like `refactor` must NOT match inside `refactoring`. CJK signals like
/// `重构` have no such morphology and no whitespace, and bilingual prompts glue
/// them to ASCII identifiers (`重构UserService`, `迁移到PostgreSQL`), so they keep
/// the original plain-substring behavior.
fn contains_word(text: &str, signal: &str) -> bool {
    if signal.is_empty() {
        return false;
    }
    if !signal.is_ascii() {
        return text.contains(signal);
    }
    let mut search_start = 0;
    while let Some(offset) = text[search_start..].find(signal) {
        let start = search_start + offset;
        let end = start + signal.len();
        let before_ok = text[..start]
            .chars()
            .next_back()
            .map_or(true, |c| !c.is_ascii_alphanumeric());
        let after_ok = text[end..]
            .chars()
            .next()
            .map_or(true, |c| !c.is_ascii_alphanumeric());
        if before_ok && after_ok {
            return true;
        }
        search_start = end;
    }
    false
}

/// Deliberately narrow lexical gate for the DeepSeek `auto` policy. False
/// negatives merely fall back to the soft reminder; false positives only upgrade
/// the reminder to the firm text tier (no forced tool call), so keep only strong
/// implementation/refactor signals.
fn high_confidence_complex_engineering_request(messages: &[Message]) -> bool {
    let Some(text) = messages
        .iter()
        .rev()
        .find(|message| message.role == Role::User && !message.synthetic)
        .map(|message| message.text.to_ascii_lowercase())
    else {
        return false;
    };
    let explicitly_read_only = [
        "do not modify",
        "don't modify",
        "without changing",
        "read-only",
        "readonly",
        "explain only",
        "analysis only",
        "不要修改",
        "无需修改",
        "禁止修改",
        "只读",
        "只分析",
        "仅分析",
        "只解释",
        "仅解释",
    ]
    .iter()
    .any(|signal| text.contains(signal));
    let has_complex_signal = [
        "architect",
        "refactor",
        "migration",
        "migrate",
        "redesign",
        "implement feature",
        "build feature",
        "架构",
        "重构",
        "迁移",
        "功能开发",
        "新增功能",
        "系统改造",
    ]
    .iter()
    .any(|signal| contains_word(&text, signal));
    if !has_complex_signal {
        return false;
    }

    // A read-only clause may be scoped to only part of a mixed request, for
    // example "不要修改文档，但请重构代码并补充测试". Do not let that clause
    // suppress an explicit implementation imperative elsewhere in the same
    // prompt. Keep this override deliberately narrow so "不要修改代码" does
    // not become a positive match merely because it contains 修改/代码.
    let explicit_change_request = [
        "please architect",
        "please refactor",
        "please migrate",
        "please redesign",
        "please implement",
        "please build",
        "but refactor",
        "then refactor",
        "请架构",
        "请重构",
        "请迁移",
        "请实现",
        "请新增",
        "请改造",
        "但请重构",
        "同时重构",
        "并重构",
    ]
    .iter()
    .any(|signal| text.contains(signal));

    !explicitly_read_only || explicit_change_request
}

#[async_trait]
impl LifecycleHooks for TodoEagerHook {
    async fn pre_request(&self, messages: &mut Vec<Message>, ctx: &TurnCtx) {
        if !self.should_activate(messages, ctx) {
            return;
        }
        // `should_activate` already passed, so branch on the raw policy — no need
        // to re-derive the current todo list via should_hard_force / a weak-model
        // helper. The two arms are mutually exclusive: `force_complex_for_weak_model`
        // is only set for DeepSeek `auto` (remapped to Preferred, never `Always`).
        let lead = if self.eagerness == TodoEagerness::Always {
            "You MUST call `todowrite` now, before any other tool or prose. Create the complete execution plan, not placeholder items: cover investigation, architecture/module design, implementation, and verification where relevant. Each item must name a concrete outcome that a later turn can execute without re-planning."
        } else if self.force_complex_for_weak_model
            && high_confidence_complex_engineering_request(messages)
        {
            "This request shows strong signals of multi-step engineering work (refactor, migration, feature build, redesign). If it genuinely spans multiple files, phases, or investigation plus changes, call `todowrite` first and lay out a concrete plan — investigation, architecture/module design, implementation, verification — with outcomes a later turn can execute without re-planning. If it is actually a single, self-contained change or purely informational, skip the list and act directly."
        } else {
            "Before acting, decide whether this task benefits from a todo list. If it has multiple requests, phases, files, dependencies, ambiguity, or requires investigation plus changes, call `todowrite` now. A useful plan covers the complete request from investigation and architecture/module design through implementation and verification, with concrete outcomes a later turn can execute without re-planning. Skip it only for a genuinely simple one-step or purely informational request."
        };
        messages.push(synthetic_system_reminder(lead));
    }

    async fn pre_request_options(
        &self,
        messages: &[Message],
        options: &mut ChatOptions,
        ctx: &TurnCtx,
    ) {
        if self.should_hard_force(messages, ctx) {
            options.tool_choice = ToolChoice::Specific("todowrite".to_string());
        }
    }
}

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

/// The static "how to drive the list with `todowrite`" rules. These are CONSTANT
/// guidance — the model already has them from the persona and from the round right
/// after it (re)plans — so re-sending them on every execution round is pure wasted
/// cache (~170 tokens/round of never-cached tail). Rides the reminder only when the
/// model JUST wrote a full list (see `just_wrote_full_list`).
const TODO_DRIVE_RULES: &str = "\n\
- The MOMENT you START an item: `todowrite` with `{\"action\":\"update\",\"id\":<id>,\"status\":\"in_progress\"}`.\n\
- The MOMENT you FINISH an item: `todowrite` with `{\"action\":\"update\",\"id\":<id>,\"status\":\"completed\"}` (do not leave a done item showing incomplete).\n\
- Update ONE item at a time (the `{\"action\":...}` shape) — do NOT resend the whole `todos` list for a single status change (the full list is only for the initial plan or a full re-plan).\n\
- Do NOT stop, summarize, or hand back while ANY item is still pending or in_progress — keep working through them, unless you truly need approval, are genuinely stuck, or the request is ambiguous.";

/// True iff the model's most recent tool-using action was a FULL `todowrite` list
/// (re)plan, as opposed to a single `todo` status update or a non-todo action. Used to
/// ride [`TODO_DRIVE_RULES`] only right after a (re)plan — the round where the model
/// most needs the "how to update as you go" guidance — instead of every round.
fn just_wrote_full_list(messages: &[Message]) -> bool {
    messages
        .iter()
        .rev()
        .find(|m| !m.tool_calls.is_empty())
        .is_some_and(|m| {
            m.tool_calls
                .iter()
                .any(|c| c.name == "todowrite" && is_todo_plan(&c.arguments))
        })
}

#[async_trait]
impl LifecycleHooks for TodoHook {
    async fn pre_request(&self, messages: &mut Vec<Message>, ctx: &TurnCtx) {
        let todos = derive_current_todos(messages);
        // Transcript-derived list may be EMPTY after a compaction drained the old
        // turns (incl. the early todowrite plan call) — recover from the persisted
        // sidecar so the model keeps seeing its plan (issue #1503). Fall back to
        // the sidecar ONLY when the transcript has nothing; otherwise the live
        // transcript is the authoritative source.
        let todos = if todos.is_empty() {
            // Only reach for the sidecar when a session context exists.
            let Some(working_dir) = self.working_dir.as_deref() else {
                return;
            };
            let Some(session_id) = ctx.session_id.as_deref() else {
                return;
            };
            sidecar_todos_for(working_dir, session_id).unwrap_or_default()
        } else {
            todos
        };
        if todos.is_empty() {
            return;
        }
        // ASCII-safe body (the model doesn't need glyph prettiness; the TUI renders
        // the pretty version). Tail-append so the cached prefix is preserved.
        // The anchor line (mid-work drift backstop) leads, so the current in_progress
        // pointer is the first thing the model sees — above the list and the rules.
        // The anchor + list ride EVERY round (the per-round drift backstop); the static
        // drive rules ride ONLY right after a (re)plan, to stop wasting cache re-sending
        // constant guidance every execution round.
        let anchor = todo_anchor_line(&todos)
            .map(|a| format!("{a}\n\n"))
            .unwrap_or_default();
        let rules = if just_wrote_full_list(messages) {
            TODO_DRIVE_RULES
        } else {
            ""
        };
        let body = format!(
            "{anchor}Current task list (each line is `#<id> <task>`) — keep it accurate and finish it:{rules}\n{}",
            render_todos_numbered(&todos, false)
        );
        messages.push(synthetic_system_reminder(&body));
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

    /// Turn ended: persist the CURRENT todo list to the session sidecar so a later
    /// compaction (which drains the transcript's todowrite calls) can't erase the
    /// list the model / vscode panel rely on (issue #1503). Best-effort and only
    /// when the transcript still yields todos (a turn that never planned has
    /// nothing to persist; a rollback/undo that truncated the transcript past the
    /// plan will simply not reach a `turn_complete` write with that stale state).
    /// `ctx.session_id` is `None` for headless drivers — nothing to key the file on.
    async fn turn_complete(&self, convo: &Conversation, _reason: &StopReason, ctx: &TurnCtx) {
        let Some(working_dir) = self.working_dir.as_deref() else {
            return;
        };
        let Some(session_id) = ctx.session_id.as_deref() else {
            return;
        };
        let todos = derive_current_todos(&convo.messages);
        if todos.is_empty() {
            return;
        }
        let items: Vec<TodoSidecarItem> = todos
            .iter()
            .map(|t| TodoSidecarItem {
                content: t.content.clone(),
                status: todo_status_str(&t.status).to_string(),
            })
            .collect();
        let manager = SessionManager::for_project(working_dir);
        let _ = manager.write_todo_sidecar(session_id, &items, convo.messages.len());
    }
}

/// Recover the todo list from the persisted sidecar when the transcript-derived
/// list is empty (post-compaction). Stale sidecars (written against MORE messages
/// than the current transcript — a rollback/undo truncated history) are discarded
/// by `read_todo_sidecar`'s marker check, so a rolled-back session never shows an
/// outdated list.
fn sidecar_todos_for(
    working_dir: &std::path::Path,
    session_id: &str,
) -> Option<Vec<TodoItem>> {
    let manager = SessionManager::for_project(working_dir);
    let sidecar = manager.read_todo_sidecar(session_id).ok()??;
    Some(
        sidecar
            .todos
            .into_iter()
            .map(|item| TodoItem {
                content: item.content,
                status: parse_todo_status(&item.status),
            })
            .collect(),
    )
}

/// Map the canonical sidecar status strings back to [`TodoStatus`].
fn parse_todo_status(status: &str) -> TodoStatus {
    match status {
        "in_progress" => TodoStatus::InProgress,
        "completed" => TodoStatus::Completed,
        _ => TodoStatus::Pending,
    }
}

/// Canonical sidecar status string for a [`TodoStatus`] (matches the vscode
/// frontend's `pending` / `in_progress` / `completed`).
fn todo_status_str(status: &TodoStatus) -> &'static str {
    match status {
        TodoStatus::Pending => "pending",
        TodoStatus::InProgress => "in_progress",
        TodoStatus::Completed => "completed",
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

    fn todo_update_msg(args: &str) -> Message {
        Message::assistant(
            "",
            vec![ToolCall {
                id: "u".into(),
                name: "todo".into(),
                arguments: args.into(),
            }],
        )
    }

    #[tokio::test]
    async fn drive_rules_ride_only_after_a_full_write_not_a_single_update() {
        let list = r#"{"todos":[{"content":"step one","status":"in_progress"},{"content":"step two","status":"pending"}]}"#;
        // Right after a full (re)plan: the drive rules ARE included.
        let mut fresh = vec![Message::user("go"), todowrite_msg(list)];
        TodoHook::default().pre_request(&mut fresh, &TurnCtx::default()).await;
        let after_plan = fresh.last().unwrap().text.clone();
        assert!(
            after_plan.contains("The MOMENT you START"),
            "rules must ride the (re)plan round:\n{after_plan}"
        );
        assert!(
            after_plan.contains("step one"),
            "list present on plan round"
        );

        // An execution round whose most recent action was a single `todo` update: the
        // rules are OMITTED (cache win), but the anchor + list still ride every round.
        let mut exec = vec![
            Message::user("go"),
            todowrite_msg(list),
            todo_update_msg(r#"{"action":"update","id":1,"status":"completed"}"#),
        ];
        TodoHook::default().pre_request(&mut exec, &TurnCtx::default()).await;
        let after_update = exec.last().unwrap().text.clone();
        assert!(
            !after_update.contains("The MOMENT you START"),
            "drive rules must NOT repeat on execution rounds:\n{after_update}"
        );
        assert!(
            after_update.contains("Current task list"),
            "list header still rides every round:\n{after_update}"
        );

        // The merged `todowrite` tool also accepts incremental action arguments.
        // Tool name alone must not misclassify that shape as a full re-plan.
        let mut merged_update = vec![
            Message::user("go"),
            todowrite_msg(list),
            todowrite_msg(r#"{"action":"update","id":1,"status":"completed"}"#),
        ];
        TodoHook::default()
            .pre_request(&mut merged_update, &TurnCtx::default())
            .await;
        assert!(
            !merged_update
                .last()
                .unwrap()
                .text
                .contains("The MOMENT you START"),
            "incremental todowrite shape must not repeat drive rules"
        );
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
        TodoHook::default().pre_request(&mut msgs, &TurnCtx::default()).await;
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
        TodoHook::default().pre_request(&mut msgs, &TurnCtx::default()).await;
        assert_eq!(msgs.len(), before + 1, "one reminder appended");
        let last = &msgs[msgs.len() - 1];
        assert_eq!(last.role, Role::User);
        assert!(last.synthetic, "runtime reminders must carry provenance");
        assert!(last.text.contains("system-reminder"), "{}", last.text);
        assert!(last.text.contains("step one"), "{}", last.text);
    }

    #[tokio::test]
    async fn no_injection_when_no_list() {
        let mut msgs = vec![Message::user("hi"), Message::assistant("hello", vec![])];
        let before = msgs.len();
        TodoHook::default().pre_request(&mut msgs, &TurnCtx::default()).await;
        assert_eq!(msgs.len(), before, "empty list → no injection");
    }

    #[tokio::test]
    async fn auto_prefers_deepseek_v4_flash_on_each_new_task() {
        let hook = TodoEagerHook::new("deepseek-v4-flash", "openai", TodoEagerness::Auto);
        let mut msgs = vec![Message::user("analyze and fix this")];
        hook.pre_request(
            &mut msgs,
            &TurnCtx {
                round: 1,
                ..Default::default()
            },
        )
        .await;
        assert!(msgs.last().unwrap().text.contains("todowrite"));
    }

    #[tokio::test]
    async fn auto_policy_is_resolved_again_for_a_model_generation() {
        let ctx = TurnCtx {
            round: 1,
            ..Default::default()
        };
        let mut ordinary = vec![Message::user("analyze and fix this")];
        TodoEagerHook::new("ordinary-model", "openai", TodoEagerness::Auto)
            .pre_request(&mut ordinary, &ctx)
            .await;
        assert_eq!(ordinary.len(), 1, "ordinary Auto stays quiet");

        let mut deepseek = vec![Message::user("analyze and fix this")];
        TodoEagerHook::new("deepseek-v4-flash", "openai", TodoEagerness::Auto)
            .pre_request(&mut deepseek, &ctx)
            .await;
        assert_eq!(deepseek.len(), 2, "new DeepSeek generation gets the nudge");
    }

    #[test]
    fn contains_word_gates_english_morphology_but_keeps_cjk_substrings() {
        // English word boundaries: `refactor` must not match inside `refactoring`,
        // but must match a real imperative.
        assert!(!contains_word("print the word refactoring", "refactor"));
        assert!(contains_word("please refactor this", "refactor"));
        // CJK has no word morphology and no whitespace; bilingual dev prompts glue
        // a CJK signal to an ASCII identifier. Those must still match as before.
        assert!(contains_word("请重构userservice模块", "重构"));
        assert!(contains_word("迁移到postgresql", "迁移"));
        assert!(contains_word("auth重构", "重构"));
    }

    #[tokio::test]
    async fn deepseek_auto_firm_nudges_complex_work_without_forcing_tool_choice() {
        // B: the weak-model complex path firms up the TEXT nudge but no longer
        // hard-forces the tool choice — that force was unsupported by DeepSeek V4
        // and dropped by the provider anyway, and forcing todos on small tasks
        // regressed efficiency for no measured quality gain. Keep the model's
        // judgment; only the explicit `always` policy hard-forces.
        let hook = TodoEagerHook::new("deepseek-v4-flash", "openai", TodoEagerness::Auto);
        let ctx = TurnCtx {
            round: 1,
            ..Default::default()
        };
        let messages = vec![Message::user("重构会话运行时并设计清晰的模块边界")];
        let mut options = ChatOptions::default();
        hook.pre_request_options(&messages, &mut options, &ctx)
            .await;
        assert_eq!(options.tool_choice, ToolChoice::Auto);

        let mut reminder = messages.clone();
        hook.pre_request(&mut reminder, &ctx).await;
        assert_eq!(reminder.len(), 2, "a firm plan reminder is still injected");
        assert!(reminder[1].text.contains("todowrite"));
        assert!(
            !reminder[1].text.contains("You MUST"),
            "firm, not a hard mandate"
        );
    }

    #[test]
    fn high_confidence_gate_ignores_a_complex_word_used_only_as_data() {
        // 021-style: the task is a simple lifetime fix that merely prints the word
        // "refactoring". Substring matching wrongly forced a plan; word-boundary
        // matching must not treat "refactoring" as a refactor request.
        let messages = vec![Message::user(
            "fix the lifetime error, then print the longest word (\"refactoring\")",
        )];
        assert!(!high_confidence_complex_engineering_request(&messages));
    }

    #[test]
    fn high_confidence_gate_matches_a_real_refactor_imperative() {
        let english = vec![Message::user("please refactor the auth module")];
        assert!(high_confidence_complex_engineering_request(&english));
        // Chinese has no word morphology; whole-word gating must not break it.
        let chinese = vec![Message::user("请重构代码并补充测试")];
        assert!(high_confidence_complex_engineering_request(&chinese));
    }

    #[tokio::test]
    async fn deepseek_auto_keeps_simple_requests_judgment_based() {
        let hook = TodoEagerHook::new("deepseek-v4-flash", "openai", TodoEagerness::Auto);
        let ctx = TurnCtx {
            round: 1,
            ..Default::default()
        };
        let messages = vec![Message::user("解释这行代码")];
        let mut options = ChatOptions::default();
        hook.pre_request_options(&messages, &mut options, &ctx)
            .await;
        assert_eq!(options.tool_choice, ToolChoice::Auto);
    }

    #[tokio::test]
    async fn deepseek_auto_respects_explicit_read_only_request() {
        let hook = TodoEagerHook::new("deepseek-v4-flash", "openai", TodoEagerness::Auto);
        let ctx = TurnCtx {
            round: 1,
            ..Default::default()
        };
        let messages = vec![Message::user("解释一下当前架构，不要修改代码")];
        let mut options = ChatOptions::default();
        hook.pre_request_options(&messages, &mut options, &ctx)
            .await;
        assert_eq!(options.tool_choice, ToolChoice::Auto);
    }

    #[tokio::test]
    async fn deepseek_auto_firm_nudges_mixed_request_with_scoped_read_only_clause() {
        // The scoped read-only clause must not suppress the complex-work detection
        // (so the firm reminder still fires), but the weak-model path no longer
        // hard-forces the tool choice.
        let hook = TodoEagerHook::new("deepseek-v4-flash", "openai", TodoEagerness::Auto);
        let ctx = TurnCtx {
            round: 1,
            ..Default::default()
        };
        let messages = vec![Message::user("不要修改文档，但请重构代码并补充测试")];
        let mut options = ChatOptions::default();
        hook.pre_request_options(&messages, &mut options, &ctx)
            .await;
        assert_eq!(options.tool_choice, ToolChoice::Auto);

        let mut reminder = messages.clone();
        hook.pre_request(&mut reminder, &ctx).await;
        assert_eq!(
            reminder.len(),
            2,
            "firm reminder fires despite read-only clause"
        );
        assert!(!reminder[1].text.contains("You MUST"));
    }

    #[tokio::test]
    async fn always_selects_todowrite_only_without_an_existing_list() {
        let hook = TodoEagerHook::new("any-model", "openai", TodoEagerness::Always);
        let ctx = TurnCtx {
            round: 1,
            ..Default::default()
        };
        let messages = vec![Message::user("do several things")];
        let mut options = ChatOptions::default();
        hook.pre_request_options(&messages, &mut options, &ctx)
            .await;
        assert_eq!(
            options.tool_choice,
            ToolChoice::Specific("todowrite".into())
        );

        let with_list = vec![
            Message::user("continue"),
            todowrite_msg(r#"{"todos":[{"content":"a","status":"pending"}]}"#),
        ];
        let mut options = ChatOptions::default();
        hook.pre_request_options(&with_list, &mut options, &ctx)
            .await;
        assert_eq!(options.tool_choice, ToolChoice::Auto);

        let completed_list = vec![
            Message::user("finish old task"),
            todowrite_msg(r#"{"todos":[{"content":"old","status":"completed"}]}"#),
            Message::user("start a different task"),
        ];
        let mut options = ChatOptions::default();
        hook.pre_request_options(&completed_list, &mut options, &ctx)
            .await;
        assert_eq!(
            options.tool_choice,
            ToolChoice::Specific("todowrite".into()),
            "a completed historical list must not suppress planning for a new task"
        );
    }

    #[test]
    fn always_degrades_explicitly_for_ollama() {
        let hook = TodoEagerHook::new("any-model", "ollama", TodoEagerness::Always);
        assert_eq!(hook.eagerness, TodoEagerness::Preferred);
    }

    #[test]
    fn always_remains_strict_for_supported_adapters() {
        let hook = TodoEagerHook::new("any-model", "openai", TodoEagerness::Always);
        assert_eq!(hook.eagerness, TodoEagerness::Always);
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
            TodoHook::default().offer_continuation(&convo).await.is_some(),
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
            TodoHook::default().offer_continuation(&convo).await.is_none(),
            "all completed → let it stop"
        );
    }

    #[tokio::test]
    async fn no_nudge_when_no_todos() {
        let convo = convo_of(vec![
            Message::user("hi"),
            Message::assistant("hi there", vec![]),
        ]);
        assert!(TodoHook::default().offer_continuation(&convo).await.is_none());
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
            TodoHook::default().offer_continuation(&convo).await.is_none(),
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
            TodoHook::default().offer_continuation(&convo).await.is_some(),
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
            TodoHook::default().offer_continuation(&convo).await.is_none(),
            "already nudged this turn → let it stop (no spin)"
        );
    }
}
