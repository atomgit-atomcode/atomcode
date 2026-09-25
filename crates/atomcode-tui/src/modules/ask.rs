//! The question panel: a question, riding the stream's tail, answered where it
//! is asked.
//!
//! It used to be a modal in the middle of the screen. A question is not a
//! different mode of being — it is the newest thing in the conversation, and it
//! belongs where the newest thing goes: the tail, under the live line and above
//! the words waiting to be sent. Put in a modal it covered the very exchange it
//! was about.
//!
//! Two things follow from riding the tail, and both are the point:
//!
//! - **The composer gets out of the way.** A question is a turn of its own:
//!   there is one thing to answer and no message to type beside it, so the panel
//!   asks for the composer's rows and the composer asks for none — the same
//!   bargain `live` strikes, for the same reason.
//! - **The keyboard has one owner.** Every key goes to the panel while it is up,
//!   which is what the modal already did; what changes is where the answer is
//!   drawn, not who is listening.
//!
//! What a page offers follows what was asked ([`crate::ask::Form`]): a choice is
//! its answers; a model's own question adds a row to type an answer of one's own
//! and a row to talk it over instead; a multiple choice ticks boxes and has a row
//! that sends them; a question that wants words is a line to type on. Several
//! questions asked together are pages behind tabs, with a last page to check the
//! answers on before they go.
//!
//! **Nothing about policy lives here.** Whether a call is asked about is the
//! approval row's business; what an answer *means*, and what each key does, are
//! [`crate::ask`]'s. This draws the sheet and says which row is which.

use crate::ask::{Asked, Form, Sheet, Slot};
use crate::i18n::product::{t as pt, Msg as PMsg};
use crate::i18n::{t, Msg};
use atomcode_harness::seams::ANSWER_ALWAYS;

use crate::caps::{Caps, Glyph};
use crate::frame::{Line, Span, Style};
use crate::module::{Height, View};
use crate::moment::{Moment, Viewport};
use crate::theme::{self, Role};

pub const ID: &str = "ask";

/// Cells a row's own furniture takes before its words: the pointer and the
/// number. Prose is wrapped to what is left, so it lines up with the answers.
const LEAD: usize = 4;

/// The most a page tab's name may take. Past that the tabs of a four-question
/// batch stop fitting a terminal, and the tab that is lit is the one that goes.
const TAB_MOST: usize = 12;

/// Nothing folds.
///
/// The log records answers, never questions: a question is not a fact until it is
/// answered, which is exactly why it lives in [`Moment`] instead. A state struct
/// with nothing in it is the honest shape for that, like `tip` and `steering`.
#[derive(Default)]
pub struct State;

pub struct Ask;

impl View for Ask {
    type State = State;

    fn id() -> &'static str {
        ID
    }

    fn absorb(_state: &mut State, _fact: &atomcode_harness::session::SessionEvent) {}

    fn render(_state: &State, vp: &Viewport<'_>) -> Vec<Line> {
        let Some(sheet) = vp.moment.asking.as_ref() else {
            return Vec::new();
        };
        let w = vp.rect.w as usize;
        if w == 0 || vp.rect.h == 0 {
            return Vec::new();
        }
        let caps = vp.moment.caps;
        layout(sheet, w, vp.rect.h as usize)
            .into_iter()
            .map(|row| draw(sheet, row, caps, w))
            .collect()
    }

    /// What the panel needs, at this width and with this page up.
    ///
    /// Counted by *laying it out* rather than by a formula, because the question
    /// wraps: how many rows one takes is a fact about the width it is asked at,
    /// and a short question is three rows where a long one is eight.
    ///
    /// A box that reports a height it does not then draw is how the modal cut the
    /// end off a long question — see `the_panel_is_as_tall_as_what_it_draws`.
    fn height(_state: &State, moment: &Moment, width: u16) -> Height {
        let Some(sheet) = moment.asking.as_ref() else {
            return Height::Hug(0);
        };
        if width == 0 {
            return Height::Hug(0);
        }
        let rows = layout(sheet, width as usize, usize::MAX).len();
        Height::Hug(rows.min(u16::MAX as usize) as u16)
    }
}

/// What the legend says: the keys this page answers to, and nothing about what
/// an answer *means* — that is the harness's, and the wording of each answer
/// comes from the answerer.
fn legend(sheet: &Sheet) -> Vec<(&'static str, String)> {
    let mut keys = vec![("↑↓", t(Msg::AskLegendChoose).into_owned())];
    if sheet.page().map(Asked::form) == Some(Form::Multiple) {
        keys.push(("space", t(Msg::AskLegendToggle).into_owned()));
    }
    // On a line with words in it the arrows move the caret, and turning the
    // page is Tab's alone — the legend says whichever is true right now.
    if sheet.editing() {
        keys.push(("←→", t(Msg::AskLegendCaret).into_owned()));
    }
    keys.push(("⏎", t(Msg::AskLegendConfirm).into_owned()));
    if sheet.is_batch() {
        let turn = match sheet.editing() {
            true => "tab",
            false => "←→",
        };
        keys.push((turn, t(Msg::AskLegendSwitch).into_owned()));
    }
    keys.push(("esc", pt(PMsg::ApprovalDeny).into_owned()));
    keys
}

/// One row of the panel, before it is drawn.
///
/// The whole layout, and the only one: `render` walks it to draw, [`geometry`]
/// walks it to say which row is which answer, and `height` counts it. A click
/// that landed on one answer and a highlight drawn on another is the failure this
/// shape rules out by construction rather than by keeping two formulas in step.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Row {
    Blank,
    /// The key legend. Its words are not in the row because they are a property
    /// of the page, not of the row — see [`legend`].
    Legend,
    /// A batch's page tabs.
    Tabs,
    /// The line between the answers and the way out of answering.
    Rule,
    /// A line of prose: who is asking, the tool, the call's arguments.
    Text {
        text: String,
        role: Role,
    },
    /// A line of the question itself.
    Prompt(String),
    /// A heading of the panel's own: the review page's.
    Title(String),
    /// A row that can be lit and taken, by index into [`Sheet::slots`].
    Slot(usize),
    /// A line of what an offered answer means, under it.
    Detail(String),
    /// A question, on the review page.
    Asked(String),
    /// Question `i`'s answer, on the review page.
    Recap(usize),
}

