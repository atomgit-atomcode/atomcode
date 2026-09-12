//! The task list: what the session is working through, and what is left.
//!
//! Ported from `atomcode-tuix`, where the panel appears above the composer for
//! as long as a plan is open. It is a *fold*, not a store: the list is derived
//! from the calls in the log each frame, so a live session, a replay and a
//! resumed one cannot disagree about it — and the derivation is
//! `atomcode_capabilities::tools::todo::reduce_todos`, the same function the
//! tool itself uses, rather than a second opinion about what the model asked
//! for.
//!
//! Nothing here is state the tool does not have. `todowrite` plans by sending
//! the whole list and patches by position, so an incremental update means
//! nothing on its own: applying it needs the list it was computed against. That
//! is why the calls are kept in order and folded in `render` instead of being
//! applied as they arrive.

use atomcode_capabilities::tools::todo::{
    is_todo_call, reduce_todos, todo_counts, todo_glyph, TodoItem, TodoStatus,
};
use atomcode_harness::seams::StopReason;
use atomcode_harness::session::SessionEvent;

use crate::el::El;
use crate::frame::Line;
use crate::module::{Height, View};
use crate::moment::{Moment, Viewport};
use crate::theme::{self, Role};

pub const ID: &str = "todo";

/// One call that touches the list, as the log recorded it.
struct Call {
    /// The call's id, so its result can be found. Not the position.
    id: String,
    name: String,
    args: String,
    /// The call came back an error. A rejected plan is not a plan, so it is
    /// kept — the result may arrive after other calls — and skipped in the fold.
    failed: bool,
}

#[derive(Default)]
pub struct State {
    calls: Vec<Call>,
}

pub struct Todo;

/// The list the panel draws: the calls that still count, folded — and nothing
/// once every item of it is done.
///
/// Empty therefore means one of two things, and both are "no panel": the model
/// has not planned anything yet, or it has finished what it planned. A finished
/// list is not worth a permanent row — the `✓` landed in the transcript with the
/// call that earned it, and `/todo` still prints the whole thing — so the panel
/// retires instead of standing there summarising work nobody has left to do.
///
/// The rule lives here rather than in `render` because `height` has to ask the
/// same question: `Hug(0)` is what hands the row back to the conversation, and a
/// `render` that disagreed with it would leave a blank line of chrome.
fn todos(state: &State) -> Vec<TodoItem> {
    let items = reduce_todos(
        state
            .calls
            .iter()
            .filter(|c| !c.failed)
            .map(|c| (c.name.as_str(), c.args.as_str())),
    );
    // `all` on an empty list is true, which is the answer we want there too.
    if items.iter().all(|t| t.status == TodoStatus::Completed) {
        return Vec::new();
    }
    items
}

impl View for Todo {
    type State = State;

