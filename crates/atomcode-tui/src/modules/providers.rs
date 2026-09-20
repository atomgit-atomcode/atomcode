//! The providers panel: the accounts and models this build can talk to, listed
//! and edited where the composer was.
//!
//! Pulled up from the bottom over the composer, the way the settings panel and a
//! question are, and for the same reason: a panel a person is working *in* is a
//! turn of its own, so it asks for the composer's rows and the composer asks for
//! none. tuix drew this one as a modal in the middle of the screen
//! (`modals/provider_panel.rs`); here it rides the tail with its sibling, so the
//! two panels a person edits their configuration in are the same shape.
//!
//! What lives here is the *drawing*. What a provider **is** arrives as data
//! ([`crate::providers::ProvidersView`]) and a change goes back over
//! [`crate::providers::Providers`] — this module knows neither the configuration
//! file nor its schema, and could not name a TOML table if it wanted to
//! (`docs/adr/0022` §3).
//!
//! Nothing folds. The log records that a provider was added, never that a form
//! had three characters typed into it, so there is no fact to fold: the panel's
//! state is in [`crate::moment::Moment`], where "what this screen is doing now"
//! belongs — see [`crate::providers::Panel`], and its doc for the one thing that
//! is *not* in there, which is the key.

use crate::frame::{Line, Span, Style};
use crate::module::{Height, View};
use crate::modules::chrome::{
    self, box_edge, edit_line, pad_to, panel_edge, search_line, LABEL_MAX, LABEL_MIN, LEAD,
};
use crate::moment::{Moment, Viewport};
use crate::providers::{
    AccountField, AccountForm, Form, Listed, ModelField, ModelForm, Panel, ProvidersView, Tab,
};
use crate::theme::{self, Role};
use crate::width;

pub const ID: &str = "providers";

/// The panel's name, in the header and in the hit test — one string, so a click
/// cannot be measured against a title that is no longer drawn.
const NAME: &str = "provider";

/// The most rows of list the panel takes, however many accounts there are.
///
/// A configuration with forty models must not push the conversation off the
/// screen to show all of them at once; past this the list scrolls under the
/// cursor. Twelve is what fits over a composer on the shortest terminal anyone
/// runs this in without the panel becoming the screen.
const MOST: usize = 12;

/// Nothing folds — see this module's own doc for why.
#[derive(Default)]
pub struct State;

pub struct Providers;

impl View for Providers {
    type State = State;

    fn id() -> &'static str {
        ID
    }

    fn absorb(_state: &mut State, _fact: &atomcode_harness::session::SessionEvent) {}

    fn render(_state: &State, vp: &Viewport<'_>) -> Vec<Line> {
        let Some(panel) = vp.moment.providers_panel.as_ref() else {
            return Vec::new();
        };
        let w = vp.rect.w as usize;
        if w == 0 || vp.rect.h == 0 {
            return Vec::new();
        }
        let view = &vp.moment.providers;
        let caps = vp.moment.caps;
        layout(view, panel, vp.rect.h as usize)
            .into_iter()
            .map(|row| draw(view, panel, row, w, caps))
            .collect()
    }

    /// What the panel needs, at this width.
    ///
    /// Counted from the *unfiltered* list, whatever is typed, for the reason the
    /// settings panel does it: the panel is the newest thing on the tail, so its
    /// bottom edge is the fixed one and a height that followed the filter would
    /// slide the search box down the screen under the person's fingers as the
    /// list narrowed.
    fn height(_state: &State, moment: &Moment, width: u16) -> Height {
        let Some(panel) = moment.providers_panel.as_ref() else {
            return Height::Hug(0);
        };
        if width == 0 {
            return Height::Hug(0);
        }
        let mut open = panel.clone();
        open.query.clear();
        Height::Hug(
            layout(&moment.providers, &open, usize::MAX)
                .len()
                .min(u16::MAX as usize) as u16,
        )
    }
}

