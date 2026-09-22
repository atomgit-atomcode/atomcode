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
//! **Nothing about policy lives here.** Whether a call is asked about is the
//! approval row's business; what an answer *means* is [`crate::ask`]'s. This
//! draws a question and says which row is pointed at.

use crate::i18n::product::{t as pt, Msg as PMsg};
use crate::i18n::{t, Msg};
use atomcode_harness::seams::{Question, ANSWER_ALWAYS};

use crate::frame::{Line, Span, Style};
use crate::module::{Height, View};
use crate::moment::{Moment, Viewport};
use crate::theme::{self, Role};

pub const ID: &str = "ask";

/// Cells an answer's own furniture takes: the pointer and the number.
///
/// Constant across rows on purpose — an answer that starts in one column when it
/// is pointed at and another when it is not is a stack that twitches as the
/// cursor moves down it.
const LEAD: usize = 4;

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
        let Some(ask) = vp.moment.asking.as_ref() else {
            return Vec::new();
        };
        let w = vp.rect.w as usize;
        if w == 0 || vp.rect.h == 0 {
            return Vec::new();
        }
        let caps = vp.moment.caps;
        let pointer = caps.g(crate::caps::Glyph::Pointer);
        layout(&ask.question, w, vp.rect.h as usize)
            .into_iter()
            .map(|row| match row {
                Row::Blank => Line::empty(),
                Row::Legend => Line::styled(
                    format!("  {}", crate::widget::keys(&legend(), caps)),
                    theme::fg(Role::Muted),
                )
                .truncate(w),
                Row::Text { text, role } => {
                    Line::styled(format!("  {text}"), theme::fg(role)).truncate(w)
                }
                Row::Answer(i) => answer_line(&ask.question, i, ask.cursor == i, pointer, w),
            })
            .collect()
    }

    /// What the panel needs, at this width and with this question in it.
    ///
    /// Counted by *laying it out* rather than by a formula, because the question
    /// wraps: how many rows one takes is a fact about the width it is asked at,
    /// and a short question is three rows where a long one is eight.
    ///
    /// A box that reports a height it does not then draw is how the modal cut the
    /// end off a long question — see `the_panel_is_as_tall_as_what_it_draws`.
    fn height(_state: &State, moment: &Moment, width: u16) -> Height {
        let Some(ask) = moment.asking.as_ref() else {
            return Height::Hug(0);
        };
        if width == 0 {
            return Height::Hug(0);
        }
        let rows = layout(&ask.question, width as usize, usize::MAX).len();
        Height::Hug(rows.min(u16::MAX as usize) as u16)
    }
}

/// What the legend says.
///
/// One line, and the same one whether the question is an approval or not: the
/// keys are the screen's business and they do not change with the question. What
/// an answer *means* is not in here — that is the harness's, and the wording of
/// each answer comes from the answerer.
fn legend() -> [(&'static str, String); 3] {
    [
        ("↑↓", t(Msg::AskLegendChoose).into_owned()),
        ("⏎", t(Msg::AskLegendConfirm).into_owned()),
        ("esc", pt(PMsg::ApprovalDeny).into_owned()),
    ]
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
    /// The key legend. Its words are not in the row because they are not a
    /// property of the question — see [`legend`].
    Legend,
    /// A line of prose: who is asking, the tool, the call's arguments, the
    /// question itself.
    Text {
        text: String,
        role: Role,
    },
    /// An answer, by index into `question.options`.
    Answer(usize),
}

/// The rows this question makes at this width, cut down to `h`.
///
/// `h` of `usize::MAX` is "how many would it take", which is what `height` asks;
/// the cut only matters once the tail has been rationed and the panel has less
/// room than it asked for.
fn layout(question: &Question, w: usize, h: usize) -> Vec<Row> {
    if w == 0 || h == 0 {
        return Vec::new();
    }
    let body = w.saturating_sub(LEAD);
    if body == 0 {
        return Vec::new();
    }
    let mut rows = vec![Row::Blank];

    // Who is asking. A delegated member's question is not this conversation's,
    // and the person answering is owed the difference — a member's name is the
    // one thing that decides whether an answer is honest.
    if let Some(who) = &question.asker {
        rows.push(Row::Text {
            text: t(Msg::AskFromMember { who }).into_owned(),
            role: Role::Warning,
        });
    }

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
            let keep = question.options.len() + 3;
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
                rows.push(Row::Text {
                    text: line,
                    role: Role::Secondary,
                });
            }
        }
    }

    // The blank between the question and its answers is not optional — losing it
    // runs the first answer into the sentence it answers — so it is pushed with
    // the answers rather than into the salvageable margins below.
    rows.push(Row::Blank);
    for i in 0..question.options.len() {
        rows.push(Row::Answer(i));
    }
    rows.extend([Row::Blank, Row::Legend]);
    fit(rows, h)
}

