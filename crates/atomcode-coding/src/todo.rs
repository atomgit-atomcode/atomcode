//! `TodoHook` — injects the current todo list as an ephemeral `<system-reminder>` at
//! the TAIL of every request, so the model always sees current progress even after the
//! originating todowrite result is compacted away. Cache-safe: tail-only, per-request
//! clone (never stored) — mirrors PlanModeGate / StatusReminderHook.

use async_trait::async_trait;
use atomcode_capabilities::reminder::synthetic_system_reminder;
use atomcode_capabilities::session::manager::{SessionManager, TodoSidecar, TodoSidecarItem};
use atomcode_capabilities::tools::todo::{
    active_todo_calls, apply_todo_action, derive_current_todos, is_todo_call, is_todo_plan,
    reduce_todos, render_todos_numbered, TodoItem, TodoStatus,
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
///
/// The nudge asks for a TRUE LIST, not for more work: the list exists to show where the work
/// actually stands, so an item that is finished gets closed, one the plan outgrew gets replaced
/// or dropped, and one still ahead stays open. An earlier wording ("If some are NOT done, keep
/// working through them") read as a demand to keep executing — it pushed models to grind on
/// items the task no longer needed instead of reconciling the list, which is the opposite of
/// what the list is for. Stopping is legitimate; stopping with a list that lies is not.
const TODO_COMPLETION_NUDGE: &str = "Before you finish: the task list still has open items. \
Take a moment to make it match where the work actually stands — the list is there to reflect \
reality, not to keep you working. \
If an item is done, mark it completed with `todowrite` \
(`{\"action\":\"update\",\"id\":<id>,\"status\":\"completed\"}`). If the plan changed and an item \
is no longer part of the task, replace or drop it rather than leaving it open. If it is genuinely \
still ahead of you, keep going — and if you are stopping, say briefly which items are open and why \
(blocked, needs approval, ambiguous, or simply no longer wanted).";

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

/// Where the list stands, stated — not a demand to check it.
///
/// Weak models drift (leave `in_progress` on a task they finished, or work with nothing
/// marked), and the `[~]` glyph in the list below is low-salience, so the current item is
/// named on its own line. It used to be an imperative — ">> You are currently ON task #N.
/// Before your NEXT action, reconcile: …" — and a demand to check before every action is a
/// demand to be seen checking: on a task that legitimately spans many steps there is
/// nothing to change, so the only visible way to comply is to say so. deepseek-flash did,
/// in 13–45% of its replies across four long sessions (2026-09-21..23: "Pointer is accurate
/// — still #6", "任务指针准确，继续"); the word "pointer" in those replies came from this
/// line and nowhere else. A permission-to-stay-quiet sentence riding beside the imperative
/// (09-19) did not stop it — the command was the stronger of the two.
///
/// Drift after a stretch of silence is named once, by [`todo_quiet_note`]; this line only
/// states the fact.
/// - An `in_progress` task → its `#<id>` and title.
/// - Nothing in progress but items open → that fact.
/// - All completed → `None`.
/// `id` is the 1-based position, matching `render_todos_numbered`.
fn todo_status_line(todos: &[TodoItem]) -> Option<String> {
    if let Some(i) = todos
        .iter()
        .position(|t| t.status == TodoStatus::InProgress)
    {
        return Some(format!("In progress: #{} \"{}\".", i + 1, todos[i].content));
    }
    let open = todos
        .iter()
        .filter(|t| t.status == TodoStatus::Pending)
        .count();
    (open > 0).then(|| format!("Nothing is marked in progress; {open} item(s) still open."))
}

/// What the list is, and the one rule about talking about it. The list is shown to the
/// person by the front end, so its state is never news: the model changes it with a call
/// when the work moves and otherwise leaves it alone — in its text as well as its calls.
const TODO_LIST_HEADER: &str = "Current task list — the person sees it in the UI, so it needs \
no comment from you: never write about which item you are on or whether the list is up to \
date. Change it with a call only when an item actually finishes, you switch to another item, \
or the plan changes.";

/// Steps without touching the list after which it is named once as possibly stale.
///
/// Not zero-tolerance: reading a file, grepping and editing between two status updates is
/// ordinary work. The same threshold the harness's `todo-reminder` row defaults to — this
/// hook is what says it in coding, which keeps that row off (see `CODING_ROWS`).
const TODO_QUIET_STEPS: usize = 3;

/// How many tool-using steps the model has taken since it last touched the list, counted
/// inside the current real-user turn — a new message from the person starts it over, as the
/// list was true when the last turn ended and the person has spoken since. Injected messages
/// are all `synthetic` in the projection, so only the person's own words reset it.
fn quiet_steps(messages: &[Message]) -> usize {
    let start = messages
        .iter()
        .rposition(|m| m.role == Role::User && !m.synthetic)
        .unwrap_or(0);
    let turn = &messages[start..];
    let since = turn
        .iter()
        .rposition(|m| m.tool_calls.iter().any(|c| is_todo_call(&c.name)))
        .map_or(0, |i| i + 1);
    turn[since..]
        .iter()
        .filter(|m| m.role == Role::Assistant && !m.tool_calls.is_empty())
        .count()
}

/// Said once, on the one request where the list has gone exactly [`TODO_QUIET_STEPS`] steps
/// untouched — the tail is rebuilt every request, so "once" needs no state, and nothing about
/// it reaches the log.
///
/// Before, two voices said this: the per-request line demanded a check every round, and the
/// harness's `todo-reminder` committed "has not been updated for N steps" every three steps
/// into the log, where each note then stayed in every later request. One long regression hunt
/// (2026-09-22) collected seven of them, and the model restated its whole diagnosis after
/// nearly each one to show it was still on the task. Now it is this sentence, once per
/// stretch, with no step count and an explicit "that is fine" for a task that is just long.
fn todo_quiet_note(todos: &[TodoItem], quiet: usize) -> Option<String> {
    if quiet != TODO_QUIET_STEPS {
        return None;
    }
    if let Some(i) = todos
        .iter()
        .position(|t| t.status == TodoStatus::InProgress)
    {
        let id = i + 1;
        return Some(format!(
            "The list has not moved for a few steps. If #{id} is finished, mark it completed \
(`{{\"action\":\"update\",\"id\":{id},\"status\":\"completed\"}}`); if you moved on, mark that \
item in progress; if the plan changed, send the new list. If #{id} is what you are doing, that \
is fine — carry on."
        ));
    }
    todos
        .iter()
        .any(|t| t.status == TodoStatus::Pending)
        .then(|| {
            "Nothing has been marked in progress for a few steps. Mark the item you are working \
on (`{\"action\":\"update\",\"id\":<id>,\"status\":\"in_progress\"}`), or send a new list if \
the plan changed."
                .to_string()
        })
}

/// The static "how to drive the list with `todowrite`" rules. These are CONSTANT
/// guidance — the model already has them from the persona and from the round right
/// after it (re)plans — so re-sending them on every execution round is pure wasted
/// cache (~170 tokens/round of never-cached tail). Rides the reminder only when the
/// model JUST wrote a full list (see `just_wrote_full_list`).
///
/// The last rule asks for a TRUE LIST rather than more work: the list records where the
/// work stands, so reconciliation (close / replace / drop) is the duty, not grinding on
/// every open item. A task that changed should change the list with it.
const TODO_DRIVE_RULES: &str = "\n\
- The MOMENT you START an item: `todowrite` with `{\"action\":\"update\",\"id\":<id>,\"status\":\"in_progress\"}`.\n\
- The MOMENT you FINISH an item: `todowrite` with `{\"action\":\"update\",\"id\":<id>,\"status\":\"completed\"}` (do not leave a done item showing incomplete).\n\
- Update ONE item at a time (the `{\"action\":...}` shape) — do NOT resend the whole `todos` list for a single status change (the full list is only for the initial plan or a full re-plan).\n\
- Keep the list TRUE as the work moves: an item you finished is marked completed, an item the task outgrew is replaced or dropped (`todowrite` with the new full list), and an item still ahead stays open. If the work changed shape, change the list with it — a stale or inflated list is worse than a short one. Stopping is allowed; stopping with a list that no longer matches reality is not.";

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
        let Some(CurrentTodos { items: todos, .. }) = current_todos(messages, || self.sidecar(ctx))
        else {
            return;
        };
        if todos.is_empty() {
            return;
        }
        // ASCII-safe body (the model doesn't need glyph prettiness; the TUI renders
        // the pretty version). Tail-append so the cached prefix is preserved.
        // Header, status line and list ride EVERY round; the static drive rules ride
        // ONLY right after a (re)plan, to stop wasting cache re-sending constant
        // guidance every execution round.
        let rules = if just_wrote_full_list(messages) {
            TODO_DRIVE_RULES
        } else {
            ""
        };
        let status = todo_status_line(&todos)
            .map(|s| format!("\n{s}"))
            .unwrap_or_default();
        let note = todo_quiet_note(&todos, quiet_steps(messages))
            .map(|n| format!(" {n}"))
            .unwrap_or_default();
        let body = format!(
            "{TODO_LIST_HEADER}{rules}\n{status}{note}\n{}",
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
    /// list the model / vscode panel rely on (issue #1503). Best-effort, and only
    /// when there is a list (see [`current_todos`]; a session that never planned
    /// has nothing to persist). What is written is the list as the model saw it —
    /// after a compaction, the previous sidecar with this turn's updates on top —
    /// so a second compaction does not roll those updates back.
    /// `ctx.session_id` is `None` for headless drivers — nothing to key the file on.
    async fn turn_complete(&self, convo: &Conversation, _reason: &StopReason, ctx: &TurnCtx) {
        let Some(working_dir) = self.working_dir.as_deref() else {
            return;
        };
        let Some(session_id) = ctx.session_id.as_deref() else {
            return;
        };
        // An explicitly emptied list is written too: left unwritten, the sidecar would
        // keep the list from before the clear and hand it back after a compaction.
        let Some(current) = current_todos(&convo.messages, || self.sidecar(ctx)) else {
            return;
        };
        let items: Vec<TodoSidecarItem> = current
            .items
            .iter()
            .map(|t| TodoSidecarItem {
                content: t.content.clone(),
                status: todo_status_str(&t.status).to_string(),
            })
            .collect();
        let manager = SessionManager::for_project(working_dir);
        let _ = manager.write_todo_sidecar(
            session_id,
            &items,
            convo.messages.len(),
            current.last_call.as_deref(),
        );
    }
}

impl TodoHook {
    /// The session's persisted todo sidecar, when there is a session to key it on.
    fn sidecar(&self, ctx: &TurnCtx) -> Option<TodoSidecar> {
        let working_dir = self.working_dir.as_deref()?;
        let session_id = ctx.session_id.as_deref()?;
        SessionManager::for_project(working_dir)
            .read_todo_sidecar(session_id)
            .ok()?
    }
}

/// The task list as it stands, and the last todo call it reflects.
struct CurrentTodos {
    items: Vec<TodoItem>,
    last_call: Option<String>,
}

/// The current task list: the transcript's todo calls, over the session's sidecar when
/// the transcript no longer holds a plan. `None` when there is no list at all.
///
/// A plan in the transcript is authoritative — including an empty one, which is the list
/// being cleared, not the list being absent. Only when compaction has drained the plan
/// (issue #1503) is the sidecar the baseline, and the updates the transcript still carries
/// are laid over it rather than dropped. Folded on their own they name ids an empty list
/// does not have and come to nothing, so the model was shown the sidecar's list from the
/// last completed turn: it marked #3 completed, still saw `[~]`, sent the same update
/// again, and was stopped by the tool-loop guard for repeating itself. A cleared list fell
/// back to the same stale sidecar and was cleared again for the same reason.
///
/// `sidecar` is read only when the transcript holds no plan.
fn current_todos(
    messages: &[Message],
    sidecar: impl FnOnce() -> Option<TodoSidecar>,
) -> Option<CurrentTodos> {
    let calls = active_todo_calls(messages);
    let last_call = calls.last().map(|call| call.id.clone());
    if calls.iter().any(|call| is_todo_plan(&call.arguments)) {
        let items = reduce_todos(
            calls
                .iter()
                .map(|call| (call.name.as_str(), call.arguments.as_str())),
        );
        return Some(CurrentTodos { items, last_call });
    }
    let (mut items, applied) = match sidecar() {
        Some(sidecar) => {
            let items = sidecar
                .todos
                .into_iter()
                .map(|item| TodoItem {
                    content: item.content,
                    status: parse_todo_status(&item.status),
                })
                .collect();
            (items, sidecar.last_call)
        }
        None if calls.is_empty() => return None,
        None => (Vec::new(), None),
    };
    // The calls the sidecar already reflects are the ones up to its last call. When that
    // call is not in the transcript, compaction took it, and every call left is newer.
    let start = applied
        .as_deref()
        .and_then(|seen| calls.iter().position(|call| call.id == seen))
        .map_or(0, |at| at + 1);
    for call in &calls[start..] {
        apply_todo_action(&mut items, &call.arguments);
    }
    Some(CurrentTodos {
        items,
        last_call: last_call.or(applied),
    })
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
        TodoHook::default()
            .pre_request(&mut fresh, &TurnCtx::default())
            .await;
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
        // rules are OMITTED (cache win), but the header + list still ride every round.
        let mut exec = vec![
            Message::user("go"),
            todowrite_msg(list),
            todo_update_msg(r#"{"action":"update","id":1,"status":"completed"}"#),
        ];
        TodoHook::default()
            .pre_request(&mut exec, &TurnCtx::default())
            .await;
        let after_update = exec.last().unwrap().text.clone();
        assert!(
            !after_update.contains("The MOMENT you START"),
            "drive rules must NOT repeat on execution rounds:\n{after_update}"
        );
        assert!(
            after_update.contains(TODO_LIST_HEADER),
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

    // ---- the status line: stated, never a demand to check -------------------------------

    fn item(content: &str, status: TodoStatus) -> TodoItem {
        TodoItem {
            content: content.into(),
            status,
        }
    }

    /// The words that turned the per-round tail into something to answer. A demand to
    /// check before every action is a demand to be seen checking: deepseek-flash replied
    /// "Pointer is accurate — still #6" to it in up to 45% of its rounds.
    const DEMANDS_A_CHECK: [&str; 5] = [
        "reconcile",
        "pointer",
        "Before your NEXT action",
        "Before you act",
        "FIRST",
    ];

    async fn tail_for(list: &str) -> String {
        let mut msgs = vec![
            Message::user("do it"),
            todowrite_msg(list),
            // An ordinary execution round: the drive rules are not riding.
            Message::assistant(
                "",
                vec![ToolCall {
                    id: "r".into(),
                    name: "read_file".into(),
                    arguments: "{}".into(),
                }],
            ),
        ];
        TodoHook::default()
            .pre_request(&mut msgs, &TurnCtx::default())
            .await;
        msgs.last().unwrap().text.clone()
    }

    #[tokio::test]
    async fn the_tail_states_where_the_list_stands_and_asks_for_no_check() {
        for (state, list) in [
            (
                "a task in progress",
                r#"{"todos":[{"content":"first","status":"completed"},{"content":"do the thing","status":"in_progress"},{"content":"later","status":"pending"}]}"#,
            ),
            (
                "open items, none in progress",
                r#"{"todos":[{"content":"first","status":"completed"},{"content":"second","status":"pending"}]}"#,
            ),
        ] {
            let text = tail_for(list).await;
            for demand in DEMANDS_A_CHECK {
                assert!(
                    !text.contains(demand),
                    "{state}: the tail must state the list, not demand a check ({demand:?}): {text}"
                );
            }
            assert!(
                text.contains(TODO_LIST_HEADER),
                "{state}: and it says the list needs no comment: {text}"
            );
        }
    }

    #[test]
    fn the_status_line_names_the_item_in_progress() {
        let todos = vec![
            item("first", TodoStatus::Completed),
            item("do the thing", TodoStatus::InProgress),
            item("later", TodoStatus::Pending),
        ];
        let s = todo_status_line(&todos).expect("in_progress → status line");
        assert!(s.contains("#2"), "the 1-based id: {s}");
        assert!(s.contains("do the thing"), "the title: {s}");
    }

    #[test]
    fn the_status_line_says_when_nothing_is_in_progress() {
        let todos = vec![
            item("first", TodoStatus::Completed),
            item("second", TodoStatus::Pending),
        ];
        let s = todo_status_line(&todos).expect("open + nothing in progress → status line");
        assert!(s.contains("Nothing is marked in progress"), "{s}");
        assert!(s.contains('1'), "and how many are open: {s}");
    }

    #[test]
    fn a_settled_list_has_no_status_line() {
        let todos = vec![
            item("a", TodoStatus::Completed),
            item("b", TodoStatus::Completed),
        ];
        assert!(todo_status_line(&todos).is_none());
    }

    #[tokio::test]
    async fn the_status_line_sits_between_the_header_and_the_list() {
        let text = tail_for(r#"{"todos":[{"content":"step one","status":"in_progress"}]}"#).await;
        let header = text.find(TODO_LIST_HEADER).expect("header");
        let status = text.find("In progress: #1").expect("status line");
        let list = text.find("1. step one").expect("list");
        assert!(header < status && status < list, "{text}");
    }

    // ---- once per stretch of silence ----------------------------------------------------

    fn step() -> Message {
        Message::assistant(
            "",
            vec![ToolCall {
                id: "s".into(),
                name: "read_file".into(),
                arguments: "{}".into(),
            }],
        )
    }

    const DOING: &str = r#"{"todos":[{"content":"read the parser","status":"in_progress"},{"content":"fix the parser","status":"pending"}]}"#;

    async fn tail(mut msgs: Vec<Message>) -> String {
        TodoHook::default()
            .pre_request(&mut msgs, &TurnCtx::default())
            .await;
        msgs.last().unwrap().text.clone()
    }

    fn after_plan(steps: usize) -> Vec<Message> {
        let mut msgs = vec![Message::user("fix the parser"), todowrite_msg(DOING)];
        msgs.extend((0..steps).map(|_| step()));
        msgs
    }

    const NOTE: &str = "has not moved for a few steps";

    #[tokio::test]
    async fn a_quiet_list_is_named_once_not_every_round() {
        let said: Vec<usize> = {
            let mut said = Vec::new();
            for steps in 0..=8 {
                if tail(after_plan(steps)).await.contains(NOTE) {
                    said.push(steps);
                }
            }
            said
        };
        assert_eq!(
            said,
            vec![TODO_QUIET_STEPS],
            "eight quiet steps: the note rides exactly one request"
        );
        let text = tail(after_plan(TODO_QUIET_STEPS)).await;
        assert!(
            text.contains("#1") && text.contains("that is fine"),
            "{text}"
        );
        let note = todo_quiet_note(
            &[item("read the parser", TodoStatus::InProgress)],
            TODO_QUIET_STEPS,
        )
        .unwrap();
        assert!(
            !note.contains(&format!("{TODO_QUIET_STEPS} steps")),
            "a long task is not late — no step count: {note}"
        );
    }

    #[tokio::test]
    async fn touching_the_list_starts_a_new_stretch() {
        let mut msgs = after_plan(5);
        msgs.push(todo_update_msg(
            r#"{"action":"update","id":1,"status":"completed"}"#,
        ));
        msgs.push(todo_update_msg(
            r#"{"action":"update","id":2,"status":"in_progress"}"#,
        ));
        msgs.extend((0..TODO_QUIET_STEPS).map(|_| step()));
        let text = tail(msgs).await;
        assert!(
            text.contains(NOTE) && text.contains("#2"),
            "the new stretch is about the item it moved to: {text}"
        );
    }

    #[tokio::test]
    async fn the_person_speaking_starts_a_new_stretch_and_a_note_does_not() {
        // Five quiet steps last turn, then the person says something: two steps into
        // the new turn is not three.
        let mut msgs = after_plan(5);
        msgs.push(Message::assistant("done for now", vec![]));
        msgs.push(Message::user("and the lexer?"));
        msgs.extend((0..2).map(|_| step()));
        assert!(!tail(msgs.clone()).await.contains(NOTE));
        // An injected note in between is not the person: it does not reset the count.
        msgs.push(synthetic_system_reminder("Current date: 2026-09-23 (Wed)"));
        msgs.push(step());
        assert!(tail(msgs).await.contains(NOTE));
    }

    #[tokio::test]
    async fn injects_reminder_when_list_present() {
        let mut msgs = vec![
            Message::user("do the thing"),
            todowrite_msg(r#"{"todos":[{"content":"step one","status":"in_progress"}]}"#),
        ];
        let before = msgs.len();
        TodoHook::default()
            .pre_request(&mut msgs, &TurnCtx::default())
            .await;
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
        TodoHook::default()
            .pre_request(&mut msgs, &TurnCtx::default())
            .await;
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
            TodoHook::default()
                .offer_continuation(&convo)
                .await
                .is_some(),
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
            TodoHook::default()
                .offer_continuation(&convo)
                .await
                .is_none(),
            "all completed → let it stop"
        );
    }

    #[tokio::test]
    async fn no_nudge_when_no_todos() {
        let convo = convo_of(vec![
            Message::user("hi"),
            Message::assistant("hi there", vec![]),
        ]);
        assert!(TodoHook::default()
            .offer_continuation(&convo)
            .await
            .is_none());
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
            TodoHook::default()
                .offer_continuation(&convo)
                .await
                .is_none(),
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
            TodoHook::default()
                .offer_continuation(&convo)
                .await
                .is_some(),
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
            TodoHook::default()
                .offer_continuation(&convo)
                .await
                .is_none(),
            "already nudged this turn → let it stop (no spin)"
        );
    }

    // ---- the list after a compaction: the sidecar plus what the transcript still carries --

    fn todo_call(id: &str, args: &str) -> Message {
        Message::assistant(
            "",
            vec![ToolCall {
                id: id.into(),
                name: "todowrite".into(),
                arguments: args.into(),
            }],
        )
    }

    fn sidecar_of(items: &[(&str, &str)], last_call: Option<&str>) -> TodoSidecar {
        TodoSidecar {
            todos: items
                .iter()
                .map(|(content, status)| TodoSidecarItem {
                    content: (*content).into(),
                    status: (*status).into(),
                })
                .collect(),
            message_count: 40,
            last_call: last_call.map(str::to_string),
        }
    }

    fn statuses(current: &CurrentTodos) -> Vec<(String, TodoStatus)> {
        current
            .items
            .iter()
            .map(|t| (t.content.clone(), t.status))
            .collect()
    }

    const THREE_STARTED: [(&str, &str); 3] = [
        ("parse the config file", "completed"),
        ("wire the loader into main", "completed"),
        ("verify resume after restart", "in_progress"),
    ];

    /// The reported loop: compaction took the plan, the model marked #3 completed, and the
    /// list it was shown still had #3 in progress — so it sent the same update again until
    /// the tool-loop guard stopped it. The update lands on the sidecar's list.
    ///
    /// Negative control: fold the transcript alone (the old path) and the update names an
    /// id an empty list does not have, which left the sidecar's `in_progress` standing.
    #[test]
    fn an_update_after_compaction_lands_on_the_sidecar_list() {
        let messages = vec![
            Message::user("carry on"),
            todo_call("c9", r#"{"action":"update","id":3,"status":"completed"}"#),
            Message::tool_result("c9", "#3 → completed", false),
        ];
        let current = current_todos(&messages, || Some(sidecar_of(&THREE_STARTED, Some("c7"))))
            .expect("a sidecar is a list");
        assert_eq!(
            current.items[2].status,
            TodoStatus::Completed,
            "{:?}",
            statuses(&current)
        );
        assert_eq!(current.last_call.as_deref(), Some("c9"));
        assert!(
            derive_current_todos(&messages).is_empty(),
            "the old path folded to nothing"
        );
    }

    /// The sidecar already reflects every call up to its `last_call`; a compaction that
    /// kept some of those calls must not apply them again — an `add` would append twice.
    #[test]
    fn calls_the_sidecar_already_reflects_are_not_applied_twice() {
        let messages = vec![
            todo_call(
                "a1",
                r#"{"action":"add","content":"document the loader flags"}"#,
            ),
            Message::tool_result("a1", "Added task: document the loader flags", false),
            Message::user("next"),
            todo_call("u2", r#"{"action":"update","id":4,"status":"in_progress"}"#),
            Message::tool_result("u2", "#4 → in_progress", false),
        ];
        let mut after_add = THREE_STARTED.to_vec();
        after_add[2].1 = "completed";
        after_add.push(("document the loader flags", "pending"));
        let current = current_todos(&messages, || Some(sidecar_of(&after_add, Some("a1"))))
            .expect("a sidecar is a list");
        assert_eq!(current.items.len(), 4, "{:?}", statuses(&current));
        assert_eq!(current.items[3].status, TodoStatus::InProgress);
    }

    /// A sidecar written before `last_call` existed says nothing about which calls it has
    /// seen; every call the transcript carries is laid over it.
    #[test]
    fn a_sidecar_without_a_last_call_takes_every_update() {
        let messages = vec![todo_call(
            "c1",
            r#"{"action":"update","id":3,"status":"completed"}"#,
        )];
        let current = current_todos(&messages, || Some(sidecar_of(&THREE_STARTED, None))).unwrap();
        assert_eq!(current.items[2].status, TodoStatus::Completed);
    }

    /// Clearing the list is a plan with nothing in it, not the absence of a plan: it must
    /// not fall back to the sidecar, which still holds the list from before the clear.
    #[test]
    fn a_cleared_list_stays_cleared() {
        let messages = vec![
            Message::user("clear your todos"),
            todo_call("c1", r#"{"todos":[]}"#),
            Message::tool_result("c1", "(no tasks)", false),
        ];
        let current = current_todos(&messages, || {
            panic!("a plan in the transcript is authoritative; the sidecar is not read")
        })
        .expect("an emptied list is still a list");
        assert!(current.items.is_empty());
        assert_eq!(current.last_call.as_deref(), Some("c1"));
    }

    /// A plan still in the transcript wins over the sidecar, as before.
    #[test]
    fn a_plan_in_the_transcript_is_the_list() {
        let messages = vec![todo_call(
            "p1",
            r#"{"todos":[{"content":"rename the session flag","status":"in_progress"}]}"#,
        )];
        let current = current_todos(&messages, || panic!("sidecar must not be read")).unwrap();
        assert_eq!(current.items.len(), 1);
        assert_eq!(current.items[0].content, "rename the session flag");
    }

    /// A rejected call is not part of the list, over the sidecar as in the transcript.
    #[test]
    fn a_failed_update_does_not_land_on_the_sidecar() {
        let messages = vec![
            todo_call("c1", r#"{"action":"update","id":3,"status":"completed"}"#),
            Message::tool_result("c1", "todowrite: bad", true),
        ];
        let current =
            current_todos(&messages, || Some(sidecar_of(&THREE_STARTED, Some("c0")))).unwrap();
        assert_eq!(current.items[2].status, TodoStatus::InProgress);
        assert_eq!(current.last_call.as_deref(), Some("c0"));
    }

    /// No plan, no sidecar, no todo calls: there is no list.
    #[test]
    fn no_list_anywhere_is_none() {
        assert!(current_todos(&[Message::user("hi")], || None).is_none());
    }
}
