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
                    UsageLine::Bar { share, about } => usage_bar(share, &about, w, vp.moment.caps),
                    UsageLine::Note(text) => Line::styled(
                        width::take_width(&format!("  {text}"), w),
                        theme::fg(Role::Muted),
                    ),
                    UsageLine::Spark {
                        values,
                        from,
                        to,
                        peak,
                    } => usage_spark(&values, &from, &to, peak, w, vp.moment.caps),
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
    Bar { share: f32, about: String },
    /// An ordinary line, dimmed.
    Note(String),
    /// A day-by-day series, drawn as one row of blocks with its span under it.
    ///
    /// One row rather than the classic front end's four-row plot: this page is
    /// a panel that shares the screen with a conversation, and the question it
    /// answers — "is it climbing, and when was the spike" — is answered by the
    /// shape alone. The peak is said in words beside it, because a block row
    /// has no axis to read a number off.
    Spark {
        values: Vec<u64>,
        from: String,
        to: String,
        peak: u64,
    },
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
            // which is the thing a person opened this page to see.
            Some(percent) => {
                let counted = match (window.calls_used, window.call_limit) {
                    (Some(used), Some(limit)) => format!(" · {used} / {limit} 次"),
                    (None, Some(limit)) => format!(" · 上限 {limit} 次"),
                    _ => String::new(),
                };
                out.push(UsageLine::Bar {
                    share: f32::from(percent) / 100.0,
                    about: format!("用掉 {percent}%{counted}"),
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
        if !window.resets_at.is_empty() {
            tail.push(format!("{} 重置", window.resets_at));
        }
        if window.resets_in_seconds > 0 {
            tail.push(format!(
                "还有 {}",
                crate::text::spoken_duration(window.resets_in_seconds as u64)
            ));
        }
        if !tail.is_empty() {
            out.push(UsageLine::Note(tail.join(" · ")));
        }
        out.push(UsageLine::Gap);
    }

    if let Some(stats) = page.stats.as_ref() {
        out.push(UsageLine::Head("总览".into()));
        let span = match (stats.from.is_empty(), stats.to.is_empty()) {
            (false, false) => format!("{} 到 {}", stats.from, stats.to),
            _ => String::new(),
        };
        let mut overview = format!(
            "{} tokens · {} 次请求",
            crate::content::token_count_u64(stats.total_tokens),
            stats.total_requests
        );
        if !span.is_empty() {
            overview.push_str(&format!(" · {span}"));
        }
        out.push(UsageLine::Note(overview));
        // Days with anything on them, out of the days the span covers: the one
        // figure the series says that the totals do not.
        let active = stats.daily.iter().filter(|d| d.tokens > 0).count();
        if !stats.daily.is_empty() {
            out.push(UsageLine::Note(format!(
                "{active} / {} 天有用量",
                stats.daily.len()
            )));
        }
        out.push(UsageLine::Gap);

        if !stats.daily.is_empty() {
            out.push(UsageLine::Head("每天用掉多少".into()));
            out.push(UsageLine::Spark {
                values: stats.daily.iter().map(|d| d.tokens).collect(),
                from: stats
                    .daily
                    .first()
                    .map(|d| d.date.clone())
                    .unwrap_or_default(),
                to: stats
                    .daily
                    .last()
                    .map(|d| d.date.clone())
                    .unwrap_or_default(),
                peak: stats.daily.iter().map(|d| d.tokens).max().unwrap_or(0),
            });
            out.push(UsageLine::Gap);
        }

        if !stats.models.is_empty() {
            out.push(UsageLine::Head("各模型用量".into()));
            let biggest = stats.models.first().map(|m| m.tokens).unwrap_or(0).max(1);
            for model in &stats.models {
                let share = if stats.total_tokens == 0 {
                    0.0
                } else {
                    model.tokens as f32 / stats.total_tokens as f32
                };
                out.push(UsageLine::Bar {
                    // Against the biggest, not against the total: the bars are
                    // there to be compared with each other, and a set where the
                    // top one is a quarter of the track wastes the track.
                    share: model.tokens as f32 / biggest as f32,
                    about: format!(
                        "{} · {} tokens · {} 次 · {:.0}%",
                        model.name,
                        crate::content::token_count_u64(model.tokens),
                        model.requests,
                        share * 100.0
                    ),
                });
            }
            out.push(UsageLine::Gap);
        }
    }

    if matches!(out.last(), Some(UsageLine::Gap)) {
        out.pop();
    }
    out
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
fn usage_bar(share: f32, about: &str, w: usize, caps: crate::caps::Caps) -> Line {
    use crate::caps::Glyph;
    let filled = ((share.clamp(0.0, 1.0) * BAR_CELLS as f32) as usize).min(BAR_CELLS);
    // Two characters, not one character in two colours. A track drawn in the
    // same block as its fill says nothing on a terminal with no colour — and
    // says nothing in a transcript either, which is where this was caught.
    let full = caps.g(Glyph::Thumb);
    let empty = if caps.unicode { "░" } else { "-" };
    let mut spans = vec![Span::styled("  ".to_string(), Style::new())];
    if filled > 0 {
        spans.push(Span::styled(full.repeat(filled), theme::fg(Role::Accent)));
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

/// A day-by-day series as one row of blocks, with its span beside it.
///
/// Eight heights from the braille-free block set, so it draws on a terminal
/// with no Unicode as well — the shape survives the downgrade even when the
/// resolution does not. Scaled to the peak, which is said in words: a row of
/// blocks has no axis, and "it doubled" is unreadable without knowing what the
/// tallest one is.
///
/// Values beyond the width are **averaged into** the columns rather than
/// dropped, so a month of days on a narrow panel is still a month — a chart
/// that silently showed the last 28 of 90 days would be answering a different
/// question.
fn usage_spark(
    values: &[u64],
    from: &str,
    to: &str,
    peak: u64,
    w: usize,
    caps: crate::caps::Caps,
) -> Line {
    const LEVELS: [&str; 8] = ["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];
    const ASCII: [&str; 8] = [".", ".", ":", ":", "-", "=", "#", "#"];
    if values.is_empty() || w == 0 {
        return Line::empty();
    }
    let cells = SPARK_CELLS.min(values.len());
    let per = values.len().div_ceil(cells);
    let levels: &[&str; 8] = if caps.unicode { &LEVELS } else { &ASCII };
    let mut bar = String::new();
    for chunk in values.chunks(per) {
        let mean = chunk.iter().sum::<u64>() / chunk.len() as u64;
        let level = if peak == 0 {
            0
        } else {
            (((mean as f64 / peak as f64) * (levels.len() - 1) as f64).round() as usize)
                .min(levels.len() - 1)
        };
        bar.push_str(levels[level]);
    }
    let about = format!(
        "  峰值 {} · {from} → {to}",
        crate::content::token_count_u64(peak)
    );
    Line::from_spans(vec![
        Span::styled("  ".to_string(), Style::new()),
        Span::styled(bar, theme::fg(Role::Accent)),
        Span::styled(about, theme::fg(Role::Muted)),
    ])
    .truncate(w)
}

/// How many columns the day series is drawn in. See [`BAR_CELLS`].
const SPARK_CELLS: usize = 28;

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

        let uncounted = usage_page(crate::settings::UsagePage {
            context: None,
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
        use atomcode_host_api::{DayUse, ModelUse, UsageStats};
        let page = usage_page(crate::settings::UsagePage {
            context: None,
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
                total_tokens: 280_800_000,
                total_requests: 2450,
            }),
        });
        let shown = drawn(&page, 80, 40).join("\n");
        assert!(shown.contains("deepseek-flash"), "{shown}");
        assert!(
            shown.contains("221.1m"),
            "tokens as a person reads them: {shown}"
        );
        assert!(shown.contains("1604"), "and requests: {shown}");
        assert!(
            shown.contains("峰值 216.6m"),
            "the chart says its peak: {shown}"
        );
        assert!(
            shown.contains("2026-08-21") && shown.contains("2026-09-20"),
            "and the span it covers: {shown}"
        );
        assert!(shown.contains("2450"), "the overview totals: {shown}");
        // The bars are of different lengths, which is the only reason to draw
        // two of them. Caught here: the first version drew the track in the
        // same block as the fill, so every bar looked full to anything that
        // does not read colour — a terminal without it, and this assertion.
        let bars: Vec<usize> = shown
            .lines()
            .filter(|line| line.contains('█'))
            .map(|line| line.matches('█').count())
            .collect();
        assert_eq!(bars.len(), 3, "two models and the day series: {shown}");
        assert!(
            bars[1] > bars[2],
            "the bigger model has the longer bar: {bars:?}\n{shown}"
        );
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