/// One row of the panel, before it is drawn.
///
/// The whole layout, and the only one: [`draw`] walks it to draw, [`geometry`]
/// walks it to say which row is which account, and `height` counts it — the
/// shape `crate::modules::settings` uses, and for the same reason.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Row {
    /// The rule the panel starts with.
    Rule,
    /// The name and the two lists: `provider  账号  模型`.
    Header,
    BoxTop,
    Search,
    BoxBottom,
    Blank,
    /// A row of the list, by its index into [`ProvidersView::listed`].
    Listed(usize),
    /// What a filter that matched nothing says.
    Nothing,
    /// How many rows are out of sight above and below.
    Scroll {
        above: usize,
        below: usize,
    },
    /// One field of the form that is up, by its index into the form's fields.
    Field(usize),
    /// What the form is for: `添加账号`, `改 deepseek`, …
    FormHead,
    Legend,
}

fn layout(view: &ProvidersView, panel: &Panel, h: usize) -> Vec<Row> {
    let mut rows = vec![Row::Rule, Row::Header];
    match &panel.form {
        Some(form) => {
            rows.push(Row::FormHead);
            rows.push(Row::Blank);
            let fields = match form {
                Form::Account(f) => f.fields(view).len(),
                Form::Model(f) => f.fields(view).len(),
            };
            for at in 0..fields {
                rows.push(Row::Field(at));
            }
            rows.push(Row::Blank);
            rows.push(Row::Legend);
        }
        None => {
            rows.push(Row::BoxTop);
            rows.push(Row::Search);
            rows.push(Row::BoxBottom);
            let listed = view.listed(panel);
            if listed.is_empty() {
                rows.push(Row::Nothing);
            } else {
                // The room the list has, once the chrome above and the blank
                // and legend below have taken theirs. `h` of `usize::MAX` is the
                // question `height` asks — "how many would it take" — and then
                // the answer is the whole list, still capped at what a panel may
                // take of the screen.
                let cap = h.saturating_sub(rows.len() + 2).min(MOST);
                let (from, to) = if listed.len() > cap {
                    // The row that says how much is out of sight comes out of
                    // the same budget. Counting it afterwards is how a panel
                    // ends up one row taller than the rect it was given — which
                    // is what `a_long_list_keeps_the_cursor_in_view` caught.
                    let room = cap.saturating_sub(1).max(1);
                    let (from, to) = window(listed.len(), panel.cursor, room);
                    rows.push(Row::Scroll {
                        above: from,
                        below: listed.len() - to,
                    });
                    (from, to)
                } else {
                    (0, listed.len())
                };
                for at in from..to {
                    rows.push(Row::Listed(at));
                }
            }
            rows.push(Row::Blank);
            rows.push(Row::Legend);
        }
    }
    rows
}

/// The slice of a list of `len` rows that keeps `cursor` in view, `room` rows
/// at a time.
///
/// Clamped rather than centred: a list shorter than the room is drawn whole, and
/// a cursor near either end keeps that end on screen instead of scrolling past
/// it to keep itself in the middle.
fn window(len: usize, cursor: usize, room: usize) -> (usize, usize) {
    if len <= room {
        return (0, len);
    }
    let half = room / 2;
    let from = cursor.saturating_sub(half).min(len - room);
    (from, from + room)
}

fn draw(view: &ProvidersView, panel: &Panel, row: Row, w: usize, caps: crate::caps::Caps) -> Line {
    match row {
        Row::Rule => panel_edge(w, caps),
        Row::Header => {
            let labels: Vec<&str> = Tab::ALL.iter().map(|t| t.label()).collect();
            let at = Tab::ALL.iter().position(|t| *t == panel.tab).unwrap_or(0);
            Line::from_spans(chrome::header_parts(NAME, &labels, at).0).truncate(w)
        }
        Row::BoxTop => box_edge(w, caps, true),
        Row::Search => search_line(&panel.query, Some(panel.query.len()), w, caps),
        Row::BoxBottom => box_edge(w, caps, false),
        Row::Blank => Line::empty(),
        Row::Nothing => Line::styled(
            width::take_width("  没有匹配的 provider", w),
            theme::fg(Role::Muted),
        ),
        Row::Scroll { above, below } => Line::styled(
            width::take_width(&format!("  ↑{above} ↓{below}"), w),
            theme::fg(Role::Muted),
        ),
        Row::Listed(at) => listed_line(view, panel, at, w, caps),
        Row::FormHead => Line::styled(
            width::take_width(&format!("  {}", form_title(panel)), w),
            theme::fg(Role::Brand),
        ),
        Row::Field(at) => field_line(view, panel, at, w, caps),
        Row::Legend => Line::styled(
            format!("  {}", crate::widget::keys(&legend(panel), caps)),
            theme::fg(Role::Muted),
        )
        .truncate(w),
    }
}