/// The rows this sheet makes at this width, cut down to `h`.
///
/// `h` of `usize::MAX` is "how many would it take", which is what `height` asks;
/// the cut only matters once the tail has been rationed and the panel has less
/// room than it asked for.
fn layout(sheet: &Sheet, w: usize, h: usize) -> Vec<Row> {
    if w == 0 || h == 0 {
        return Vec::new();
    }
    let body = w.saturating_sub(LEAD);
    if body == 0 {
        return Vec::new();
    }
    let mut rows = Vec::new();
    if sheet.is_batch() {
        rows.push(Row::Tabs);
    }
    rows.push(Row::Blank);
    match sheet.page() {
        Some(asked) => question_rows(sheet, asked, w, body, h, &mut rows),
        None => review_rows(sheet, &mut rows),
    }
    rows.extend([Row::Blank, Row::Legend]);
    fit(rows, h)
}

/// A question's page: who asks, what, and the rows that answer it.
fn question_rows(
    sheet: &Sheet,
    asked: &Asked,
    w: usize,
    body: usize,
    h: usize,
    rows: &mut Vec<Row>,
) {
    let question = &asked.question;
    // Who is asking. A delegated member's question is not this conversation's,
    // and the person answering is owed the difference — a member's name is the
    // one thing that decides whether an answer is honest.
    if let Some(who) = &question.asker {
        rows.push(Row::Text {
            text: t(Msg::AskFromMember { who }).into_owned(),
            role: Role::Warning,
        });
    }
    let slots = sheet.slots();

    match &question.about {
        // An approval is about a *call*, and what a call does is pulled out of
        // its arguments rather than dumped at the reader as JSON: a thousand
        // lines of `content` in a box someone is reading under time pressure is
        // not something anyone reads, so bulk payloads are measured instead of
        // shown. See [`crate::ask::highlights`].
        Some(about) => {
            rows.push(Row::Text {
                text: about.tool.clone(),
                role: Role::Accent,
            });
            // Keep room for the answers, their blank, and the legend, so a long
            // command (a heredoc) WRAPS into what is left rather than shoving the
            // options off the panel. Whatever fits is the full command; past the
            // budget a single `…` says the rest is there — the exact bytes still
            // execute, this is what the reader is shown of them.
            let keep = slots.len() + 3;
            let budget = h.saturating_sub(rows.len().saturating_add(keep)).max(1);
            let mut used = 0usize;
            let mut clipped = false;
            'outer: for (key, value) in crate::ask::highlights(&about.arguments) {
                let head = match key.is_empty() {
                    true => value,
                    false => format!("{key} {value}"),
                };
                for line in crate::ask::textwrap(&head, body) {
                    if used >= budget {
                        clipped = true;
                        break 'outer;
                    }
                    rows.push(Row::Text {
                        text: line,
                        role: Role::Secondary,
                    });
                    used += 1;
                }
            }
            if clipped {
                rows.push(Row::Text {
                    text: "…".to_string(),
                    role: Role::Muted,
                });
            }
        }
        // Not an approval: the sentence is all there is, and it is the question.
        None => {
            for line in crate::ask::textwrap(&question.prompt, body) {
                rows.push(Row::Prompt(line));
            }
        }
    }

    // The blank between the question and its answers is not optional — losing it
    // runs the first answer into the sentence it answers — so it is pushed with
    // the answers rather than into the salvageable margins below.
    rows.push(Row::Blank);
    let under = w.saturating_sub(detail_indent(sheet)).max(1);
    for (k, slot) in slots.iter().enumerate() {
        if *slot == Slot::Chat {
            rows.push(Row::Rule);
        }
        rows.push(Row::Slot(k));
        if let Slot::Pick(i) = slot {
            if let Some(means) = asked.description(*i) {
                for line in crate::ask::textwrap(means, under) {
                    rows.push(Row::Detail(line));
                }
            }
        }
    }
}

/// A batch's last page: every question with the answer it was given, and the
/// two ways out.
fn review_rows(sheet: &Sheet, rows: &mut Vec<Row>) {
    rows.push(Row::Title(t(Msg::AskReviewTitle).into_owned()));
    rows.push(Row::Blank);
    for (i, asked) in sheet.asked.iter().enumerate() {
        rows.push(Row::Asked(crate::ask::one_line(&asked.question.prompt)));
        rows.push(Row::Recap(i));
    }
    rows.push(Row::Blank);
    rows.push(Row::Text {
        text: t(Msg::AskReviewReady).into_owned(),
        role: Role::Secondary,
    });
    for k in 0..sheet.slots().len() {
        rows.push(Row::Slot(k));
    }
}

/// Cut the layout down to the height it was given, least important row first.
///
/// The order is deliberate: the margin above the question goes, then the legend
/// and its blank, then what the answers mean, then the question's OWN lines (the
/// text between the header and the answers) from the bottom up — and only as a
/// last resort the answers. The answers are the one thing the panel is for; a
/// short terminal that cannot hold a long command AND its options drops command
/// lines, never an option a person still has to pick from. `geometry` fits the
/// same way, so a click still lands on the row it lit.
fn fit(mut rows: Vec<Row>, h: usize) -> Vec<Row> {
    if rows.len() <= h {
        return rows;
    }
    // The margin: first under the tabs, or first of all.
    if let Some(i) = rows.iter().take(2).position(|r| *r == Row::Blank) {
        rows.remove(i);
    }
    if rows.len() > h && rows.last() == Some(&Row::Legend) {
        rows.pop();
        if rows.last() == Some(&Row::Blank) {
            rows.pop();
        }
    }
    while rows.len() > h {
        let Some(cut) = rows.iter().rposition(|r| matches!(r, Row::Detail(_))) else {
            break;
        };
        rows.remove(cut);
    }
    // Still too tall: shed the question's own lines before the answers. Each pass
    // drops the LAST prose row that sits before the first answer — the tail of
    // the command (its `…` marker first, then its bottom lines), keeping the
    // header and the answers. Only when no such line is left does the final
    // truncate reach the answers, which no panel this short could have shown in
    // full.
    while rows.len() > h {
        let first_answer = rows.iter().position(|r| matches!(r, Row::Slot(_)));
        let Some(cut) = first_answer.and_then(|a| {
            rows[..a]
                .iter()
                .rposition(|r| matches!(r, Row::Text { .. } | Row::Prompt(_)))
        }) else {
            break;
        };
        rows.remove(cut);
    }
    rows.truncate(h);
    rows
}

