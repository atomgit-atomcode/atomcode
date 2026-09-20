//! The settings panel: the configuration, listed and edited where the composer
//! was.
//!
//! Pulled up from the bottom, over the composer, the way a question panel is —
//! and for the same reason. A panel a person is working *in* is a turn of its
//! own: there is nothing to type beside it, so it asks for the composer's rows
//! and the composer asks for none. A modal in the middle of the screen would
//! cover the very conversation being configured.
//!
//! What lives here is the *drawing*: the list, the search box, the caret, the
//! legend. What a setting *is* arrives as data ([`crate::settings::SettingsView`]),
//! and changing one goes back over [`crate::settings::Settings`]. This module
//! knows neither the configuration file nor the schema — that is the launcher's
//! (`docs/adr/0022` §3).
//!
//! Nothing folds. The log records what was changed, never that a search box had
//! four characters in it, so there is no fact to fold: the panel's state is in
//! [`crate::moment::Moment`], where "what this screen is doing now" belongs.

use crate::frame::{Line, Span, Style};
use crate::module::{Height, View};
use crate::moment::{Moment, Viewport};
use crate::settings::{Panel, SettingKind, SettingsView, Tab};
use crate::theme::{self, Role};
use crate::width;

pub const ID: &str = "settings";

/// Cells a row's own furniture takes: the pointer and its gap.
const LEAD: usize = 2;

/// The narrowest the label column is drawn before a value is put right after it.
const LABEL_MIN: usize = 8;

/// The widest, so one long label does not push every value off the edge.
const LABEL_MAX: usize = 30;

/// Nothing folds — see this module's own doc for why.
#[derive(Default)]
pub struct State;

pub struct Settings;

impl View for Settings {
    type State = State;

    fn id() -> &'static str {
        ID
    }

    fn absorb(_state: &mut State, _fact: &atomcode_harness::session::SessionEvent) {}

    fn render(_state: &State, vp: &Viewport<'_>) -> Vec<Line> {
        let Some(panel) = vp.moment.settings_panel.as_ref() else {
            return Vec::new();
        };
        let w = vp.rect.w as usize;
        if w == 0 || vp.rect.h == 0 {
            return Vec::new();
        }
        let rows = layout(
            &vp.moment.settings,
            panel,
            vp.moment.usage.as_ref(),
            w,
            vp.rect.h as usize,
        );
        rows.into_iter()
            .map(|row| match row {
                Row::Header => header_line(panel.tab, w, vp.moment.caps),
                Row::Rule => panel_edge(w, vp.moment.caps),
                Row::Usage { line } => match line {
                    UsageLine::Head(text) => Line::styled(
                        width::take_width(&format!("  {text}"), w),
                        theme::fg(Role::Brand).bold(),
                    ),
                    UsageLine::Bar {
                        share,
                        about,
                        spent,
                    } => usage_bar(share, &about, spent, w, vp.moment.caps),
                    UsageLine::Heat { label, cells } => heat_row(&label, &cells, w),
                    UsageLine::HeatKey { less, more } => heat_key(&less, &more, w),
                    UsageLine::Pair { label, value } => Line::from_spans(vec![
                        Span::styled(format!("  {label}"), theme::fg(Role::Muted)),
                        Span::raw(value),
                    ])
                    .truncate(w),
                    UsageLine::Note(text) => Line::styled(
                        width::take_width(&format!("  {text}"), w),
                        theme::fg(Role::Muted),
                    ),
                    UsageLine::ChartRow { axis, dots, ink } => {
                        chart_row(&axis, &dots, &ink, w, vp.moment.caps)
                    }
                    UsageLine::ChartDates { gutter, text } => Line::styled(
                        width::take_width(&format!("  {} {text}", " ".repeat(gutter)), w),
                        theme::fg(Role::Muted),
                    ),
                    UsageLine::Cols { text, head, mark } => table_row(&text, head, mark, w),
                    UsageLine::Gap => Line::empty(),
                },
                Row::Elsewhere { tab } => Line::styled(
                    width::take_width(&format!("  {}", elsewhere(tab)), w),
                    theme::fg(Role::Muted),
                ),
                Row::Blank => Line::empty(),
                Row::BoxTop => box_edge(w, vp.moment.caps, true),
                Row::Search { caret } => search_line(&panel.query, caret, w, vp.moment.caps),
                Row::BoxBottom => box_edge(w, vp.moment.caps, false),
                Row::Setting { index } => {
                    setting_line(&vp.moment.settings, panel, index, w, vp.moment.caps)
                }
                Row::Editing {
                    label,
                    value,
                    caret,
                } => edit_line(&label, &value, caret, w, vp.moment.caps),
                Row::Nothing => Line::styled(
                    width::take_width("  没有匹配的设置", w),
                    theme::fg(Role::Muted),
                ),
                Row::Legend => Line::styled(
                    format!("  {}", crate::widget::keys(&legend(panel), vp.moment.caps)),
                    theme::fg(Role::Muted),
                )
                .truncate(w),
            })
            .collect()
    }

    /// What the panel needs, at this width.
    ///
    /// Counted by laying it out rather than by a formula, for the reason the
    /// question panel does it that way: how many rows a list takes is a fact
    /// about the width it is asked at, and one long label wraps to two rows
    /// where a short one takes one. A box that reports a height it does not then
    /// draw is how a panel cuts its own end off.
    ///
    /// **Counted with the query empty, whatever is typed.** The panel is the
    /// newest thing on the tail, so the tail's split is measured from the bottom
    /// up and the panel's *bottom* edge is the fixed one; a height that followed
    /// the filtered list would move the top edge — and the search box lives
    /// there, so it would slide down the screen under the person's fingers as
    /// the list narrowed. One character shortening the list would be the box
    /// jumping a row. So the height is the one the panel opened with, and a
    /// shorter list is drawn as a shorter list inside it, with the difference
    /// left blank.
    fn height(_state: &State, moment: &Moment, width: u16) -> Height {
        if moment.settings_panel.is_none() || width == 0 {
            return Height::Hug(0);
        }
        // The same number [`layout`] pads to, from the same function, so the
        // report and the drawing cannot disagree about how tall the panel is.
        Height::Hug(anchor(&moment.settings).min(u16::MAX as usize) as u16)
    }
}

/// One row of the panel, before it is drawn.
///
/// The whole layout, and the only one: `render` walks it to draw, [`geometry`]
/// walks it to say which row is which setting, and `height` counts it. A click
/// that landed on one setting and a highlight drawn on another is the failure
/// this shape rules out by construction rather than by keeping two formulas in
/// step.
#[derive(Clone, Debug, PartialEq)]
enum Row {
    /// The panel's name and its pages, on one row: `设置  Config  Status …`.
    ///
    /// One row, not two. The name and the pages answer the same question —
    /// which panel, and which page of it — and stacking them costs a row of
    /// screen to say what a person reads as one line anyway.
    Header,
    /// A straight rule across the panel.
    Rule,
    /// A page other than the settings, and everything it has to say.
    ///
    /// Carries its own lines rather than an index, because a page that is not
    /// the settings list has nothing in common with it: no cursor, no filter, no
    /// row to edit. One row per line is what [`fit`] already understands, so a
    /// page of prose costs nothing to add.
    Elsewhere {
        tab: crate::settings::Tab,
    },
    /// One line of the Usage page, already worded and measured.
    ///
    /// Carries the text rather than an index for the reason [`Elsewhere`] does:
    /// the page is prose and bars, with no cursor and nothing to filter, and one
    /// row per line is what [`fit`] already understands.
    ///
    /// [`Elsewhere`]: Row::Elsewhere
    Usage {
        line: UsageLine,
    },
    /// A margin row, above the box or above the list.
    Blank,
    /// The search box's top edge.
    BoxTop,
    /// What is typed into the search box. `caret` is the gap to draw, or `None`
    /// while a row's field has the keyboard instead — a caret blinking in a box
    /// nobody is typing into is a lie about where the next character goes.
    ///
    /// The box is drawn as a frame rather than as a bare line, which is what
    /// makes it *look* like something that takes keys: the panel opens with the
    /// keyboard already in it (see [`crate::settings::Panel`]), and a person
    /// should be able to see that without being told.
    Search {
        caret: Option<usize>,
    },
    /// The search box's bottom edge.
    BoxBottom,
    /// A setting, by index into the *filtered* rows.
    Setting {
        index: usize,
    },
    /// The row being typed into, drawn as a field in the place the row was.
    ///
    /// Carries its label too: a field that replaced its row entirely would take
    /// the only word saying *which* setting is being changed off the screen, and
    /// a value being typed with nothing naming it is a value nobody can check.
    Editing {
        label: String,
        value: String,
        caret: usize,
    },
    /// The filtered list is empty.
    Nothing,
    Legend,
}

/// Where the border of the panel and of the search box stands.
///
/// **One number, one place.** The corners of the box, its walls, and the pointer
/// on every row below it are the same column — a `┌` one cell to the right of
/// the `│` under it is the misalignment this constant exists to make impossible.
/// Content therefore starts at [`LEAD`], which is one cell of wall and one of
/// air.
const BORDER_COL: usize = 0;

/// The rows this panel makes at this width, cut down to `h`.
///
/// `h` of `usize::MAX` is "how many would it take", which is what `height` asks;
/// the cut only matters once the tail has been rationed and the panel has less
/// room than it asked for.
fn layout(
    settings: &SettingsView,
    panel: &Panel,
    usage: Option<&crate::settings::UsagePage>,
    w: usize,
    h: usize,
) -> Vec<Row> {
    if w == 0 || h == 0 {
        return Vec::new();
    }
    let mut rows = rows_for(settings, panel, usage);
    // A shorter list is a shorter list inside the same box, not a shorter box.
    //
    // The height is anchored when the panel opens ([`anchor`]), so the rows a
    // filter took away are filled in **above the closing rule**: both edges of
    // the outer frame stay put, the search box stays where the hand left it, and
    // only the list inside gets shorter. Without this the closing rule would
    // float up under a two-item result and the panel would look cut off.
    //
    // Filled to the *anchor* and not to the rect: the rect may be taller than
    // the panel ever asks for, and drawing into rows nobody asked for would be a
    // report about itself the panel then contradicts.
    let anchor = anchor(settings);
    if rows.len() < anchor {
        let at = rows.len() - 1;
        rows.splice(at..at, std::iter::repeat_n(Row::Blank, anchor - rows.len()));
    }
    fit(rows, h)
}

/// The height the panel opened with, in rows — the one it keeps.
///
/// Counted by laying out the rows with the query *empty*, so a filter changes
/// what is inside the panel and never how tall it is. This is the single
/// definition of the anchor: [`layout`] pads up to it and
/// [`Settings::height`](crate::module::View::height) reports it, so the two
/// cannot disagree about how tall the panel is.
///
/// **Neither the query nor the width changes it.** The query is held empty for
/// the reason above; the width is not a parameter because every row this panel
/// draws is *truncated* to its rect rather than wrapped into it (`width::take_width`
/// in the drawing functions), so a label too long for one width is cut short,
/// not given a second row. One row per setting, always — which is also what
/// makes the anchor a number that can be computed once.
fn anchor(settings: &SettingsView) -> usize {
    rows_for(settings, &Panel::new(), None).len()
}

/// The panel's rows as the query leaves them, before any padding or cutting.
///
/// Takes no width: which rows are drawn is decided by the query, and how tall
/// each of them is at a given width is the caller's business — [`anchor`] counts
/// them for a width it is given. A width parameter here would be accepted and
/// ignored, which is the shape of a bug waiting for someone to rely on it.
fn rows_for(
    settings: &SettingsView,
    panel: &Panel,
    usage: Option<&crate::settings::UsagePage>,
) -> Vec<Row> {
    let shown = settings.matching(&panel.query);
    // The name and the pages on one row, then the rule that closes the header —
    // what the panel is, and which page of it, before anything it has to say.
    let mut rows = vec![Row::Header];
    rows.push(Row::Rule);

    // The other pages are not the settings list, so the search box goes with
    // them: a filter with nothing to filter would be a box that collects
    // characters and changes nothing.
    if panel.tab == Tab::Usage {
        rows.push(Row::Blank);
        rows.extend(
            usage_lines(usage)
                .into_iter()
                .map(|line| Row::Usage { line }),
        );
        rows.extend([Row::Blank, Row::Rule]);
        return rows;
    }
    if panel.tab != Tab::Config {
        rows.push(Row::Blank);
        rows.push(Row::Elsewhere { tab: panel.tab });
        rows.extend([Row::Blank, Row::Rule]);
        return rows;
    }

    // The box is three rows — two edges and the text — because that is what
    // says "keys go here" without a caption saying it. The caret is drawn unless
    // a row's own field has the keyboard: two carets would be two answers to
    // where the next character goes.
    rows.push(Row::BoxTop);
    rows.push(Row::Search {
        caret: panel.editing.is_none().then_some(panel.query_caret),
    });
    rows.push(Row::BoxBottom);
    // The legend goes here, against the box, and not at the foot of the panel.
    //
    // It is about the search box — what the arrows and the return key would do
    // to the list right under it — so putting it *at* the list's head makes it
    // read as that list's footer. At the panel's foot it was a screen away from
    // the thing it explains, and on a full panel it was the row most easily
    // pushed off the end by a short rect.
    rows.push(Row::Legend);
    rows.push(Row::Blank);

    if shown.is_empty() {
        rows.push(Row::Nothing);
    }
    for (i, row) in shown.iter().enumerate() {
        // The row being edited is drawn as the field, in the place the row was:
        // an edit that appeared somewhere else on screen would be a second
        // thing to look at.
        match &panel.editing {
            Some(edit) if edit.id == row.id => rows.push(Row::Editing {
                label: row.label.clone(),
                value: edit.value.clone(),
                caret: edit.value.len(),
            }),
            _ => rows.push(Row::Setting { index: i }),
        }
    }

    // Only the margin and the closing rule are left at the foot: the legend
    // moved up under the search box (see above), so nothing here explains the
    // list from a screen away.
    rows.extend([Row::Blank, Row::Rule]);
    rows
}

/// Cut the layout down to the height it was given, least important row first.
///
/// What gives way is the furniture that sits *lowest on the screen* — the
/// panel's closing rule, the legend, the margins, the opening rule — and the
/// order falls out of the layout rather than being listed again here: the last
/// furniture row in the vector is the one furthest down, so removing
/// `rposition`-wise takes them from the bottom up.
///
/// Truncating the end instead would take the settings first — the one thing the
/// panel is for — and leave a box explaining how to work an empty list. That is
/// the same bargain the question panel strikes; what changed with the frame is
/// which rows *are* furniture, so the search box and its two edges are never
/// sacrificed: they are where the typing goes.
fn fit(mut rows: Vec<Row>, h: usize) -> Vec<Row> {
    if rows.len() <= h {
        return rows;
    }
    while rows.len() > h {
        let Some(at) = rows
            .iter()
            .rposition(|r| matches!(r, Row::Rule | Row::Legend | Row::Blank))
        else {
            break;
        };
        rows.remove(at);
    }
    rows.truncate(h);
    rows
}