/// Cut the layout down to the height it was given, least important row first.
///
/// The order is deliberate: the margin above the question goes, then the legend
/// and its blank, then the command's OWN lines (the text between the header and
/// the answers) from the bottom up — and only as a last resort the answers. The
/// answers are the one thing the panel is for; a short terminal that cannot hold
/// a long command AND its options drops command lines, never an option a person
/// still has to pick from. `geometry` fits the same way, so a click still lands
/// on the row it lit.
fn fit(mut rows: Vec<Row>, h: usize) -> Vec<Row> {
    if rows.len() <= h {
        return rows;
    }
    if rows.first() == Some(&Row::Blank) {
        rows.remove(0);
    }
    if rows.last() == Some(&Row::Legend) {
        rows.pop();
        if rows.last() == Some(&Row::Blank) {
            rows.pop();
        }
    }
    // Still too tall: shed the command's own lines before the answers. Each pass
    // drops the LAST text row that sits before the first answer — the tail of the
    // command (its `…` marker first, then its bottom lines), keeping the header
    // and the answers. Only when no such line is left does the final truncate
    // reach the answers, which no panel this short could have shown in full.
    while rows.len() > h {
        let first_answer = rows.iter().position(|r| matches!(r, Row::Answer(_)));
        let Some(cut) = first_answer.and_then(|a| {
            rows[..a]
                .iter()
                .rposition(|r| matches!(r, Row::Text { .. }))
        }) else {
            break;
        };
        rows.remove(cut);
    }
    rows.truncate(h);
    rows
}