fn draw(sheet: &Sheet, row: Row, caps: Caps, w: usize) -> Line {
    match row {
        Row::Blank => Line::empty(),
        Row::Legend => Line::styled(
            format!("  {}", crate::widget::keys(&legend(sheet), caps)),
            theme::fg(Role::Muted),
        )
        .truncate(w),
        Row::Tabs => tabs_line(sheet, caps, w),
        Row::Rule => Line::styled(
            format!(
                "  {}",
                caps.g(Glyph::Horizontal).repeat(w.saturating_sub(LEAD))
            ),
            theme::fg(Role::Muted),
        )
        .truncate(w),
        Row::Text { text, role } => Line::styled(format!("  {text}"), theme::fg(role)).truncate(w),
        // The question stands out from what surrounds it: a bar down its left and
        // its words in bold — it is the one sentence on the panel that has to be
        // read before anything else is.
        Row::Prompt(text) => Line::from_spans(vec![
            Span::styled(
                format!("{} ", caps.g(Glyph::Vertical)),
                theme::fg(Role::Accent),
            ),
            Span::styled(text, theme::fg(Role::PanelFg).bold()),
        ])
        .truncate(w),
        Row::Title(text) => {
            Line::styled(format!("  {text}"), theme::fg(Role::PanelFg).bold()).truncate(w)
        }
        Row::Slot(k) => slot_line(sheet, k, caps, w),
        Row::Detail(text) => Line::styled(
            format!("{}{text}", " ".repeat(detail_indent(sheet))),
            theme::fg(Role::Muted),
        )
        .truncate(w),
        Row::Asked(text) => Line::from_spans(vec![
            Span::styled(
                format!("  {} ", caps.g(Glyph::Bullet)),
                theme::fg(Role::Muted),
            ),
            Span::styled(text, theme::fg(Role::Secondary)),
        ])
        .truncate(w),
        Row::Recap(i) => {
            let arrow = caps.g(Glyph::Right);
            match sheet.recap(i) {
                Some(said) => Line::styled(format!("    {arrow} {said}"), theme::fg(Role::Success)),
                None => Line::styled(
                    format!("    {arrow} {}", t(Msg::AskUnanswered)),
                    theme::fg(Role::Muted),
                ),
            }
            .truncate(w)
        }
    }
}

/// A batch's tabs: one per question, marked for whether it has its answer, and
/// the review page's last. The page that is up is drawn reversed.
fn tabs_line(sheet: &Sheet, caps: Caps, w: usize) -> Line {
    let lit = |on: bool| match on {
        true => theme::fg(Role::Accent).reverse(),
        false => theme::fg(Role::Secondary),
    };
    let mut spans = vec![Span::styled(
        format!("{} ", caps.g(Glyph::Left)),
        theme::fg(Role::Muted),
    )];
    for (i, asked) in sheet.asked.iter().enumerate() {
        let answered = sheet.drafts.get(i).is_some_and(|d| d.answer.is_some());
        let mark = caps.g(match answered {
            true => Glyph::Checked,
            false => Glyph::Unchecked,
        });
        let name = crate::width::take_width(&asked.title(), TAB_MOST);
        spans.push(Span::styled(
            format!(" {mark} {name} "),
            lit(i == sheet.tab),
        ));
        spans.push(Span::raw(" "));
    }
    spans.push(Span::styled(
        format!(" {} {} ", caps.g(Glyph::Ok), t(Msg::AskSubmit)),
        lit(sheet.reviewing()),
    ));
    spans.push(Span::styled(
        format!(" {}", caps.g(Glyph::Right)),
        theme::fg(Role::Muted),
    ));
    Line::from_spans(spans).truncate(w)
}

/// How wide the number column is on this page: the widest number and its
/// stop, and a space. One width for every row, so the words line up.
fn number_width(sheet: &Sheet) -> usize {
    let most = (0..sheet.slots().len())
        .filter_map(|k| sheet.number(k))
        .max()
        .unwrap_or(1);
    most.to_string().len() + 2
}

/// Where the lines under an answer start: under the answer's own words.
fn detail_indent(sheet: &Sheet) -> usize {
    let boxes = match sheet.page().map(Asked::form) {
        Some(Form::Multiple) => 2,
        _ => 0,
    };
    2 + number_width(sheet) + boxes
}