/// The panel's name and its pages: `设置  Config  Status  Usage  Stats`.
///
/// The name says which panel this is; the pages say which page of it. The one
/// showing is the one that is lit — the same `PanelSelBg` the pointed-at setting
/// row uses, so "this is the selected thing" means one thing across the panel
/// rather than two.
///
/// The tabs are drawn **all the time**, including when there is not room for
/// them: this is the row that says the panel has four pages, and a row that hid
/// three of them when it got tight would leave a person on a page they cannot
/// see a way out of.
fn header_line(tab: crate::settings::Tab, w: usize, caps: crate::caps::Caps) -> Line {
    let (spans, _) = header_parts(tab);
    let _ = caps;
    Line::from_spans(spans).truncate(w)
}

/// The header's spans **and** where each tab sits in columns.
///
/// One function for both, because a pointer has to find the tab that was drawn.
/// Two computations — one for the spans, one for the ranges — would agree until
/// a label changed length, and then a click would switch to the tab next to the
/// one under the pointer. This is the same rule the panel's other clickable
/// things follow: the hit is read off what was drawn, never re-derived.
///
/// The ranges do **not** depend on which page is showing: lighting a tab changes
/// its style, not its width or its place. A hit-test that took the current page
/// would be a range that moved under a pointer that had not.
fn header_parts(
    tab: crate::settings::Tab,
) -> (Vec<Span>, Vec<(crate::settings::Tab, usize, usize)>) {
    // (text, style, the tab this cell belongs to — the padding included, so a
    // click on the space beside a label takes that label's tab rather than the
    // one it abuts).
    let mut pieces: Vec<(String, Style, Option<crate::settings::Tab>)> = vec![
        // Two cells in, which is where every other thing the panel says starts:
        // the box's text sits after its `│ ` and a setting's label after its
        // pointer. A title one cell further left than everything under it reads
        // as a mistake — and it was one, found by
        // `the_boxs_corners_line_up_with_its_walls_and_the_labels_below`.
        ("  ".to_string(), Style::new(), None),
        ("设置".to_string(), theme::fg(Role::Brand).bold(), None),
        ("   ".to_string(), Style::new(), None),
    ];
    for (i, t) in crate::settings::Tab::ALL.iter().enumerate() {
        if i > 0 {
            pieces.push(("  ".to_string(), Style::new(), None));
        }
        let here = *t == tab;
        let style = if here {
            theme::bg(Role::PanelSelBg).under(theme::fg(Role::PanelFg))
        } else {
            theme::fg(Role::Muted)
        };
        // The pad goes *inside* the highlight, so the lit tab is a band rather
        // than a patch behind its letters — the same choice the answer rows make.
        pieces.push((format!(" {} ", t.label()), style, Some(*t)));
    }

    let mut spans = Vec::with_capacity(pieces.len());
    let mut ranges: Vec<(crate::settings::Tab, usize, usize)> = Vec::new();
    let mut col = 0usize;
    for (text, style, owner) in pieces {
        let w = width::str_width(&text);
        if let Some(t) = owner {
            ranges.push((t, col, col + w));
        }
        col += w;
        spans.push(Span::styled(text, style));
    }
    (spans, ranges)
}

/// Which tab is under this cell of the header row.
///
/// `col` is measured from the panel's left edge, which is what the host knows
/// and what a click carries.
pub fn tab_at(col: usize) -> Option<crate::settings::Tab> {
    header_parts(crate::settings::Tab::ALL[0])
        .1
        .into_iter()
        .find(|(_, from, to)| (*from..*to).contains(&col))
        .map(|(tab, _, _)| tab)
}

/// What a page that is not the settings has to say for itself.
///
/// One line, and honest about being the only one: these pages exist so the
/// panel has somewhere to grow, and a page that invented content it does not
/// have would be worse than one that says what it is waiting for. Each names
/// the thing that would fill it, so the next person knows where to look.
fn elsewhere(tab: crate::settings::Tab) -> &'static str {
    match tab {
        // Not reachable — the settings page draws the box and the list instead —
        // but a total function beats a `panic!` for the day someone adds a row.
        crate::settings::Tab::Config => "",
        crate::settings::Tab::Status => {
            "这个会话跑在什么上面(模型、推理档、压缩,来自 agent 的描述)"
        }
        // Unreachable now: the Usage page draws itself. Kept total for the same
        // reason `Config` is — and the wording corrected, because what this page
        // turned out to be is the account's allowance, not this session's tokens.
        crate::settings::Tab::Usage => "",
        crate::settings::Tab::Stats => "这次会话干了什么(回合数、工具调用、耗时,从日志折出来)",
    }
}

/// A line of the Usage page.
#[derive(Clone, Debug, PartialEq)]
enum UsageLine {
    /// A section's name, in the panel's own heading style.
    Head(String),
    /// A bar, how much of it is filled, and what that measures.
    ///
    /// `share` is 0..=1. `about` is the words beside it — and they carry what
    /// the bar is *of*, because two bars on this page measure different things
    /// and a reader cannot tell them apart from the bar alone.
    Bar {
        share: f32,
        about: String,
        /// Drawn as a warning rather than in the page's own ink. What it means
        /// is "this one is spent" — a bar that looked the same full as it did
        /// at half would make a person read the number to find out, which is
        /// the thing the bar is there to save them.
        spent: bool,
    },
    /// An ordinary line, dimmed.
    Note(String),
    /// One plot row of the day chart: what the gutter says at this height, and
    /// which of the row's sub-cells the line passes through.
    ///
    /// A bitmask per cell rather than a glyph, because which glyph a set of
    /// sub-cells is depends on what the terminal can draw, and that is only
    /// known at render time. The dots are the data; braille is one rendering of
    /// them and `:` is another.
    ChartRow {
        axis: String,
        dots: Vec<u8>,
        /// Which series owns each cell — the index the colour comes from, or
        /// [`NO_SERIES`] for a chart that is not split by model.
        ///
        /// Per cell and not per row, because the lines cross: at a cell two of
        /// them pass through, the dots merge and one of them has to be the one
        /// that names the colour. Taking the biggest is arbitrary but stable,
        /// which is what a legend needs.
        ink: Vec<u8>,
    },
    /// The dates under the plot, already laid out into the plot's own columns.
    ///
    /// Laid out here rather than at render for the reason [`UsageLine::Cols`]
    /// is: where a tick goes depends on which day it names and how wide the
    /// plot is, and both of those are the chart's, not the screen's.
    ChartDates { gutter: usize, text: String },
    /// A row of an aligned table, already padded. `head` draws the column names,
    /// and `mark` is the series dot that ties the row to a line on the chart.
    ///
    /// Padded here rather than at render: column widths come from what is *in*
    /// the columns, not from how wide the screen is, so they are the same at
    /// every width — which is the only reason a column of numbers is readable.
    Cols {
        text: String,
        head: bool,
        mark: Option<u8>,
    },
    /// One weekday's row of the calendar.
    ///
    /// `None` is a day outside the span — left blank, so the grid has the shape
    /// of the months it covers rather than a rectangle with filler in it.
    /// `Some(level)` is a day in it, including a day with nothing on it: that
    /// one is drawn too, because where the gaps are is half of what a calendar
    /// says and a hole in the grid reads as missing data.
    Heat {
        label: String,
        cells: Vec<Option<u8>>,
    },
    /// The ramp, with a word at each end.
    HeatKey { less: String, more: String },
    /// A label and its value, the label already padded so the values line up.
    ///
    /// Padded by **display width**, not by character count: `请求次数` is four
    /// characters and eight cells while `总 Token 数` is nine characters and
    /// eleven, so counting characters puts every value at a different column.
    /// The classic front end had this bug and fixed it; inheriting the fix is
    /// cheaper than rediscovering it.
    Pair { label: String, value: String },
    /// A blank line inside the page.
    Gap,
}

/// The Usage page, as lines.
///
/// **The two bars are not the same measurement**, and the wording is where that
/// is kept honest:
///
/// * the context bar is a real proportion — the host says how many tokens of the
///   window this session is using;
/// * an allowance bar is what the **account service** counted — `used_percent`,
///   which travels from `status_v2` through the host. It was being dropped on
///   the way (`daemon/runtime_host.rs`), which is why the first version of this
///   page drew time through the window instead and had to say so. A bar with no
///   percentage behind it is not drawn at all: an empty track reads as "none
///   used", and not knowing is a different thing from zero.
fn usage_lines(page: Option<&crate::settings::UsagePage>) -> Vec<UsageLine> {
    let Some(page) = page else {
        return vec![UsageLine::Note("正在问宿主…".into())];
    };
    let mut out = Vec::new();

    if let Some(context) = page.context.as_ref() {
        out.push(UsageLine::Head("本会话".into()));
        let share = if context.window == 0 {
            0.0
        } else {
            context.used as f32 / context.window as f32
        };
        out.push(UsageLine::Bar {
            share,
            about: format!(
                "上下文已用 {:.0}% · {} / {}",
                share * 100.0,
                crate::content::token_count(context.used),
                crate::content::token_count(context.window)
            ),
            spent: false,
        });
        out.push(UsageLine::Note(format!("模型 {}", context.model)));
        out.push(UsageLine::Gap);
    }

    // Said, not returned on: an account with no metered windows may still have
    // spent something, and the first version of this bailed out here — so a
    // host that reported per-model figures and no windows drew none of them.
    if page.windows.is_empty() {
        out.push(UsageLine::Head("额度".into()));
        out.push(UsageLine::Note("这个宿主不计额度".into()));
        out.push(UsageLine::Gap);
    }

    for window in &page.windows {
        out.push(UsageLine::Head(window.label.clone()));
        match window.used_percent {
            // What the account service counted. The bar is *of the allowance*,
            // which is the thing a person opened this page to see. A tenth of a
            // percent because at the top of a window that is the digit that is
            // still moving.
            Some(percent) => {
                let counted = match (window.calls_used, window.call_limit) {
                    (Some(used), Some(limit)) => format!(" · {used} / {limit} 次"),
                    (None, Some(limit)) => format!(" · 上限 {limit} 次"),
                    _ => String::new(),
                };
                out.push(UsageLine::Bar {
                    share: f32::from(percent) / 100.0,
                    about: format!("用掉 {percent}%{counted}"),
                    spent: window.exhausted || percent >= 100,
                });
            }
            // No bar at all rather than an empty one: an empty track reads as
            // "none used", and not knowing is a different thing from zero.
            None => out.push(UsageLine::Note("这个窗口没报用量".into())),
        }
        let mut tail = Vec::new();
        if window.exhausted {
            tail.push("用完了".to_string());
        }
        if window.resets_in_seconds > 0 {
            tail.push(format!(
                "剩余重置时间 {}",
                countdown(window.resets_in_seconds)
            ));
        }
        if !window.resets_at.is_empty() {
            tail.push(format!("{} 重置", window.resets_at));
        }
        if !tail.is_empty() {
            out.push(UsageLine::Note(tail.join(" · ")));
        }
        out.push(UsageLine::Gap);
    }

    // The plan behind the windows. Drawn even when it has run out — a person
    // whose plan lapsed needs to be told that, and drawing nothing looks like
    // never having had one.
    if let Some(plan) = page.plan.as_ref() {
        out.push(UsageLine::Head(format!(
            "{} · {}",
            plan.plan,
            match plan.active {
                true => "生效中",
                false => "已过期",
            }
        )));
        if !plan.claimed_at.is_empty() || !plan.expires_at.is_empty() {
            out.push(UsageLine::Note(format!(
                "领取 {} · 到期 {}",
                blank_as_unknown(&plan.claimed_at),
                blank_as_unknown(&plan.expires_at)
            )));
        }
        if plan.total_days > 0 {
            out.push(UsageLine::Note(format!(
                "剩余 {}/{} 天",
                plan.remaining_days, plan.total_days
            )));
            // Of the plan's whole term, how much is gone. The bar fills as the
            // plan is used up, like the allowance bars above it — two bars on
            // one page that filled in opposite directions would be two bars a
            // person has to stop and think about.
            let gone =
                (plan.total_days - plan.remaining_days).max(0) as f32 / plan.total_days as f32;
            out.push(UsageLine::Bar {
                share: gone,
                about: format!("{:.1}%", gone * 100.0),
                spent: plan.remaining_days <= 0,
            });
        }
        out.push(UsageLine::Gap);
    }

    if let Some(stats) = page.stats.as_ref() {
        out.push(UsageLine::Head("总览".into()));
        if !stats.from.is_empty() && !stats.to.is_empty() {
            out.push(UsageLine::Note(format!("{} 到 {}", stats.from, stats.to)));
        }
        out.push(UsageLine::Gap);

        if !stats.daily.is_empty() {
            out.extend(heat_calendar(&stats.daily));
            out.push(UsageLine::Gap);
        }

        // The figures a person reads off this page one at a time, in a column.
        // They were a run-on sentence before: "300m tokens · 3007 次请求 · …"
        // is four numbers a reader has to parse apart, and four of the seven
        // were not there at all.
        let active = stats.daily.iter().filter(|day| day.tokens > 0).count();
        let (longest, current) = streaks(&stats.daily);
        let busiest = stats
            .daily
            .iter()
            .filter(|day| day.tokens > 0)
            .max_by_key(|day| day.tokens)
            .map(|day| day.date.clone());
        let mut rows = Vec::new();
        // Biggest first is how the host sends them, so the head of the list is
        // the answer — no second pass to find it.
        if let Some(favourite) = stats.models.first() {
            rows.push(("最常用模型".to_string(), favourite.name.clone()));
        }
        rows.push((
            "总 Token 数".to_string(),
            crate::content::token_count_u64(stats.total_tokens),
        ));
        rows.push(("请求次数".to_string(), stats.total_requests.to_string()));
        if !stats.daily.is_empty() {
            rows.push((
                "活跃天数".to_string(),
                format!("{active} / {}", stats.daily.len()),
            ));
        }
        if let Some(day) = busiest {
            rows.push(("最活跃日期".to_string(), day));
        }
        if !stats.daily.is_empty() {
            rows.push(("最长连续天数".to_string(), format!("{longest} 天")));
            rows.push(("当前连续天数".to_string(), format!("{current} 天")));
        }
        out.extend(pairs(rows));
        out.push(UsageLine::Gap);

        if !stats.daily.is_empty() {
            out.push(UsageLine::Head("每天用掉多少".into()));
            out.extend(day_chart(&stats.daily, &stats.series));
            out.push(UsageLine::Gap);
        }

        if !stats.models.is_empty() {
            out.push(UsageLine::Head("各模型用量".into()));
            out.extend(model_table(&stats.models, stats.total_tokens));
            out.push(UsageLine::Gap);
        }
    }

    if matches!(out.last(), Some(UsageLine::Gap)) {
        out.pop();
    }
    out
}