    fn id() -> &'static str {
        ID
    }

    fn absorb(state: &mut State, fact: &SessionEvent) {
        match fact {
            // The call is recorded when the model makes it, before anyone knows
            // whether it will be accepted. `todowrite` validates its own
            // arguments, so a bad plan comes back as an error result below.
            SessionEvent::AssistantMessage { tool_calls, .. } => {
                for call in tool_calls.iter().filter(|c| is_todo_call(&c.name)) {
                    state.calls.push(Call {
                        id: call.id.clone(),
                        name: call.name.clone(),
                        args: call.arguments.clone(),
                        failed: false,
                    });
                }
            }

            SessionEvent::ToolResultLogged {
                call_id, is_error, ..
            } => {
                if !*is_error {
                    return;
                }
                if let Some(call) = state.calls.iter_mut().find(|c| c.id == *call_id) {
                    call.failed = true;
                }
            }

            // A cancel retires the plan: the list was for work that did not
            // happen, and the person who stopped it did not ask for the rest.
            // The calls stay in the log — only the *active* list is dropped, so
            // a resumed session reads this the same way. A later continue is a
            // fresh plan against the same history, which is exactly what
            // `derive_current_todos` does with the interruption boundary.
            SessionEvent::TurnEnd {
                stop: StopReason::Cancelled,
                ..
            } => state.calls.clear(),

            _ => {}
        }
    }

    fn render(state: &State, vp: &Viewport<'_>) -> Vec<Line> {
        let w = vp.rect.w;
        if w == 0 || vp.rect.h == 0 {
            return Vec::new();
        }
        let items = todos(state);
        if items.is_empty() {
            // Nothing to say: no plan yet, or every item of one finished (see
            // `todos`). `Hug(0)` already asked for nothing; drawing a header
            // saying "no tasks" would be a row of chrome on a screen that is
            // mostly conversation.
            return Vec::new();
        }

        let rows = window(&items, vp.rect.h as usize);
        let (completed, in_progress, total) = todo_counts(&items);
        let open = total.saturating_sub(completed + in_progress);
        let unicode = vp.moment.caps.unicode;
        let label = theme::fg(Role::Secondary).bold();
        let counts = theme::fg(Role::Muted);
        let accent = theme::fg(Role::Brand);
        // Done work recedes, but not by `dim`: stacking SGR 2 on a colour the
        // palette already picked for "quiet" is how it became unreadable. The
        // role is the whole of the recession.
        let done = theme::fg(Role::Muted);
        let plain = theme::fg(Role::Secondary);

        let mut out = El::row(vec![
            El::styled("任务 ", label),
            El::styled(
                format!("({completed} 已完成, {in_progress} 进行中, {open} 待办)"),
                counts,
            ),
        ])
        .lay(w);

        for row in rows {
            match row {
                Row::Item {
                    index,
                    status,
                    content,
                } => {
                    // Three states, three looks: the one being worked on is the
                    // one to find at a glance, a finished one recedes, and a
                    // pending one is plain. A completed item is dimmed rather
                    // than coloured — it is no longer news.
                    let (style, body) = match status {
                        TodoStatus::InProgress => (accent.bold(), plain.bold()),
                        TodoStatus::Completed => (done, done),
                        TodoStatus::Pending => (plain, plain),
                    };
                    out.extend(
                        El::row(vec![
                            El::styled(format!("{}  ", todo_glyph(status, unicode)), style),
                            El::styled(format!("#{}  {content}", index + 1), body),
                        ])
                        .lay(w),
                    );
                }
                // Not a task, and not shaped like one: no glyph, so it cannot be
                // mistaken for an item, but it does say that items are missing.
                Row::More { hidden } => out.extend(
                    El::row(vec![El::styled(
                        format!("   +{hidden} 更多"),
                        theme::fg(Role::Muted),
                    )])
                    .lay(w),
                ),
            }
        }
        out
    }

    /// A header plus a line each, content-sized: the host caps it, so a plan
    /// that outgrows the screen loses the fold, not the panel.
    fn height(state: &State, _: &Moment, _: u16) -> Height {
        let items = todos(state).len();
        Height::Hug(if items == 0 {
            0
        } else {
            (items + 1).min(u16::MAX as usize) as u16
        })
    }
}

/// One line of the body, after the fold and before the styles.
enum Row {
    Item {
        /// Position in the whole list, not in the window — the id the model
        /// patches by, and the number a person would quote.
        index: usize,
        status: TodoStatus,
        content: String,
    },
    More {
        hidden: usize,
    },
}