/// One row that can be taken, lit when it is the one pointed at.
fn slot_line(sheet: &Sheet, k: usize, caps: Caps, w: usize) -> Line {
    let slots = sheet.slots();
    let Some(&slot) = slots.get(k) else {
        return Line::empty();
    };
    let here = sheet.cursor() == k;
    // The pointed-at row is the same panel one step brighter, not a reversed
    // video bar — see `theme::Role::PanelSelBg` for why, and `menu::Menu::render`
    // for the same choice made for the same reason.
    let style = match here {
        true => theme::bg(Role::PanelSelBg).under(theme::fg(Role::PanelFg)),
        false => Style::new(),
    };
    let quiet = theme::fg(Role::Muted).under(style);
    let mut spans = vec![Span::styled(
        match here {
            true => format!("{} ", caps.g(Glyph::Prompt)),
            false => "  ".to_string(),
        },
        theme::fg(Role::Accent).under(style),
    )];
    let number = sheet.number(k).map(|n| format!("{n}.")).unwrap_or_default();
    spans.push(Span::styled(
        format!("{number:<width$}", width = number_width(sheet)),
        style,
    ));

    let asked = sheet.page();
    let draft = sheet.draft();
    if asked.map(Asked::form) == Some(Form::Multiple) {
        let ticked = match slot {
            Slot::Pick(i) => {
                Some(draft.is_some_and(|d| d.checked.get(i).copied().unwrap_or(false)))
            }
            Slot::Other => Some(draft.is_some_and(|d| !d.typed.trim().is_empty())),
            _ => None,
        };
        if let Some(ticked) = ticked {
            let mark = match ticked {
                true => Glyph::Checked,
                false => Glyph::Unchecked,
            };
            spans.push(Span::styled(format!("{} ", caps.g(mark)), style));
        }
    }

    match slot {
        Slot::Pick(i) => {
            let Some(answer) = asked.and_then(|a| a.question.options.get(i)) else {
                return Line::from_spans(spans).truncate(w);
            };
            spans.push(Span::styled(
                crate::ask::answer_label(&answer.value, &answer.label),
                style,
            ));
            // What "always" would actually cover. A person saying it is owed the
            // scope they are saying it to — and "every call of this tool" is a
            // very different promise from "this one command".
            if answer.value == ANSWER_ALWAYS {
                let grant = asked
                    .and_then(|a| a.question.about.as_ref())
                    .and_then(|a| a.grant.as_deref());
                if let Some(grant) = grant {
                    let covers = match grant.trim().is_empty() {
                        true => t(Msg::AskGrantWholeTool).into_owned(),
                        false => t(Msg::AskGrantOnly {
                            what: &crate::ask::one_line(grant),
                        })
                        .into_owned(),
                    };
                    spans.push(Span::styled(
                        format!("  {covers}"),
                        match here {
                            true => style,
                            false => theme::fg(Role::Muted),
                        },
                    ));
                }
            }
        }
        Slot::Other | Slot::Input => {
            let typed = draft.map(|d| d.typed.as_str()).unwrap_or("");
            // The caret is a reversed cell at the end of what was typed, drawn
            // only on the row that is taking the keys.
            let caret = || Span::styled(" ", Style::new().reverse());
            if typed.is_empty() {
                if here {
                    spans.push(caret());
                }
                spans.push(Span::styled(t(Msg::AskTypeSomething).into_owned(), quiet));
            } else if here {
                let used: usize = spans.iter().map(Span::width).sum();
                let at = draft.map_or(typed.len(), |d| d.caret);
                spans.extend(caret_in(typed, at, style, w.saturating_sub(used)));
            } else {
                // The end of what was typed is the part being worked on, so a
                // line too long for the row shows its tail, not its head.
                let used: usize = spans.iter().map(Span::width).sum();
                spans.push(Span::styled(
                    crate::width::take_width_from_end(typed, w.saturating_sub(used)),
                    style,
                ));
            }
        }
        Slot::Submit => spans.push(Span::styled(
            match sheet.is_batch() {
                true => t(Msg::AskNext),
                false => t(Msg::AskSubmit),
            }
            .into_owned(),
            style,
        )),
        Slot::Chat => spans.push(Span::styled(t(Msg::AskChatInstead).into_owned(), style)),
        Slot::Send => spans.push(Span::styled(t(Msg::AskReviewSend).into_owned(), style)),
        Slot::Cancel => spans.push(Span::styled(t(Msg::AskReviewCancel).into_owned(), style)),
    }
    // Filled to the rect, so the highlight is a band across the row rather than a
    // patch behind the words: the pointed-at row is a surface, and a surface that
    // stops at its last letter is a smudge.
    pad(Line::from_spans(spans), w, style)
}

/// What was typed, with the caret on the cell it is at, kept in `room` cells.
///
/// The caret is drawn the way every other field on this screen draws it
/// ([`crate::modules::chrome::caret_spans`]). A line longer than the room
/// scrolls so the caret stays on screen: as much of what comes before it as
/// fits — counted in cells, so a wide character is two — and what comes after
/// is cut at the edge.
fn caret_in(typed: &str, at: usize, style: Style, room: usize) -> Vec<Span> {
    let at = crate::text::snap(typed, at);
    let (before, rest) = typed.split_at(at);
    // The caret's own cell: the character it sits on, or one blank past the end.
    let on = rest.chars().next().map_or(1, crate::width::char_width);
    let fits = room.saturating_sub(on);
    let shown = match crate::width::str_width(before) <= fits {
        true => before.to_string(),
        false => crate::width::take_width_from_end(before, fits),
    };
    let line = format!("{shown}{rest}");
    crate::modules::chrome::caret_spans(&line, shown.len(), style, room)
}

fn pad(line: Line, w: usize, style: Style) -> Line {
    let used = line.width();
    if used >= w {
        return line.truncate(w);
    }
    let mut spans = line.spans;
    spans.push(Span::styled(" ".repeat(w - used), style));
    Line::from_spans(spans).truncate(w)
}

/// Which screen row each answer is on, for a click to read.
///
/// Built by the same [`layout`] the frame used, so a click and the row it lights
/// up cannot come from two different arrangements of one panel.
pub struct Geometry {
    /// One entry per drawn row, top to bottom: the row of [`Sheet::slots`] on
    /// it, when it holds one. `None` for a row that is prose, blank, the legend —
    /// or an answer the rect was too short to reach.
    rows: Vec<Option<usize>>,
}

impl Geometry {
    /// Which answer is on this screen row.
    ///
    /// `row` is measured from the top of the panel's rect, which is what the host
    /// knows and what a click carries.
    pub fn answer_at(&self, row: usize) -> Option<usize> {
        self.rows.get(row).copied().flatten()
    }
}