fn form_title(panel: &Panel) -> String {
    match &panel.form {
        Some(Form::Account(form)) => match &form.editing {
            Some(id) => format!("改 {id}"),
            None => "添加 provider".to_string(),
        },
        Some(Form::Model(form)) => match &form.editing {
            Some(id) => format!("改 {id}"),
            None => format!("给 {} 添加模型", form.account_id()),
        },
        None => String::new(),
    }
}

/// One row of the list: what it is, and what is worth knowing about it.
fn listed_line(
    view: &ProvidersView,
    panel: &Panel,
    at: usize,
    w: usize,
    caps: crate::caps::Caps,
) -> Line {
    let listed = view.listed(panel);
    let Some(what) = listed.get(at).copied() else {
        return Line::empty();
    };
    let here = at == panel.cursor;
    let base = if here {
        theme::bg(Role::PanelSelBg).under(theme::fg(Role::PanelFg))
    } else {
        Style::new()
    };
    let pointer = if here {
        format!("{} ", caps.g(crate::caps::Glyph::Pointer))
    } else {
        "  ".to_string()
    };
    let (mark, label, about, dim) = match what {
        Listed::Add => (
            " ".to_string(),
            match panel.tab {
                Tab::Accounts => "添加 provider".to_string(),
                Tab::Models => "添加模型".to_string(),
            },
            "^a".to_string(),
            true,
        ),
        Listed::Account(i) => {
            let Some(row) = view.accounts().get(i) else {
                return Line::empty();
            };
            let mut about = vec![row.protocol.clone()];
            if row.configured {
                about.push(format!("{} 个模型", row.models));
                about.push(if row.has_key {
                    "有密钥".to_string()
                } else {
                    "没有密钥".to_string()
                });
                if row.managed {
                    about.push("登录管理".to_string());
                }
            } else {
                about.push("未配置".to_string());
            }
            let on = view
                .models()
                .iter()
                .any(|m| m.current && m.account == row.id);
            (
                mark_for(on, caps),
                row.label.clone(),
                about.join(" · "),
                !row.configured,
            )
        }
        Listed::Model(i) => {
            let Some(row) = view.models().get(i) else {
                return Line::empty();
            };
            let mut about = vec![format!("{}k", row.window / 1000)];
            if panel.drill.is_none() {
                about.insert(0, row.account.clone());
            }
            if let Some(effort) = &row.effort {
                about.push(effort.clone());
            }
            if row.vision == Some(true) {
                about.push("视觉".to_string());
            }
            if row.managed {
                about.push("登录管理".to_string());
            }
            (
                mark_for(row.current, caps),
                row.id.clone(),
                about.join(" · "),
                false,
            )
        }
    };
    // The row a second ctrl-d would delete says so where the row's own
    // description was: a person about to throw something away should read it on
    // the thing being thrown away, not in a corner.
    // `is_some_and`, not `==`: two `None`s compare equal, and an unarmed panel
    // would then draw the warning on the add row, which has no id at all.
    let armed = panel.pending_delete.as_deref().is_some_and(|armed| {
        Some(armed)
            == match what {
                Listed::Account(i) => view.accounts().get(i).map(|r| r.id.as_str()),
                Listed::Model(i) => view.models().get(i).map(|r| r.id.as_str()),
                Listed::Add => None,
            }
    });
    let label_room = w.saturating_sub(LEAD + 2).clamp(LABEL_MIN, LABEL_MAX);
    let shown = width::take_width(&label, label_room);
    let pad = label_room.saturating_sub(width::str_width(&shown));
    let label_style = if dim {
        base.under(theme::fg(Role::Muted))
    } else {
        base
    };
    let about_style = if armed {
        base.under(theme::fg(Role::Warning))
    } else {
        base.under(theme::fg(Role::Muted))
    };
    let about = if armed {
        "再按一次 ^d 删除".to_string()
    } else {
        about
    };
    let spans = vec![
        Span::styled(pointer, base),
        Span::styled(format!("{mark} "), label_style),
        Span::styled(shown, label_style),
        Span::styled(" ".repeat(pad + 2), base),
        Span::styled(about, about_style),
    ];
    pad_to(Line::from_spans(spans), w, base)
}

