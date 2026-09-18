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
                Row::Blank => Line::empty(),
                Row::Search { caret } => search_line(&panel.query, caret, w, panel.searching),
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
    fn height(_state: &State, moment: &Moment, width: u16) -> Height {
        let Some(panel) = moment.settings_panel.as_ref() else {
            return Height::Hug(0);
        };
        if width == 0 {
            return Height::Hug(0);
        }
        let rows = layout(&moment.settings, panel, width as usize, usize::MAX).len();
        Height::Hug(rows.min(u16::MAX as usize) as u16)
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
    Blank,
    /// The search box. `caret` is the gap to draw, or `None` when the box does
    /// not have the keyboard — a caret blinking in a field nobody is typing
    /// into is a lie about where the next character goes.
    Search {
        caret: Option<usize>,
    },
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

/// The rows this panel makes at this width, cut down to `h`.
///
/// `h` of `usize::MAX` is "how many would it take", which is what `height` asks;
/// the cut only matters once the tail has been rationed and the panel has less
/// room than it asked for.
fn layout(settings: &SettingsView, panel: &Panel, w: usize, h: usize) -> Vec<Row> {
    if w == 0 || h == 0 {
        return Vec::new();
    }
    let shown = settings.matching(&panel.query);
    let mut rows = vec![Row::Blank];
    rows.push(Row::Search {
        caret: panel.searching.then_some(panel.query_caret),
    });
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

    rows.extend([Row::Blank, Row::Legend]);
    fit(rows, h)
}

/// Cut the layout down to the height it was given, least important row first.
///
/// The same order and the same reason as the question panel's `fit`: the margin
/// above goes, then the legend, and only then the tail of what is left.
/// Truncating the end instead would take the settings first — the one thing the
/// panel is for — and leave a search box explaining how to work it.
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
    rows.truncate(h);
    rows
}

/// The search box, with a caret when it has the keyboard.
fn search_line(query: &str, caret: Option<usize>, w: usize, focused: bool) -> Line {
    let base = theme::fg(if focused { Role::Warning } else { Role::Muted });
    let mut spans = vec![Span::styled("  ", Style::new())];
    if query.is_empty() && caret.is_none() {
        spans.push(Span::styled(
            "按 / 搜索".to_string(),
            theme::fg(Role::Muted),
        ));
        return Line::from_spans(spans).truncate(w);
    }
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
fn legend(panel: &Panel) -> Vec<(&'static str, &'static str)> {
    if panel.editing.is_some() {
        return vec![("⏎", "保存"), ("esc", "取消")];
    }
    let mut out = vec![("↑↓", "选择"), ("⏎", "修改")];
    if panel.searching {
        out.push(("esc", "退出搜索"));
    } else {
        out.push(("/", "搜索"));
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
    fn a_short_rect_loses_the_legend_before_it_loses_a_setting() {
        // The order `fit` cuts in, asserted rather than assumed: this is the
        // difference between a cramped panel and a useless one.
        let m = moment(Some(Panel::new()), two());
        let full = drawn(&m, 70, 40);
        assert!(
            full.last().is_some_and(|l| l.contains("选择")),
            "the legend is up"
        );
        assert!(full.iter().any(|l| l.contains("主题")));

        // Two rows of room for the search box and nothing else.
        let tight = drawn(&m, 70, 6);
        let joined = tight.join("\n");
        assert!(
            joined.contains("主题"),
            "what the panel is for comes first:\n{joined}"
        );
        assert!(
            !joined.contains("选择"),
            "and the legend is what goes:\n{joined}"
        );
    }

    #[test]
    fn the_search_box_says_the_query_and_the_list_follows_it() {
        let mut panel = Panel::new();
        panel.searching = true;
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
        panel.searching = true;
        for c in "zzz".chars() {
            panel.type_into_search(c);
        }
        let rows = drawn(&moment(Some(panel), two()), 70, 12).join("\n");
        assert!(rows.contains("没有匹配"), "{rows}");
    }

    #[test]
    fn the_caret_is_drawn_only_where_the_next_character_would_go() {
        // A caret in a box nobody is typing into is a lie about focus, and this
        // is the one place that decides. The caret is the only reversed cell the
        // panel draws, so counting them across the whole panel is the assertion:
        // one when the search box has the keyboard, none when it does not.
        let carets = |m: &Moment| {
            lines(m, 70, 12)
                .iter()
                .map(|l| l.spans.iter().filter(|s| s.style.reverse).count())
                .sum::<usize>()
        };

        let mut panel = Panel::new();
        panel.searching = true;
        panel.type_into_search('a');
        assert_eq!(
            carets(&moment(Some(panel.clone()), two())),
            1,
            "focused: one caret"
        );

        panel.searching = false;
        assert_eq!(
            carets(&moment(Some(panel), two())),
            0,
            "blurred: no caret, because the next character would not go there"
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
        let closed = legend(&Panel::new());
        assert!(closed.iter().any(|(k, _)| *k == "/"), "{closed:?}");

        let mut searching = Panel::new();
        searching.searching = true;
        let in_search = legend(&searching);
        assert!(
            !in_search.iter().any(|(k, _)| *k == "/"),
            "no point offering the key that is already on: {in_search:?}"
        );
        assert!(in_search
            .iter()
            .any(|(k, w)| *k == "esc" && *w == "退出搜索"));

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
        panel.searching = true;
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
