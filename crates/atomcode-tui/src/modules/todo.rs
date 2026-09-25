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

use crate::i18n::{t, Msg};
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
    /// The turn it was made in: a turn the person took back is not part of the
    /// plan any more, and the panel follows the projection (`docs/adr/0024` §17).
    turn: u64,
    /// The call came back an error. A rejected plan is not a plan, so it is
    /// kept — the result may arrive after other calls — and skipped in the fold.
    failed: bool,
}

/// The calls that shape the task list, in the order the log recorded them, and
/// the list they fold to.
///
/// Its own type because two readers need the same answer: the panel draws the
/// list, and the transcript's turn-end line says whether a turn that ended on
/// its own left work open. Each keeps a `Plan` and feeds it the same facts, so
/// the line cannot call a turn finished while the panel still shows items.
#[derive(Default)]
pub(crate) struct Plan {
    calls: Vec<Call>,
}

impl Plan {
    /// Take in one fact. `true` when the list may have changed.
    pub(crate) fn absorb(&mut self, fact: &SessionEvent) -> bool {
        match fact {
            // The call is recorded when the model makes it, before anyone knows
            // whether it will be accepted. `todowrite` validates its own
            // arguments, so a bad plan comes back as an error result below.
            SessionEvent::AssistantMessage {
                tool_calls, turn, ..
            } => {
                let before = self.calls.len();
                for call in tool_calls.iter().filter(|c| is_todo_call(&c.name)) {
                    self.calls.push(Call {
                        id: call.id.clone(),
                        name: call.name.clone(),
                        args: call.arguments.clone(),
                        turn: *turn,
                        failed: false,
                    });
                }
                self.calls.len() != before
            }

            SessionEvent::ToolResultLogged {
                call_id, is_error, ..
            } if *is_error => match self.calls.iter_mut().find(|c| c.id == *call_id) {
                Some(call) => {
                    call.failed = true;
                    true
                }
                None => false,
            },

            // A cancel retires the plan: the list was for work that did not
            // happen, and the person who stopped it did not ask for the rest.
            // The calls stay in the log — only the *active* list is dropped, so
            // a resumed session reads this the same way. A later continue is a
            // fresh plan against the same history, which is exactly what
            // `derive_current_todos` does with the interruption boundary.
            SessionEvent::TurnEnd {
                stop: StopReason::Cancelled,
                ..
            } => {
                self.calls.clear();
                true
            }

            _ => false,
        }
    }

    /// The list, leaving out the turns in `undone`. Completed items included.
    pub(crate) fn fold(&self, undone: &std::collections::BTreeSet<u64>) -> Vec<TodoItem> {
        reduce_todos(
            self.calls
                .iter()
                .filter(|c| !c.failed && !undone.contains(&c.turn))
                .map(|c| (c.name.as_str(), c.args.as_str())),
        )
    }

    /// Forget the calls of `turn` and every turn after it — what an undo that
    /// takes the conversation back to before `turn` leaves of the plan.
    pub(crate) fn retract_from(&mut self, turn: u64) {
        self.calls.retain(|c| c.turn < turn);
    }

    /// How many items are not completed yet.
    pub(crate) fn open_items(&self) -> usize {
        self.fold(&std::collections::BTreeSet::new())
            .iter()
            .filter(|t| t.status != TodoStatus::Completed)
            .count()
    }
}

#[derive(Default)]
pub struct State {
    plan: Plan,
    /// The fold of `calls`, kept rather than recomputed.
    ///
    /// `render` and `height` both have to ask whether there is a panel at all —
    /// `Hug(0)` is what hands the row back to the conversation, so if they
    /// disagreed the screen would carry a blank line of chrome — and the honest
    /// way to make them agree is one answer, not the same `reduce_todos` call
    /// twice. It also costs: `reduce_todos` walks every call ever made, and a
    /// frame asked it twice.
    items: Vec<TodoItem>,
}

pub struct Todo;

/// Fold the calls into the list the panel draws.
///
/// Empty means one of two things, and both are "no panel": the model has not
/// planned anything yet, or it has finished what it planned. A finished list is
/// not worth a permanent row — the `✓` landed in the transcript with the call
/// that earned it, and `/todo` still prints the whole thing — so the panel
/// retires instead of standing there summarising work nobody has left to do.
///
/// Called only where `calls` changes, which is what makes it a fold per fact
/// rather than a fold per frame.
fn refold(state: &mut State) {
    state.items = fold_calls(state, &std::collections::BTreeSet::new());
}