/// The longest run of working days in the span, and the run it ends on.
///
/// The second is not the first: a person wants to know both "how long have I
/// kept this up" and "how long did I ever". Counted oldest-first, which is the
/// order the days arrive in, so the run at the end is the run that is still
/// going.
fn streaks(daily: &[atomcode_host_api::DayUse]) -> (usize, usize) {
    let mut longest = 0;
    let mut run = 0;
    for day in daily {
        run = match day.tokens > 0 {
            true => run + 1,
            false => 0,
        };
        longest = longest.max(run);
    }
    (longest, run)
}

/// A date the service left empty, said as such.
///
/// An unactivated claim has no date. Printing the empty string would leave
/// `领取  · 到期 2036-07-30`, which reads as a rendering fault rather than as
/// the thing it is.
fn blank_as_unknown(text: &str) -> &str {
    match text.is_empty() {
        true => "—",
        false => text,
    }
}

/// How wide the bar itself is drawn, in cells.
///
/// Fixed rather than a share of the panel: two bars of different widths cannot
/// be compared by eye, and comparing them is the only reason to draw two.
const BAR_CELLS: usize = 28;

/// One bar and the words beside it.
///
/// Filled with a block and unfilled with the same block in the muted colour, so
/// the pair reads as one track at any terminal that draws colour, and as a
/// solid run at one that does not. The share is clamped and rounded down: a bar
/// that showed a full track at 99% would say the thing it is there to warn about
/// has already happened.
fn usage_bar(share: f32, about: &str, spent: bool, w: usize, caps: crate::caps::Caps) -> Line {
    use crate::caps::Glyph;
    let filled = ((share.clamp(0.0, 1.0) * BAR_CELLS as f32) as usize).min(BAR_CELLS);
    // Two characters, not one character in two colours. A track drawn in the
    // same block as its fill says nothing on a terminal with no colour — and
    // says nothing in a transcript either, which is where this was caught.
    let full = caps.g(Glyph::Thumb);
    let empty = if caps.unicode { "░" } else { "-" };
    let mut spans = vec![Span::styled("  ".to_string(), Style::new())];
    if filled > 0 {
        let ink = match spent {
            true => theme::fg(Role::Error),
            false => theme::fg(Role::Accent),
        };
        spans.push(Span::styled(full.repeat(filled), ink));
    }
    if filled < BAR_CELLS {
        spans.push(Span::styled(
            empty.repeat(BAR_CELLS - filled),
            theme::fg(Role::Border),
        ));
    }
    spans.push(Span::styled(format!("  {about}"), theme::fg(Role::Muted)));
    Line::from_spans(spans).truncate(w)
}

/// How many rows tall the day chart's plot is, and how many columns wide.
///
/// Both fixed, for the reason [`BAR_CELLS`] is: a chart that changed width with
/// the panel could not be compared with the one you looked at yesterday, and
/// every row this panel draws is truncated to its rect rather than reflowed
/// into it — so the width is a property of the chart, not of the screen. It
/// also keeps [`rows_for`] width-free, which is what lets the panel's height be
/// computed once.
const CHART_ROWS: usize = 6;
const CHART_CELLS: usize = 52;
/// How wide the gutter of y-axis labels is, in cells. The classic front end's
/// figure: wide enough for `216.6m`, and the same on every row — a gutter that
/// changed width between a labelled row and a blank one would make the plot
/// jitter sideways as the eye went down it.
const AXIS_CELLS: usize = 7;

/// How many dots a cell holds, and which bit each one is.
///
/// Braille is the only cell in a terminal font that addresses more than one
/// point, which is what a line chart needs: a block can say "this row is
/// filled", it cannot say "the line passes a quarter of the way down". Eight
/// dots to a cell puts the plot at 88 × 24 where blocks would give 44 × 6.
///
/// The bit layout is the one Unicode fixed, not one chosen here: the fourth row
/// of dots was added to the standard after the first three, so its bits sit
/// above the others instead of continuing the run.
const DOT_ROWS: usize = 4;
const DOT_COLS: usize = 2;
const DOTS: [[u8; DOT_COLS]; DOT_ROWS] = [[0x01, 0x08], [0x02, 0x10], [0x04, 0x20], [0x40, 0x80]];

/// The day series as a plot with an axis to read numbers off.
///
/// The first version of this page drew one row of blocks and said the peak in
/// words beside it. That answers "is it climbing"; it does not answer "how much
/// was that spike" or "when", which is what the classic front end's plot
/// answered and what this is here to carry over. An axis is the difference
/// between a shape and a measurement.
///
/// **More days than columns are averaged into the columns, not dropped.** A
/// chart that silently showed the last 44 of 90 days would answer a different
/// question from the one the heading asks, and it would answer it without
/// saying so. The dates under the floor are taken from the columns themselves,
/// so a tick always sits over the day it names.
fn day_chart(
    daily: &[atomcode_host_api::DayUse],
    series: &[atomcode_host_api::ModelSeries],
) -> Vec<UsageLine> {
    // One line per model when the service broke the days down, and one line for
    // the total when it did not. Not both: two answers to "how much on the
    // 19th" drawn over each other is a chart a reader has to be told how to
    // read, and the total is the sum of the lines already there.
    let lines: Vec<&[u64]> = match series.is_empty() {
        false => series.iter().map(|s| s.daily.as_slice()).collect(),
        true => Vec::new(),
    };
    let totals: Vec<u64> = daily.iter().map(|d| d.tokens).collect();
    let lines: Vec<&[u64]> = match lines.is_empty() {
        true => vec![totals.as_slice()],
        false => lines,
    };
    let split = !series.is_empty();

    // One scale for every line, or they cannot be compared — which is the whole
    // reason to draw them on one chart.
    let peak = lines
        .iter()
        .flat_map(|line| line.iter().copied())
        .max()
        .unwrap_or(0);
    if peak == 0 {
        return vec![UsageLine::Note("这段时间没有用量".into())];
    }
    let dots_wide = CHART_CELLS * DOT_COLS;
    let dots_tall = CHART_ROWS * DOT_ROWS;

    let mut dots = vec![0u8; CHART_CELLS * CHART_ROWS];
    let mut ink = vec![NO_SERIES; CHART_CELLS * CHART_ROWS];
    for (which, line) in lines.iter().enumerate() {
        if line.is_empty() {
            continue;
        }
        // Sampled at the dot, not at the day: the plot is 104 dots across and
        // the series is however long it is, so each dot asks the series what it
        // was at that point. Nearest rather than averaged, which is what the
        // classic front end does — a line chart draws where the series *was*,
        // and an average of three days is a value it never had.
        let sample = |dx: usize| -> usize {
            let at = match line.len() {
                1 => 0,
                n => dx * (n - 1) / (dots_wide - 1).max(1),
            };
            let filled = ((line[at] as u128 * (dots_tall - 1) as u128) / peak as u128) as usize;
            (dots_tall - 1).saturating_sub(filled)
        };
        let owner = match split {
            true => u8::try_from(which).unwrap_or(NO_SERIES),
            false => NO_SERIES,
        };
        // A connected line, not a scatter: every dot column is joined to the one
        // before it, so a climb is a stroke. Drawing only the points leaves a
        // rise as two marks with a gap between them, which at this width reads
        // as noise.
        let mut previous = sample(0);
        let mark = |dx: usize, dy: usize, dots: &mut Vec<u8>, ink: &mut Vec<u8>| {
            let cell = (dy / DOT_ROWS) * CHART_CELLS + dx / DOT_COLS;
            dots[cell] |= DOTS[dy % DOT_ROWS][dx % DOT_COLS];
            // First one to reach a cell keeps it. The lines are in size order,
            // so the colour of a crossing is the bigger model's — stable, which
            // is what a legend needs, rather than whichever was drawn last.
            if ink[cell] == NO_SERIES {
                ink[cell] = owner;
            }
        };
        mark(0, previous, &mut dots, &mut ink);
        for dx in 1..dots_wide {
            let here = sample(dx);
            for dy in previous.min(here)..=previous.max(here) {
                mark(dx, dy, &mut dots, &mut ink);
            }
            previous = here;
        }
    }

    let mut out = Vec::new();
    for row in 0..CHART_ROWS {
        // The bottom row *is* the floor: the flat run of dots along it is the
        // line at zero, not a rule drawn under the chart. Labelled every other
        // row above it, at the fraction of the peak that row stands for.
        let label = match (row + 1 == CHART_ROWS, row % 2) {
            (true, _) => "0".to_string(),
            (false, 0) => crate::content::token_count_u64(
                (peak as u128 * (CHART_ROWS - 1 - row) as u128 / (CHART_ROWS - 1) as u128) as u64,
            ),
            _ => String::new(),
        };
        let span = row * CHART_CELLS..(row + 1) * CHART_CELLS;
        out.push(UsageLine::ChartRow {
            axis: pad_left(&label, AXIS_CELLS),
            dots: dots[span.clone()].to_vec(),
            ink: ink[span].to_vec(),
        });
    }
    out.push(UsageLine::ChartDates {
        gutter: AXIS_CELLS,
        text: date_ticks(daily),
    });
    out
}

/// A cell no series claimed — drawn in the ordinary chart ink.
const NO_SERIES: u8 = u8::MAX;

