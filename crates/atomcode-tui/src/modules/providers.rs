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
use crate::i18n::product::{t as pt, Msg as PMsg};
use crate::i18n::{t, Msg};
use crate::module::{Height, View};
use crate::modules::chrome::{
    self, box_edge, edit_line, pad_to, panel_edge, search_line, LABEL_MAX, LABEL_MIN, LEAD,
};
use crate::moment::{Moment, Viewport};
use crate::providers::{
    picked_rows, AccountField, AccountForm, Form, Listed, ModelField, ModelForm, Panel, Picking,
    ProvidersView, Tab,
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
    Capabilities(usize),
    SelectedModels,
    DiscoveryError,
    AccountSection(bool),
    /// The model picker over the add form: the listing is on its way.
    PickLoading,
    /// …what is typed to filter it.
    PickFilter,
    /// …one listed model, by its index into the filtered rows.
    Pick(usize),
}

fn layout(view: &ProvidersView, panel: &Panel, h: usize) -> Vec<Row> {
    let mut rows = vec![Row::Rule, Row::Header];
    if let Some(Form::Model(ModelForm {
        picking: Some(picking),
        ..
    })) = &panel.form
    {
        rows.push(Row::FormHead);
        rows.push(Row::Blank);
        match picking {
            Picking::Loading { .. } => rows.push(Row::PickLoading),
            Picking::Ready {
                items,
                query,
                cursor,
            } => {
                rows.push(Row::PickFilter);
                let listed = picked_rows(items, query).len();
                if listed == 0 {
                    rows.push(Row::Nothing);
                } else {
                    // The same budget and window the list keeps (below).
                    let cap = h.saturating_sub(rows.len() + 2).min(MOST);
                    let (from, to) = if listed > cap {
                        let room = cap.saturating_sub(1).max(1);
                        let (from, to) = window(listed, *cursor, room);
                        rows.push(Row::Scroll {
                            above: from,
                            below: listed - to,
                        });
                        (from, to)
                    } else {
                        (0, listed)
                    };
                    rows.extend((from..to).map(Row::Pick));
                }
            }
        }
        rows.push(Row::Blank);
        rows.push(Row::Legend);
        return rows;
    }
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
            if let Form::Model(form) = form {
                if !form.model.is_empty() {
                    rows.extend((0..3).map(Row::Capabilities));
                    if !form.selected.is_empty() {
                        rows.push(Row::SelectedModels);
                    }
                }
                if form.discovery_error.is_some() {
                    rows.push(Row::DiscoveryError);
                }
            }
            rows.push(Row::Blank);
            rows.push(Row::Legend);
        }
        None => {
            rows.push(Row::BoxTop);
            rows.push(Row::Search);
            rows.push(Row::BoxBottom);
            let listed = view.listed(panel);
            let mut entries = Vec::new();
            let mut section = None;
            for (at, item) in listed.iter().enumerate() {
                if let Listed::Account(i) = item {
                    let configured = view.accounts()[*i].configured;
                    if section != Some(configured) {
                        entries.push(Row::AccountSection(configured));
                        section = Some(configured);
                    }
                }
                entries.push(Row::Listed(at));
            }
            if entries.is_empty() {
                rows.push(Row::Nothing);
            } else {
                // Section headings consume the same row budget as entries.
                // The cursor still refers to the selectable list's indices.
                let cap = h.saturating_sub(rows.len() + 2).min(MOST);
                let cursor = entries
                    .iter()
                    .position(|row| *row == Row::Listed(panel.cursor))
                    .unwrap_or(0);
                let (from, to) = if entries.len() > cap {
                    let room = cap.saturating_sub(1).max(1);
                    let (from, to) = window(entries.len(), cursor, room);
                    rows.push(Row::Scroll {
                        above: from,
                        below: entries.len() - to,
                    });
                    (from, to)
                } else {
                    (0, entries.len())
                };
                rows.extend_from_slice(&entries[from..to]);
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
            let labels: Vec<String> = Tab::ALL.iter().map(|t| t.label()).collect();
            let labels: Vec<&str> = labels.iter().map(String::as_str).collect();
            let at = Tab::ALL.iter().position(|t| *t == panel.tab).unwrap_or(0);
            Line::from_spans(chrome::header_parts(NAME, &labels, at).0).truncate(w)
        }
        Row::BoxTop => box_edge(w, caps, true),
        Row::Search => search_line(&panel.query, Some(panel.query.len()), w, caps),
        Row::BoxBottom => box_edge(w, caps, false),
        Row::Blank => Line::empty(),
        Row::Nothing => Line::styled(
            width::take_width(&t(Msg::ProvidersNoMatch), w),
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
        Row::AccountSection(configured) => Line::styled(
            format!(
                "  {}",
                t(if configured {
                    Msg::ProviderConnected
                } else {
                    Msg::ProviderAvailable
                })
            ),
            theme::fg(Role::Muted),
        )
        .truncate(w),
        Row::Capabilities(at) => {
            let text = match &panel.form {
                Some(Form::Model(form)) => match at {
                    0 => capability_text(
                        form.window.or_else(|| {
                            form.window_typed
                                .as_deref()
                                .and_then(crate::providers::parse_window)
                        }),
                        form.max_tokens,
                        None,
                        None,
                        &[],
                    ),
                    1 => format!(
                        "{} · {}",
                        t(Msg::FieldVision),
                        t(match form.vision {
                            Some(true) => Msg::VisionYes,
                            Some(false) => Msg::VisionNo,
                            None => Msg::ProviderCapabilitiesUnknown,
                        })
                    ),
                    _ => capability_text(
                        None,
                        None,
                        None,
                        form.effort.as_deref(),
                        &form.declared_efforts(view),
                    ),
                },
                _ => String::new(),
            };
            Line::styled(format!("  {text}"), theme::fg(Role::Muted)).truncate(w)
        }
        Row::SelectedModels => {
            let text = match &panel.form {
                Some(Form::Model(form)) => format!(
                    "{}: {}",
                    t(Msg::ProviderAdditionalModels),
                    form.selected
                        .iter()
                        .map(|row| row.id.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                _ => String::new(),
            };
            Line::styled(format!("  {text}"), theme::fg(Role::Secondary)).truncate(w)
        }
        Row::DiscoveryError => {
            let text = match &panel.form {
                Some(Form::Model(form)) => form.discovery_error.as_deref().unwrap_or_default(),
                _ => "",
            };
            Line::styled(format!("  {text}"), theme::fg(Role::Warning)).truncate(w)
        }
        Row::PickLoading => Line::styled(
            width::take_width(&format!("  {}", t(Msg::ProviderDiscovering)), w),
            theme::fg(Role::Muted),
        ),
        Row::PickFilter => {
            let (query, count) = match &panel.form {
                Some(Form::Model(ModelForm {
                    picking: Some(Picking::Ready { query, .. }),
                    selected,
                    ..
                })) => (query.as_str(), selected.len()),
                _ => ("", 0),
            };
            Line::styled(
                width::take_width(
                    &format!("  [x] {count} · {}", t(Msg::ProviderPickFilter { query })),
                    w,
                ),
                theme::fg(Role::Secondary),
            )
        }
        Row::Pick(at) => pick_line(panel, at, w),
        Row::Legend => {
            let mut line = Line::styled("  ", theme::fg(Role::Muted));
            for (index, (text, key)) in legend_buttons(view, panel, caps).into_iter().enumerate() {
                if index > 0 {
                    line.push(Span::styled(
                        format!(" {} ", caps.g(crate::caps::Glyph::Separator)),
                        theme::fg(Role::Muted),
                    ));
                }
                line.push(Span::styled(
                    text,
                    theme::fg(if key.is_some() {
                        Role::Accent
                    } else {
                        Role::Muted
                    }),
                ));
            }
            line.truncate(w)
        }
    }
}

fn capability_text(
    window: Option<usize>,
    output: Option<usize>,
    vision: Option<bool>,
    effort: Option<&str>,
    levels: &[String],
) -> String {
    let mut parts = Vec::new();
    if let Some(window) = window {
        parts.push(format!(
            "{} {}",
            t(Msg::FieldWindow),
            crate::content::token_count(window as u32)
        ));
    }
    if let Some(output) = output {
        parts.push(format!(
            "{} {}",
            t(Msg::ProviderOutputLimit),
            crate::content::token_count(output as u32)
        ));
    }
    if let Some(vision) = vision {
        parts.push(format!(
            "{} {}",
            t(Msg::FieldVision),
            t(if vision {
                Msg::VisionYes
            } else {
                Msg::VisionNo
            })
        ));
    }
    if !levels.is_empty() {
        parts.push(format!("{} {}", t(Msg::FieldEffort), levels.join("/")));
    }
    if let Some(effort) = effort {
        parts.push(format!("{} {effort}", t(Msg::StatusEffortDefault)));
    }
    if parts.is_empty() {
        t(Msg::ProviderCapabilitiesUnknown).into_owned()
    } else {
        parts.join(" · ")
    }
}

/// One row of the model picker: the id, and the context window the listing
/// gave at the right when it gave one. The row the arrows are on is lit.
fn pick_line(panel: &Panel, at: usize, w: usize) -> Line {
    let Some(Form::Model(ModelForm {
        picking:
            Some(Picking::Ready {
                items,
                query,
                cursor,
            }),
        ..
    })) = &panel.form
    else {
        return Line::empty();
    };
    let rows = picked_rows(items, query);
    let Some(item) = rows.get(at) else {
        return Line::empty();
    };
    let here = at == *cursor;
    let selected = matches!(&panel.form, Some(Form::Model(form)) if form.selected.iter().any(|row| row.id == item.id));
    let mark = match (here, selected) {
        (true, true) => "› [x] ",
        (false, true) => "  [x] ",
        (true, false) => "› [ ] ",
        (false, false) => "  [ ] ",
    };
    let window = capability_text(
        item.window,
        None,
        item.vision,
        None,
        item.levels.as_deref().unwrap_or_default(),
    );
    // Keep the model id legible even when its capability list is long.
    let left = format!("  {mark}{}", item.id);
    let minimum = width::str_width(&left).min(w / 2).max(16).min(w);
    let window = width::take_width(&window, w.saturating_sub(minimum + 1));
    let room = w.saturating_sub(width::str_width(&window) + 1);
    let left = width::take_width(&left, room);
    let pad = w.saturating_sub(width::str_width(&left) + width::str_width(&window));
    let text = format!("{left}{}{window}", " ".repeat(pad));
    let style = if here {
        theme::bg(Role::PanelSelBg).under(theme::fg(Role::PanelFg))
    } else {
        theme::fg(Role::Secondary)
    };
    Line::styled(text, style).truncate(w)
}

fn form_title(panel: &Panel) -> String {
    match &panel.form {
        Some(Form::Account(form)) if form.preset.is_some() => t(Msg::ProviderConnectTitle {
            id: form.preset.as_deref().unwrap_or_default(),
        })
        .into_owned(),
        Some(Form::Account(form)) => match &form.editing {
            Some(id) => t(Msg::ProviderFormEdit { id }).into_owned(),
            None => t(Msg::ProviderFormAddAccount).into_owned(),
        },
        Some(Form::Model(form)) => match &form.editing {
            Some(id) => t(Msg::ProviderFormEdit { id }).into_owned(),
            None => t(Msg::ProviderFormAddModelTo {
                account: form.account_id(),
            })
            .into_owned(),
        },
        None => String::new(),
    }
}

/// The same right-hand edit cells are used for drawing and hit testing.
fn row_edit_button(
    view: &ProvidersView,
    panel: &Panel,
    at: usize,
    w: usize,
) -> Option<(usize, String)> {
    let Listed::Model(i) = *view.listed(panel).get(at)? else {
        return None;
    };
    if view.models().get(i)?.managed || w < 32 {
        return None;
    }
    let text = format!("[ {} ]", t(Msg::ProviderEdit));
    Some((w.checked_sub(width::str_width(&text))?, text))
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
    let Some((mark, label, about, dim)) = listed_parts(view, panel, what, caps) else {
        return Line::empty();
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
                Listed::Effort(_) | Listed::Group(_) | Listed::Add => None,
            }
    });
    let edit = row_edit_button(view, panel, at, w);
    let content_w = edit
        .as_ref()
        .map(|(start, _)| start.saturating_sub(1))
        .unwrap_or(w);
    let label_room = label_column(view, panel, content_w, caps);
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
        t(Msg::ArmedDelete).into_owned()
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
    let mut line = pad_to(Line::from_spans(spans), content_w, base);
    if let Some((_, text)) = edit {
        line.push(Span::styled(" ", base));
        line.push(Span::styled(text, base.under(theme::fg(Role::Accent))));
    }
    line
}