/// The items that fit in `rows` lines, including the header.
///
/// A plan can be longer than the screen, and the host clips from the end — so
/// without this the one item a person is waiting on could be the one item below
/// the fold. The window therefore prefers the tail (where the recent work is)
/// and pulls back only as far as the frontier needs: the in-progress item, else
/// the first pending one. Items dropped *below* the window get a `+N` row,
/// because nothing else would say they are there; the ones dropped above do not
/// need one — they are numbered, so a window starting at `#6` already reads as
/// "five above the fold" rather than as the whole list.
fn window(items: &[TodoItem], rows: usize) -> Vec<Row> {
    let body = rows.saturating_sub(1); // the header
    let item = |index: usize, item: &TodoItem| Row::Item {
        index,
        status: item.status,
        content: item.content.clone(),
    };

    if items.len() <= body {
        return items.iter().enumerate().map(|(i, t)| item(i, t)).collect();
    }
    if body == 0 {
        return Vec::new();
    }

    let frontier = items
        .iter()
        .position(|t| t.status == TodoStatus::InProgress)
        .or_else(|| items.iter().position(|t| t.status == TodoStatus::Pending))
        .unwrap_or(items.len() - 1);
    // The tail by default, pulled back only as far as the frontier needs.
    let start = (items.len() - body).min(frontier);
    let mut end = (start + body).min(items.len());
    let more = items.len() - end > 0 && body > 1;
    if more {
        end -= 1; // the row the `+N` marker takes
    }
    let mut out: Vec<Row> = items
        .iter()
        .enumerate()
        .take(end)
        .skip(start)
        .map(|(i, t)| item(i, t))
        .collect();
    if more {
        out.push(Row::More {
            hidden: items.len() - end,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::Rect;
    use crate::moment::Moment;
    use atomcode_kernel::tool::ToolCall;

    crate::tui_conformance!(view Todo as todo_conformance);

    fn call(id: &str, name: &str, args: &str) -> SessionEvent {
        SessionEvent::AssistantMessage {
            turn: 1,
            round: 1,
            text: String::new(),
            reasoning: String::new(),
            tool_calls: vec![ToolCall {
                id: id.into(),
                name: name.into(),
                arguments: args.into(),
            }],
        }
    }

    fn result(id: &str, is_error: bool) -> SessionEvent {
        SessionEvent::ToolResultLogged {
            turn: 1,
            round: 1,
            call_id: id.into(),
            content: String::new(),
            is_error,
            images: Vec::new(),
        }
    }

    fn plan(args: &str) -> SessionEvent {
        call("c1", "todowrite", args)
    }

    fn fold(facts: &[SessionEvent]) -> State {
        let mut state = State::default();
        for fact in facts {
            Todo::absorb(&mut state, fact);
        }
        state
    }

    fn drew(state: &State, w: u16, h: u16) -> Vec<String> {
        let moment = Moment::default();
        let vp = Viewport::new(Rect::sized(w, h), &moment);
        Todo::render(state, &vp)
            .iter()
            .map(|l| l.plain().trim_end().to_string())
            .collect()
    }

    fn asks(state: &State) -> Height {
        Todo::height(state, &Moment::default(), 80)
    }

    #[test]
    fn a_plan_draws_one_line_per_item_plus_the_header() {
        let state = fold(&[plan(
            r#"{"todos":[{"content":"读代码","status":"completed"},{"content":"写面板","status":"in_progress"},{"content":"跑测试","status":"pending"}]}"#,
        )]);
        let lines = drew(&state, 60, 10);
        assert_eq!(lines.len(), 4, "{lines:#?}");
        assert!(lines[0].contains("1 已完成"), "{:?}", lines[0]);
        assert!(lines[0].contains("1 进行中"), "{:?}", lines[0]);
        assert!(lines[0].contains("1 待办"), "{:?}", lines[0]);
        assert!(
            lines[1].contains("#1") && lines[1].contains("读代码"),
            "{:?}",
            lines[1]
        );
        assert!(
            lines[2].contains("#2") && lines[2].contains("写面板"),
            "{:?}",
            lines[2]
        );
        assert!(
            lines[3].contains("#3") && lines[3].contains("跑测试"),
            "{:?}",
            lines[3]
        );
        assert_eq!(asks(&state), Height::Hug(4));
    }

    #[test]
    fn no_plan_is_not_a_panel() {
        // Neither a header, nor a row asked for: the panel is only on screen
        // when it has something to say.
        let state = fold(&[call("c1", "read_file", r#"{"file_path":"a.rs"}"#)]);
        assert_eq!(drew(&state, 60, 10), Vec::<String>::new());
        assert_eq!(asks(&state), Height::Hug(0));
    }

    #[test]
    fn a_finished_plan_is_not_a_panel_either() {
        // The other end of `no_plan_is_not_a_panel`: a list with nothing left in
        // it is not something to keep on screen. The `✓` is in the transcript
        // with the call that earned it, and a panel that stayed would be the one
        // row of chrome that never leaves.
        let two = || {
            plan(
                r#"{"todos":[{"content":"读代码","status":"in_progress"},{"content":"写面板","status":"pending"}]}"#,
            )
        };
        let finish = |id: usize| {
            call(
                &format!("c{id}"),
                "todowrite",
                &format!(r#"{{"action":"update","id":{id},"status":"completed"}}"#),
            )
        };

        // Partly finished is not finished: the panel is there, and it says so.
        let partial = fold(&[two(), finish(1)]);
        assert_eq!(asks(&partial), Height::Hug(3));
        assert!(
            drew(&partial, 60, 10)[0].contains("1 已完成"),
            "the count still has something to report"
        );

        // The last item done: nothing to work through, so nothing to show.
        let done = fold(&[two(), finish(1), finish(2)]);
        assert_eq!(drew(&done, 60, 10), Vec::<String>::new());
        assert_eq!(asks(&done), Height::Hug(0));
    }

    #[test]
    fn an_incremental_update_needs_the_list_it_was_written_against() {
        // The point of folding in order rather than accumulating: `#2` only
        // means the second item of the plan in force.
        let state = fold(&[
            plan(
                r#"{"todos":[{"content":"a","status":"pending"},{"content":"b","status":"pending"}]}"#,
            ),
            call(
                "c2",
                "todowrite",
                r#"{"action":"update","id":2,"status":"in_progress"}"#,
            ),
            call("c3", "todowrite", r#"{"action":"add","content":"c"}"#),
        ]);
        let lines = drew(&state, 60, 10);
        assert_eq!(lines.len(), 4, "{lines:#?}");
        assert!(
            lines[2].starts_with("[•]") && lines[2].contains("b"),
            "{:?}",
            lines[2]
        );
        assert!(lines[3].contains("c"), "{:?}", lines[3]);
    }

    #[test]
    fn a_rejected_plan_leaves_the_earlier_list_in_force() {
        let state = fold(&[
            plan(r#"{"todos":[{"content":"a","status":"in_progress"}]}"#),
            result("c1", false),
            call(
                "c2",
                "todowrite",
                r#"{"todos":[{"content":"b","status":"pending"}]}"#,
            ),
            result("c2", true),
        ]);
        let lines = drew(&state, 60, 10);
        assert_eq!(lines.len(), 2, "{lines:#?}");
        assert!(
            lines[1].contains("a") && !lines[1].contains("b"),
            "{:?}",
            lines[1]
        );
    }

    #[test]
    fn a_cancel_retires_the_plan_and_a_later_one_starts_fresh() {
        let mut state = fold(&[plan(
            r#"{"todos":[{"content":"a","status":"in_progress"}]}"#,
        )]);
        Todo::absorb(
            &mut state,
            &SessionEvent::TurnEnd {
                turn: 1,
                stop: StopReason::Cancelled,
                error: None,
            },
        );
        assert_eq!(drew(&state, 60, 10), Vec::<String>::new());
        assert_eq!(asks(&state), Height::Hug(0));

        // A turn that merely failed is not a person changing their mind.
        Todo::absorb(
            &mut state,
            &SessionEvent::TurnEnd {
                turn: 2,
                stop: StopReason::Stopped,
                error: None,
            },
        );
        assert_eq!(
            drew(&state, 60, 10),
            Vec::<String>::new(),
            "a stop that is not a cancel must not touch the list either way"
        );

        // And the list keeps being folded from the log, so a new plan lands.
        Todo::absorb(
            &mut state,
            &plan(r#"{"todos":[{"content":"b","status":"pending"}]}"#),
        );
        let lines = drew(&state, 60, 10);
        assert_eq!(lines.len(), 2, "{lines:#?}");
        assert!(lines[1].contains("b"), "{:?}", lines[1]);
    }

    #[test]
    fn the_legacy_tool_name_folds_the_same() {
        let state = fold(&[plan(r#"{"todos":[{"content":"a","status":"pending"}]}"#)]);
        let legacy = fold(&[call(
            "c1",
            "todo",
            r#"{"todos":[{"content":"a","status":"pending"}]}"#,
        )]);
        assert_eq!(drew(&state, 60, 10), drew(&legacy, 60, 10));
    }

    #[test]
    fn a_long_plan_keeps_the_frontier_and_says_what_it_hid() {
        let build = |frontier: usize| {
            serde_json::json!({
                "todos": (0..12)
                    .map(|i| serde_json::json!({
                        "content": format!("task {i}"),
                        "status": if i == frontier { "in_progress" } else { "pending" },
                    }))
                    .collect::<Vec<_>>()
            })
            .to_string()
        };

        // Eight rows = header + 6 items + the `+N` marker, and the frontier is
        // the tenth: the window is the tail, which already contains it, so the
        // tail is what stays.
        let state = fold(&[plan(&build(9))]);
        let lines = drew(&state, 60, 8);
        assert_eq!(lines.len(), 8, "{lines:#?}");
        assert!(
            lines
                .iter()
                .any(|l| l.contains("#10") && l.contains("task 9")),
            "{lines:#?}"
        );
        assert!(!lines.iter().any(|l| l.contains("更多")), "{lines:#?}");

        // The frontier is the *first* item: the tail cannot hold it, so the
        // window is pulled back to the front and the rows it gives up say so.
        let state = fold(&[plan(&build(0))]);
        let lines = drew(&state, 60, 8);
        assert_eq!(lines.len(), 8, "{lines:#?}");
        assert!(
            lines[1].contains("#1") && lines[1].contains("task 0"),
            "{lines:#?}"
        );
        assert!(lines[7].contains("+6 更多"), "{:?}", lines[7]);

        // Nothing is claimed to be missing when everything fits.
        let all = drew(&state, 60, 40);
        assert_eq!(all.len(), 13, "{all:#?}");
        assert!(!all.iter().any(|l| l.contains("更多")), "{all:#?}");

        // Header only, when that is all there is room for.
        let one = drew(&state, 60, 1);
        assert_eq!(one.len(), 1, "{one:#?}");
        assert!(one[0].contains("11 待办"), "{:?}", one[0]);
    }
}