/// The layout the panel drew, for the host to read a click against.
pub fn geometry(sheet: &Sheet, vp: &Viewport<'_>) -> Geometry {
    let rows = if vp.rect.w == 0 || vp.rect.h == 0 {
        Vec::new()
    } else {
        layout(sheet, vp.rect.w as usize, vp.rect.h as usize)
    };
    Geometry {
        rows: rows
            .iter()
            .map(|r| match r {
                Row::Slot(k) => Some(*k),
                _ => None,
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::Rect;
    use crate::module::{Mounted, ViewObject};
    use crate::moment::Ask as MomentAsk;
    use atomcode_capabilities::tools::request_user_input::UserInputRequest;
    use atomcode_harness::seams::{
        AboutCall, Answer, Question as Q, ANSWER_ALLOW, ANSWER_ALWAYS, ANSWER_DENY,
    };

    crate::tui_conformance!(view Ask as ask_conformance);

    fn approval(asker: Option<&str>, tool: &str, args: &str, grant: Option<&str>) -> Q {
        Q {
            prompt: format!("Allow `{tool}` to run?"),
            options: vec![
                Answer::labelled(ANSWER_ALLOW, "allow once"),
                Answer::labelled(ANSWER_ALWAYS, "always allow"),
                Answer::labelled(ANSWER_DENY, "deny"),
            ],
            asker: asker.map(str::to_string),
            about: Some(AboutCall {
                tool: tool.into(),
                arguments: args.into(),
                grant: grant.map(str::to_string),
            }),
        }
    }

    /// A model's own question, as the wire brings it.
    fn model(payload: serde_json::Value) -> Asked {
        let request: UserInputRequest = serde_json::from_value(payload).expect("a request");
        crate::ask::question_for(
            atomcode_capabilities::tools::request_user_input::REQUEST_USER_INPUT_KIND,
            &serde_json::to_value(request).unwrap(),
            &[],
        )
        .expect("drawn")
    }

    fn multiple() -> Asked {
        model(serde_json::json!({
            "header": "语言",
            "question": "要支持哪些语言?",
            "mode": "multiple",
            "options": [
                { "label": "Python", "description": "脚本和胶水代码都用它" },
                { "label": "Rust" },
                { "label": "Go" },
            ],
        }))
    }

    fn text() -> Asked {
        model(serde_json::json!({
            "header": "名字",
            "question": "新仓库叫什么?",
            "mode": "text",
        }))
    }

    /// A moment with this question up and `cursor` pointed at it.
    fn asking(question: Q, cursor: usize) -> Moment {
        let mut sheet = MomentAsk::one(question);
        sheet.point_at(cursor);
        Moment {
            asking: Some(sheet),
            ..Moment::default()
        }
    }

    fn with(sheet: Sheet) -> Moment {
        Moment {
            asking: Some(sheet),
            ..Moment::default()
        }
    }

    fn drawn(moment: &Moment, w: u16, h: u16) -> Vec<String> {
        let vp = Viewport::new(Rect::sized(w, h), moment);
        Ask::render(&State, &vp)
            .iter()
            .map(|l| l.plain().trim_end().to_string())
            .collect()
    }

    /// The panel as the host lays it out: asked for its height, then given
    /// exactly that many rows. What the tail does.
    fn framed(moment: &Moment, w: u16) -> Vec<String> {
        let h = match Ask::height(&State, moment, w) {
            Height::Hug(n) | Height::Fixed(n) => n,
            Height::Fill => unreachable!("the panel never fills"),
        };
        drawn(moment, w, h)
    }

    fn press(sheet: &mut Sheet, key: crate::surface::Key) {
        let _ = sheet.key(crate::surface::KeyPress::plain(key));
    }

    #[test]
    fn nothing_asked_is_not_a_panel() {
        // The row the composer keeps: a panel that sat there saying "no question"
        // would be chrome on a screen that is mostly conversation.
        assert!(drawn(&Moment::default(), 40, 6).is_empty());
        assert_eq!(Ask::height(&State, &Moment::default(), 80), Height::Hug(0));
    }

    /// The failure the modal had: a box that reported a height it did not then
    /// draw, so the frame cut the end off the question. Long prompts are the
    /// case, and a member's question with a wrapped sentence is the longest there
    /// is.
    #[test]
    fn the_panel_is_as_tall_as_what_it_draws() {
        let long = Q::plain(
            "要把 request_user_input 整条链路退役,还是只把它的面板拆干净?前者要动 ACP 的 \
             elicitation 映射与四个 UserQuestions 实现,后者只碰 ask.rs;两条路的影响面差一个 \
             数量级,我需要你先定这一条。",
            &["只拆面板", "退役整条链路", "先看一下再定"],
        );
        for w in [30u16, 60, 120] {
            let moment = asking(long.clone(), 0);
            let drew = framed(&moment, w).len();
            let claimed = match Ask::height(&State, &moment, w) {
                Height::Hug(n) => n as usize,
                other => panic!("unexpected {other:?}"),
            };
            assert_eq!(
                drew, claimed,
                "at width {w} the panel drew {drew} rows and claimed {claimed}"
            );
            // And nothing of the question was dropped to get there.
            let joined = framed(&moment, w).join("\n");
            assert!(
                joined.contains("先定这一条"),
                "the end of the question is on screen at width {w}:\n{joined}"
            );
        }
    }

    /// Every page of every kind of question, at every width a terminal might
    /// have: as tall as it says and never wider than its rect. The pages a model's
    /// question adds — the typing row, the boxes, the tabs, the review page — are
    /// the ones with new furniture on them, and furniture is what overruns.
    #[test]
    fn every_page_is_as_tall_as_it_says_and_no_wider_than_its_rect() {
        let mut typed = Sheet::one(text());
        typed.type_text(&"很长的名字".repeat(20));
        let mut ticked = Sheet::one(multiple());
        press(&mut ticked, crate::surface::Key::Char(' '));
        let mut batch = Sheet::new(1, vec![multiple(), text(), multiple(), text()]);
        let first = batch.clone();
        batch.tab = batch.asked.len();
        let sheets = [
            Sheet::one(approval(
                Some("scribe"),
                "bash",
                r#"{"command":"ls"}"#,
                Some(""),
            )),
            Sheet::one(multiple()),
            ticked,
            Sheet::one(text()),
            typed,
            first,
            batch,
        ];
        for sheet in sheets {
            let moment = with(sheet.clone());
            for w in [8u16, 12, 20, 33, 60, 120] {
                let claimed = match Ask::height(&State, &moment, w) {
                    Height::Hug(n) => n,
                    other => panic!("unexpected {other:?}"),
                };
                let vp = Viewport::new(Rect::sized(w, claimed), &moment);
                let lines = Ask::render(&State, &vp);
                assert_eq!(lines.len(), claimed as usize, "at width {w}: {sheet:?}");
                for line in &lines {
                    assert!(
                        line.width() <= w as usize,
                        "{} cells at width {w}: {:?}",
                        line.width(),
                        line.plain()
                    );
                }
            }
        }
    }

    /// A height the tail rationed away takes the legend and the margin, never the
    /// answers: a panel that says nothing and explains how to work it is worse
    /// than a short one.
    #[test]
    fn a_short_rect_loses_the_legend_before_it_loses_the_answers() {
        // A plain question, so the rows are easy to count: one for the prompt,
        // one blank, one per answer, then the legend's own blank and the legend.
        let question = Q::plain("Keep going?", &["yes", "no", "maybe"]);
        let full = framed(&asking(question.clone(), 0), 60);
        assert!(full.join("\n").contains("选择"), "the legend is up");

        // Room for the prompt, the gap, and every answer — and nothing else.
        let tight = drawn(&asking(question.clone(), 0), 60, 5);
        let tight = tight.join("\n");
        assert!(!tight.contains("选择"), "the legend went first:\n{tight}");
        for answer in &question.options {
            assert!(
                tight.contains(&crate::ask::answer_label(&answer.value, &answer.label)),
                "every answer is still up:\n{tight}"
            );
        }
        // And the gap between the question and its answers survives: running the
        // first answer into the sentence it answers is what that row is for.
        let rows = layout(&Sheet::one(question), 60, 5);
        assert_eq!(
            rows.iter().position(|r| matches!(r, Row::Slot(0))),
            rows.iter()
                .position(|r| matches!(r, Row::Prompt(_)))
                .map(|p| p + 2),
            "{rows:?}"
        );
    }

    #[test]
    fn a_short_rect_sheds_the_command_lines_before_the_answers() {
        // An approval whose command is a long heredoc, at a height too short to
        // hold the whole command AND its options: the command's own lines are what
        // go — never an answer, because a person cannot pick an option that was
        // truncated off the panel.
        let cmd = (0..40)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let args = serde_json::json!({ "command": cmd }).to_string();
        let question = approval(None, "bash (writes outside the workspace)", &args, Some(""));
        let rows = layout(&Sheet::one(question.clone()), 40, 5);
        assert!(rows.len() <= 5, "fits the height: {rows:?}");
        assert_eq!(
            rows.iter().filter(|r| matches!(r, Row::Slot(_))).count(),
            question.options.len(),
            "every option survives the squeeze: {rows:?}"
        );
    }

    /// The row a click lands on is the row the frame drew. Built from one layout,
    /// so a click and a highlight cannot come from two arrangements.
    ///
    /// The legend carries the word 「拒绝」 for the esc key, so "does this row
    /// mention an answer" is not a test that can be written by substring. What is
    /// checked instead is the invariant the two consumers share: one row per
    /// layout row, the answers where the layout says they are, and nothing else
    /// reading as one.
    #[test]
    fn a_click_reads_the_row_the_frame_drew() {
        let question = approval(None, "write_file", r#"{"file_path":"notes.md"}"#, None);
        let moment = asking(question.clone(), 0);
        let sheet = moment.asking.clone().unwrap();
        let w = 60u16;
        let h = match Ask::height(&State, &moment, w) {
            Height::Hug(n) => n,
            other => panic!("unexpected {other:?}"),
        };
        let vp = Viewport::new(Rect::sized(w, h), &moment);
        let geom = geometry(&sheet, &vp);
        let lines = Ask::render(&State, &vp)
            .iter()
            .map(|l| l.plain().trim_end().to_string())
            .collect::<Vec<_>>();
        let rows = layout(&sheet, w as usize, h as usize);

        assert_eq!(
            rows.len(),
            lines.len(),
            "one drawn line per laid-out row:\n{}",
            lines.join("\n")
        );
        for (row, (laid, line)) in rows.iter().zip(&lines).enumerate() {
            match laid {
                Row::Slot(i) => {
                    assert_eq!(geom.answer_at(row), Some(*i), "row {row}: {line:?}");
                    assert!(
                        line.contains(&crate::ask::answer_label(
                            &question.options[*i].value,
                            &question.options[*i].label
                        )),
                        "row {row} says answer {i} but draws {line:?}"
                    );
                }
                _ => assert_eq!(
                    geom.answer_at(row),
                    None,
                    "row {row} is not an answer: {line:?}"
                ),
            }
        }
        // Every answer is reachable, and by the row it is on.
        for i in 0..question.options.len() {
            let row = (0..lines.len())
                .find(|r| geom.answer_at(*r) == Some(i))
                .unwrap_or_else(|| panic!("answer {i} has no row"));
            assert_eq!(geom.answer_at(row), Some(i));
        }
    }

    /// Only one row is pointed at, and it is the one a confirm takes.
    #[test]
    fn exactly_one_answer_is_lit_and_it_is_the_one_a_confirm_takes() {
        let question = approval(None, "write_file", "{}", None);
        let pointer = Caps::default().g(Glyph::Prompt);
        for cursor in 0..question.options.len() {
            let moment = asking(question.clone(), cursor);
            let lines = framed(&moment, 60);
            let lit: Vec<&String> = lines.iter().filter(|l| l.starts_with(pointer)).collect();
            assert_eq!(
                lit.len(),
                1,
                "one lit row for cursor {cursor}:\n{}",
                lines.join("\n")
            );
            let a = &question.options[cursor];
            assert!(
                lit[0].contains(&crate::ask::answer_label(&a.value, &a.label)),
                "the lit row is the pointed-at answer: {lit:?}"
            );
            let mut sheet = moment.asking.clone().unwrap();
            assert_eq!(
                sheet.enter(),
                crate::ask::Step::Deliver(vec![Some(crate::ask::Reply::from(a.value.as_str()))]),
                "and a confirm takes it"
            );
        }
    }

    /// Who is asking belongs on the panel: a member's question is not the
    /// conversation's, and the person answering is owed the difference.
    #[test]
    fn the_panel_names_the_member_that_is_asking() {
        let mine = framed(&asking(approval(None, "write_file", "{}", None), 0), 60).join("\n");
        assert!(!mine.contains("来自成员"), "{mine}");
        let theirs = framed(
            &asking(approval(Some("scribe"), "write_file", "{}", None), 0),
            60,
        )
        .join("\n");
        assert!(theirs.contains("scribe"), "{theirs}");
    }

    /// What "always" would cover, shown where the person says it.
    #[test]
    fn always_says_what_it_would_cover() {
        let nothing = framed(
            &asking(approval(None, "bash", r#"{"command":"ls"}"#, None), 0),
            60,
        )
        .join("\n");
        assert!(!nothing.contains("仅限"), "{nothing}");

        let command = framed(
            &asking(
                approval(None, "bash", r#"{"command":"rm -rf /"}"#, Some("rm -rf /")),
                0,
            ),
            60,
        )
        .join("\n");
        assert!(command.contains("仅限 rm -rf /"), "{command}");

        let whole = framed(&asking(approval(None, "write_file", "{}", Some("")), 0), 60).join("\n");
        assert!(whole.contains("这个工具的全部调用"), "{whole}");
    }

    /// An approval shows the call, not its JSON — the panel is read under time
    /// pressure, and a thousand lines of `content` is not something anyone reads.
    #[test]
    fn the_panel_shows_what_the_call_does_not_its_json() {
        let moment = asking(
            approval(
                None,
                "write_file",
                r#"{"file_path":"notes.md","content":"x\ny\nz"}"#,
                None,
            ),
            0,
        );
        let out = framed(&moment, 60).join("\n");
        assert!(out.contains("write_file"), "{out}");
        assert!(out.contains("notes.md"), "{out}");
        assert!(!out.contains("\"file_path\""), "not dumped as JSON:\n{out}");
        assert!(
            out.contains("3 行"),
            "a bulk payload is measured, not shown:\n{out}"
        );
        // An approval is a choice between its answers: nothing to type, nothing
        // to talk over — those are a model's question's, not a gate's.
        assert!(!out.contains("自己输入"), "{out}");
        assert!(!out.contains("改为直接对话"), "{out}");
    }

    /// The legend is the panel's keys: the same three on every choice, and the
    /// ones a page adds where it adds them.
    #[test]
    fn the_legend_is_one_line_and_says_the_keys() {
        let out = framed(&asking(approval(None, "write_file", "{}", None), 0), 60).join("\n");
        assert!(out.contains("↑↓ 选择"), "{out}");
        assert!(out.contains("⏎ 确认"), "{out}");
        assert!(out.contains("esc 拒绝"), "{out}");
        assert!(
            !out.contains("space"),
            "nothing to tick on a choice:\n{out}"
        );

        let boxes = framed(&with(Sheet::one(multiple())), 60).join("\n");
        assert!(boxes.contains("space 勾选"), "{boxes}");
        let pages = framed(&with(Sheet::new(1, vec![text(), multiple()])), 80).join("\n");
        assert!(pages.contains("←→ 切换题目"), "{pages}");
    }

    /// A model's multiple choice is a box per answer, what each answer means
    /// under it, a row of one's own words, a row that sends them, and a way out
    /// of answering below a line — and a ticked box looks ticked.
    #[test]
    fn a_multiple_choice_draws_a_box_per_answer_and_a_row_that_sends_them() {
        let mut sheet = Sheet::one(multiple());
        let on = Caps::default().g(Glyph::Checked);
        let off = Caps::default().g(Glyph::Unchecked);
        let before = framed(&with(sheet.clone()), 60);
        let row = |lines: &[String], word: &str| {
            lines
                .iter()
                .find(|l| l.contains(word))
                .cloned()
                .unwrap_or_else(|| panic!("no `{word}` row:\n{}", lines.join("\n")))
        };
        assert!(
            row(&before, "Python").contains(&format!("1. {off} Python")),
            "{before:?}"
        );
        let means = before
            .iter()
            .position(|l| l.contains("脚本和胶水代码都用它"))
            .expect("what an answer means is drawn");
        assert!(
            before[means - 1].contains("Python"),
            "under the answer it explains"
        );
        assert!(
            !before[means].contains("Python"),
            "on a line of its own: {before:?}"
        );
        assert!(
            row(&before, "自己输入").contains(&format!("4. {off}")),
            "{before:?}"
        );
        assert!(before.iter().any(|l| l.trim() == "提交"), "{before:?}");
        let chat = before
            .iter()
            .position(|l| l.contains("改为直接对话"))
            .unwrap();
        assert!(
            before[chat - 1].contains(Caps::default().g(Glyph::Horizontal)),
            "the way out sits below a line: {before:?}"
        );
        assert!(row(&before, "改为直接对话").contains("5."), "{before:?}");

        press(&mut sheet, crate::surface::Key::Char(' '));
        let after = framed(&with(sheet), 60);
        assert!(
            row(&after, "Python").contains(&format!("1. {on} Python")),
            "{after:?}"
        );
        assert!(
            row(&after, "Rust").contains(&format!("2. {off} Rust")),
            "{after:?}"
        );
    }

    /// A question that wants words is a line to type on — not a yes and a no.
    #[test]
    fn a_text_question_is_a_line_to_type_on() {
        let mut sheet = Sheet::one(text());
        let empty = framed(&with(sheet.clone()), 60).join("\n");
        assert!(empty.contains("新仓库叫什么"), "{empty}");
        assert!(empty.contains("1. "), "{empty}");
        assert!(
            empty.contains("自己输入"),
            "an empty line says what it is for:\n{empty}"
        );
        for no in ["好", "不了", "yes"] {
            assert!(!empty.contains(no), "no `{no}` to pick:\n{empty}");
        }
        sheet.type_text("atomcode-lab");
        let typed = framed(&with(sheet), 60).join("\n");
        assert!(typed.contains("atomcode-lab"), "{typed}");
        assert!(!typed.contains("自己输入"), "{typed}");
    }

    /// Several questions put together are pages behind tabs, marked as they are
    /// answered, with a review page at the end that lists every answer.
    #[test]
    fn a_batch_draws_tabs_and_a_page_to_review_the_answers_on() {
        use crate::surface::Key;
        let mut sheet = Sheet::new(7, vec![multiple(), text()]);
        let on = Caps::default().g(Glyph::Checked);
        let off = Caps::default().g(Glyph::Unchecked);
        let tabs = framed(&with(sheet.clone()), 80)[0].clone();
        assert!(tabs.contains(&format!("{off} 语言")), "{tabs}");
        assert!(tabs.contains(&format!("{off} 名字")), "{tabs}");
        assert!(
            tabs.contains("提交"),
            "the review page is the last tab: {tabs}"
        );

        // Tick Rust, send it, and the page turns to the next question.
        press(&mut sheet, Key::Down);
        press(&mut sheet, Key::Char(' '));
        press(&mut sheet, Key::Down);
        press(&mut sheet, Key::Down);
        press(&mut sheet, Key::Down);
        assert_eq!(sheet.pointed(), Some(Slot::Submit));
        let next = framed(&with(sheet.clone()), 80);
        assert!(next.iter().any(|l| l.contains("下一题")), "{next:?}");
        press(&mut sheet, Key::Enter);
        assert_eq!(sheet.tab, 1, "on to the next question");
        let turned = framed(&with(sheet.clone()), 80);
        assert!(turned[0].contains(&format!("{on} 语言")), "{turned:?}");
        assert!(turned.join("\n").contains("新仓库叫什么"), "{turned:?}");

        // Skip the second to the review page: the answer given, and the one not.
        press(&mut sheet, Key::Right);
        assert!(sheet.reviewing());
        let review = framed(&with(sheet), 80).join("\n");
        assert!(review.contains("核对你的回答"), "{review}");
        assert!(review.contains("要支持哪些语言"), "{review}");
        assert!(review.contains("Rust"), "{review}");
        assert!(review.contains("（未回答）"), "{review}");
        assert!(review.contains("确认提交这些回答吗"), "{review}");
        assert!(review.contains("1. 提交回答"), "{review}");
        assert!(review.contains("2. 取消"), "{review}");
    }

    /// `point_at` clamps rather than rejecting: a pointer on the padding, or an
    /// arrow pressed past the end, means the nearest answer.
    #[test]
    fn pointing_clamps_to_the_answers_there_are() {
        let question = approval(None, "write_file", "{}", None);
        let last = question.options.len() - 1;
        let mut ask = MomentAsk::one(question);
        assert_eq!(ask.cursor(), 0, "the first answer starts lit");
        assert!(ask.point_at(2));
        assert_eq!(ask.cursor(), 2);
        assert!(!ask.point_at(2), "pointing where it already is is not news");

        // Past the end is the last answer — and is news only if that moves it.
        ask.point_at(0);
        assert!(
            ask.point_at(99),
            "past the end is a move to the last answer"
        );
        assert_eq!(ask.cursor(), last);
        assert!(!ask.point_at(99), "and it stays there");
    }

    /// A module that named its own id differently would fail `every_module_is_covered`
    /// rather than silently lose the whole suite.
    #[test]
    fn the_row_id_and_the_module_id_are_one_string() {
        assert_eq!(Ask::id(), ID);
        assert_eq!(Mounted::<Ask>::new().id(), ID);
    }

    /// The typing row as drawn: which cell the caret is on (the reversed one),
    /// counted in cells from the left edge, and what it covers.
    fn caret_cell(sheet: &Sheet, w: u16) -> (usize, String) {
        let moment = with(sheet.clone());
        let h = match Ask::height(&State, &moment, w) {
            Height::Hug(n) => n,
            other => panic!("unexpected {other:?}"),
        };
        let vp = Viewport::new(Rect::sized(w, h), &moment);
        let lines = Ask::render(&State, &vp);
        let row = lines
            .iter()
            .find(|l| l.spans.iter().any(|s| s.style.reverse))
            .unwrap_or_else(|| panic!("no caret drawn: {lines:?}"));
        assert!(
            row.width() <= w as usize,
            "{} cells at width {w}",
            row.width()
        );
        let at = row.spans.iter().position(|s| s.style.reverse).unwrap();
        let column = row.spans[..at].iter().map(Span::width).sum();
        (column, row.spans[at].text.clone())
    }

    /// The caret is drawn on the cell it is at — after two wide characters that
    /// is four cells along, not two — and a line too long for its row scrolls
    /// so the caret stays on screen at either end.
    #[test]
    fn the_caret_is_drawn_on_the_cell_it_is_at() {
        use crate::surface::Key;
        let mut sheet = Sheet::one(text());
        sheet.type_text("中文字");
        press(&mut sheet, Key::Left);
        let (column, on) = caret_cell(&sheet, 40);
        let (end, _) = {
            let mut at_end = sheet.clone();
            press(&mut at_end, Key::End);
            caret_cell(&at_end, 40)
        };
        assert_eq!(on, "字", "the caret covers the character it is before");
        assert_eq!(end - column, 2, "and 字 is two cells wide");
        press(&mut sheet, Key::Home);
        let (start, on) = caret_cell(&sheet, 40);
        assert_eq!(on, "中");
        assert_eq!(column - start, 4, "two wide characters are four cells");

        let mut long = Sheet::one(text());
        long.type_text(&format!("{}终点", "起".repeat(60)));
        for w in [20u16, 33] {
            let (column, on) = caret_cell(&long, w);
            assert_eq!(on, " ", "at the end, a block past the last character");
            assert!(column < w as usize, "and still on screen at width {w}");
            let mut home = long.clone();
            press(&mut home, Key::Home);
            assert_eq!(
                caret_cell(&home, w).1,
                "起",
                "the start scrolls back into view"
            );
        }
    }

    /// The legend follows what the arrows do right now: move the caret on a
    /// line with words in it, turn the page otherwise — and then Tab is named
    /// for turning the page.
    #[test]
    fn the_legend_says_what_the_arrows_do_now() {
        let mut one = Sheet::one(text());
        let empty = framed(&with(one.clone()), 80).join("\n");
        assert!(!empty.contains("移动光标"), "{empty}");
        one.type_text("ab");
        let editing = framed(&with(one), 80).join("\n");
        assert!(editing.contains("←→ 移动光标"), "{editing}");

        let mut batch = Sheet::new(1, vec![text(), multiple()]);
        let turning = framed(&with(batch.clone()), 100).join("\n");
        assert!(turning.contains("←→ 切换题目"), "{turning}");
        batch.type_text("ab");
        let typing = framed(&with(batch), 100).join("\n");
        assert!(typing.contains("←→ 移动光标"), "{typing}");
        assert!(typing.contains("tab 切换题目"), "{typing}");
    }
}