/// The calls, folded into a list, leaving out the turns that were taken back.
fn fold_calls(state: &State, undone: &std::collections::BTreeSet<u64>) -> Vec<TodoItem> {
    let mut items = state.plan.fold(undone);
    // `all` on an empty list is true, which is the answer we want there too.
    if items.iter().all(|t| t.status == TodoStatus::Completed) {
        items.clear();
    }
    items
}

/// What the panel draws now: the kept fold when nothing was taken back, and one
/// that leaves the taken-back turns out when something was. `render` and
/// `height` both ask this, so they cannot disagree about whether there is a
/// panel at all.
fn shown(state: &State, moment: &Moment) -> Vec<TodoItem> {
    if moment.undone.is_empty() {
        return state.items.clone();
    }
    fold_calls(state, &moment.undone)
}

impl View for Todo {
    type State = State;

    fn id() -> &'static str {
        ID
    }

    fn absorb(state: &mut State, fact: &SessionEvent) {
        if state.plan.absorb(fact) {
            refold(state);
        }
    }

    fn render(state: &State, vp: &Viewport<'_>) -> Vec<Line> {
        let w = vp.rect.w;
        if w == 0 || vp.rect.h == 0 {
            return Vec::new();
        }
        let items = &shown(state, vp.moment);
        if items.is_empty() {
            // Nothing to say: no plan yet, or every item of one finished (see
            // `refold`). `Hug(0)` already asked for nothing; drawing a header
            // saying "no tasks" would be a row of chrome on a screen that is
            // mostly conversation.
            return Vec::new();
        }

        let margin = if vp.rect.h >= ROWS {
            MARGIN as usize
        } else {
            0
        };
        // The window is told what is left once the margin has been taken off the
        // rect — it counts the header itself, so handing it the full height
        // would push one item below the fold to make room for a blank row.
        let rows = window(items, (vp.rect.h as usize).saturating_sub(margin));
        let (completed, in_progress, total) = todo_counts(items);
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

        // The margin, only when the rect can seat it as well as the header —
        // the same bargain `live` strikes, and for the same reason: the host
        // clips to `height`, so emitting a blank row it would keep in place of
        // the header is the one way this could draw a plan panel that says
        // nothing. Above the header only: below the last item is the
        // conversation's own edge, and a margin there would be a second row of
        // nothing between the panel and whatever the reader scrolled to.
        let mut out: Vec<Line> = Vec::with_capacity(ROWS as usize);
        for _ in 0..margin {
            out.push(Line::empty());
        }

        out.extend(
            El::row(vec![
                // The header word is the product's — the other front end's todo
                // panel is titled with it, and one panel in two wordings is two
                // panels to a person.
                El::styled(
                    format!(
                        "{} ",
                        crate::i18n::product::t(crate::i18n::product::Msg::TodoPanelTitle)
                    ),
                    label,
                ),
                El::styled(
                    t(Msg::TodoCounts {
                        completed,
                        in_progress,
                        open,
                    })
                    .into_owned(),
                    counts,
                ),
            ])
            .lay(w),
        );

        // One number column for the rows on screen: `#9` and `#10` side by side
        // would start their text a column apart. Right-aligned, so the text
        // column is the same on every row.
        let digits = rows
            .iter()
            .filter_map(|row| match row {
                Row::Item { index, .. } => Some((index + 1).to_string().len()),
                Row::More { .. } => None,
            })
            .max()
            .unwrap_or(1);
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
                            El::styled(format!("#{:>digits$}  {content}", index + 1), body),
                        ])
                        .lay(w),
                    );
                }
                // Not a task, and not shaped like one: no glyph, so it cannot be
                // mistaken for an item, but it does say that items are missing.
                Row::More { hidden } => out.extend(
                    El::row(vec![El::styled(
                        // Under the `#`, where every item's number starts: the
                        // glyph's three columns and the two after it.
                        format!(
                            "     {}",
                            crate::i18n::product::t(crate::i18n::product::Msg::TodoPanelMore {
                                n: hidden
                            })
                        ),
                        theme::fg(Role::Muted),
                    )])
                    .lay(w),
                ),
            }
        }
        out
    }

    /// The header, the margin above it, and a line each. Content-sized: the
    /// host caps it, so a plan that outgrows the screen loses the fold, not the
    /// panel.
    ///
    /// The margin is counted here because this is where the row is *asked for*:
    /// a `gap` in the layout would be counted whether or not the panel is up,
    /// leaving a blank row of chrome over the conversation between plans. Asked
    /// for here, it arrives and leaves with the panel, from the same predicate
    /// `render` draws from. See `live.rs` for the same bargain.
    fn height(state: &State, moment: &Moment, _: u16) -> Height {
        let items = shown(state, moment).len();
        Height::Hug(if items == 0 {
            0
        } else {
            (items + 1 + MARGIN as usize).min(u16::MAX as usize) as u16
        })
    }
}

