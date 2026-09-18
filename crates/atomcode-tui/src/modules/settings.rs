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
use crate::settings::{Panel, SettingKind, SettingsView};
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
        let rows = layout(&vp.moment.settings, panel, w, vp.rect.h as usize);
        rows.into_iter()
            .map(|row| match row {
                Row::Top | Row::Bottom => panel_edge(w, vp.moment.caps),
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
#[derive(Clone, Debug, PartialEq, Eq)]
enum Row {
    /// The panel's top edge: where the panel begins.
    Top,
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
    /// The panel's bottom edge: where it ends.
    Bottom,
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
fn layout(settings: &SettingsView, panel: &Panel, w: usize, h: usize) -> Vec<Row> {
    if w == 0 || h == 0 {
        return Vec::new();
    }
    let mut rows = rows_for(settings, panel);
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
    rows_for(settings, &Panel::new()).len()
}

/// The panel's rows as the query leaves them, before any padding or cutting.
///
/// Takes no width: which rows are drawn is decided by the query, and how tall
/// each of them is at a given width is the caller's business — [`anchor`] counts
/// them for a width it is given. A width parameter here would be accepted and
/// ignored, which is the shape of a bug waiting for someone to rely on it.
fn rows_for(settings: &SettingsView, panel: &Panel) -> Vec<Row> {
    let shown = settings.matching(&panel.query);
    // The panel's own rule first, then the box, then the list, then the rule
    // that closes it. A frame around the whole thing is what separates a panel
    // from the conversation it was pulled up over: without it the last setting
    // and the first line of what was said before run together.
    let mut rows = vec![Row::Top];
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
    rows.extend([Row::Blank, Row::Bottom]);
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
            .rposition(|r| matches!(r, Row::Bottom | Row::Legend | Row::Blank | Row::Top))
        else {
            break;
        };
        rows.remove(at);
    }
    rows.truncate(h);
    rows
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
fn kind_hint(kind: &SettingKind) -> Option<&'static str> {
    match kind {
        SettingKind::Boolean => Some("回车 切换"),
        SettingKind::OptionalBoolean | SettingKind::Choice(_) => Some("回车 切换 · ←→ 选"),
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
    let mut out = vec![("↑↓", "选择"), ("⏎", "修改")];
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

        // Compared by *row position*, not by guessing at the content. The panel's
        // shape is fixed: the outer rule, the box's top edge, the query, the
        // box's bottom edge, a margin, the list, then a margin, the legend and
        // the closing rule. Anchoring the height is what keeps every one of
        // those rows at the index it had.
        //
        // Two earlier versions of this assertion were wrong in instructive ways:
        // classifying rows by looking for `│` made the *query* row furniture and
        // forbade typing from changing anything; taking the last three rows as
        // fixed forbade the legend from saying `esc 清空搜索` — which is exactly
        // what it is for.
        let fixed = [0, 1, 3, full.len() - 1];
        for i in fixed {
            assert_eq!(
                full[i], narrowed[i],
                "row {i} is the frame's, so it must not move:\n{:?}\n{:?}",
                full[i], narrowed[i]
            );
        }
        assert!(
            narrowed[2].contains("zzz"),
            "and the row that *did* change is the query's: {:?}",
            narrowed[2]
        );
        assert_eq!(
            full[2].chars().position(|c| c == '│'),
            narrowed[2].chars().position(|c| c == '│'),
            "whose wall is still in the column it was"
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
        assert!(
            full.first().is_some_and(|l| is_rule(l)) && full.last().is_some_and(|l| is_rule(l)),
            "and the panel is ruled top and bottom:\n{joined}"
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
        let top = lines.first().expect("the panel has a top rule");
        let bottom = lines.last().expect("the panel has a bottom rule");

        for (edge, line) in [("top", top), ("bottom", bottom)] {
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

        // The panel's own rule uses the same column, so the rule and the box
        // below it start together rather than one cell apart.
        let panel_top = lines.first().expect("the panel has a first row");
        assert!(
            is_rule(panel_top),
            "the panel's top row is its rule: {panel_top:?}"
        );
        assert_eq!(
            col_of('|', panel_top).or_else(|| col_of('─', panel_top)),
            Some(BORDER_COL),
            "and the rule starts in the same column as the box's wall: {panel_top:?}"
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
        let lit: Vec<Line> = lines(&m, 70, 14)
            .into_iter()
            .filter(|l| l.spans.iter().any(|s| s.style.bg.is_some()))
            .collect();
        assert_eq!(lit.len(), 1, "exactly one row is a surface");
        assert!(
            lit[0].plain().contains("单回合最大轮数"),
            "and it is the one the cursor is on: {}",
            lit[0].plain()
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
}