/// How wide one day of the calendar is drawn, and how wide the weekday gutter
/// beside it is. Both the classic front end's: four cells a day makes sixty
/// days a grid you can pick a week out of, and adjacent cells join into a solid
/// block rather than a row of dots.
const DAY_CELLS: usize = 4;
const WEEKDAY_CELLS: usize = 4;
/// Sunday first, which is where the month headers are anchored from.
const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Days since 1970-01-01 for a proleptic-Gregorian date (Howard Hinnant's
/// algorithm).
///
/// Here rather than from a date crate because this is the whole of what the
/// calendar needs — which weekday a date is and which week it falls in — and a
/// dependency for two arithmetic expressions is a dependency to keep in step
/// with for no return.
fn epoch_day(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) as i64 + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `YYYY-MM-DD`, or nothing. A date the service worded differently is skipped
/// rather than guessed at.
fn ymd(text: &str) -> Option<(i64, u32, u32)> {
    let mut parts = text.split('-');
    let y = parts.next()?.parse().ok()?;
    let m = parts.next()?.parse().ok()?;
    let d = parts.next()?.parse().ok()?;
    Some((y, m, d))
}

/// 1970-01-01 was a Thursday, so `(epoch + 4) mod 7` counts from Sunday.
fn weekday(epoch: i64) -> usize {
    (((epoch % 7) + 4).rem_euclid(7)) as usize
}

/// Which step of the ramp a day's tokens land on, against the busiest day.
///
/// Zero keeps step zero — "nothing happened" is a step of its own and must not
/// round up into "a little happened". Everything else lands on 1..=5, so the
/// quietest working day is still visibly a working day.
fn heat_levels(daily: &[u64]) -> Vec<u8> {
    let peak = daily.iter().copied().max().unwrap_or(0);
    daily
        .iter()
        .map(|value| match (peak, value) {
            (0, _) | (_, 0) => 0,
            (peak, value) => {
                let steps = f64::from(crate::theme::HEAT - 1);
                (((*value as f64 / peak as f64) * steps).ceil() as u8)
                    .clamp(1, crate::theme::HEAT - 1)
            }
        })
        .collect()
}

/// The calendar: a month header, then one row per weekday.
///
/// Weeks run down the columns the way every contribution graph does, because
/// the question it answers is "which days do I work" and that reads across a
/// row. Days outside the span stay blank so the grid keeps the ragged ends the
/// months actually have.
fn heat_calendar(daily: &[atomcode_host_api::DayUse]) -> Vec<UsageLine> {
    let levels = heat_levels(&daily.iter().map(|d| d.tokens).collect::<Vec<_>>());
    let dated: Vec<(i64, u8, u32)> = daily
        .iter()
        .zip(levels)
        .filter_map(|(day, level)| {
            let (y, m, d) = ymd(&day.date)?;
            Some((epoch_day(y, m, d), level, m))
        })
        .collect();
    // Anchored on the earliest date across the whole set, not on the first
    // entry: the service is not promised to be sorted, and a day older than the
    // first would land in a negative column.
    let Some(first) = dated.iter().map(|(epoch, _, _)| *epoch).min() else {
        return Vec::new();
    };
    let start = first - weekday(first) as i64;
    let column = |epoch: i64| ((epoch - start) / 7) as usize;
    let columns = dated
        .iter()
        .map(|(epoch, _, _)| column(*epoch))
        .max()
        .unwrap_or(0)
        + 1;

    let mut grid: Vec<Option<u8>> = vec![None; columns * 7];
    // Where each month first appears, for the header above the grid.
    let mut month_at: Vec<(usize, u32)> = Vec::new();
    let mut previous = 0u32;
    for (epoch, level, month) in &dated {
        grid[weekday(*epoch) * columns + column(*epoch)] = Some(*level);
        if *month != previous {
            month_at.push((column(*epoch), *month));
            previous = *month;
        }
    }
    month_at.sort_by_key(|(col, _)| *col);

    let mut header = vec![' '; columns * DAY_CELLS];
    for (i, (col, month)) in month_at.iter().enumerate() {
        let name = MONTHS[(*month as usize).saturating_sub(1).min(11)];
        let at = col * DAY_CELLS;
        // Room to the next month, or to the end. A name that would not fit
        // before the next one is left out rather than clipped: half a month
        // name over the wrong column says less than nothing there.
        let until = match month_at.get(i + 1) {
            Some((next, _)) => (next * DAY_CELLS).saturating_sub(1),
            None => header.len(),
        };
        if until.saturating_sub(at) < name.len() {
            continue;
        }
        for (j, c) in name.chars().enumerate() {
            if at + j < header.len() {
                header[at + j] = c;
            }
        }
    }

    let mut out = vec![UsageLine::Note(format!(
        "{}{}",
        " ".repeat(WEEKDAY_CELLS),
        header.into_iter().collect::<String>().trim_end()
    ))];
    out.extend((0..7).map(|row| UsageLine::Heat {
        label: pad_right(WEEKDAYS[row], WEEKDAY_CELLS),
        cells: grid[row * columns..(row + 1) * columns].to_vec(),
    }));
    out.push(UsageLine::Gap);
    out.push(UsageLine::HeatKey {
        less: "少".into(),
        more: "多".into(),
    });
    out
}

/// One weekday's row: its name, then a block per week at that week's shade.
fn heat_row(label: &str, cells: &[Option<u8>], w: usize) -> Line {
    let mut spans = vec![Span::styled(format!("  {label}"), theme::fg(Role::Muted))];
    // Runs of one shade become one span: the same picture, far fewer spans.
    let mut run = 0usize;
    let mut shade: Option<Option<u8>> = None;
    let flush = |shade: Option<Option<u8>>, run: usize, spans: &mut Vec<Span>| {
        if run == 0 {
            return;
        }
        let cells = DAY_CELLS * run;
        match shade {
            Some(Some(level)) => spans.push(Span::styled(
                "█".repeat(cells),
                theme::fg(Role::Heat(level)),
            )),
            _ => spans.push(Span::raw(" ".repeat(cells))),
        }
    };
    for cell in cells {
        if shade != Some(*cell) {
            flush(shade, run, &mut spans);
            shade = Some(*cell);
            run = 0;
        }
        run += 1;
    }
    flush(shade, run, &mut spans);
    Line::from_spans(spans).truncate(w)
}

/// The ramp itself, so a shade can be read back as "more" or "less".
///
/// The empty step is left out: it means "a day with nothing on it", and putting
/// it in a scale of amounts invites reading it as the smallest amount.
fn heat_key(less: &str, more: &str, w: usize) -> Line {
    let mut spans = vec![Span::styled(format!("  {less} "), theme::fg(Role::Muted))];
    for level in 1..crate::theme::HEAT {
        spans.push(Span::styled("██".to_string(), theme::fg(Role::Heat(level))));
    }
    spans.push(Span::styled(format!(" {more}"), theme::fg(Role::Muted)));
    Line::from_spans(spans).truncate(w)
}

/// Label/value rows whose values start at the same column.
fn pairs(rows: Vec<(String, String)>) -> Vec<UsageLine> {
    let widest = rows
        .iter()
        .map(|(label, _)| width::str_width(label))
        .max()
        .unwrap_or(0);
    rows.into_iter()
        .map(|(label, value)| UsageLine::Pair {
            label: pad_right(&label, widest + 2),
            value,
        })
        .collect()
}

/// `01:02:03` — the countdown the classic front end shows.
///
/// Seconds and not "about an hour": this is the number a person watches when
/// they are waiting for a window to come back, and rounding it to the nearest
/// hour makes the last minute of the wait look like the first.
fn countdown(seconds: i64) -> String {
    let seconds = seconds.max(0);
    format!(
        "{:02}:{:02}:{:02}",
        seconds / 3600,
        (seconds % 3600) / 60,
        seconds % 60
    )
}

/// The dates under the plot, laid into the plot's own columns.
///
/// Up to five ticks. The ends carry the full date because the year is context
/// the rest of the page does not repeat; the ones between carry `MM-DD`, which
/// is what fits. A tick that would land on the one before it is **pushed right**
/// rather than dropped — it still points at a later day than its neighbour,
/// which is the ordering a reader actually uses the axis for.
fn date_ticks(daily: &[atomcode_host_api::DayUse]) -> String {
    let n = daily.len();
    if n == 1 {
        return daily[0].date.clone();
    }
    let mut buf: Vec<char> = vec![' '; CHART_CELLS];
    let mut used = 0usize;
    let ticks = n.clamp(2, 5);
    for k in 0..ticks {
        let at = k * (n - 1) / (ticks - 1);
        let date = daily[at].date.as_str();
        let last = k + 1 == ticks;
        let label: String = match k == 0 || last {
            true => date.to_string(),
            false => date.get(5..).unwrap_or(date).to_string(),
        };
        let cells = label.chars().count();
        let start = if k == 0 {
            0
        } else if last {
            CHART_CELLS.saturating_sub(cells)
        } else {
            (at * CHART_CELLS.saturating_sub(1) / (n - 1))
                .saturating_sub(cells / 2)
                .min(CHART_CELLS.saturating_sub(cells))
                .max(used)
        };
        for (j, c) in label.chars().enumerate() {
            if start + j < CHART_CELLS {
                buf[start + j] = c;
            }
        }
        used = (start + cells + 1).min(CHART_CELLS);
    }
    buf.into_iter().collect::<String>().trim_end().to_string()
}

/// Per-model spend as an aligned table — the classic front end's, column for
/// column: name, tokens, requests, share.
///
/// Columns rather than a bar each: the numbers are what this section is for,
/// and four numbers read off a column are four comparisons where four bars are
/// one. The day chart above carries the picture.
///
/// The widths are fixed rather than measured from the names, so the columns sit
/// where they sat yesterday. A name too long for its column is elided in the
/// middle — the tail of a model name is where its version is, and that is the
/// half a person is telling two of them apart by.
fn model_table(models: &[atomcode_host_api::ModelUse], total: u64) -> Vec<UsageLine> {
    const NAME_CELLS: usize = 26;
    const TOKEN_CELLS: usize = 10;
    const CALL_CELLS: usize = 9;
    const SHARE_CELLS: usize = 7;
    let lay = |name: &str, tokens: &str, calls: &str, share: &str| {
        format!(
            "{}{}{}{}",
            pad_right(name, NAME_CELLS),
            pad_left(tokens, TOKEN_CELLS),
            pad_left(calls, CALL_CELLS),
            pad_left(share, SHARE_CELLS),
        )
    };
    let mut out = vec![UsageLine::Cols {
        text: lay("模型", "tokens", "请求", "占比"),
        head: true,
        mark: None,
    }];
    out.extend(models.iter().enumerate().map(|(which, model)| {
        let share = match total {
            0 => 0.0,
            t => model.tokens as f64 / t as f64 * 100.0,
        };
        UsageLine::Cols {
            text: lay(
                &width::elide_middle(&model.name, NAME_CELLS - 1),
                &crate::content::token_count_u64(model.tokens),
                &model.requests.to_string(),
                &format!("{share:.0}%"),
            ),
            head: false,
            // The dot is the legend. It is the same index the chart coloured
            // the line by, which is the whole reason the series arrive in the
            // order the table is in.
            mark: Some(u8::try_from(which).unwrap_or(NO_SERIES)),
        }
    }));
    out
}

/// Pad to `cells` display columns, text at the left.
fn pad_right(text: &str, cells: usize) -> String {
    let have = width::str_width(text);
    format!("{text}{}", " ".repeat(cells.saturating_sub(have)))
}

/// Pad to `cells` display columns, text at the right — for a column of numbers,
/// where the digits have to line up or the column is decoration.
fn pad_left(text: &str, cells: usize) -> String {
    let have = width::str_width(text);
    format!("{}{text}", " ".repeat(cells.saturating_sub(have)))
}

/// One plot row: the gutter, then the line where it passes through this row.
///
/// Braille when the terminal has it, and `'` / `:` / `.` when it does not —
/// three ASCII cells for "high in this row", "through it" and "low in it",
/// which keeps the line readable as a line at one eighth the resolution. The
/// axis beside it is what makes even that much readable: the shape degrades,
/// the numbers do not.
///
/// Runs of one colour become one span rather than one span per cell: the same
/// picture, a fifth of the spans, and a diff of the frame that reads.
fn chart_row(axis: &str, dots: &[u8], ink: &[u8], w: usize, caps: crate::caps::Caps) -> Line {
    let mut spans = vec![Span::styled(format!("  {axis} "), theme::fg(Role::Muted))];
    let mut run = String::new();
    let mut run_ink = NO_SERIES;
    for (cell, mask) in dots.iter().enumerate() {
        let here = ink.get(cell).copied().unwrap_or(NO_SERIES);
        if here != run_ink && !run.is_empty() {
            spans.push(Span::styled(std::mem::take(&mut run), series_ink(run_ink)));
        }
        run_ink = here;
        run.push(dot_cell(*mask, caps));
    }
    if !run.is_empty() {
        spans.push(Span::styled(run, series_ink(run_ink)));
    }
    Line::from_spans(spans).truncate(w)
}

/// The ink a chart cell is drawn in: its series' colour, or the page's accent
/// when the chart is not split by model.
fn series_ink(which: u8) -> Style {
    match which {
        NO_SERIES => theme::fg(Role::Accent),
        n => theme::fg(Role::Series(n % crate::theme::SERIES)),
    }
}

/// One cell of the plot, at whatever resolution the terminal has.
fn dot_cell(mask: u8, caps: crate::caps::Caps) -> char {
    if mask == 0 {
        return ' ';
    }
    if caps.unicode {
        return char::from_u32(0x2800 + u32::from(mask)).unwrap_or(' ');
    }
    // Which half of the cell the line is in, as one ASCII character.
    let high = mask & (DOTS[0][0] | DOTS[0][1] | DOTS[1][0] | DOTS[1][1]) != 0;
    let low = mask & (DOTS[2][0] | DOTS[2][1] | DOTS[3][0] | DOTS[3][1]) != 0;
    match (high, low) {
        (true, true) => ':',
        (true, false) => '\'',
        _ => '.',
    }
}

/// One row of the per-model table, with the dot that ties it to its line.
fn table_row(text: &str, head: bool, mark: Option<u8>, w: usize) -> Line {
    let (dot, ink) = match mark {
        Some(which) => ("● ", series_ink(which)),
        None => ("  ", Style::new()),
    };
    let rest = match head {
        true => theme::fg(Role::Muted),
        false => Style::new(),
    };
    Line::from_spans(vec![
        Span::styled(format!("  {dot}"), ink),
        Span::styled(text.to_string(), rest),
    ])
    .truncate(w)
}

/// One edge of the search box: `┌───┐` above, `└───┘` below.
///
/// Drawn through [`Caps`] rather than with literal box characters, so a terminal
/// that cannot show them gets `+---+` instead of a row of question marks. The
/// panel is a frame in the shape it draws, not a claim about the font.
///
/// The corners stand in [`BORDER_COL`] and the run carries out to the last cell,
/// which is what makes them line up with the walls of the rows between them.
/// They used to be pushed one cell right by a leading space, so the top-left
/// corner sat over the `│` under it by exactly that cell — the misalignment the
/// `BORDER_COL` constant is here to stop happening again.
fn box_edge(w: usize, caps: crate::caps::Caps, top: bool) -> Line {
    use crate::caps::Glyph;
    if w == 0 {
        return Line::empty();
    }
    // Narrower than a frame is not a frame: below this there is no room for two
    // corners and a run between them, and half a box reads as damage. A plain
    // rule instead, which still reads as "a box is here, it just does not fit".
    if w < 4 {
        return Line::styled(caps.g(Glyph::Horizontal).repeat(w), theme::fg(Role::Border))
            .truncate(w);
    }
    let (left, right) = match top {
        true => (Glyph::TopLeft, Glyph::TopRight),
        false => (Glyph::BottomLeft, Glyph::BottomRight),
    };
    let run = w.saturating_sub(2 + BORDER_COL);
    let mut spans = vec![Span::styled(" ".repeat(BORDER_COL), Style::new())];
    spans.push(Span::styled(
        format!("{}{}", caps.g(left), caps.g(Glyph::Horizontal).repeat(run)),
        theme::fg(Role::Border),
    ));
    spans.push(Span::styled(
        caps.g(right).to_string(),
        theme::fg(Role::Border),
    ));
    Line::from_spans(spans).truncate(w)
}

/// The panel's own top or bottom rule: a straight line, all the way across.
///
/// **No corners.** The frame it draws had them, and on a terminal the pair of
/// them at the left read as a second box around the panel — a `┌` over a `┌`,
/// which says "here is another container" when what it means is "the panel
/// starts here". A rule is enough to say that, and it does not compete with the
/// one box the panel actually has: the search field's.
///
/// Drawn through [`Caps`], so an ASCII terminal gets `-` rather than `─`.
fn panel_edge(w: usize, caps: crate::caps::Caps) -> Line {
    use crate::caps::Glyph;
    if w == 0 {
        return Line::empty();
    }
    Line::styled(caps.g(Glyph::Horizontal).repeat(w), theme::fg(Role::Border)).truncate(w)
}

/// The search box's text, with a caret while the box has the keyboard.
///
/// No placeholder caption. The box is drawn as a box and the caret is in it,
/// which says "type here" better than a sentence about a key — and the key that
/// sentence used to name is gone: it was the character it ate.
fn search_line(query: &str, caret: Option<usize>, w: usize, caps: crate::caps::Caps) -> Line {
    use crate::caps::Glyph;
    if w == 0 {
        return Line::empty();
    }
    // Too narrow for a frame: the text stands alone rather than behind a
    // one-cell wall that would eat it.
    let (lead, base) = if w < 4 {
        (String::new(), theme::fg(Role::Muted))
    } else {
        (
            format!("{} ", caps.g(Glyph::Vertical)),
            theme::fg(Role::Warning),
        )
    };
    let mut spans = vec![Span::styled(lead, theme::fg(Role::Border))];
    match caret {
        Some(at) => spans.extend(caret_spans(query, at, base, w.saturating_sub(2))),
        None => spans.push(Span::styled(query.to_string(), base)),
    }
    Line::from_spans(spans).truncate(w)
}

/// The text being typed into a row, with its caret.
///
/// The label stays on the left, so the value being typed is still named: a field
/// that took the whole row would leave the person looking at a number with
/// nothing saying what it is a number *of*.
fn edit_line(label: &str, value: &str, caret: usize, w: usize, caps: crate::caps::Caps) -> Line {
    let base = theme::fg(Role::Warning);
    let label_room = w
        .saturating_sub(LEAD)
        .saturating_sub(2)
        .clamp(LABEL_MIN, LABEL_MAX);
    let label = width::take_width(label, label_room);
    let pad = label_room.saturating_sub(width::str_width(&label));
    let mut spans = vec![
        Span::styled(format!("{} ", caps.g(crate::caps::Glyph::Pointer)), base),
        Span::styled(label, base),
        Span::styled(" ".repeat(pad + 2), base),
    ];
    // The field gets what is left after the label, and the caret is drawn inside
    // that — a value longer than the room scrolls off the end rather than
    // wrapping, which is what every field on a terminal does.
    let room = w.saturating_sub(LEAD + label_room + 2);
    spans.extend(caret_spans(value, caret, base, room));
    Line::from_spans(spans).truncate(w)
}

/// `text` split around `at`, with a block where the next character goes.
///
/// The caret is drawn *over* the character at `at` rather than before it, which
/// is what a terminal caret does and what makes the end of a line work: there is
/// no character to sit on, so a block is appended instead and both cases read
/// the same.
fn caret_spans(text: &str, at: usize, base: Style, room: usize) -> Vec<Span> {
    let at = at.min(text.len());
    // Snap to a character boundary: a byte offset from the middle of a
    // multi-byte character would panic on the slice, and every path that moves
    // the caret already keeps it on a boundary — this is the belt to that
    // braces, on the one function that slices.
    let at = (0..=at)
        .rev()
        .find(|i| text.is_char_boundary(*i))
        .unwrap_or(0);
    let before = &text[..at];
    let rest = &text[at..];
    let mut slots = rest.chars();
    let on = slots.next();
    let after: String = slots.collect();
    let caret = Style::new().reverse();
    let mut spans = vec![Span::styled(before.to_string(), base)];
    match on {
        Some(c) => spans.push(Span::styled(c.to_string(), caret)),
        None => spans.push(Span::styled(" ", caret)),
    }
    spans.push(Span::styled(after, base));
    // The caret's own cell comes out of the room, so a full-width line still
    // has somewhere to put it.
    let _ = room;
    spans
}

/// One setting's row: the label, then the value, then when a change lands.
fn setting_line(
    settings: &SettingsView,
    panel: &Panel,
    index: usize,
    w: usize,
    caps: crate::caps::Caps,
) -> Line {
    let shown = settings.matching(&panel.query);
    let Some(row) = shown.get(index) else {
        return Line::empty();
    };
    let here = panel.is_pointed_at(index);
    let base = if here {
        theme::bg(Role::PanelSelBg).under(theme::fg(Role::PanelFg))
    } else {
        Style::new()
    };
    // The label column is as wide as the longest label that fits, so the values
    // line up in a column of their own — a list of key/value pairs read as a
    // column is the whole point of a list like this.
    let label_room = w
        .saturating_sub(LEAD)
        .saturating_sub(2)
        .clamp(LABEL_MIN, LABEL_MAX);
    let label = width::take_width(&row.label, label_room);
    let pad = label_room.saturating_sub(width::str_width(&label));

    let mut spans = vec![Span::styled(
        if here {
            // The glyph comes from the frame's caps, never from `Caps::detect()`:
            // a render that read the environment would be right on the machine
            // that wrote it and silently wrong on the user's, and it would make
            // the whole test loop depend on `TERM`.
            format!("{} ", caps.g(crate::caps::Glyph::Pointer))
        } else {
            "  ".to_string()
        },
        base,
    )];
    spans.push(Span::styled(label, base));
    spans.push(Span::styled(" ".repeat(pad + 2), base));
    spans.push(Span::styled(
        value_text(row),
        if here { base } else { theme::fg(Role::Accent) },
    ));
    if here {
        if let Some(hint) = kind_hint(&row.kind) {
            spans.push(Span::styled(format!("  {hint}"), base));
        }
        spans.push(Span::styled(format!("  · {}", row.applies.say()), base));
    }
    pad_to(Line::from_spans(spans), w, base)
}

/// What the value column says.
///
/// An unset optional setting shows the word for "unset" rather than nothing: an
/// empty column beside a label reads as a setting that failed to load, and the
/// three-stage cycle is exactly what a person needs to see to know a third
/// state exists.
fn value_text(row: &crate::settings::SettingRow) -> String {
    if row.value.is_empty() {
        match row.kind {
            SettingKind::OptionalBoolean => "auto".to_string(),
            _ => "（未设置）".to_string(),
        }
    } else {
        row.value.clone()
    }
}

/// A short word for the gesture, shown only on the pointed-at row.
///
/// No `←→` for the choice kinds. There never was: nothing bound left and right
/// to the value, so the hint named a key that did nothing — and now that they
/// switch pages, it would name a key that does something else. A hint is a
/// promise about what a key does, and the only key that changes a value here is
/// the return key.
fn kind_hint(kind: &SettingKind) -> Option<&'static str> {
    match kind {
        SettingKind::Boolean => Some("回车 切换"),
        SettingKind::OptionalBoolean | SettingKind::Choice(_) => Some("回车 切换"),
        SettingKind::Integer { .. } | SettingKind::Text => Some("回车 编辑"),
    }
}