/// The blank row above the header — the padding that keeps the plan off the
/// conversation, which is the thing it sits directly under.
///
/// Above only. Under the last item is the stream's own bottom edge, and the tail
/// is laid out from the bottom up (ADR 0020), so a margin below would be a row
/// of nothing claimed from the conversation to say the same thing twice.
const MARGIN: u16 = 1;

/// What the panel asks for when it is up: the margin, the header, and at least
/// one item. The threshold `render` uses to decide whether the margin fits.
const ROWS: u16 = 1 + MARGIN;

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
            reasoning_blocks: Vec::new(),
            meta: None,
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

    /// The same plan, made in a named turn — for the projection, which is about
    /// which turn a call belongs to.
    fn plan_in(turn: u64, id: &str, args: &str) -> SessionEvent {
        match call(id, "todowrite", args) {
            SessionEvent::AssistantMessage {
                round,
                text,
                reasoning,
                tool_calls,
                reasoning_blocks,
                meta,
                ..
            } => SessionEvent::AssistantMessage {
                turn,
                round,
                text,
                reasoning,
                tool_calls,
                reasoning_blocks,
                meta,
            },
            other => other,
        }
    }

    /// What the panel draws for a moment that took some turns back.
    fn drew_undone(state: &State, w: u16, h: u16, undone: &[u64]) -> (Vec<String>, Height) {
        let moment = Moment {
            undone: undone.iter().copied().collect(),
            ..Moment::default()
        };
        let vp = Viewport::new(Rect::sized(w, h), &moment);
        let lines = Todo::render(state, &vp)
            .iter()
            .map(|l| l.plain().trim_end().to_string())
            .collect();
        (lines, Todo::height(state, &moment, w))
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
        assert_eq!(lines.len(), 5, "{lines:#?}");
        // The margin first, blank — the row that keeps the plan off the words it
        // sits directly under. It is the module's own row, asked for in
        // `height`, so it arrives and leaves with the panel.
        assert_eq!(lines[0], "", "{lines:#?}");
        assert!(lines[1].contains("1 已完成"), "{:?}", lines[1]);
        assert!(lines[1].contains("1 进行中"), "{:?}", lines[1]);
        assert!(lines[1].contains("1 待办"), "{:?}", lines[1]);
        assert!(
            lines[2].contains("#1") && lines[2].contains("读代码"),
            "{:?}",
            lines[2]
        );
        assert!(
            lines[3].contains("#2") && lines[3].contains("写面板"),
            "{:?}",
            lines[3]
        );
        assert!(
            lines[4].contains("#3") && lines[4].contains("跑测试"),
            "{:?}",
            lines[4]
        );
        assert_eq!(asks(&state), Height::Hug(5));
    }

    /// The panel is the plan as it stands *now*, and a turn the person took back
    /// did not happen: its `todowrite` leaves the fold with it, and the list the
    /// turn before it wrote comes back (`docs/adr/0024` §17).
    ///
    /// The discriminating shape is a plan that was *finished* in the turn that
    /// was taken back: keeping it would leave no panel at all, so the panel
    /// arriving is the projection, not a redraw.
    #[test]
    fn a_plan_written_in_a_turn_that_was_taken_back_leaves_the_panel() {
        let state = fold(&[
            plan_in(
                1,
                "c1",
                r#"{"todos":[{"content":"读代码","status":"in_progress"},{"content":"写面板","status":"pending"}]}"#,
            ),
            plan_in(
                2,
                "c2",
                r#"{"todos":[{"content":"读代码","status":"completed"},{"content":"写面板","status":"completed"}]}"#,
            ),
        ]);
        // Turn 2 finished the plan, so with both turns standing there is no
        // panel — `a_finished_plan_is_not_a_panel_either`.
        assert_eq!(drew(&state, 60, 10), Vec::<String>::new());
        assert_eq!(asks(&state), Height::Hug(0));

        let (lines, height) = drew_undone(&state, 60, 10, &[2]);
        assert_eq!(lines.len(), 4, "the turn-1 list is back: {lines:#?}");
        assert!(lines[1].contains("1 进行中"), "{:?}", lines[1]);
        assert!(
            lines[2].contains("#1") && lines[2].contains("读代码"),
            "{:?}",
            lines[2]
        );
        assert!(
            lines[3].contains("#2") && lines[3].contains("写面板"),
            "{:?}",
            lines[3]
        );
        // The rows the panel drew are the rows it asked for: a panel that drew
        // more than it asked for would be drawn over the conversation.
        assert_eq!(height, Height::Hug(4));
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
        assert_eq!(asks(&partial), Height::Hug(4));
        assert!(
            drew(&partial, 60, 10)[1].contains("1 已完成"),
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
        assert_eq!(lines.len(), 5, "{lines:#?}");
        assert!(
            lines[3].starts_with("[•]") && lines[3].contains("b"),
            "{:?}",
            lines[3]
        );
        assert!(lines[4].contains("c"), "{:?}", lines[4]);
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
        assert_eq!(lines.len(), 3, "{lines:#?}");
        assert!(
            lines[2].contains("a") && !lines[2].contains("b"),
            "{:?}",
            lines[2]
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
        assert_eq!(lines.len(), 3, "{lines:#?}");
        assert!(lines[2].contains("b"), "{:?}", lines[2]);
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

    /// Every item's text starts in the same column, however many digits its
    /// number has, and the `+N` line sits under the numbers.
    #[test]
    #[allow(
        clippy::string_slice,
        reason = "test: offset is a `find` on the same line, a char boundary"
    )]
    fn the_text_column_lines_up_across_one_and_two_digit_numbers() {
        let body = serde_json::json!({
            "todos": (0..12)
                .map(|i| serde_json::json!({
                    "content": format!("task {i}"),
                    "status": if i == 0 { "in_progress" } else { "pending" },
                }))
                .collect::<Vec<_>>()
        })
        .to_string();
        let state = fold(&[plan(&body)]);
        let column = |line: &str, needle: &str| {
            crate::width::str_width(&line[..line.find(needle).expect(needle)])
        };

        let all = drew(&state, 60, 40);
        let starts: Vec<usize> = (0..12)
            .map(|i| {
                let line = all
                    .iter()
                    .find(|l| l.ends_with(&format!("task {i}")))
                    .unwrap_or_else(|| panic!("task {i}: {all:#?}"));
                column(line, &format!("task {i}"))
            })
            .collect();
        assert!(
            starts.iter().all(|c| *c == starts[0]),
            "{starts:?}\n{all:#?}"
        );

        // Pulled to the front, the window hides the tail and says so.
        let cut = drew(&state, 60, 8);
        let more = cut.iter().find(|l| l.contains("更多")).expect("a +N line");
        let first = cut.iter().find(|l| l.ends_with("task 0")).expect("task 0");
        assert_eq!(column(more, "+"), column(first, "#"), "{cut:#?}");
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
        // The margin costs one of the eight rows, which is one item fewer than
        // the panel without a margin would show — the trade the margin is.
        let state = fold(&[plan(&build(0))]);
        let lines = drew(&state, 60, 8);
        assert_eq!(lines.len(), 8, "{lines:#?}");
        assert!(
            lines[2].contains("#1") && lines[2].contains("task 0"),
            "{lines:#?}"
        );
        assert!(lines[7].contains("+7 更多"), "{:?}", lines[7]);

        // Nothing is claimed to be missing when everything fits.
        let all = drew(&state, 60, 40);
        assert_eq!(all.len(), 14, "{all:#?}");
        assert!(!all.iter().any(|l| l.contains("更多")), "{all:#?}");

        // Header only, when that is all there is room for. One row cannot seat
        // the margin as well, so the margin stands down and the header keeps the
        // row: at a height that cannot hold both, the words win.
        let one = drew(&state, 60, 1);
        assert_eq!(one.len(), 1, "{one:#?}");
        assert!(one[0].contains("11 待办"), "{:?}", one[0]);
    }
}