/// One answer's row, lit up when it is the one pointed at.
fn answer_line(question: &Question, i: usize, here: bool, pointer: &str, w: usize) -> Line {
    let Some(answer) = question.options.get(i) else {
        return Line::empty();
    };
    // The pointed-at row is the same panel one step brighter, not a reversed
    // video bar — see `theme::Role::PanelSelBg` for why, and `menu::Menu::render`
    // for the same choice made for the same reason.
    let base = if here {
        theme::bg(Role::PanelSelBg).under(theme::fg(Role::PanelFg))
    } else {
        Style::new()
    };
    let mut spans = vec![
        Span::styled(
            if here {
                format!("{pointer} ")
            } else {
                "  ".to_string()
            },
            if here { base } else { theme::fg(Role::Accent) },
        ),
        Span::styled(format!("{}  ", i + 1), base),
        Span::styled(crate::ask::answer_label(&answer.value, &answer.label), base),
    ];
    // What "always" would actually cover. A person saying it is owed the scope
    // they are saying it to — and "every call of this tool" is a very different
    // promise from "this one command".
    if answer.value == ANSWER_ALWAYS {
        if let Some(grant) = question.about.as_ref().and_then(|a| a.grant.as_deref()) {
            let covers = match grant.trim().is_empty() {
                true => t(Msg::AskGrantWholeTool).into_owned(),
                false => t(Msg::AskGrantOnly {
                    what: &crate::ask::one_line(grant),
                })
                .into_owned(),
            };
            spans.push(Span::styled(
                format!("  {covers}"),
                if here { base } else { theme::fg(Role::Muted) },
            ));
        }
    }
    // Filled to the rect, so the highlight is a band across the row rather than a
    // patch behind the words: the pointed-at row is a surface, and a surface that
    // stops at its last letter is a smudge.
    pad(Line::from_spans(spans), w, base)
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
    /// One entry per drawn row, top to bottom: the answer on it, when it holds
    /// one. `None` for a row that is prose, blank, the legend — or an answer the
    /// rect was too short to reach.
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
pub fn geometry(question: &Question, vp: &Viewport<'_>) -> Geometry {
    let rows = if vp.rect.w == 0 || vp.rect.h == 0 {
        Vec::new()
    } else {
        layout(question, vp.rect.w as usize, vp.rect.h as usize)
    };
    Geometry {
        rows: rows
            .iter()
            .map(|r| match r {
                Row::Answer(i) => Some(*i),
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

    /// A moment with this question up and `cursor` pointed at it.
    fn asking(question: Q, cursor: usize) -> Moment {
        Moment {
            asking: Some(MomentAsk { question, cursor }),
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
        let rows = layout(&question, 60, 5);
        assert_eq!(
            rows.iter().position(|r| matches!(r, Row::Answer(0))),
            rows.iter()
                .position(|r| matches!(r, Row::Text { .. }))
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
        let rows = layout(&question, 40, 5);
        assert!(rows.len() <= 5, "fits the height: {rows:?}");
        assert_eq!(
            rows.iter().filter(|r| matches!(r, Row::Answer(_))).count(),
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
        let w = 60u16;
        let h = match Ask::height(&State, &moment, w) {
            Height::Hug(n) => n,
            other => panic!("unexpected {other:?}"),
        };
        let vp = Viewport::new(Rect::sized(w, h), &moment);
        let geom = geometry(&question, &vp);
        let lines = Ask::render(&State, &vp)
            .iter()
            .map(|l| l.plain().trim_end().to_string())
            .collect::<Vec<_>>();
        let rows = layout(&question, w as usize, h as usize);

        assert_eq!(
            rows.len(),
            lines.len(),
            "one drawn line per laid-out row:\n{}",
            lines.join("\n")
        );
        for (row, (laid, line)) in rows.iter().zip(&lines).enumerate() {
            match laid {
                Row::Answer(i) => {
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

    /// Only one row is pointed at, and it is the one a confirm would take.
    #[test]
    fn exactly_one_answer_is_lit_and_it_is_the_one_a_confirm_takes() {
        let question = approval(None, "write_file", "{}", None);
        for cursor in 0..question.options.len() {
            let moment = asking(question.clone(), cursor);
            let lines = framed(&moment, 60);
            let lit = lines
                .iter()
                .filter(|l| {
                    question
                        .options
                        .iter()
                        .any(|a| l.contains(&crate::ask::answer_label(&a.value, &a.label)))
                        && l.starts_with(
                            crate::caps::Caps::default().g(crate::caps::Glyph::Pointer),
                        )
                })
                .count();
            assert_eq!(
                lit,
                1,
                "one lit row for cursor {cursor}:\n{}",
                lines.join("\n")
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
    }

    /// The legend is the panel's keys, and it does not change with the question:
    /// what an answer *means* is the harness's business, the keys are the
    /// screen's.
    #[test]
    fn the_legend_is_one_line_and_says_the_keys() {
        let out = framed(&asking(approval(None, "write_file", "{}", None), 0), 60).join("\n");
        assert!(out.contains("↑↓ 选择"), "{out}");
        assert!(out.contains("⏎ 确认"), "{out}");
        assert!(out.contains("esc 拒绝"), "{out}");
    }

    /// `point_at` clamps rather than rejecting: a pointer on the padding, or an
    /// arrow pressed past the end, means the nearest answer.
    #[test]
    fn pointing_clamps_to_the_answers_there_are() {
        let question = approval(None, "write_file", "{}", None);
        let last = question.options.len() - 1;
        let mut ask = MomentAsk::new(question);
        assert_eq!(ask.cursor, 0, "the first answer starts lit");
        assert!(ask.point_at(2));
        assert_eq!(ask.cursor, 2);
        assert!(!ask.point_at(2), "pointing where it already is is not news");

        // Past the end is the last answer — and is news only if that moves it.
        ask.point_at(0);
        assert!(
            ask.point_at(99),
            "past the end is a move to the last answer"
        );
        assert_eq!(ask.cursor, last);
        assert!(!ask.point_at(99), "and it stays there");
    }

    /// A module that named its own id differently would fail `every_module_is_covered`
    /// rather than silently lose the whole suite.
    #[test]
    fn the_row_id_and_the_module_id_are_one_string() {
        assert_eq!(Ask::id(), ID);
        assert_eq!(Mounted::<Ask>::new().id(), ID);
    }
}