/// The mark on a row that is the one in use.
fn mark_for(on: bool, caps: crate::caps::Caps) -> String {
    match on {
        true => caps.g(crate::caps::Glyph::Bullet).to_string(),
        false => " ".to_string(),
    }
}

/// One field of the form: its label, and the gesture that changes it.
fn field_line(
    view: &ProvidersView,
    panel: &Panel,
    at: usize,
    w: usize,
    caps: crate::caps::Caps,
) -> Line {
    let Some(form) = panel.form.as_ref() else {
        return Line::empty();
    };
    let (label, value, focused, typing, caret) = match form {
        Form::Account(f) => {
            let fields = f.fields(view);
            let Some(field) = fields.get(at).copied() else {
                return Line::empty();
            };
            let focused = field == f.focus;
            account_field(view, f, field, focused)
        }
        Form::Model(f) => {
            let fields = f.fields(view);
            let Some(field) = fields.get(at).copied() else {
                return Line::empty();
            };
            let focused = field == f.focus;
            model_field(view, f, field, focused)
        }
    };
    // A text field with the keyboard is drawn with its caret in it; everything
    // else is drawn as a value, because a caret on a toggle would say "type
    // here" about a field nothing can be typed into.
    if focused && typing {
        return edit_line(&label, &value, caret, w, caps);
    }
    let base = if focused {
        theme::bg(Role::PanelSelBg).under(theme::fg(Role::PanelFg))
    } else {
        Style::new()
    };
    let pointer = if focused {
        format!("{} ", caps.g(crate::caps::Glyph::Pointer))
    } else {
        "  ".to_string()
    };
    let label_room = w.saturating_sub(LEAD + 2).clamp(LABEL_MIN, LABEL_MAX);
    let shown = width::take_width(&label, label_room);
    let pad = label_room.saturating_sub(width::str_width(&shown));
    let spans = vec![
        Span::styled(pointer, base),
        Span::styled(shown, base),
        Span::styled(" ".repeat(pad + 2), base),
        Span::styled(value, base),
    ];
    pad_to(Line::from_spans(spans), w, base)
}

/// (label, value, focused, is a text field, caret).
fn account_field(
    view: &ProvidersView,
    form: &AccountForm,
    field: AccountField,
    focused: bool,
) -> (String, String, bool, bool, usize) {
    match field {
        AccountField::Name => ("名字".into(), form.name.clone(), focused, true, form.caret),
        AccountField::Protocol => (
            "协议".into(),
            cycled(&form.protocol_label(view), focused),
            focused,
            false,
            0,
        ),
        AccountField::Endpoint => (
            "地址".into(),
            form.endpoint.clone(),
            focused,
            true,
            form.caret,
        ),
        AccountField::Key => (
            "密钥".into(),
            dots(form.key_len, form.editing.is_some()),
            focused,
            false,
            0,
        ),
    }
}

fn model_field(
    view: &ProvidersView,
    form: &ModelForm,
    field: ModelField,
    focused: bool,
) -> (String, String, bool, bool, usize) {
    match field {
        ModelField::Account => (
            "provider".into(),
            cycled(form.account_id(), focused),
            focused,
            false,
            0,
        ),
        ModelField::Key => ("密钥".into(), dots(form.key_len, false), focused, false, 0),
        ModelField::Model => ("模型".into(), form.model.clone(), focused, true, form.caret),
        ModelField::Vision => (
            "看图".into(),
            cycled(
                match form.vision {
                    None => "自动",
                    Some(true) => "能",
                    Some(false) => "不能",
                },
                focused,
            ),
            focused,
            false,
            0,
        ),
        ModelField::Effort => (
            "思考强度".into(),
            cycled(form.effort.as_deref().unwrap_or("不支持"), focused),
            focused,
            false,
            0,
        ),
        ModelField::Levels => (
            "可选强度".into(),
            levels_text(view, form, focused),
            focused,
            false,
            0,
        ),
        ModelField::Window => (
            "上下文".into(),
            form.window.clone(),
            focused,
            true,
            form.caret,
        ),
        ModelField::Default => (
            "存完就用".into(),
            cycled(if form.default { "是" } else { "否" }, focused),
            focused,
            false,
            0,
        ),
    }
}