/// What one listed row says: its mark, its label, what is worth knowing about
/// it, and whether it is drawn dim. `None` for a row the view no longer has.
fn listed_parts(
    view: &ProvidersView,
    panel: &Panel,
    what: Listed,
    caps: crate::caps::Caps,
) -> Option<(String, String, String, bool)> {
    Some(match what {
        // 一组的小标题:账号的名字,暗一档,没有标记也没有右侧说明——它不是
        // 一行可以做点什么的东西,是一条分界。
        Listed::Group(first) => {
            let Some(row) = view.models().get(first) else {
                return None;
            };
            (" ".to_string(), row.account.clone(), String::new(), true)
        }
        Listed::Add => (
            " ".to_string(),
            match panel.tab {
                Tab::Accounts => t(Msg::ProviderFormAddAccount),
                Tab::Models => t(Msg::ProviderFormAddModel),
            }
            .into_owned(),
            "^a".to_string(),
            true,
        ),
        Listed::Account(i) => {
            let Some(row) = view.accounts().get(i) else {
                return None;
            };
            let host = row.endpoint_host();
            let mut about = vec![if host.is_empty() {
                row.protocol.clone()
            } else {
                host.to_string()
            }];
            if row.configured {
                about.push(t(Msg::ProviderModelCount { n: row.models }).into_owned());
                about.push(
                    if row.has_key {
                        t(Msg::ProviderHasKey)
                    } else {
                        t(Msg::ProviderNoKey)
                    }
                    .into_owned(),
                );
                if row.managed {
                    about.push(t(Msg::ProviderManaged).into_owned());
                }
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
        Listed::Effort(i) => {
            let Some(pick) = panel.efforts.as_ref() else {
                return None;
            };
            let Some(level) = pick.levels.get(i) else {
                return None;
            };
            // 收尾那一行是把强度交回端点,它要说的正是「不指定」。
            let about = if i + 1 == pick.levels.len() {
                t(Msg::EffortDefaultAbout).into_owned()
            } else {
                String::new()
            };
            (" ".to_string(), level.clone(), about, false)
        }
        Listed::Model(i) => {
            let Some(row) = view.models().get(i) else {
                return None;
            };
            let mut about = vec![crate::providers::window_label(row.window)];
            if panel.drill.is_none() {
                about.insert(0, row.account.clone());
            }
            if let Some(effort) = &row.effort {
                about.push(effort.clone());
            }
            if row.vision == Some(true) {
                about.push(t(Msg::ProviderVision).into_owned());
            }
            if row.managed {
                about.push(t(Msg::ProviderManaged).into_owned());
            }
            (
                mark_for(row.current, caps),
                row.id.clone(),
                about.join(" · "),
                false,
            )
        }
    })
}

/// How much room the descriptions keep at the right of a list row, at least.
const ABOUT_MIN: usize = 20;

/// How wide the label column of the list is drawn: as wide as the widest label
/// listed, so every row lines up and none is cut — as long as the descriptions
/// still keep [`ABOUT_MIN`] beside it.
///
/// Not the form's fixed [`LABEL_MAX`]: a form's labels are short field names,
/// while a list's are model ids, and an OpenRouter id
/// (`openrouter/thinkingmachines/…`) is past thirty columns before it says
/// which model it is. Capped at thirty, every such row was cut to the same
/// prefix on a terminal with a hundred columns to spare, and rows that differ
/// only at the end could not be told apart.
fn label_column(view: &ProvidersView, panel: &Panel, w: usize, caps: crate::caps::Caps) -> usize {
    let widest = view
        .listed(panel)
        .into_iter()
        .filter_map(|what| listed_parts(view, panel, what, caps))
        .map(|(_, label, _, _)| width::str_width(&label))
        .max()
        .unwrap_or(0);
    // Pointer and mark before the label, two spaces after it.
    let room = w.saturating_sub(LEAD + 2 + 2 + ABOUT_MIN);
    widest.clamp(LABEL_MIN, room.max(LABEL_MIN))
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
            account_field(view, f, field, focused, caps)
        }
        Form::Model(f) => {
            let fields = f.fields(view);
            let Some(field) = fields.get(at).copied() else {
                return Line::empty();
            };
            let focused = field == f.focus;
            model_field(view, f, field, focused, caps)
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
    caps: crate::caps::Caps,
) -> (String, String, bool, bool, usize) {
    match field {
        AccountField::Name => (
            t(Msg::FieldName).into_owned(),
            form.name.clone(),
            focused,
            true,
            form.caret,
        ),
        AccountField::Protocol => (
            t(Msg::FieldProtocol).into_owned(),
            cycled(&form.protocol_label(view), focused),
            focused,
            false,
            0,
        ),
        AccountField::Endpoint => (
            t(Msg::FieldAddress).into_owned(),
            form.endpoint.clone(),
            focused,
            true,
            form.caret,
        ),
        AccountField::Key => (
            t(Msg::FieldKey).into_owned(),
            dots(form.key_len, form.editing.is_some(), caps),
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
    caps: crate::caps::Caps,
) -> (String, String, bool, bool, usize) {
    match field {
        ModelField::Account => (
            "provider".into(),
            cycled(form.account_id(), focused),
            focused,
            false,
            0,
        ),
        ModelField::Key => (
            t(Msg::FieldKey).into_owned(),
            dots(form.key_len, false, caps),
            focused,
            false,
            0,
        ),
        ModelField::Model => (
            pt(PMsg::ProviderPanelFieldModel).into_owned(),
            form.model.clone(),
            focused,
            true,
            form.caret,
        ),
        ModelField::Vision => (
            t(Msg::FieldVision).into_owned(),
            cycled(
                &match form.vision {
                    None => pt(PMsg::ProviderPanelVisionAuto),
                    Some(true) => t(Msg::VisionYes),
                    Some(false) => t(Msg::VisionNo),
                },
                focused,
            ),
            focused,
            false,
            0,
        ),
        ModelField::Effort => (
            t(Msg::FieldEffort).into_owned(),
            cycled(
                &match form.effort.as_deref() {
                    Some(effort) => effort.into(),
                    None => t(Msg::EffortUnsupported),
                },
                focused,
            ),
            focused,
            false,
            0,
        ),
        ModelField::Levels => (
            t(Msg::FieldLevels).into_owned(),
            levels_text(view, form, focused, caps),
            focused,
            false,
            0,
        ),
        ModelField::Window => match &form.window_typed {
            // The custom stop is a text field while it has the keyboard, and
            // says what it holds (or that it is the custom one) when it has not.
            Some(typed) if focused => (
                t(Msg::FieldWindow).into_owned(),
                typed.clone(),
                focused,
                true,
                form.caret,
            ),
            Some(typed) => (
                t(Msg::FieldWindow).into_owned(),
                match typed.trim().is_empty() {
                    true => t(Msg::WindowCustom).into_owned(),
                    false => typed.clone(),
                },
                focused,
                false,
                0,
            ),
            None => (
                t(Msg::FieldWindow).into_owned(),
                cycled(
                    &match form.window {
                        Some(window) => crate::providers::window_label(window),
                        None => t(Msg::WindowAutomatic).into_owned(),
                    },
                    focused,
                ),
                focused,
                false,
                0,
            ),
        },
        ModelField::Default => (
            t(Msg::FieldUseAfterSaving).into_owned(),
            cycled(
                &if form.default {
                    t(Msg::YesWord)
                } else {
                    t(Msg::NoWord)
                },
                focused,
            ),
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
fn dots(len: usize, editing: bool, caps: crate::caps::Caps) -> String {
    if len == 0 {
        return match editing {
            true => t(Msg::KeyLeaveBlankToKeep).into_owned(),
            false => String::new(),
        };
    }
    // Through `Caps` rather than a literal `•`: an ASCII terminal draws that as
    // a tofu box, and a field of tofu is a worse answer about a credential than
    // a field of asterisks.
    caps.g(crate::caps::Glyph::Bullet)
        .to_string()
        .repeat(len.min(32))
}

fn levels_text(
    view: &ProvidersView,
    form: &ModelForm,
    focused: bool,
    caps: crate::caps::Caps,
) -> String {
    view.efforts()
        .iter()
        .enumerate()
        .map(|(i, level)| {
            let on = form.levels.get(i).copied().unwrap_or(true);
            let mark = match on {
                true => caps.g(crate::caps::Glyph::Ok).to_string(),
                // A space, not a second glyph: the row is read as a list of what
                // is *on*, and a mark for "off" would compete with the tick.
                false => " ".to_string(),
            };
            if focused && i == form.level {
                format!("[{level}{mark}]")
            } else {
                format!(" {level}{mark} ")
            }
        })
        .collect::<Vec<_>>()
        .join("")
}

fn legend(panel: &Panel) -> Vec<(String, String)> {
    let key = |k: &str, msg: Msg<'_>| (k.to_string(), t(msg).into_owned());
    match &panel.form {
        Some(Form::Model(ModelForm {
            picking: Some(picking),
            ..
        })) => match picking {
            Picking::Loading { .. } => vec![
                key("^n", Msg::ProviderManualAdd),
                key("esc", Msg::LegendCancel),
            ],
            Picking::Ready { .. } => {
                vec![
                    key("⏎", Msg::ProviderContinue),
                    key("space", Msg::LegendSelect),
                    key("^n", Msg::ProviderManualAdd),
                    key("esc", Msg::LegendBack),
                    (t(Msg::ProviderPickKeys).into_owned(), String::new()),
                ]
            }
        },
        Some(Form::Account(form)) if form.preset.is_some() => vec![
            key("⏎", Msg::ProviderContinue),
            key("^o", Msg::ProviderAdvanced),
            key("esc", Msg::LegendCancel),
        ],
        Some(Form::Model(form)) if form.connecting => vec![
            key("⏎", Msg::ProviderConnectUse),
            key("^s", Msg::ProviderSaveOnly),
            key("^o", Msg::ProviderAdvanced),
            key("^f", Msg::ProviderDiscoverHint),
            key("esc", Msg::LegendBack),
        ],
        // Adding, on the model field: the id can be picked rather than typed.
        Some(Form::Model(form)) if form.focus == ModelField::Model && form.editing.is_none() => {
            vec![
                key("^f", Msg::ProviderDiscoverHint),
                key("⇥", Msg::LegendNextField),
                key("⏎", Msg::LegendSave),
                key("esc", Msg::LegendCancel),
            ]
        }
        // On the levels row the keys do something the other fields' legend
        // does not say: Space turns the bracketed level on or off, and the
        // arrows move between levels rather than change a value. Said where
        // the eye is, the way the other front end says it on that row.
        Some(Form::Model(form)) if form.focus == ModelField::Levels => {
            vec![
                key("space", Msg::LegendToggleLevel),
                key("←→", Msg::LegendPickLevel),
                key("⇥", Msg::LegendNextField),
                key("⏎", Msg::LegendSave),
                key("esc", Msg::LegendCancel),
            ]
        }
        // The window row: typing a number is a custom window, and the legend
        // has to say so — on a preset nothing else would tell a person that the
        // digits keys do anything here, and in the custom field the arrows
        // leave it rather than move the caret.
        Some(Form::Model(form)) if form.focus == ModelField::Window => match form.window_typed {
            Some(_) => vec![
                (t(Msg::LegendTypeWindow).into_owned(), String::new()),
                key("←→", Msg::LegendBackToPresets),
                key("⏎", Msg::LegendSave),
                key("esc", Msg::LegendCancel),
            ],
            None => vec![
                key("←→", Msg::LegendChangeValue),
                key("0-9", Msg::WindowCustom),
                key("⇥", Msg::LegendNextField),
                key("⏎", Msg::LegendSave),
                key("esc", Msg::LegendCancel),
            ],
        },
        Some(Form::Account(_)) | Some(Form::Model(_)) => {
            vec![
                key("⇥", Msg::LegendNextField),
                key("←→", Msg::LegendChangeValue),
                key("⏎", Msg::LegendSave),
                key("esc", Msg::LegendCancel),
            ]
        }
        None => {
            // 挑强度那一层:一句话说清这一层是干什么的,按键只有两个要记。
            if let Some(pick) = &panel.efforts {
                return vec![
                    (
                        t(Msg::EffortPickAfterModel { model: &pick.model }).into_owned(),
                        String::new(),
                    ),
                    key("⏎", Msg::LegendThisOne),
                    key("esc", Msg::LegendBack),
                ];
            }
            if panel.pending_delete.is_some() {
                // Says what the next press does, because that is the only thing
                // about this state a person has to know — and it is the press
                // that throws something away.
                return vec![
                    key("^d", Msg::LegendPressAgainToDelete),
                    (
                        t(Msg::LegendAnyOtherKey).into_owned(),
                        t(Msg::LegendCancel).into_owned(),
                    ),
                ];
            }
            let mut out = vec![
                ("^a".to_string(), pt(PMsg::ProviderMenuAdd).into_owned()),
                key("^n", Msg::ProviderManualAdd),
                ("^d".to_string(), pt(PMsg::ProviderMenuDelete).into_owned()),
                key("↑↓", Msg::LegendSelect),
            ];
            if panel.tab == Tab::Accounts {
                out.insert(1, key("^e", Msg::ProviderEdit));
            }
            out.push(match panel.tab {
                Tab::Accounts => key("⏎", Msg::LegendSeeItsModels),
                Tab::Models => key("⏎", Msg::LegendSwitchToIt),
            });
            out.push(key("⇥", Msg::LegendChangePage));
            out.push(key("esc", Msg::LegendClose));
            out
        }
    }
}

/// Draw and hit-test the same button text, including terminal key names.
fn legend_buttons(
    view: &ProvidersView,
    panel: &Panel,
    caps: crate::caps::Caps,
) -> Vec<(String, Option<crate::surface::KeyPress>)> {
    use crate::surface::KeyPress;
    let (editable, deletable) = match view.listed(panel).get(panel.cursor) {
        Some(Listed::Account(i)) => view
            .accounts()
            .get(*i)
            .map(|row| (!row.managed, !row.managed && row.configured))
            .unwrap_or_default(),
        Some(Listed::Model(i)) => view
            .models()
            .get(*i)
            .map(|row| (!row.managed, !row.managed))
            .unwrap_or_default(),
        _ => (false, false),
    };
    legend(panel)
        .into_iter()
        .map(|(key, label)| {
            let press = if panel.form.is_some() {
                match key.as_str() {
                    "^o" => Some(KeyPress::ctrl('o')),
                    "^s" => Some(KeyPress::ctrl('s')),
                    "^f" => Some(KeyPress::ctrl('f')),
                    "^n" => Some(KeyPress::ctrl('n')),
                    "space" => Some(KeyPress::plain(crate::surface::Key::Char(' '))),
                    "⏎" => Some(KeyPress::plain(crate::surface::Key::Enter)),
                    "esc" => Some(KeyPress::plain(crate::surface::Key::Esc)),
                    _ => None,
                }
            } else if panel.efforts.is_none() {
                match key.as_str() {
                    "^a" => Some(KeyPress::ctrl('a')),
                    "^n" => Some(KeyPress::ctrl('n')),
                    "^e" if editable => Some(KeyPress::ctrl('e')),
                    "^d" if deletable => Some(KeyPress::ctrl('d')),
                    _ => None,
                }
            } else {
                None
            };
            let text = crate::widget::keys(&[(key.clone(), label)], caps);
            (
                if press.is_some() {
                    format!("[ {text} ]")
                } else {
                    text
                },
                press,
            )
        })
        .collect()
}

/// Only visible cells of a drawn action can activate its existing key path.
pub fn action_at(
    moment: &Moment,
    vp: &Viewport<'_>,
    col: usize,
    row: usize,
) -> Option<crate::surface::KeyPress> {
    let panel = moment.providers_panel.as_ref()?;
    let rows = layout(&moment.providers, panel, vp.rect.h as usize);
    if let Some(Row::Listed(at)) = rows.get(row) {
        let (start, text) = row_edit_button(&moment.providers, panel, *at, vp.rect.w as usize)?;
        return (col >= start && col < start + width::str_width(&text))
            .then(|| crate::surface::KeyPress::ctrl('e'));
    }
    if !matches!(rows.get(row), Some(Row::Legend)) {
        return None;
    }
    let mut start = 2;
    let gap = width::str_width(&format!(
        " {} ",
        moment.caps.g(crate::caps::Glyph::Separator)
    ));
    for (text, key) in legend_buttons(&moment.providers, panel, moment.caps) {
        let end = start + width::str_width(&text);
        if col >= start && col < end && end <= vp.rect.w as usize {
            return key;
        }
        start = end + gap;
    }
    None
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
    // 小标题不接鼠标:点它既不该选中它(停不住),也不该顺手把它底下那个模型
    // 换上——点一条分界线不是在挑东西。
    let listed = moment.providers.listed(panel);
    Geometry {
        header: rows.iter().position(|row| *row == Row::Header),
        rows: rows
            .into_iter()
            .map(|row| match row {
                Row::Listed(at) if listed.get(at).is_some_and(|row| row.selectable()) => Some(at),
                Row::Pick(at) => Some(at),
                _ => None,
            })
            .collect(),
    }
}

/// Which list is under this cell of the header row.
pub fn tab_at(col: usize) -> Option<Tab> {
    let labels: Vec<String> = Tab::ALL.iter().map(|t| t.label()).collect();
    let labels: Vec<&str> = labels.iter().map(String::as_str).collect();
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

    fn button_hit(m: &Moment, key: crate::surface::KeyPress) -> (usize, usize) {
        let vp = Viewport::new(Rect::sized(240, 30), m);
        (0..30)
            .flat_map(|row| (0..240).map(move |col| (col, row)))
            .find(|(col, row)| action_at(m, &vp, *col, *row) == Some(key))
            .expect("button is visible")
    }

    #[test]
    fn editing_by_click_and_keyboard_opens_the_same_selected_form() {
        use crate::surface::KeyPress;
        for tab in Tab::ALL {
            let mut panel = Panel::new();
            panel.show(tab);
            let view = view();
            view.settle_cursor(&mut panel);
            let m = moment(Some(panel.clone()));
            let (col, row) = button_hit(&m, KeyPress::ctrl('e'));
            let vp = Viewport::new(Rect::sized(240, 30), &m);
            let click = action_at(&m, &vp, col, row).unwrap();
            let mut by_click = panel.clone();
            let mut by_key = panel;
            let mut secret = String::new();
            assert_eq!(
                crate::providers::key(&view, &mut by_click, &mut secret, click),
                crate::providers::Step::Stay
            );
            crate::providers::key(&view, &mut by_key, &mut secret, KeyPress::ctrl('e'));
            assert_eq!(by_click, by_key);
            assert!(by_click.form.is_some());
            let drawn_row = &lines(&m, 240, 30)[row];
            let text: String = drawn_row
                .spans
                .iter()
                .map(|span| span.text.as_str())
                .collect();
            assert!(
                text.contains(match tab {
                    Tab::Accounts => "[ ^e",
                    Tab::Models => "[ 编辑 ]",
                }),
                "{text}"
            );
            let narrow = Viewport::new(Rect::sized((col + 1) as u16, 30), &m);
            assert_eq!(
                action_at(&m, &narrow, col, row),
                match tab {
                    Tab::Accounts => None,
                    // A row action follows the right edge when the panel resizes.
                    Tab::Models => Some(KeyPress::ctrl('e')),
                },
                "hit testing follows the button's current geometry"
            );
        }
    }

    #[test]
    fn adding_and_deleting_buttons_use_the_existing_key_actions() {
        use crate::surface::KeyPress;
        let mut panel = Panel::new();
        let view = view();
        view.settle_cursor(&mut panel);
        let m = moment(Some(panel.clone()));
        let vp = Viewport::new(Rect::sized(240, 30), &m);
        let mut secret = String::new();
        let (col, row) = button_hit(&m, KeyPress::ctrl('a'));
        let mut adding = panel.clone();
        crate::providers::key(
            &view,
            &mut adding,
            &mut secret,
            action_at(&m, &vp, col, row).unwrap(),
        );
        assert!(adding.form.is_some());
        for confirmation in [false, true] {
            let m = moment(Some(panel.clone()));
            let vp = Viewport::new(Rect::sized(240, 30), &m);
            let (col, row) = button_hit(&m, KeyPress::ctrl('d'));
            let step = crate::providers::key(
                &view,
                &mut panel,
                &mut secret,
                action_at(&m, &vp, col, row).unwrap(),
            );
            if confirmation {
                assert!(matches!(step, crate::providers::Step::DeleteAccount { .. }));
            } else {
                assert_eq!(step, crate::providers::Step::Stay);
                assert!(panel.pending_delete.is_some());
            }
        }
    }

    #[test]
    fn edit_and_manual_add_buttons_are_visible_and_clickable_at_normal_width() {
        use crate::surface::KeyPress;
        let panel = Panel::new();
        let m = moment(Some(panel));
        let vp = Viewport::new(Rect::sized(80, 30), &m);
        for wanted in [KeyPress::ctrl('e'), KeyPress::ctrl('n')] {
            let hit = (0..30)
                .flat_map(|row| (0..80).map(move |col| (col, row)))
                .find(|(col, row)| action_at(&m, &vp, *col, *row) == Some(wanted));
            assert!(hit.is_some(), "{wanted:?} must fit in an 80-column panel");
            let mut panel = m.providers_panel.clone().unwrap();
            crate::providers::key(&m.providers, &mut panel, &mut String::new(), wanted);
            assert!(matches!(panel.form, Some(Form::Account(_))));
        }
    }

    #[test]
    fn every_editable_model_has_its_own_edit_hit_and_body_clicks_still_select() {
        use crate::surface::KeyPress;
        let mut panel = Panel::new();
        panel.tab = Tab::Models;
        let m = moment(Some(panel));
        let vp = Viewport::new(Rect::sized(80, 30), &m);
        let mut checked = 0;
        for (row, item) in layout(&m.providers, m.providers_panel.as_ref().unwrap(), 30)
            .iter()
            .enumerate()
        {
            let Row::Listed(at) = item else {
                continue;
            };
            let Some(Listed::Model(i)) = m
                .providers
                .listed(m.providers_panel.as_ref().unwrap())
                .get(*at)
                .copied()
            else {
                continue;
            };
            let model = &m.providers.models()[i];
            assert!(drawn(&m, 80, 30).contains(&model.model));
            assert_eq!(
                action_at(&m, &vp, 79, row),
                (!model.managed).then(|| KeyPress::ctrl('e'))
            );
            assert_eq!(action_at(&m, &vp, 4, row), None);
            if !model.managed {
                let mut target = m.providers_panel.clone().unwrap();
                target.cursor = *at;
                crate::providers::key(
                    &m.providers,
                    &mut target,
                    &mut String::new(),
                    action_at(&m, &vp, 79, row).unwrap(),
                );
                let Some(Form::Model(form)) = target.form else {
                    panic!("model edit");
                };
                assert_eq!(form.editing.as_deref(), Some(model.id.as_str()));
                checked += 1;
            }
        }
        assert!(checked > 1, "each model needs an independent edit action");
    }

    #[test]
    fn an_add_row_has_no_edit_or_delete_button_action() {
        let mut panel = Panel::new();
        panel.cursor = view().listed(&panel).len() - 1;
        let m = moment(Some(panel));
        let vp = Viewport::new(Rect::sized(240, 30), &m);
        for row in 0..30 {
            for col in 0..240 {
                assert!(
                    !matches!(action_at(&m, &vp, col, row), Some(key) if key == crate::surface::KeyPress::ctrl('e') || key == crate::surface::KeyPress::ctrl('d'))
                );
            }
        }
    }

    #[test]
    fn managed_and_unconfigured_accounts_disable_unavailable_actions() {
        use crate::surface::KeyPress;
        for (managed, configured) in [(true, true), (false, false)] {
            let mut row = account("offered", 0);
            row.managed = managed;
            row.configured = configured;
            let view = ProvidersView::new(vec![row], vec![], vec![], vec![]);
            let panel = Panel::new();
            let buttons = legend_buttons(&view, &panel, crate::caps::Caps::default());
            assert!(!buttons
                .iter()
                .any(|(_, key)| *key == Some(KeyPress::ctrl('d'))));
            assert_eq!(
                buttons
                    .iter()
                    .any(|(_, key)| *key == Some(KeyPress::ctrl('e'))),
                !managed
            );
        }
    }

    fn picker_panel(picking: Picking) -> Panel {
        let mut form = ModelForm::add(&view(), Some("local")).expect("an account");
        form.focus = ModelField::Model;
        form.picking = Some(picking);
        Panel {
            tab: Tab::Models,
            form: Some(Form::Model(form)),
            ..Panel::new()
        }
    }

    /// The picker draws over the add form: what is being filtered, the rows
    /// that match with the window the listing gave, the lit one first, and
    /// its keys — and never wider than its rect.
    #[test]
    fn the_model_picker_is_drawn_over_the_form() {
        let items = vec![
            crate::providers::Discovered {
                id: "qwen3-coder".into(),
                window: Some(256_000),
                ..Default::default()
            },
            crate::providers::Discovered {
                id: "glm-5".into(),
                window: None,
                ..Default::default()
            },
        ];
        let m = moment(Some(picker_panel(Picking::Ready {
            items: items.clone(),
            query: "qwen".into(),
            cursor: 0,
        })));
        let screen = drawn(&m, 60, 24);
        assert!(screen.contains("筛选:qwen"), "{screen}");
        assert!(screen.contains("› [ ] qwen3-coder"), "{screen}");
        assert!(screen.contains("256.0K"), "{screen}");
        assert!(!screen.contains("glm-5"), "filtered out:\n{screen}");
        assert!(
            screen.contains("继续") && screen.contains("手动添加"),
            "{screen}"
        );
        for w in [4u16, 20, 60] {
            for line in lines(&m, w, 24) {
                assert!(line.width() <= w as usize, "{} > {w}", line.width());
            }
        }

        let loading = moment(Some(picker_panel(Picking::Loading {
            account: "local".into(),
        })));
        assert!(drawn(&loading, 60, 24).contains("正在从服务端拉取"));
    }

    /// Adding, on the model field: the legend names the key that picks it.
    #[test]
    fn the_add_form_says_the_model_can_be_fetched() {
        let mut form = ModelForm::add(&view(), Some("local")).expect("an account");
        form.focus = ModelField::Model;
        let m = moment(Some(Panel {
            tab: Tab::Models,
            form: Some(Form::Model(form)),
            ..Panel::new()
        }));
        let screen = drawn(&m, 80, 24);
        assert!(
            screen.contains("^f") && screen.contains("从服务端拉取"),
            "{screen}"
        );
    }

    #[test]
    fn account_rows_show_and_search_the_actual_host_without_credentials() {
        let mut row = account("custom", 0);
        row.endpoint = "https://user:secret@proxy.example:8443/v1?key=private".into();
        let view = ProvidersView::new(vec![row], Vec::new(), Vec::new(), Vec::new());
        let m = Moment {
            providers: view.clone(),
            providers_panel: Some(Panel::new()),
            ..Moment::default()
        };
        let text = drawn(&m, 120, 24);
        assert!(text.contains("proxy.example:8443"), "{text}");
        assert!(
            !text.contains("secret") && !text.contains("private") && !text.contains("/v1"),
            "{text}"
        );
        let panel = Panel {
            query: "proxy.example".into(),
            ..Panel::new()
        };
        assert!(view.listed(&panel).contains(&Listed::Account(0)));
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

    /// OpenRouter ids run past thirty columns before they say which model they
    /// are. On a wide terminal each is drawn whole, and the descriptions still
    /// line up; on a narrow one the ids give way first and the descriptions keep
    /// their room.
    #[test]
    fn long_model_ids_are_drawn_whole_when_there_is_room() {
        let ids = [
            "openrouter/thinkingmachines/inkling-2-preview",
            "openrouter/nvidia/nemotron-3-ultra-253b",
            "openrouter/dots-studio/dots-3-large",
        ];
        let m = Moment {
            providers: ProvidersView::new(
                vec![account("openrouter", 3)],
                ids.iter()
                    .map(|id| model(id, "openrouter", false))
                    .collect(),
                Vec::new(),
                Vec::new(),
            ),
            providers_panel: Some(Panel {
                tab: Tab::Models,
                ..Panel::new()
            }),
            ..Moment::default()
        };

        let wide = drawn(&m, 200, 24);
        for id in ids {
            assert!(wide.contains(id), "{id} is cut:\n{wide}");
        }
        let about_at: Vec<usize> = wide
            .lines()
            .filter(|l| ids.iter().any(|id| l.contains(id)))
            .map(|l| {
                let (before, _) = l.split_once("openrouter · ").expect("described");
                width::str_width(before)
            })
            .collect();
        assert_eq!(about_at.len(), ids.len());
        assert!(
            about_at.windows(2).all(|p| p[0] == p[1]),
            "the descriptions line up: {about_at:?}\n{wide}"
        );

        let narrow = drawn(&m, 60, 24);
        assert!(
            narrow.contains("openrouter · 128K"),
            "the description keeps its room when the ids cannot:\n{narrow}"
        );
        for line in lines(&m, 60, 24) {
            assert!(line.width() <= 60);
        }
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
        let dot = crate::caps::Caps::default().g(crate::caps::Glyph::Bullet);
        assert!(out.contains(&dot.to_string().repeat(5)), "{out}");
        assert!(out.contains("密钥"), "{out}");
    }

    /// On the levels row the legend says what Space does there — turn the
    /// bracketed level on or off — which the other fields' legend does not.
    /// Off that row it is the ordinary legend again.
    #[test]
    fn the_levels_row_says_space_turns_a_level_on_or_off() {
        let mut form = ModelForm::add(&view(), None).expect("an account to add to");
        form.focus = ModelField::Levels;
        let m = moment(Some(Panel {
            form: Some(Form::Model(form.clone())),
            ..Panel::new()
        }));
        let out = drawn(&m, 100, 24);
        assert!(out.contains("启用/停用这一档"), "{out}");
        assert!(out.contains("space"), "{out}");

        form.focus = ModelField::Window;
        let m = moment(Some(Panel {
            form: Some(Form::Model(form)),
            ..Panel::new()
        }));
        let out = drawn(&m, 100, 24);
        assert!(!out.contains("启用/停用这一档"), "{out}");
        assert!(
            out.contains("‹ 自动（按协议默认） ›"),
            "the window is a choice, automatic to begin with: {out}"
        );
    }

    /// The window row says that typing a number makes a custom window, and
    /// the custom field says what it takes and how to leave it.
    #[test]
    fn the_window_row_says_how_to_type_a_custom_one() {
        let mut form = ModelForm::add(&view(), None).expect("an account to add to");
        form.focus = ModelField::Window;
        let panel = |form: &ModelForm| {
            moment(Some(Panel {
                form: Some(Form::Model(form.clone())),
                ..Panel::new()
            }))
        };
        let out = drawn(&panel(&form), 100, 24);
        assert!(out.contains("0-9"), "{out}");

        form.window_typed = Some("200k".into());
        form.caret = 4;
        let out = drawn(&panel(&form), 100, 24);
        assert!(out.contains("200k"), "{out}");
        assert!(out.contains("200k、1m"), "{out}");
        assert!(out.contains("换回预设"), "{out}");
    }

    #[test]
    fn a_connection_hides_address_and_offers_clickable_advanced_settings() {
        use crate::surface::KeyPress;
        let v = view();
        let form = AccountForm::connect(&v, v.account("deepseek").unwrap());
        let m = moment(Some(Panel {
            form: Some(Form::Account(form)),
            ..Panel::new()
        }));
        let text = drawn(&m, 120, 24);
        assert!(text.contains(&t(Msg::FieldKey).to_string()), "{text}");
        assert!(!text.contains("https://"), "{text}");
        let (col, row) = button_hit(&m, KeyPress::ctrl('o'));
        let vp = Viewport::new(Rect::sized(120, 24), &m);
        assert_eq!(action_at(&m, &vp, col, row), Some(KeyPress::ctrl('o')));
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
    #[allow(
        clippy::string_slice,
        reason = "test: `at` is a `find` on the same header, a char boundary"
    )]
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