/// What the legend says, which depends on what the keys would do right now.
///
/// No longer brands a key for the search box: there is none. Typing goes to the
/// box, so the legend says what Escape would do *instead* — the only thing about
/// the box a person has to be told.
fn legend(panel: &Panel) -> Vec<(&'static str, &'static str)> {
    if panel.editing.is_some() {
        return vec![("⏎", "保存"), ("esc", "取消")];
    }
    if panel.pending_reset.is_some() {
        // Says what the next press does, because that is the only thing about
        // this state a person has to know — and it is the press that throws
        // something away.
        return vec![("del", "再按一次恢复默认"), ("其它键", "取消")];
    }
    let mut out = vec![("↑↓", "选择"), ("⏎", "修改"), ("del", "恢复默认")];
    if !panel.query.is_empty() {
        out.push(("esc", "清空搜索"));
    }
    out.push(("esc", "关闭"));
    out
}

fn pad_to(line: Line, w: usize, style: Style) -> Line {
    let used = line.width();
    if used >= w {
        return line.truncate(w);
    }
    let mut spans = line.spans;
    spans.push(Span::styled(" ".repeat(w - used), style));
    Line::from_spans(spans).truncate(w)
}

/// Which screen row each setting is on, for a click to read.
///
/// Built by the same [`layout`] the frame used, so a click and the row it lights
/// up cannot come from two different arrangements of one panel.
pub struct Geometry {
    rows: Vec<Option<usize>>,
}

impl Geometry {
    /// Which setting is on this screen row, if one is.
    ///
    /// `row` is measured from the top of the panel's rect, which is what the
    /// host knows and what a click carries.
    pub fn setting_at(&self, row: usize) -> Option<usize> {
        self.rows.get(row).copied().flatten()
    }
}

/// The layout the panel drew, for the host to read a click against.
pub fn geometry(moment: &Moment, vp: &Viewport<'_>) -> Geometry {
    let Some(panel) = moment.settings_panel.as_ref() else {
        return Geometry { rows: Vec::new() };
    };
    let rows = if vp.rect.w == 0 || vp.rect.h == 0 {
        Vec::new()
    } else {
        layout(
            &moment.settings,
            panel,
            moment.usage.as_ref(),
            vp.rect.w as usize,
            vp.rect.h as usize,
        )
    };
    Geometry {
        rows: rows
            .iter()
            .map(|r| match r {
                Row::Setting { index } => Some(*index),
                // An edit in progress is not clickable: the keyboard is already
                // in it, and a click on it would only move the highlight under
                // the person's hands.
                _ => None,
            })
            .collect(),
    }
}