/// A value the arrows cycle, wearing its arrows while it has the keyboard.
fn cycled(value: &str, focused: bool) -> String {
    match focused {
        true => format!("‹ {value} ›"),
        false => value.to_string(),
    }
}

/// A key, as many dots as it has characters.
///
/// Never the characters — this module could not draw them if it wanted to: what
/// it is handed is a count (`crate::providers`). An empty field on an edit says
/// what empty *means* there, which is "the stored one stays".
fn dots(len: usize, editing: bool) -> String {
    if len == 0 {
        return match editing {
            true => "（留空则不改）".to_string(),
            false => String::new(),
        };
    }
    "•".repeat(len.min(32))
}

fn levels_text(view: &ProvidersView, form: &ModelForm, focused: bool) -> String {
    view.efforts()
        .iter()
        .enumerate()
        .map(|(i, level)| {
            let on = form.levels.get(i).copied().unwrap_or(true);
            let mark = if on { "✓" } else { "·" };
            if focused && i == form.level {
                format!("[{level}{mark}]")
            } else {
                format!(" {level}{mark} ")
            }
        })
        .collect::<Vec<_>>()
        .join("")
}

fn legend(panel: &Panel) -> Vec<(&'static str, &'static str)> {
    match &panel.form {
        Some(Form::Account(_)) | Some(Form::Model(_)) => {
            vec![
                ("⇥", "下一项"),
                ("←→", "改"),
                ("⏎", "保存"),
                ("esc", "取消"),
            ]
        }
        None => {
            if panel.pending_delete.is_some() {
                // Says what the next press does, because that is the only thing
                // about this state a person has to know — and it is the press
                // that throws something away.
                return vec![("^d", "再按一次删除"), ("其它键", "取消")];
            }
            let mut out = vec![("↑↓", "选择")];
            out.push(match panel.tab {
                Tab::Accounts => ("⏎", "看它的模型"),
                Tab::Models => ("⏎", "换过去"),
            });
            out.push(("^a", "添加"));
            out.push(("^e", "修改"));
            out.push(("^d", "删除"));
            out.push(("⇥", "换页"));
            out.push(("esc", "关闭"));
            out
        }
    }
}

/// Which listed row each screen row carries, for a click to read.
///
/// Built by the same [`layout`] the frame used, so a click and the row it lights
/// up cannot come from two different arrangements of one panel.
pub struct Geometry {
    rows: Vec<Option<usize>>,
    /// Which screen row the tabs were drawn on.
    ///
    /// Read off the laid-out rows rather than assumed to be the first, for the
    /// same reason every other hit here is read off the layout: this panel opens
    /// with a rule, so "the header is the panel's first row" was true of the
    /// settings panel and false of this one — and a click on a tab did nothing
    /// at all until the first real run found it.
    header: Option<usize>,
}

impl Geometry {
    /// Which listed row is on this screen row, if one is.
    pub fn listed_at(&self, row: usize) -> Option<usize> {
        self.rows.get(row).copied().flatten()
    }

    /// Which screen row carries the tabs.
    pub fn header_row(&self) -> Option<usize> {
        self.header
    }
}

pub fn geometry(moment: &Moment, vp: &Viewport<'_>) -> Geometry {
    let Some(panel) = moment.providers_panel.as_ref() else {
        return Geometry {
            rows: Vec::new(),
            header: None,
        };
    };
    let rows = layout(&moment.providers, panel, vp.rect.h as usize);
    Geometry {
        header: rows.iter().position(|row| *row == Row::Header),
        rows: rows
            .into_iter()
            .map(|row| match row {
                Row::Listed(at) => Some(at),
                _ => None,
            })
            .collect(),
    }
}

/// Which list is under this cell of the header row.
pub fn tab_at(col: usize) -> Option<Tab> {
    let labels: Vec<&str> = Tab::ALL.iter().map(|t| t.label()).collect();
    chrome::tab_at(NAME, &labels, col).map(|at| Tab::ALL[at])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::Rect;
    use crate::providers::{AccountRow, ModelRow, Protocol};

    fn account(id: &str, models: usize) -> AccountRow {
        AccountRow {
            id: id.into(),
            label: id.into(),
            protocol: "OpenAI".into(),
            endpoint: "https://api.example.com/v1".into(),
            models,
            has_key: true,
            managed: false,
            configured: true,
        }
    }

    fn model(id: &str, account: &str, current: bool) -> ModelRow {
        ModelRow {
            id: id.into(),
            account: account.into(),
            model: id.into(),
            window: 128_000,
            vision: None,
            effort: None,
            levels: Vec::new(),
            current,
            managed: false,
        }
    }

    fn view() -> ProvidersView {
        ProvidersView::new(
            vec![account("deepseek", 1), account("local", 2)],
            vec![
                model("deepseek/chat", "deepseek", true),
                model("local/a", "local", false),
                model("local/b", "local", false),
            ],
            vec![Protocol {
                id: "openai-compatible".into(),
                label: "OpenAI".into(),
                endpoint: Some("https://api.openai.com/v1".into()),
                needs_key: true,
            }],
            vec!["low".into(), "high".into()],
        )
    }

    fn moment(panel: Option<Panel>) -> Moment {
        Moment {
            providers: view(),
            providers_panel: panel,
            ..Moment::default()
        }
    }

    fn lines(m: &Moment, w: u16, h: u16) -> Vec<Line> {
        let vp = Viewport::new(Rect::sized(w, h), m);
        Providers::render(&State, &vp)
    }

    fn drawn(m: &Moment, w: u16, h: u16) -> String {
        lines(m, w, h)
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|s| s.text.as_str())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_panel_that_is_not_up_draws_nothing() {
        assert!(lines(&moment(None), 60, 20).is_empty());
    }

    /// The check every panel in this crate keeps: a row wider than its rect is a
    /// row that writes over whatever is beside it.
    #[test]
    fn no_row_ever_draws_wider_than_its_rect() {
        for w in [4u16, 12, 40, 80, 200] {
            for panel in [
                Panel::new(),
                Panel {
                    tab: Tab::Models,
                    query: "loc".into(),
                    ..Panel::new()
                },
                Panel {
                    form: Some(Form::Account(AccountForm::add(&view()))),
                    ..Panel::new()
                },
                Panel {
                    tab: Tab::Models,
                    form: ModelForm::add(&view(), None).map(Form::Model),
                    ..Panel::new()
                },
            ] {
                let m = moment(Some(panel));
                for line in lines(&m, w, 24) {
                    assert!(line.width() <= w as usize, "{} > {w}", line.width());
                }
            }
        }
    }

    #[test]
    fn the_panel_is_as_tall_as_what_it_draws() {
        let m = moment(Some(Panel::new()));
        let Height::Hug(asked) = Providers::height(&State, &m, 60) else {
            panic!("a panel hugs");
        };
        assert_eq!(lines(&m, 60, asked).len(), asked as usize);
    }

    #[test]
    fn the_list_says_what_is_under_each_account() {
        let out = drawn(&moment(Some(Panel::new())), 80, 24);
        assert!(out.contains("deepseek"), "{out}");
        assert!(out.contains("1 个模型"), "{out}");
        assert!(out.contains("有密钥"), "{out}");
        assert!(out.contains("添加 provider"), "the add row is last: {out}");
    }

    /// The panel draws a key as dots because dots are all it is given — the
    /// characters never reach this side (`crate::providers`).
    #[test]
    fn a_key_is_drawn_as_dots() {
        let mut form = AccountForm::add(&view());
        form.focus = AccountField::Key;
        form.key_len = 5;
        let m = moment(Some(Panel {
            form: Some(Form::Account(form)),
            ..Panel::new()
        }));
        let out = drawn(&m, 80, 24);
        assert!(out.contains("•••••"), "{out}");
        assert!(out.contains("密钥"), "{out}");
    }

    #[test]
    fn an_edit_says_what_an_empty_key_field_means() {
        let v = view();
        let form = AccountForm::edit(&v, v.account("deepseek").unwrap());
        let m = moment(Some(Panel {
            form: Some(Form::Account(form)),
            ..Panel::new()
        }));
        assert!(drawn(&m, 80, 24).contains("留空则不改"));
    }

    /// Two `None`s compare equal, so an unarmed panel used to draw "press again
    /// to delete" on the add row — the one row with no id to arm.
    #[test]
    fn the_add_row_never_says_something_is_about_to_be_deleted() {
        let m = moment(Some(Panel::new()));
        let out = drawn(&m, 80, 24);
        assert!(!out.contains("再按一次"), "{out}");
        // And when something *is* armed, it says so on that row and nowhere else.
        let armed = moment(Some(Panel {
            pending_delete: Some("deepseek".into()),
            ..Panel::new()
        }));
        let out = drawn(&armed, 80, 24);
        // On the row it is armed on, and nowhere else. (The legend says it too,
        // in its own words — that is the key's caption, not a row's warning.)
        assert_eq!(out.matches("再按一次 ^d 删除").count(), 1, "{out}");
    }

    /// A click is answered from the layout that was drawn, not from a formula
    /// beside it: move the panel's chrome and this moves with it.
    #[test]
    fn a_click_finds_the_row_that_was_drawn() {
        let m = moment(Some(Panel::new()));
        let vp = Viewport::new(Rect::sized(80, 24), &m);
        let geom = geometry(&m, &vp);
        let rows = layout(&m.providers, m.providers_panel.as_ref().unwrap(), 24);
        let first = rows
            .iter()
            .position(|row| matches!(row, Row::Listed(0)))
            .expect("the first account is drawn");
        assert_eq!(geom.listed_at(first), Some(0));
        assert_eq!(geom.listed_at(first + 1), Some(1));
        assert_eq!(geom.listed_at(0), None, "the rule is not a row");
    }

    /// The panel opens with a rule, so its tabs are not on its first row — and
    /// a hit test that assumed they were is a tab nobody can click.
    #[test]
    fn the_tabs_are_found_on_the_row_they_were_drawn_on() {
        let m = moment(Some(Panel::new()));
        let vp = Viewport::new(Rect::sized(80, 24), &m);
        assert_eq!(geometry(&m, &vp).header_row(), Some(1));
    }

    #[test]
    fn the_header_is_where_a_click_on_a_tab_lands() {
        // The hit test walks the same header the frame drew, so the second tab's
        // own cells answer with the second tab.
        let out = drawn(&moment(Some(Panel::new())), 80, 24);
        let header = out.lines().nth(1).expect("the header is the second row");
        let at = header.find("模型").expect("the tab is drawn");
        // `find` is a byte offset and the hit test counts cells; the label is
        // past the ASCII name, so count the cells of what precedes it.
        let col = crate::width::str_width(&header[..at]);
        assert_eq!(tab_at(col), Some(Tab::Models));
        assert_eq!(tab_at(0), None, "the margin belongs to nobody");
    }

    /// A list longer than the room scrolls under the cursor rather than pushing
    /// the panel past what it asked for.
    #[test]
    fn a_long_list_keeps_the_cursor_in_view() {
        let many: Vec<AccountRow> = (0..40).map(|i| account(&format!("a{i}"), 0)).collect();
        let v = ProvidersView::new(many, Vec::new(), Vec::new(), Vec::new());
        let panel = Panel {
            cursor: 30,
            ..Panel::new()
        };
        let m = Moment {
            providers: v,
            providers_panel: Some(panel.clone()),
            ..Moment::default()
        };
        let rows = layout(&m.providers, &panel, 14);
        let shown: Vec<usize> = rows
            .iter()
            .filter_map(|row| match row {
                Row::Listed(at) => Some(*at),
                _ => None,
            })
            .collect();
        assert!(shown.contains(&30), "the cursor is on screen: {shown:?}");
        assert!(shown.len() <= MOST, "and the panel stays a panel");
        assert!(
            lines(&m, 60, 14).len() <= 14,
            "never taller than the room it was given"
        );
    }
}