/// One line of the panel's own prose, for the states a caller wants to test
/// without composing a frame.
pub fn value_column(row: &crate::settings::SettingRow) -> String {
    value_text(row)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::Rect;
    use crate::module::{Mounted, ViewObject};
    use crate::settings::{Applies, Edit, SettingRow};
    use crate::surface::Key;

    fn row(id: &str, label: &str, value: &str, kind: SettingKind) -> SettingRow {
        SettingRow {
            id: id.into(),
            label: label.into(),
            value: value.into(),
            kind,
            applies: Applies::Reload,
        }
    }

    fn two() -> SettingsView {
        SettingsView::new(vec![
            row(
                "ui.theme",
                "主题",
                "dark",
                SettingKind::Choice(vec!["auto".into(), "dark".into(), "light".into()]),
            ),
            row(
                "coding.max_rounds",
                "单回合最大轮数",
                "50",
                SettingKind::Integer { min: 0, max: 10000 },
            ),
        ])
    }

    fn moment(panel: Option<Panel>, settings: SettingsView) -> Moment {
        Moment {
            settings,
            settings_panel: panel,
            ..Moment::default()
        }
    }

    fn lines(m: &Moment, w: u16, h: u16) -> Vec<Line> {
        let vp = Viewport::new(Rect::sized(w, h), m);
        Settings::render(&State, &vp)
    }

    fn drawn(m: &Moment, w: u16, h: u16) -> Vec<String> {
        lines(m, w, h).into_iter().map(|l| l.plain()).collect()
    }

    fn mounted() -> Arc<Mounted<Settings>> {
        Arc::new(Mounted::<Settings>::new())
    }
    use std::sync::Arc;

    fn usage_page(page: crate::settings::UsagePage) -> Moment {
        let mut panel = Panel::new();
        panel.show(crate::settings::Tab::Usage);
        Moment {
            settings: two(),
            settings_panel: Some(panel),
            usage: Some(page),
            ..Moment::default()
        }
    }

    fn window(label: &str, used: Option<u8>) -> atomcode_host_api::UsageWindow {
        atomcode_host_api::UsageWindow {
            label: label.into(),
            exhausted: false,
            resets_at: "14:30".into(),
            resets_in_seconds: 3600,
            window_seconds: 18_000,
            used_percent: used,
            calls_used: used.map(|_| 420),
            call_limit: Some(1000),
        }
    }

    /// The Usage page draws what the account service counted, and says when it
    /// counted nothing.
    ///
    /// The second half is the criterion that matters. A bar is read as a
    /// proportion of the thing it is named after, so a window whose percentage
    /// the host did not report must not get an empty track — that reads as
    /// "none used", which is a claim, and the honest answer is that nobody
    /// said. The first version of this page had no percentage to draw at all
    /// (`usage_percent` was being dropped in `daemon/runtime_host.rs`) and drew
    /// time through the window instead; this pins the fixed behaviour.
    #[test]
    fn the_usage_page_draws_what_was_counted_and_no_bar_for_what_was_not() {
        let counted = usage_page(crate::settings::UsagePage {
            context: None,
            plan: None,
            windows: vec![window("5 小时", Some(42))],
            stats: None,
        });
        let shown = drawn(&counted, 80, 20).join("\n");
        assert!(shown.contains("5 小时"), "{shown}");
        assert!(shown.contains("用掉 42%"), "the counted share: {shown}");
        assert!(
            shown.contains("420 / 1000 次"),
            "and what it counted: {shown}"
        );
        assert!(shown.contains('█'), "with a bar: {shown}");

        // Two windows at different shares draw bars of different lengths. This
        // is the assertion that catches a track drawn in the *same* glyph as
        // its fill — which is what the first version did, so every bar looked
        // full to anything that does not read colour: a terminal without it,
        // and this test. It lived on the per-model bars until those became a
        // table; the protection belongs wherever a bar still is.
        let pair = usage_page(crate::settings::UsagePage {
            context: None,
            plan: None,
            windows: vec![window("5 小时", Some(90)), window("每周", Some(10))],
            stats: None,
        });
        let shown = drawn(&pair, 80, 24).join("\n");
        let bars: Vec<usize> = shown
            .lines()
            .filter(|line| line.contains('█'))
            .map(|line| line.matches('█').count())
            .collect();
        assert_eq!(bars.len(), 2, "one bar per window: {shown}");
        assert!(
            bars[0] > bars[1],
            "the fuller window has the longer bar: {bars:?}\n{shown}"
        );

        let uncounted = usage_page(crate::settings::UsagePage {
            context: None,
            plan: None,
            windows: vec![window("每周", None)],
            stats: None,
        });
        let shown = drawn(&uncounted, 80, 20).join("\n");
        assert!(shown.contains("没报用量"), "says nobody said: {shown}");
        assert!(
            !shown.contains('█'),
            "and draws no bar, because an empty one would claim zero: {shown}"
        );
    }

    /// Per-model spend and the day series reach the page.
    ///
    /// These are the figures the classic front end's `/usage` showed and the
    /// row-assembled screen could not: they come from the account service
    /// (`UsageStats`), not from the allowance windows, and they had no way
    /// across the contract at all.
    #[test]
    fn the_usage_page_shows_what_went_through_per_model_and_per_day() {
        use atomcode_host_api::{DayUse, ModelSeries, ModelUse, UsageStats};
        let page = usage_page(crate::settings::UsagePage {
            context: None,
            plan: None,
            windows: Vec::new(),
            stats: Some(UsageStats {
                from: "2026-08-21".into(),
                to: "2026-09-20".into(),
                models: vec![
                    ModelUse {
                        name: "deepseek-flash".into(),
                        tokens: 221_100_000,
                        requests: 1604,
                    },
                    ModelUse {
                        name: "glm5.3-flash-pro".into(),
                        tokens: 59_700_000,
                        requests: 846,
                    },
                ],
                daily: vec![
                    DayUse {
                        date: "2026-09-19".into(),
                        tokens: 4_500_000,
                        requests: 40,
                    },
                    DayUse {
                        date: "2026-09-20".into(),
                        tokens: 216_600_000,
                        requests: 1600,
                    },
                ],
                series: vec![
                    ModelSeries {
                        name: "deepseek-flash".into(),
                        daily: vec![4_500_000, 216_600_000],
                    },
                    ModelSeries {
                        name: "glm5.3-flash-pro".into(),
                        daily: vec![0, 59_700_000],
                    },
                ],
                total_tokens: 280_800_000,
                total_requests: 2450,
            }),
        });
        let shown = drawn(&page, 96, 60).join("\n");
        assert!(shown.contains("deepseek-flash"), "{shown}");
        assert!(
            shown.contains("221.1m"),
            "tokens as a person reads them: {shown}"
        );
        assert!(shown.contains("1604"), "and requests: {shown}");
        assert!(shown.contains("2450"), "the overview totals: {shown}");
        assert!(
            shown.contains("2026-08-21") && shown.contains("2026-09-20"),
            "and the span it covers: {shown}"
        );

        // The chart has an axis, which is the whole difference between a shape
        // and a measurement. The first version of this page drew one row of
        // blocks and put the peak in words beside it — that says "it spiked",
        // not "how much". The fractions are the classic front end's: the peak
        // at the top, then the row's share of the way down to zero.
        assert!(shown.contains("216.6m"), "the top of the axis: {shown}");
        assert!(
            shown.contains("130m") && shown.contains("43.3m"),
            "and the heights between it and zero: {shown}"
        );
        // The bottom row is the floor *and* a row of the plot: the flat run
        // along it is the line at zero. A chart that drew a rule there instead
        // would be claiming a baseline it had not measured.
        let floor = shown
            .lines()
            .position(|line| line.trim_start().starts_with("0 "))
            .unwrap_or_else(|| panic!("a row labelled zero: {shown}"));
        let rows: Vec<&str> = shown.lines().collect();
        assert!(
            rows[floor].chars().any(is_plot_ink),
            "with the line drawn along it: {:?}",
            rows[floor]
        );
        assert!(
            rows[floor + 1].contains("2026-09-19") && rows[floor + 1].contains("2026-09-20"),
            "and both ends of the series directly under it: {shown}"
        );

        // The per-model figures are a table, and a table whose columns do not
        // line up is four numbers in a row. Asserted on where the numbers
        // *end*, because that is what right-aligning them is for.
        assert!(
            shown.contains("模型") && shown.contains("请求") && shown.contains("占比"),
            "the columns are named: {shown}"
        );
        let ends = |needle: &str| -> usize {
            let line = shown
                .lines()
                .find(|line| line.contains(needle))
                .unwrap_or_else(|| panic!("{needle} is on the page: {shown}"));
            line.find(needle).expect("just found it") + needle.len()
        };
        assert_eq!(
            ends("221.1m"),
            ends("59.7m"),
            "the token column lines up: {shown}"
        );
        assert_eq!(
            ends("1604"),
            ends(" 846"),
            "and the request column: {shown}"
        );
    }

    /// The dot beside a model is the chart's legend, so it has to be that
    /// model's line's colour — and two models' lines have to differ.
    ///
    /// The colours are the point of drawing several lines at all: without them
    /// the chart is one shape with no way to say whose. Asserted on the styles
    /// rather than the text, because this is the one thing about this page that
    /// the characters cannot show.
    #[test]
    fn each_model_is_drawn_in_its_own_colour_and_its_row_carries_it() {
        use atomcode_host_api::{DayUse, ModelSeries, ModelUse, UsageStats};
        let page = usage_page(crate::settings::UsagePage {
            context: None,
            plan: None,
            windows: Vec::new(),
            stats: Some(UsageStats {
                from: "2026-09-19".into(),
                to: "2026-09-20".into(),
                models: vec![
                    ModelUse {
                        name: "big".into(),
                        tokens: 200,
                        requests: 2,
                    },
                    ModelUse {
                        name: "small".into(),
                        tokens: 100,
                        requests: 1,
                    },
                ],
                daily: vec![
                    DayUse {
                        date: "2026-09-19".into(),
                        tokens: 300,
                        requests: 3,
                    },
                    DayUse {
                        date: "2026-09-20".into(),
                        tokens: 0,
                        requests: 0,
                    },
                ],
                // Different heights, so the two lines cannot land on the same
                // cells and the colours are actually being told apart.
                series: vec![
                    ModelSeries {
                        name: "big".into(),
                        daily: vec![200, 0],
                    },
                    ModelSeries {
                        name: "small".into(),
                        daily: vec![100, 0],
                    },
                ],
                total_tokens: 300,
                total_requests: 3,
            }),
        });
        let rows = lines(&page, 96, 60);
        let ink_of = |needle: &str| -> crate::frame::Style {
            // The row of the *table*, named by its dot. A plain name match
            // found the overview's "most used model" line instead once the page
            // grew that far — a criterion that reaches for the first line
            // mentioning something is a criterion about page order.
            let row = rows
                .iter()
                .find(|row| row.plain().contains(needle) && row.plain().contains('●'))
                .unwrap_or_else(|| panic!("{needle} has a row in the table"));
            row.spans
                .iter()
                .find(|span| span.text.contains('●'))
                .unwrap_or_else(|| panic!("{needle}'s row carries a legend dot: {row:?}"))
                .style
        };
        let big = ink_of("big");
        let small = ink_of("small");
        assert_ne!(
            big, small,
            "two models, two colours — one colour is no legend"
        );

        // And each dot's colour is drawn somewhere in the plot, which is what
        // makes it a legend rather than a decoration beside a name.
        let plotted: Vec<crate::frame::Style> = rows
            .iter()
            .filter(|row| row.plain().chars().any(is_plot_ink))
            .flat_map(|row| row.spans.iter())
            .filter(|span| span.text.chars().any(is_plot_ink))
            .map(|span| span.style)
            .collect();
        for (name, ink) in [("big", big), ("small", small)] {
            assert!(
                plotted.contains(&ink),
                "{name}'s colour is on the chart: {plotted:?}"
            );
        }
    }

    /// Every day is on the chart, wherever in the span it fell.
    ///
    /// A chart that quietly showed the last 52 of 90 days would answer a
    /// different question from the one its heading asks, and it would answer it
    /// without saying so — the shape would look reasonable either way, which is
    /// why this is a criterion and not something to eyeball. The spending is
    /// put at the very *start* of the span on purpose: that is the end such a
    /// chart would drop.
    #[test]
    fn a_long_run_of_days_keeps_the_days_at_both_ends() {
        use atomcode_host_api::{DayUse, UsageStats};
        let daily: Vec<DayUse> = (0..90)
            .map(|d| DayUse {
                date: format!("2026-{:02}-{:02}", 6 + d / 30, 1 + d % 30),
                tokens: if d < 3 { 9_000_000 } else { 0 },
                requests: if d < 3 { 30 } else { 0 },
            })
            .collect();
        let first = daily[0].date.clone();
        let last = daily[89].date.clone();
        let page = usage_page(crate::settings::UsagePage {
            context: None,
            plan: None,
            windows: Vec::new(),
            stats: Some(UsageStats {
                from: first.clone(),
                to: last.clone(),
                models: Vec::new(),
                daily,
                // Not broken down by model: this host counts days only, and the
                // chart still has to draw. One line, in the page's own ink.
                series: Vec::new(),
                total_tokens: 27_000_000,
                total_requests: 90,
            }),
        });
        let shown = drawn(&page, 96, 60).join("\n");
        assert!(
            shown.contains("9m"),
            "the peak is on the axis even though it is at the far left: {shown}"
        );
        let top = shown
            .lines()
            .find(|line| line.contains("9m") && line.chars().any(is_plot_ink))
            .unwrap_or_else(|| panic!("the top row of the plot has the line on it: {shown}"));
        let ink = top.chars().position(is_plot_ink).expect("just found it");
        let axis = top.find("9m").expect("the label") + 2;
        assert!(
            ink - axis < CHART_CELLS / 4,
            "and it is drawn at the left, where those days are: {top:?}"
        );
        assert!(
            shown.contains(&first) && shown.contains(&last),
            "both ends of ninety days are named: {shown}"
        );
    }

    /// The plan behind the windows is drawn, and an expired one is drawn too.
    ///
    /// The second half is the criterion. A plan that ran out is the case a
    /// person most needs told, and the easy mistake is to treat "not active"
    /// as "nothing to show" — which looks exactly like never having had a
    /// plan. Both states have to reach the page, and say which they are.
    #[test]
    fn a_plan_is_drawn_whether_or_not_it_still_runs() {
        use atomcode_host_api::Entitlement;
        let page = |active: bool, left: i32| {
            usage_page(crate::settings::UsagePage {
                context: None,
                plan: Some(Entitlement {
                    plan: "CodingPlan Pro".into(),
                    active,
                    claimed_at: "2026-07-30".into(),
                    expires_at: "2036-07-30".into(),
                    remaining_days: left,
                    total_days: 3653,
                }),
                windows: Vec::new(),
                stats: None,
            })
        };
        let live = drawn(&page(true, 3601), 80, 24).join("\n");
        assert!(live.contains("CodingPlan Pro"), "the plan's name: {live}");
        assert!(live.contains("生效中"), "and that it runs: {live}");
        assert!(
            live.contains("领取 2026-07-30") && live.contains("到期 2036-07-30"),
            "both dates: {live}"
        );
        assert!(live.contains("剩余 3601/3653 天"), "and the days: {live}");
        // 52 of 3653 days gone. The bar is of the term, so it is nearly empty —
        // a bar that filled as days *remained* would read as "nearly out" on
        // day one.
        assert!(
            live.contains("1.4%"),
            "how much of the term is gone: {live}"
        );

        let over = drawn(&page(false, 0), 80, 24).join("\n");
        assert!(
            over.contains("CodingPlan Pro") && over.contains("已过期"),
            "an expired plan is still drawn, and says so: {over}"
        );
    }

    /// The calendar has a square per day in the span and nothing outside it,
    /// and a busy day is a different shade from a quiet one.
    ///
    /// The blanks are the point: the span starts on a Thursday, so Sunday to
    /// Wednesday of that first week are days that did not happen. A grid that
    /// filled them in would be claiming four days of data it was never given,
    /// and would put every later day on the wrong weekday.
    #[test]
    fn the_calendar_starts_on_the_first_real_day_and_shades_by_how_much() {
        use atomcode_host_api::{DayUse, UsageStats};
        // 2026-07-23 is a Thursday.
        let daily: Vec<DayUse> = (0..14)
            .map(|d| DayUse {
                date: format!("2026-07-{:02}", 23 + d),
                tokens: match d {
                    0 => 1,
                    7 => 1_000_000,
                    _ => 0,
                },
                requests: 0,
            })
            .collect();
        let page = usage_page(crate::settings::UsagePage {
            context: None,
            plan: None,
            windows: Vec::new(),
            stats: Some(UsageStats {
                from: "2026-07-23".into(),
                to: "2026-08-05".into(),
                models: Vec::new(),
                daily,
                series: Vec::new(),
                total_tokens: 1_000_001,
                total_requests: 2,
            }),
        });
        let rows = lines(&page, 92, 60);
        let row = |name: &str| -> &crate::frame::Line {
            rows.iter()
                .find(|row| row.plain().trim_start().starts_with(name))
                .unwrap_or_else(|| panic!("a {name} row"))
        };
        // Thursday is the first day of the span, so its row starts at the very
        // first column; Sunday's first two days are outside it, so its row
        // begins with a blank week.
        // A square per day in the span, and not one more. This is the whole
        // claim: pad the grid out to a rectangle and this goes up, drop the
        // days that did not fit a column and it goes down.
        let squares: usize = WEEKDAYS
            .iter()
            .map(|name| row(name).plain().chars().filter(|c| *c == '█').count())
            .sum();
        assert_eq!(squares, 14 * DAY_CELLS, "fourteen days, fourteen squares");
        assert!(
            row("Thu").plain().starts_with("  Thu █"),
            "Thursday is the first day of the span, so it starts at the edge: {:?}",
            row("Thu").plain()
        );
        assert!(
            row("Sun")
                .plain()
                .starts_with(&format!("  Sun{}", " ".repeat(DAY_CELLS))),
            "the Sunday before the span is left blank: {:?}",
            row("Sun").plain()
        );

        // The two working days differ by six orders of magnitude, so they must
        // not be the same shade — that is the whole claim a heat map makes.
        let shade_at = |name: &str, nth: usize| -> crate::frame::Style {
            row(name)
                .spans
                .iter()
                .filter(|span| span.text.contains('█'))
                .nth(nth)
                .unwrap_or_else(|| panic!("{name} has {} filled runs", nth + 1))
                .style
        };
        assert_ne!(
            shade_at("Thu", 0),
            shade_at("Thu", 1),
            "one token and a million are not the same shade"
        );
    }

    /// The seven figures the classic front end showed are all here, in a column
    /// whose values line up.
    ///
    /// Lining up is asserted because it was a reported bug over there: padding
    /// CJK labels by character count puts every value at a different terminal
    /// column, since `请求次数` is four characters and eight cells. Four of the
    /// seven figures were missing here entirely.
    #[test]
    fn the_overview_says_all_seven_figures_with_the_values_in_one_column() {
        use atomcode_host_api::{DayUse, ModelUse, UsageStats};
        // Two runs of working days: three, then a gap, then two that reach the
        // end — so "longest" and "current" cannot be the same number, and a
        // page that computed one and printed it twice would go red.
        let pattern = [true, true, true, false, false, true, true];
        let daily: Vec<DayUse> = pattern
            .iter()
            .enumerate()
            .map(|(d, busy)| DayUse {
                date: format!("2026-07-{:02}", 23 + d),
                tokens: match (busy, d) {
                    (true, 6) => 900,
                    (true, _) => 100,
                    _ => 0,
                },
                requests: 1,
            })
            .collect();
        let page = usage_page(crate::settings::UsagePage {
            context: None,
            plan: None,
            windows: Vec::new(),
            stats: Some(UsageStats {
                from: "2026-07-23".into(),
                to: "2026-07-29".into(),
                models: vec![
                    ModelUse {
                        name: "the-one-it-uses".into(),
                        tokens: 1200,
                        requests: 4,
                    },
                    ModelUse {
                        name: "the-other".into(),
                        tokens: 100,
                        requests: 1,
                    },
                ],
                daily,
                series: Vec::new(),
                total_tokens: 1300,
                total_requests: 5,
            }),
        });
        let shown = drawn(&page, 92, 60).join("\n");
        for wanted in [
            "最常用模型",
            "总 Token 数",
            "请求次数",
            "活跃天数",
            "最活跃日期",
            "最长连续天数",
            "当前连续天数",
        ] {
            assert!(shown.contains(wanted), "{wanted} is on the page: {shown}");
        }
        // Biggest first is how the host sends them, so the head of the list is
        // the answer.
        assert!(shown.contains("the-one-it-uses"), "{shown}");
        assert!(
            shown.contains("5 / 7"),
            "five working days of seven: {shown}"
        );
        assert!(
            shown.contains("2026-07-29"),
            "the busiest day, not the last one: {shown}"
        );
        assert!(shown.contains("最长连续天数  3 天"), "{shown}");
        assert!(shown.contains("当前连续天数  2 天"), "{shown}");

        // Every value starts at the same column. Measured in display cells, not
        // characters — which is the whole point.
        let starts: Vec<usize> = shown
            .lines()
            .filter(|line| line.contains("连续天数") || line.contains("请求次数"))
            .map(|line| {
                let at = line.rfind("  ").expect("two spaces before the value");
                crate::width::str_width(&line[..at])
            })
            .collect();
        assert_eq!(starts.len(), 3, "three of the pairs: {shown}");
        assert!(
            starts.windows(2).all(|pair| pair[0] == pair[1]),
            "the values line up: {starts:?}\n{shown}"
        );
    }

    /// A window that is spent says so with the bar, and the countdown is exact.
    #[test]
    fn a_spent_window_is_drawn_as_a_warning_and_counted_down_to_the_second() {
        let mut spent = window("每周", Some(100));
        spent.exhausted = true;
        spent.resets_in_seconds = 11;
        let page = usage_page(crate::settings::UsagePage {
            context: None,
            plan: None,
            windows: vec![spent],
            stats: None,
        });
        let rows = lines(&page, 80, 24);
        let shown: Vec<String> = rows.iter().map(|row| row.plain()).collect();
        assert!(
            shown
                .iter()
                .any(|line| line.contains("剩余重置时间 00:00:11")),
            "to the second, because that is the number being watched: {shown:?}"
        );
        let bar = rows
            .iter()
            .find(|row| row.plain().contains('█'))
            .expect("a bar");
        let filled = bar
            .spans
            .iter()
            .find(|span| span.text.contains('█'))
            .expect("the filled part");
        assert_eq!(
            filled.style,
            theme::fg(Role::Error),
            "a spent window's bar is a warning, not the page's own ink"
        );
    }

    /// A braille cell with anything in it — the ink the plot is drawn with.
    fn is_plot_ink(c: char) -> bool {
        ('\u{2801}'..='\u{28FF}').contains(&c)
    }

    /// Before the host has answered, the page says so rather than showing zero.
    #[test]
    fn the_usage_page_says_it_is_asking_before_it_has_an_answer() {
        let mut panel = Panel::new();
        panel.show(crate::settings::Tab::Usage);
        let waiting = Moment {
            settings: two(),
            settings_panel: Some(panel),
            usage: None,
            ..Moment::default()
        };
        let shown = drawn(&waiting, 80, 20).join("\n");
        assert!(shown.contains("正在问宿主"), "{shown}");
        assert!(
            !shown.contains('█'),
            "and nothing that looks like data: {shown}"
        );
    }

    /// Whether a row is one of the panel's rules: a run of `─` and nothing else.
    ///
    /// The panel's own rules are straight lines ([`panel_edge`]), so this is the
    /// shape they have — as opposed to the search box's edges, which carry
    /// corners, and the rows in between. Used by several criteria that have to
    /// tell "the panel starts here" from "a setting is drawn here".
    fn is_rule(line: &str) -> bool {
        !line.is_empty() && line.chars().all(|c| c == '─')
    }

    #[test]
    fn nothing_open_is_not_a_panel() {
        let m = moment(None, two());
        assert!(drawn(&m, 60, 12).is_empty(), "no panel, no rows");
        assert_eq!(
            <Settings as View>::height(&State, &m, 60),
            Height::Hug(0),
            "and it asks for no room, so the composer keeps its place"
        );
    }

    #[test]
    fn the_panel_is_as_tall_as_what_it_draws() {
        // The property the question panel had to learn the hard way: a box that
        // reports a height it does not then draw cuts its own end off. Three
        // things have to hold at once, and only the first is the obvious one:
        //
        // - it never draws more than it asked for (the report is not a lie);
        // - with room, what it asks for is exactly what it draws;
        // - short of room, it draws no more than the rect — and fewer rows is
        //   `fit` dropping the margin and the legend rather than truncating the
        //   list, which is the same bargain the question panel strikes: the
        //   settings are the last thing to go, not the first.
        let m = moment(Some(Panel::new()), two());
        for w in [20u16, 40, 80, 120] {
            let asked = match <Settings as View>::height(&State, &m, w) {
                Height::Hug(n) | Height::Fixed(n) => n as usize,
                Height::Fill => unreachable!(),
            };
            for h in [6u16, 10, 24, 40] {
                let drawn = drawn(&m, w, h).len();
                assert!(drawn <= asked, "{w}x{h}: drew {drawn} of {asked} asked");
                assert!(drawn <= h as usize, "{w}x{h}: drew {drawn} into {h} rows");
                if (h as usize) >= asked {
                    assert_eq!(drawn, asked, "{w}x{h}: room for all of it");
                }
            }
        }
    }

    #[test]
    fn the_panel_keeps_its_height_while_the_list_narrows() {
        // The complaint this was written for: the panel rides the tail, the tail
        // is measured from the bottom, so a height that followed the filter
        // would move the panel's *top* — where the search box is. Typing one
        // character would slide the box down the screen under the person's
        // fingers.
        let m = moment(Some(Panel::new()), two());
        let base = match <Settings as View>::height(&State, &m, 70) {
            Height::Hug(n) => n,
            other => panic!("unexpected {other:?}"),
        };

        for query in ["主", "单回合", "zzz", "zzzzzz", "a"] {
            let mut panel = Panel::new();
            for c in query.chars() {
                panel.type_into_search(c);
            }
            let narrowed = moment(Some(panel.clone()), two());
            let now = match <Settings as View>::height(&State, &narrowed, 70) {
                Height::Hug(n) => n,
                other => panic!("unexpected {other:?}"),
            };
            assert_eq!(
                now,
                base,
                "`{query}` narrowed the list to {} rows and the panel changed height",
                two().matching(&panel.query).len()
            );

            // And it still draws exactly that many rows — the anchor is not a
            // report the drawing then contradicts.
            assert_eq!(
                drawn(&narrowed, 70, 40).len(),
                base as usize,
                "the panel draws the height it reports, for `{query}`"
            );
        }
    }

    #[test]
    fn a_narrowed_list_keeps_the_box_and_both_edges_where_they_were() {
        // The point of anchoring, asserted on the rows rather than on the count:
        // the search box and the two rules around the panel do not move. A list
        // that got shorter is drawn shorter *inside* them.
        let m = moment(Some(Panel::new()), two());
        let full = drawn(&m, 70, 40);

        let mut panel = Panel::new();
        for c in "zzz".chars() {
            panel.type_into_search(c);
        }
        let narrowed = drawn(&moment(Some(panel), two()), 70, 40);

        assert_eq!(
            full.len(),
            narrowed.len(),
            "same height, so the frame is where it was:\n{}\n---\n{}",
            full.join("\n"),
            narrowed.join("\n")
        );

        // Located by *content*, not by a hardcoded index: the header rows were
        // added above the box and every index below them moved, which is exactly
        // the brittleness a fixed `[0, 1, 3]` had. What is being asserted is
        // that the furniture does not move — so the test finds the furniture
        // rather than remembering where it used to be.
        assert_eq!(full[0], narrowed[0], "the header is where it was");
        assert!(
            full[0].contains("设置"),
            "and it is the header: {:?}",
            full[0]
        );
        assert!(
            full[0].contains("Config") && full[0].contains("Status") && full[0].contains("Stats"),
            "with every page on it: {:?}",
            full[0]
        );

        // The box's own two edges: found by shape, and both still drawn.
        let box_edge_at = |rows: &[String]| {
            (
                rows.iter().position(|l| l.contains('┌')),
                rows.iter().position(|l| l.contains('└')),
            )
        };
        let (full_top, full_bottom) = box_edge_at(&full);
        let (narrow_top, narrow_bottom) = box_edge_at(&narrowed);
        assert_eq!(
            (full_top, full_bottom),
            (narrow_top, narrow_bottom),
            "the box's edges are where they were:\n{}\n---\n{}",
            full.join("\n"),
            narrowed.join("\n")
        );

        // The query row, between them, is the one that changed.
        let query = full_top.expect("the box is drawn") + 1;
        assert!(
            narrowed[query].contains("zzz"),
            "the row that changed is the query's: {:?}",
            narrowed[query]
        );
        assert_eq!(
            full[query].chars().position(|c| c == '│'),
            narrowed[query].chars().position(|c| c == '│'),
            "whose wall is still in the column it was"
        );
        assert!(
            is_rule(full.last().unwrap_or(&String::new()))
                && is_rule(narrowed.last().unwrap_or(&String::new())),
            "and the panel still closes with its rule"
        );
        assert!(
            narrowed.join("\n").contains("没有匹配"),
            "and the shorter list says so rather than going blank:\n{}",
            narrowed.join("\n")
        );
    }

    #[test]
    fn a_short_rect_loses_the_furniture_before_it_loses_a_setting() {
        // The order `fit` cuts in, asserted rather than assumed: this is the
        // difference between a cramped panel and a useless one.
        let m = moment(Some(Panel::new()), two());
        let full = drawn(&m, 70, 40);
        let joined = full.join("\n");
        assert!(joined.contains("选择"), "the legend is up:\n{joined}");
        assert!(joined.contains("主题"), "and so is a setting:\n{joined}");
        // The header is the panel's first row now, and the rule closes it; the
        // panel still ends with a rule.
        assert!(
            full.first().is_some_and(|l| l.contains("设置")),
            "the header is the first row:\n{joined}"
        );
        assert!(
            full.get(1).is_some_and(|l| is_rule(l)),
            "and the rule closes the header:\n{joined}"
        );
        assert!(
            full.last().is_some_and(|l| is_rule(l)),
            "and the panel ends with its rule:\n{joined}"
        );

        // Six rows of room: the rules and the legend are furniture and go
        // before what the panel is for.
        let tight = drawn(&m, 70, 6);
        let cramped = tight.join("\n");
        assert!(
            cramped.contains("主题"),
            "what the panel is for comes first:\n{cramped}"
        );
        assert!(
            !cramped.contains("选择"),
            "and the legend is furniture, so it goes:\n{cramped}"
        );
    }

    /// The panel's rules are straight lines with no corners at their ends.
    ///
    /// A corner here drew a `┌` directly above the search box's own `┌`, which
    /// reads as a second box around the panel — a container that is not there.
    /// The rule alone says where the panel starts and stops.
    #[test]
    fn the_panels_rules_have_no_corners() {
        let m = moment(Some(Panel::new()), two());
        let lines = drawn(&m, 70, 40);
        // Found by shape, not by position: the panel's first row is the header
        // now, and its last is the rule. Both rules — the one under the header
        // and the one that closes the panel — are what this is about.
        let rules: Vec<&String> = lines.iter().filter(|l| is_rule(l)).collect();
        assert!(
            rules.len() >= 2,
            "the panel has a rule under its header and one at its foot:\n{}",
            lines.join("\n")
        );

        for (edge, line) in [("first", rules[0]), ("last", rules[rules.len() - 1])] {
            assert!(
                is_rule(line),
                "the {edge} rule is a straight line: {line:?}"
            );
            for corner in ['┌', '┐', '└', '┘'] {
                assert!(
                    !line.contains(corner),
                    "the {edge} rule has no `{corner}`: {line:?}"
                );
            }
        }
    }

    /// The legend sits against the search box, where it describes the list.
    ///
    /// It used to be the panel's last row — a screen away from the thing it
    /// explains, and the first furniture a short rect pushed off the end.
    #[test]
    fn the_legend_is_under_the_search_box_and_not_at_the_foot() {
        let m = moment(Some(Panel::new()), two());
        let lines = drawn(&m, 70, 40);

        let legend = lines
            .iter()
            .position(|l| l.contains("选择"))
            .expect("the legend is drawn");
        let box_bottom = lines
            .iter()
            .position(|l| l.starts_with('└'))
            .expect("the search box has a bottom edge");
        let first_setting = lines
            .iter()
            .position(|l| l.contains("主题"))
            .expect("a setting is drawn");

        assert!(
            legend > box_bottom,
            "the legend is below the box, not inside or above it: {legend} vs {box_bottom}"
        );
        assert!(
            legend < first_setting,
            "and above the list it describes: {legend} vs {first_setting}"
        );
        assert!(
            is_rule(lines.last().expect("a bottom rule")),
            "so the panel's last row is the rule, not the legend: {:?}",
            lines.last()
        );
    }

    /// The box's corners stand over its own walls, and the text starts in the
    /// column the labels below it start in.
    ///
    /// This is the misalignment that was reported from a terminal: the corners
    /// were pushed one cell right by a leading space, so the frame was drawn
    /// against a column its own walls did not use.
    ///
    /// **The box is located from its own text row, not from the first `┌` in
    /// the panel.** The first version of this criterion looked for the first
    /// line starting with `┌` and the first starting with `└` — but the panel
    /// has a frame too, so those two are the panel's top edge and the *inner*
    /// box's bottom edge: two different boxes, both at column 0, and the
    /// assertion held whatever the box did. Found by falsification, which is
    /// what it is for.
    #[test]
    fn the_boxs_corners_line_up_with_its_walls_and_the_labels_below() {
        // Something typed, so the box's text column holds a visible character:
        // with an empty query the caret is a reversed *space* and there is
        // nothing to locate. This is also the case the complaint was about — a
        // query jogging the answer to a column of its own.
        let mut panel = Panel::new();
        for c in "主".chars() {
            panel.type_into_search(c);
        }
        let m = moment(Some(panel), two());
        let lines = drawn(&m, 70, 40);

        let col_of = |needle: char, line: &str| line.chars().position(|c| c == needle);
        let text_row = lines
            .iter()
            .position(|l| l.contains('主'))
            .expect("the typed query is drawn");
        let (top, bottom) = (text_row - 1, text_row + 1);

        // The two edges immediately around the text are the box's, and their
        // corners are in the wall's column — the same one the panel's own frame
        // uses, which is what makes the nested frames line up rather than
        // nearly line up.
        assert_eq!(
            col_of('┌', &lines[top]),
            Some(BORDER_COL),
            "the box's top-left corner is in the border column: {:?}",
            lines[top]
        );
        assert_eq!(
            col_of('└', &lines[bottom]),
            Some(BORDER_COL),
            "and its bottom-left corner agrees: {:?}",
            lines[bottom]
        );
        assert_eq!(
            col_of('│', &lines[text_row]),
            Some(BORDER_COL),
            "and the wall between them stands under both: {:?}",
            lines[text_row]
        );

        // The panel's own header occupies the same column, so the header's text
        // and the box below it start together rather than one cell apart.
        let panel_top = lines.first().expect("the panel has a first row");
        assert!(
            panel_top.contains("设置"),
            "the panel's first row is its header: {panel_top:?}"
        );
        // And the header's text is *not* indented past the box's wall: `设置` is
        // drawn one cell in, the same `BORDER_COL + 1` the box's text column is.
        assert_eq!(
            col_of('设', panel_top),
            col_of('主', &lines[text_row]),
            "the header and the box's text share a column: {panel_top:?}"
        );

        // And what the box holds starts in the same column as the labels it
        // filters — otherwise typing would jog the answer to a different column
        // from the row it is about.
        let label_line = lines
            .iter()
            .find(|l| l.contains("主题"))
            .expect("a label is drawn")
            .clone();
        assert_eq!(
            col_of('主', &lines[text_row]),
            col_of('主', &label_line),
            "the box's text and the labels share a column:\n{:?}\n{:?}",
            lines[text_row],
            label_line
        );
    }

    /// Every row of the frame is the full width, and none of it overflows.
    ///
    /// A frame whose top rule stops short of its own walls is not a frame — it
    /// is the ragged edge this asks about, and a terminal shows it as a panel
    /// that appears to have been cut off.
    #[test]
    fn the_frame_spans_the_rect_and_never_overflows_it() {
        let m = moment(Some(Panel::new()), two());
        for w in [4usize, 5, 8, 20, 40, 79, 80] {
            let lines = drawn(&m, w as u16, 40);
            let top = lines
                .iter()
                .find(|l| l.starts_with('┌'))
                .expect("a top rule");
            assert_eq!(
                top.chars().count(),
                w,
                "the rule fills the rect at {w}: {top:?}"
            );
            assert!(
                top.ends_with('┐'),
                "and closes on the right at {w}: {top:?}"
            );
            for line in &lines {
                assert!(
                    line.chars().count() <= w,
                    "no row is wider than the rect at {w}: {line:?}"
                );
            }
        }
    }

    #[test]
    fn the_search_box_says_the_query_and_the_list_follows_it() {
        let mut panel = Panel::new();
        for c in "rounds".chars() {
            panel.type_into_search(c);
        }
        let m = moment(Some(panel.clone()), two());
        let rows = drawn(&m, 70, 12).join("\n");
        assert!(rows.contains("rounds"), "what was typed is drawn:\n{rows}");
        assert!(rows.contains("单回合最大轮数"), "and it filtered:\n{rows}");
        assert!(!rows.contains("主题"), "the other row is gone:\n{rows}");
    }

    #[test]
    fn an_empty_result_says_so_rather_than_going_blank() {
        let mut panel = Panel::new();
        for c in "zzz".chars() {
            panel.type_into_search(c);
        }
        let rows = drawn(&moment(Some(panel), two()), 70, 12).join("\n");
        assert!(rows.contains("没有匹配"), "{rows}");
    }

    #[test]
    fn the_caret_is_drawn_only_where_the_next_character_would_go() {
        // A caret in a box nobody is typing into is a lie about where the next
        // character goes. There are exactly two places it can be: the search box
        // (the panel's resting state) and a row's field. The caret is the only
        // reversed cell the panel draws, so counting them is the assertion —
        // one, never two.
        let carets = |m: &Moment| {
            lines(m, 70, 14)
                .iter()
                .map(|l| l.spans.iter().filter(|s| s.style.reverse).count())
                .sum::<usize>()
        };

        let mut panel = Panel::new();
        panel.type_into_search('a');
        assert_eq!(
            carets(&moment(Some(panel.clone()), two())),
            1,
            "resting: the caret is in the search box"
        );

        // A row's field takes the keyboard, and the caret with it. Still one:
        // the search box does not keep a second one blinking.
        panel.cursor = 1;
        crate::settings::key(
            &two(),
            &mut panel,
            crate::surface::KeyPress::plain(Key::Enter),
        );
        assert!(panel.editing.is_some(), "the number's field is open");
        assert_eq!(
            carets(&moment(Some(panel), two())),
            1,
            "and there is still exactly one caret on screen"
        );
    }

    #[test]
    fn the_pointed_row_is_the_one_a_confirm_would_change() {
        let mut panel = Panel::new();
        panel.cursor = 1;
        let m = moment(Some(panel), two());
        // One surface among the *settings*: the header's lit tab is also drawn
        // with a background — the same one, which is what makes "selected" mean
        // one thing across the panel — so the rows below the header are what
        // this criterion is about.
        let header = drawn(&m, 70, 14)
            .iter()
            .position(|l| l.contains("设置"))
            .expect("the header is drawn");
        let lit: Vec<Line> = lines(&m, 70, 14)
            .into_iter()
            .skip(header + 1)
            .filter(|l| l.spans.iter().any(|s| s.style.bg.is_some()))
            .collect();
        assert_eq!(lit.len(), 1, "exactly one setting row is a surface");
        assert!(
            lit[0].plain().contains("单回合最大轮数"),
            "and it is the one the cursor is on: {}",
            lit[0].plain()
        );
    }

    /// The lit tab and the lit setting row are the same surface.
    ///
    /// One meaning for "this is selected", so a person does not have to learn
    /// two. Asserted because it is the kind of thing that drifts the first time
    /// either is restyled on its own.
    #[test]
    fn the_lit_tab_and_the_lit_row_use_the_same_background() {
        use crate::theme::{self, Role};
        let m = moment(Some(Panel::new()), two());
        let lines = lines(&m, 70, 40);
        let want = theme::bg(Role::PanelSelBg).bg;

        let tab = lines
            .iter()
            .find(|l| l.plain().contains("Config"))
            .expect("the tab row is drawn");
        let row = lines
            .iter()
            .find(|l| l.plain().contains("主题"))
            .expect("the pointed row is drawn");

        let bg_of = |line: &Line| {
            line.spans
                .iter()
                .filter(|s| s.style.bg == want)
                .map(|s| s.text.clone())
                .collect::<Vec<_>>()
        };
        assert!(
            !bg_of(tab).is_empty(),
            "the showing tab is lit: {:?}",
            tab.plain()
        );
        assert!(
            !bg_of(row).is_empty(),
            "and so is the pointed row: {:?}",
            row.plain()
        );
    }

    #[test]
    fn the_legend_says_what_the_keys_would_do_right_now() {
        // With nothing typed there is no `/` entry, because there is no such key:
        // letters go straight into the box.
        let closed = legend(&Panel::new());
        assert!(
            !closed.iter().any(|(k, _)| *k == "/"),
            "no key opens the search box, so none is offered: {closed:?}"
        );
        assert!(closed.iter().any(|(k, w)| *k == "esc" && *w == "关闭"));

        // With something typed, Escape is spent on the search first, and the
        // legend says so rather than promising a close that will not happen.
        let mut searching = Panel::new();
        searching.type_into_search('第');
        let in_search = legend(&searching);
        assert!(
            in_search
                .iter()
                .any(|(k, w)| *k == "esc" && *w == "清空搜索"),
            "{in_search:?}"
        );

        let mut editing = Panel::new();
        editing.editing = Some(Edit {
            id: "coding.max_rounds".into(),
            value: "60".into(),
        });
        let while_editing = legend(&editing);
        assert!(while_editing.iter().any(|(k, w)| *k == "⏎" && *w == "保存"));
        assert!(
            !while_editing.iter().any(|(k, _)| *k == "↑↓"),
            "the arrows do not walk a list while a field has the keyboard: {while_editing:?}"
        );
    }

    #[test]
    fn a_row_being_edited_is_drawn_as_the_field_in_its_own_place() {
        let mut panel = Panel::new();
        panel.editing = Some(Edit {
            id: "coding.max_rounds".into(),
            value: "123".into(),
        });
        let rows = drawn(&moment(Some(panel), two()), 70, 14);
        let at = rows
            .iter()
            .position(|l| l.contains("123"))
            .expect("the value being typed is on screen");
        let label = rows
            .iter()
            .position(|l| l.contains("单回合最大轮数"))
            .expect("the row it belongs to is where it was");
        assert_eq!(at, label, "the field stands where the row stood");
    }

    #[test]
    fn a_click_reads_the_row_the_frame_drew() {
        let mut panel = Panel::new();
        panel.cursor = 1;
        let m = moment(Some(panel), two());
        for h in [10u16, 14, 20] {
            let vp = Viewport::new(Rect::sized(70, h), &m);
            let geom = geometry(&m, &vp);
            let rows = drawn(&m, 70, h);
            let mut seen = Vec::new();
            for (i, line) in rows.iter().enumerate() {
                if let Some(idx) = geom.setting_at(i) {
                    seen.push((idx, line.clone()));
                }
            }
            assert_eq!(seen.len(), 2, "both settings are reachable by click at {h}");
            assert!(seen[0].1.contains("主题"), "{:?}", seen[0]);
            assert!(seen[1].1.contains("单回合最大轮数"), "{:?}", seen[1]);
        }
    }

    #[test]
    fn an_unset_optional_shows_the_word_for_it_not_an_empty_column() {
        let unset = row(
            "telemetry.enabled",
            "遥测",
            "",
            SettingKind::OptionalBoolean,
        );
        assert_eq!(value_text(&unset), "auto");
        let unset_text = row("init_prompt_file", "自定义", "", SettingKind::Text);
        assert_eq!(value_text(&unset_text), "（未设置）");
    }

    #[test]
    fn no_row_ever_draws_wider_than_its_rect() {
        // The containment check the whole crate is held to, at the sizes a
        // terminal actually hands over.
        let mut panel = Panel::new();
        panel.type_into_search('设');
        for w in [0u16, 1, 3, 8, 20, 40, 120, 200] {
            for h in [0u16, 1, 3, 8, 24, 60] {
                let m = moment(Some(panel.clone()), two());
                for line in lines(&m, w, h) {
                    assert!(line.width() <= w as usize, "{w}x{h}: {}", line.plain());
                }
            }
        }
    }

    #[test]
    fn the_row_id_and_the_module_id_are_one_string() {
        assert_eq!(<Settings as View>::id(), ID);
        assert_eq!(ID, "settings");
    }

    #[test]
    fn a_caret_in_the_middle_of_a_multibyte_word_does_not_panic() {
        // Every path that moves the caret keeps it on a boundary; this is the
        // belt to that braces on the one function that slices. The window is not
        // a character boundary, so this walks into the middle of one on purpose.
        for at in 0.."设置面板".len() + 1 {
            let line = edit_line("主题", "设置面板", at, 40, crate::caps::Caps::detect());
            assert!(line.width() <= 40);
        }
    }

    #[test]
    fn the_mount_is_a_view_like_any_other() {
        // Registry shape: the row mounts this type by its `View::id`, so the two
        // names have to be the same string.
        let view: Arc<dyn ViewObject> = mounted();
        assert_eq!(view.id(), ID);
    }

    #[test]
    fn every_page_draws_itself_and_its_own_words() {
        // The failure this rules out is a blank panel: a page that renders
        // nothing looks exactly like a panel that failed to load, and every
        // count-based criterion in this file would stay green while a person
        // stared at an empty box.
        //
        // Written after finding there was *no* criterion on the other pages at
        // all — `elsewhere` was wired up and nothing asserted it reached the
        // screen.
        for tab in Tab::ALL {
            let mut panel = Panel::new();
            panel.show(tab);
            let m = moment(Some(panel), two());
            let lines = drawn(&m, 70, 40);
            let joined = lines.join("\n");

            assert!(
                joined.contains("设置"),
                "{tab:?}: the header is on every page:\n{joined}"
            );
            assert!(
                joined.contains(tab.label()),
                "{tab:?}: and its own tab is on the row:\n{joined}"
            );

            if tab == Tab::Config {
                assert!(joined.contains("主题"), "the settings are drawn:\n{joined}");
                assert!(joined.contains('┌'), "and the search box:\n{joined}");
                continue;
            }
            assert!(
                !joined.contains('┌'),
                "{tab:?} draws no search box — it has nothing to filter:\n{joined}"
            );
            if tab == Tab::Usage {
                // This page draws itself now, from what the host answered. With
                // no answer yet that is one line saying so — which is still a
                // page that is not blank, the thing this criterion is about.
                assert!(
                    joined.contains("正在问宿主"),
                    "{tab:?} draws what it has:\n{joined}"
                );
                continue;
            }
            let words = elsewhere(tab);
            assert!(!words.is_empty(), "{tab:?} has something to say");
            assert!(
                joined.contains(words),
                "{tab:?} draws its own line:\n{joined}"
            );
        }
    }

    #[test]
    fn every_other_page_says_something_different() {
        // A page that repeated another page's line would be a page with no
        // content of its own, drawn as though it had.
        let mut said: Vec<&str> = Vec::new();
        for tab in Tab::ALL {
            if tab == Tab::Config {
                continue;
            }
            let words = elsewhere(tab);
            assert!(!said.contains(&words), "{tab:?} repeats another page");
            said.push(words);
        }
        assert_eq!(said.len(), Tab::ALL.len() - 1);
    }

    // ---- clicking the tabs ------------------------------------------------

    /// Clicking a tab lands on the tab that is drawn under the pointer.
    ///
    /// Read off the same [`header_parts`] the frame drew with, which is the
    /// whole point: a hit test with its own arithmetic is a click that switches
    /// to the tab beside the one under the pointer the first time a label
    /// changes length.
    #[test]
    fn a_press_on_a_tab_switches_to_the_one_under_the_pointer() {
        for tab in Tab::ALL {
            // A column inside the tab's own range, found the way a pointer would
            // — from the widths the header actually occupies.
            let (_, ranges) = header_parts(tab);
            let (_, from, to) = ranges
                .iter()
                .find(|(t, _, _)| *t == tab)
                .copied()
                .unwrap_or_else(|| panic!("{tab:?} has a range on the row"));

            for col in [from, from + (to - from) / 2, to - 1] {
                assert_eq!(
                    tab_at(col),
                    Some(tab),
                    "{tab:?}: the cell at column {col} is its own"
                );
            }
        }
    }

    #[test]
    fn a_press_between_the_tabs_or_on_the_title_is_no_tab() {
        // The title, and the gaps that space the tabs apart, are not tabs. A
        // press there is a press on the panel's chrome — it must not switch
        // pages, or someone aiming at "Status" with a two-cell miss would land
        // on "Usage" and never know why.
        assert_eq!(tab_at(0), None, "the title's first cell");
        assert_eq!(tab_at(1), None);

        let (_, ranges) = header_parts(Tab::Config);
        for pair in ranges.windows(2) {
            let gap_start = pair[0].2;
            let gap_end = pair[1].1;
            for col in gap_start..gap_end {
                assert_eq!(
                    tab_at(col),
                    None,
                    "column {col} is the gap between {:?} and {:?}",
                    pair[0].0,
                    pair[1].0
                );
            }
        }
    }

    #[test]
    fn a_press_past_the_last_tab_is_no_tab() {
        let (_, ranges) = header_parts(Tab::Config);
        let end = ranges.last().expect("there are tabs").2;
        for col in end..end + 20 {
            assert_eq!(tab_at(col), None, "column {col} is past the row");
        }
    }

    #[test]
    fn the_hit_ranges_do_not_move_with_the_page_that_is_showing() {
        // Lighting a tab changes its style, not its width or its place. A range
        // that moved with the current page would be a target that slid out from
        // under a pointer that had not moved.
        let base = header_parts(Tab::Config).1;
        for tab in Tab::ALL {
            assert_eq!(header_parts(tab).1, base, "{tab:?} moved the tabs around");
        }
    }

    /// The drawn row and the hit ranges are one layout.
    ///
    /// The property the whole design rests on: for every tab, the cells the hit
    /// test claims are the cells the label was drawn in. Checked against the
    /// *rendered* line, not against the ranges again.
    ///
    /// Indexed by **display cell**, not by `char`: `设置` is two characters and
    /// four cells, so a character index is two ahead of the column the pointer
    /// reports from the moment the title is drawn. The first version of this
    /// criterion indexed `chars()` and read `"onfig"` — which is the bug it
    /// caught, in the test rather than in the panel.
    #[test]
    fn the_cells_a_tab_claims_are_the_cells_it_was_drawn_in() {
        let m = moment(Some(Panel::new()), two());
        let row = drawn(&m, 70, 40)
            .into_iter()
            .find(|l| l.contains("设置"))
            .expect("the header is drawn");

        // One entry per display cell. A wide character's second cell is a
        // sentinel rather than a space, so it cannot be mistaken for a label.
        let mut cells: Vec<char> = Vec::new();
        for c in row.chars() {
            let w = width::str_width(&c.to_string());
            if w == 0 {
                continue;
            }
            cells.push(c);
            cells.extend(std::iter::repeat_n('\u{0}', w.saturating_sub(1)));
        }

        for tab in Tab::ALL {
            let (_, ranges) = header_parts(tab);
            let (_, from, to) = ranges.iter().find(|(t, _, _)| *t == tab).copied().unwrap();
            let drawn: String = cells[from..to.min(cells.len())].iter().collect();
            assert!(
                drawn.contains(tab.label()),
                "{tab:?}: cells {from}..{to} say {drawn:?}, not its label"
            );
        }
    }
}
