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
    self, box_edge, edit_line_with_label_width, pad_to, panel_edge, search_line, LABEL_MAX,
    LABEL_MIN, LEAD,
};
use crate::moment::{Moment, Viewport};
use crate::providers::{
    AccountField, AccountForm, DiscoveryState, Form, Listed, ModelField, ModelForm, Panel,
    ProvidersView, Screen, Section,
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
        layout(view, panel, vp.rect.h as usize, w)
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
            layout(&moment.providers, &open, usize::MAX, width as usize)
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
    Card(String),
    /// The rule the panel starts with.
    Rule,
    /// The name of the screen: `model`, `provider`, `provider › deepseek`.
    Header,
    BoxTop,
    Search,
    BoxBottom,
    Blank,
    /// A row of the list, by its index into [`ProvidersView::listed`].
    Listed(usize),
    /// A model row's description, on its own line under the model — dim,
    /// indented, and never selectable. By the model's index into
    /// [`ProvidersView::models`].
    ModelDesc(usize),
    /// What a filter that matched nothing says.
    Nothing,
    /// How many rows are out of sight above and below.
    Scroll {
        above: usize,
        below: usize,
    },
    /// One field of the form that is up, by its index into the form's fields.
    Field(usize),
    /// What the form is for: `添加 provider`, `改 deepseek`, …
    FormHead,
    Legend,
    Capabilities(usize),
    DiscoveryMessage(String),
    AccountSection(bool),
}

fn layout(view: &ProvidersView, panel: &Panel, h: usize, w: usize) -> Vec<Row> {
    let mut rows = vec![Row::Rule, Row::Header];
    if let Some(card) = &panel.card {
        rows.push(Row::Card(t(Msg::ModelCardHelp).into_owned()));
        let content = crate::model_cards::lines(view, card, w.saturating_sub(2).max(1));
        let room = crate::model_cards::viewport_rows(content.len(), h);
        let start = card.scroll.min(content.len().saturating_sub(room.max(1)));
        rows.extend(
            content
                .iter()
                .skip(start)
                .take(room)
                .cloned()
                .map(Row::Card),
        );
        rows.push(Row::Card(format!(
            "Esc {} · d {}",
            t(Msg::LegendBack),
            t(Msg::ModelCardNotes)
        )));
        rows.push(Row::Card(format!(
            "{}–{} / {}",
            start + 1,
            (start + room).min(content.len()),
            content.len()
        )));
        return rows;
    }

    match &panel.form {
        Some(form) => {
            rows.push(Row::FormHead);
            rows.push(Row::Blank);
            if let Form::Account(f) = form {
                if !f.fields(view).contains(&AccountField::Protocol) {
                    rows.push(Row::Card(format!(
                        "  {}  {}",
                        t(Msg::FieldProtocol),
                        f.protocol_label(view)
                    )));
                }
                if !f.fields(view).contains(&AccountField::Endpoint) && !f.endpoint.is_empty() {
                    rows.extend(
                        width::wrap(
                            &format!("{}  {}", t(Msg::FieldAddress), f.endpoint),
                            w.saturating_sub(2).max(1),
                        )
                        .into_iter()
                        .map(Row::Card),
                    );
                }
            }
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
                }
            }
            if let Form::Account(form) = form {
                if let Some(discovery) = &form.discovery {
                    let message = match &discovery.result {
                        None => t(Msg::ProviderDiscovering).into_owned(),
                        Some(Err(error)) => error.clone(),
                        Some(Ok(models)) => t(Msg::OllamaModelsFound {
                            count: models.len(),
                        })
                        .into_owned(),
                    };
                    rows.extend(
                        width::wrap(&message, w.saturating_sub(2).max(1))
                            .into_iter()
                            .map(Row::DiscoveryMessage),
                    );
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
            // The screen's add row is drawn under the scrolling part, so a long
            // list never takes it out of sight.
            let pinned = listed
                .last()
                .filter(|row| row.pinned())
                .map(|_| listed.len() - 1);
            let mut entries = Vec::new();
            let mut section = None;
            for (at, item) in listed.iter().enumerate() {
                if Some(at) == pinned {
                    continue;
                }
                if let Listed::Account(i) = item {
                    let configured = view.accounts()[*i].configured;
                    if section != Some(configured) {
                        entries.push(Row::AccountSection(configured));
                        section = Some(configured);
                    }
                }
                entries.push(Row::Listed(at));
                // A model the gateway gave a description to says it on the line
                // under its own, dim and not selectable — the row budget and
                // the scroll window treat it like any other row.
                if let Listed::Model(i) | Listed::Configured(i) = item {
                    if view
                        .models()
                        .get(*i)
                        .is_some_and(|m| m.localized_desc().is_some())
                    {
                        entries.push(Row::ModelDesc(*i));
                    }
                }
            }
            if entries.is_empty() {
                if !panel.query.is_empty() || pinned.is_none() {
                    rows.push(Row::Nothing);
                }
            } else {
                // Section headings consume the same row budget as entries, and
                // so does the pinned row under them.
                let reserved = rows.len() + 2 + usize::from(pinned.is_some());
                let cap = h.saturating_sub(reserved).min(MOST);
                // On the pinned row the window rests at the end of the list:
                // that is where the row it is under left off.
                let cursor = if pinned == Some(panel.cursor) {
                    entries.len() - 1
                } else {
                    entries
                        .iter()
                        .position(|row| *row == Row::Listed(panel.cursor))
                        .unwrap_or(0)
                };
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
            if let Some(at) = pinned {
                rows.push(Row::Listed(at));
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
            Line::from_spans(chrome::header_parts(&header_title(view, panel), &[], 0).0).truncate(w)
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
        Row::ModelDesc(i) => {
            let Some(row) = view.models().get(i) else {
                return Line::empty();
            };
            let Some(desc) = row.localized_desc() else {
                return Line::empty();
            };
            Line::styled(
                width::take_width(&format!("    {desc}"), w),
                theme::fg(Role::Muted),
            )
        }
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
                        form.max_tokens_typed
                            .as_deref()
                            .map(crate::providers::parse_output_tokens)
                            .unwrap_or(form.max_tokens),
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
        Row::DiscoveryMessage(text) => {
            Line::styled(format!("  {text}"), theme::fg(Role::Warning)).truncate(w)
        }
        Row::Card(text) => {
            Line::styled(format!("  {text}"), theme::fg(Role::Secondary)).truncate(w)
        }
        Row::Legend => {
            let mut line = Line::styled("  ", theme::fg(Role::Muted));
            for (index, (text, _)) in legend_buttons(view, panel, caps, w).into_iter().enumerate() {
                if index > 0 {
                    line.push(Span::styled(
                        format!(" {} ", caps.g(crate::caps::Glyph::Separator)),
                        theme::fg(Role::Muted),
                    ));
                }
                line.push(Span::styled(text, theme::fg(Role::Muted)));
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

/// The panel's title for the screen that is showing.
fn header_title(view: &ProvidersView, panel: &Panel) -> String {
    match &panel.screen {
        Screen::Models => "model".to_string(),
        Screen::Providers => NAME.to_string(),
        Screen::Detail(account) => {
            let label = view
                .account(account)
                .map(|row| row.label.as_str())
                .unwrap_or(account.as_str());
            format!("{NAME} › {label}")
        }
    }
}

/// What a listed row says: its mark, its label, the facts worth knowing about
/// it (dropped from the end when they do not fit), whether it is drawn dim,
/// and whether the facts are a warning.
struct Parts {
    mark: String,
    label: String,
    about: Vec<String>,
    dim: bool,
    warn: bool,
}

/// One row of the list.
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
    let Some(parts) = listed_parts(view, panel, what, caps) else {
        return Line::empty();
    };
    let here = view.pointed(panel).map(|(row, _)| row) == Some(at);
    let base = if here {
        theme::bg(Role::PanelSelBg).under(theme::fg(Role::PanelFg))
    } else {
        Style::new()
    };
    // Headings sit flush with the labels under them and carry no pointer.
    if matches!(what, Listed::Group(_) | Listed::Section(_)) {
        return Line::styled(
            width::take_width(&format!("  {}", parts.label), w),
            theme::fg(Role::Muted),
        );
    }
    let pointer = if here {
        format!("{} ", caps.g(crate::caps::Glyph::Pointer))
    } else {
        "  ".to_string()
    };
    // The row a second Ctrl+D would delete says so where the row's own
    // description was: a person about to throw something away should read it on
    // the thing being thrown away, not in a corner.
    let armed = panel.pending_delete.as_deref().is_some_and(|armed| {
        Some(armed)
            == match what {
                Listed::Account(i) => view.accounts().get(i).map(|r| r.id.as_str()),
                Listed::Model(i) | Listed::Configured(i) => {
                    view.models().get(i).map(|r| r.id.as_str())
                }
                _ => None,
            }
    });
    // Rows that are actions rather than things — the add rows, a failed
    // listing — span the width: there is no column of facts to line up with.
    let wide = what.pinned() || matches!(what, Listed::Failed | Listed::Loading);
    let label_room = if wide {
        w.saturating_sub(LEAD + 2)
    } else {
        label_column(view, panel, w, caps)
    };
    let shown = width::take_width(&parts.label, label_room);
    let pad = if wide {
        0
    } else {
        label_room.saturating_sub(width::str_width(&shown))
    };
    let label_style = if parts.dim {
        base.under(theme::fg(Role::Muted))
    } else {
        base
    };
    let about_style = if armed || parts.warn {
        base.under(theme::fg(Role::Warning))
    } else {
        base.under(theme::fg(Role::Muted))
    };
    let room = w.saturating_sub(LEAD + 2 + width::str_width(&shown) + pad + 2);
    let about = if armed {
        width::take_width(&t(Msg::ProviderArmedDelete), room)
    } else {
        fit_facts(&parts.about, room)
    };
    let spans = vec![
        Span::styled(pointer, base),
        Span::styled(format!("{} ", parts.mark), label_style),
        Span::styled(shown, label_style),
        Span::styled(" ".repeat(pad + 2), base),
        Span::styled(about, about_style),
    ];
    pad_to(Line::from_spans(spans), w, base)
}

/// As many of `facts` as fit in `room`, joined, dropping from the end: the
/// facts are listed most-important-first, so a narrow terminal loses the
/// reasoning levels before the vision mark, and that before the output budget —
/// the context window, first, stays.
fn fit_facts(facts: &[String], room: usize) -> String {
    for keep in (1..=facts.len()).rev() {
        let text = facts[..keep].join(" · ");
        if width::str_width(&text) <= room {
            return text;
        }
    }
    facts
        .first()
        .map(|first| width::take_width(first, room))
        .unwrap_or_default()
}

/// A model's facts, most important first: window, output, vision, reasoning.
fn model_facts(
    window: Option<usize>,
    output: Option<usize>,
    vision: Option<bool>,
    levels: &[String],
    effort: Option<&str>,
) -> Vec<String> {
    let mut facts = Vec::new();
    if let Some(window) = window.filter(|w| *w > 0) {
        facts.push(crate::providers::window_label(window));
    }
    if let Some(output) = output {
        facts.push(format!(
            "{} {}",
            t(Msg::ProviderOutputLimit),
            crate::content::token_count(output as u32)
        ));
    }
    if vision == Some(true) {
        facts.push(t(Msg::ProviderVision).into_owned());
    }
    if !levels.is_empty() {
        facts.push(format!("{} {}", t(Msg::FieldEffort), levels.join("/")));
    } else if let Some(effort) = effort {
        facts.push(format!("{} {effort}", t(Msg::FieldEffort)));
    }
    facts
}

/// What one listed row says. `None` for a row the view no longer has.
fn listed_parts(
    view: &ProvidersView,
    panel: &Panel,
    what: Listed,
    caps: crate::caps::Caps,
) -> Option<Parts> {
    let plain = |label: String, about: Vec<String>, dim: bool| Parts {
        mark: " ".to_string(),
        label,
        about,
        dim,
        warn: false,
    };
    Some(match what {
        // 一组的小标题:账号的名字——它不是一行可以做点什么的东西,是一条分界。
        Listed::Group(first) => plain(view.models().get(first)?.account.clone(), vec![], true),
        Listed::Section(section) => plain(
            t(match section {
                Section::Added => Msg::ProviderSectionAdded,
                Section::Server => Msg::ProviderSectionServer,
                Section::Catalog => Msg::ProviderCatalogCandidates,
            })
            .into_owned(),
            vec![],
            true,
        ),
        Listed::AddModel => plain(t(Msg::ProviderAddModelRow).into_owned(), vec![], false),
        Listed::AddAccount => plain(t(Msg::ProviderAddProviderRow).into_owned(), vec![], false),
        Listed::Manual => {
            let typed = panel.query.trim();
            let label = if typed.is_empty() || view.exact_server_match(panel) {
                t(Msg::ProviderManualRow).into_owned()
            } else {
                t(Msg::ProviderManualRowWith { query: typed }).into_owned()
            };
            plain(label, vec![], false)
        }
        Listed::Loading => plain(t(Msg::ProviderDiscoveryLoading).into_owned(), vec![], true),
        Listed::Failed => {
            let why = match &panel.screen {
                Screen::Detail(account) => match panel.discovery.get(account).map(|d| &d.state) {
                    Some(DiscoveryState::Failed(why)) => why.clone(),
                    _ => String::new(),
                },
                _ => String::new(),
            };
            Parts {
                mark: " ".to_string(),
                label: why,
                about: vec![],
                dim: false,
                warn: true,
            }
        }
        Listed::Connection => {
            let Screen::Detail(account) = &panel.screen else {
                return None;
            };
            let row = view.account(account)?;
            let about = if row.managed {
                vec![t(Msg::ProviderManagedHint).into_owned()]
            } else {
                let mut about = vec![row.protocol.clone()];
                let host = row.endpoint_host();
                if !host.is_empty() {
                    about.push(host.to_string());
                }
                about.push(
                    t(if row.has_key {
                        Msg::ProviderHasKey
                    } else {
                        Msg::ProviderNoKey
                    })
                    .into_owned(),
                );
                vec![about.join(" · ")]
            };
            plain(t(Msg::ProviderConnectionRow).into_owned(), about, false)
        }
        Listed::Account(i) => {
            let row = view.accounts().get(i)?;
            let host = row.endpoint_host();
            let choices = view.connections(&row.id);
            let protocol = if !row.configured && choices.len() > 1 {
                choices
                    .iter()
                    .map(|choice| choice.protocol.label.as_str())
                    .collect::<Vec<_>>()
                    .join(" / ")
            } else {
                row.protocol.clone()
            };
            let mut about = Vec::new();
            if row.configured {
                if row.managed {
                    about.push(t(Msg::ProviderManaged).into_owned());
                } else {
                    about.push(
                        t(if row.has_key {
                            Msg::ProviderHasKey
                        } else {
                            Msg::ProviderNoKey
                        })
                        .into_owned(),
                    );
                }
                about.push(t(Msg::ProviderModelCount { n: row.models }).into_owned());
            }
            about.push(protocol);
            if !host.is_empty() {
                about.push(host.to_string());
            }
            let on = view
                .models()
                .iter()
                .any(|m| m.current && m.account == row.id);
            Parts {
                mark: mark_for(on, caps),
                label: row.label.clone(),
                about,
                dim: !row.configured,
                warn: false,
            }
        }
        Listed::Effort(i) => {
            let pick = panel.efforts.as_ref()?;
            let level = pick.levels.get(i)?;
            // 收尾那一行是把强度交回端点,它要说的正是「不指定」。
            let about = if i + 1 == pick.levels.len() {
                vec![t(Msg::EffortDefaultAbout).into_owned()]
            } else {
                vec![]
            };
            plain(level.clone(), about, false)
        }
        Listed::Model(i) | Listed::Configured(i) => {
            let row = view.models().get(i)?;
            let mut about = model_facts(
                Some(row.window),
                row.max_tokens,
                row.vision,
                &[],
                row.effort.as_deref(),
            );
            if row.managed {
                about.insert(0, t(Msg::ProviderManaged).into_owned());
            }
            Parts {
                mark: mark_for(row.current, caps),
                // The gateway's display alias wins when it has one, in the
                // interface language: the name a person picks by is the one
                // the server labels the model with, not the wire id.
                label: row
                    .localized_alias()
                    .map(str::to_string)
                    .unwrap_or_else(|| row.id.clone()),
                about,
                dim: false,
                warn: false,
            }
        }
        Listed::Available(j) => {
            let item = view.available(panel, j)?;
            let Screen::Detail(account) = &panel.screen else {
                return None;
            };
            let key = (account.clone(), item.id.clone());
            let (about, warn) = if panel.adding.contains(&key) {
                (vec![t(Msg::ProviderAdding).into_owned()], false)
            } else if let Some(why) = panel.add_errors.get(&key) {
                (vec![why.clone()], true)
            } else {
                let facts = model_facts(
                    item.window,
                    item.max_tokens,
                    item.vision,
                    item.levels.as_deref().unwrap_or_default(),
                    item.effort.as_deref(),
                );
                match facts.is_empty() {
                    true => (vec![t(Msg::ProviderNotProvided).into_owned()], false),
                    false => (facts, false),
                }
            };
            Parts {
                mark: " ".to_string(),
                label: item.id.clone(),
                about,
                dim: false,
                warn,
            }
        }
    })
}

/// How much room the descriptions keep at the right of a list row, at least.
const ABOUT_MIN: usize = 20;

/// How wide the label column of the list is drawn: as wide as the widest label
/// listed, so every row lines up and none is cut — as long as the descriptions
/// still keep [`ABOUT_MIN`] beside it.
///
/// Not the form's [`LABEL_MAX`] cap: a form's labels are short field names,
/// while a list's are model ids, and an OpenRouter id
/// (`openrouter/thinkingmachines/…`) is past thirty columns before it says
/// which model it is.
fn label_column(view: &ProvidersView, panel: &Panel, w: usize, caps: crate::caps::Caps) -> usize {
    let widest = view
        .listed(panel)
        .into_iter()
        .filter(|what| {
            !what.pinned()
                && !matches!(
                    what,
                    Listed::Group(_) | Listed::Section(_) | Listed::Failed | Listed::Loading
                )
        })
        .filter_map(|what| listed_parts(view, panel, what, caps))
        .map(|parts| width::str_width(&parts.label))
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
    let label_width = match form {
        Form::Account(f) => f
            .fields(view)
            .iter()
            .map(|field| width::str_width(&account_field(view, f, *field, false, caps).0))
            .max(),
        Form::Model(f) => f
            .fields(view)
            .iter()
            .map(|field| width::str_width(&model_field(view, f, *field, false, caps).0))
            .max(),
    }
    .unwrap_or(LABEL_MIN)
    .clamp(LABEL_MIN, LABEL_MAX);
    if focused && typing {
        return edit_line_with_label_width(&label, &value, caret, w, caps, label_width);
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
    let label_room = label_width.min(w.saturating_sub(LEAD + 2 + 8));
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
        AccountField::Key => {
            let value = dots(form.key_len, form.editing.is_some(), caps);
            let caret = if form.key_len == 0 { 0 } else { value.len() };
            (t(Msg::FieldKey).into_owned(), value, focused, true, caret)
        }
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
        ModelField::Key => {
            let value = dots(form.key_len, false, caps);
            let caret = value.len();
            (t(Msg::FieldKey).into_owned(), value, focused, true, caret)
        }
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
        ModelField::Output => match &form.max_tokens_typed {
            // The custom stop is a text field while it has the keyboard, and
            // says what it holds (or that it is the custom one) when it has not.
            Some(typed) if focused => (
                t(Msg::FieldOutputTokens).into_owned(),
                typed.clone(),
                focused,
                true,
                form.caret,
            ),
            Some(typed) => (
                t(Msg::FieldOutputTokens).into_owned(),
                match typed.trim().is_empty() {
                    true => t(Msg::WindowCustom).into_owned(),
                    false => typed.clone(),
                },
                focused,
                false,
                0,
            ),
            None => (
                t(Msg::FieldOutputTokens).into_owned(),
                cycled(
                    &match form.max_tokens {
                        Some(tokens) => tokens.to_string(),
                        None => t(Msg::OutputAutomatic).into_owned(),
                    },
                    focused,
                ),
                focused,
                false,
                0,
            ),
        },
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

/// One action in the footer: its key, what it does, and how readily it gives
/// way on a narrow terminal — 0 never does (the row's main action and Esc),
/// and the highest number goes first.
struct Hint {
    key: String,
    label: String,
    yield_rank: u8,
}

fn hint(key: &str, msg: Msg<'_>, yield_rank: u8) -> Hint {
    Hint {
        key: key.to_string(),
        label: t(msg).into_owned(),
        yield_rank,
    }
}

/// What the footer says for what has the keyboard: only the actions this row
/// takes, so a key that is listed is a key that does something.
fn legend(view: &ProvidersView, panel: &Panel) -> Vec<Hint> {
    match &panel.form {
        Some(Form::Account(form)) if form.is_ollama(view) => vec![
            hint("⏎", Msg::ProviderContinue, 0),
            // The only way to ask again on this form: it stays on a narrow
            // terminal with the main action.
            hint("Ctrl+F", Msg::ProviderDiscoverHint, 0),
            hint("Ctrl+O", Msg::ProviderAdvanced, 2),
            hint("esc", Msg::LegendCancel, 0),
        ],
        Some(Form::Account(form)) if form.preset.is_some() => {
            let mut keys = vec![
                hint("⏎", Msg::ProviderContinue, 0),
                hint("Ctrl+O", Msg::ProviderAdvanced, 2),
                hint("esc", Msg::LegendCancel, 0),
            ];
            if form.fields(view).contains(&AccountField::Protocol) {
                keys.insert(1, hint("←→", Msg::LegendChangeValue, 1));
            }
            keys
        }
        // On the levels row the keys do something the other fields' legend
        // does not say: Space turns the bracketed level on or off, and the
        // arrows move between levels rather than change a value.
        Some(Form::Model(form)) if form.focus == ModelField::Levels => vec![
            hint("space", Msg::LegendToggleLevel, 1),
            hint("←→", Msg::LegendPickLevel, 2),
            hint("⇥", Msg::LegendNextField, 3),
            hint("⏎", Msg::LegendSave, 0),
            hint("esc", Msg::LegendCancel, 0),
        ],
        Some(Form::Model(form)) if form.focus == ModelField::Output => {
            let invalid = form
                .max_tokens_typed
                .as_deref()
                .is_some_and(|typed| crate::providers::parse_output_tokens(typed).is_none());
            vec![
                Hint {
                    key: String::new(),
                    label: t(if invalid {
                        Msg::OutputInvalid
                    } else {
                        Msg::LegendTypeOutput
                    })
                    .into_owned(),
                    yield_rank: 1,
                },
                hint("←→", Msg::LegendChangeValue, 2),
                hint("⇥", Msg::LegendNextField, 3),
                hint("⏎", Msg::LegendSave, 0),
                hint("esc", Msg::LegendCancel, 0),
            ]
        }
        // The window row: typing a number is a custom window, and the legend
        // has to say so — on a preset nothing else would tell a person that the
        // digit keys do anything here.
        Some(Form::Model(form)) if form.focus == ModelField::Window => match form.window_typed {
            Some(_) => vec![
                Hint {
                    key: String::new(),
                    label: t(Msg::LegendTypeWindow).into_owned(),
                    yield_rank: 1,
                },
                hint("←→", Msg::LegendBackToPresets, 2),
                hint("⏎", Msg::LegendSave, 0),
                hint("esc", Msg::LegendCancel, 0),
            ],
            None => vec![
                hint("←→", Msg::LegendChangeValue, 1),
                hint("0-9", Msg::WindowCustom, 2),
                hint("⇥", Msg::LegendNextField, 3),
                hint("⏎", Msg::LegendSave, 0),
                hint("esc", Msg::LegendCancel, 0),
            ],
        },
        Some(_) => vec![
            hint("⇥", Msg::LegendNextField, 2),
            hint("←→", Msg::LegendChangeValue, 1),
            hint("⏎", Msg::LegendSave, 0),
            hint("esc", Msg::LegendCancel, 0),
        ],
        None => list_legend(view, panel),
    }
}

fn list_legend(view: &ProvidersView, panel: &Panel) -> Vec<Hint> {
    // 挑强度那一层:一句话说清这一层是干什么的,按键只有两个要记。
    if let Some(pick) = &panel.efforts {
        return vec![
            Hint {
                key: String::new(),
                label: t(Msg::EffortPickAfterModel { model: &pick.model }).into_owned(),
                yield_rank: 1,
            },
            hint("⏎", Msg::LegendThisOne, 0),
            hint("esc", Msg::LegendBack, 0),
        ];
    }
    if panel.pending_delete.is_some() {
        // Says what the next press does, because that is the only thing about
        // this state a person has to know — and it is the press that throws
        // something away.
        return vec![
            hint("Ctrl+D", Msg::LegendPressAgainToDelete, 0),
            Hint {
                key: t(Msg::LegendAnyOtherKey).into_owned(),
                label: t(Msg::LegendCancel).into_owned(),
                yield_rank: 0,
            },
        ];
    }
    let esc = match (&panel.screen, panel.from_models) {
        (Screen::Models, _) | (Screen::Providers, false) => hint("esc", Msg::LegendClose, 0),
        _ => hint("esc", Msg::LegendBack, 0),
    };
    let mut out = match view.pointed(panel).map(|(_, row)| row) {
        Some(Listed::Model(i) | Listed::Configured(i)) => {
            let managed = view.models().get(i).is_some_and(|row| row.managed);
            let mut out = vec![hint("⏎", Msg::LegendSwitch, 0)];
            if !managed {
                out.push(hint("Ctrl+E", Msg::LegendEditIt, 1));
                out.push(hint("Ctrl+D", Msg::LegendRemove, 2));
            }
            out.push(hint("F2", Msg::ModelCardTitle, 3));
            out
        }
        Some(Listed::Account(i)) => {
            let row = view.accounts().get(i);
            let mut out = vec![hint("⏎", Msg::LegendOpen, 0)];
            if row.is_some_and(|row| row.configured && !row.managed) {
                out.push(hint("Ctrl+E", Msg::LegendEditIt, 1));
                out.push(hint("Ctrl+D", Msg::LegendDelete, 2));
            }
            out
        }
        Some(Listed::AddModel) => vec![hint("⏎", Msg::LegendOpen, 0)],
        Some(Listed::AddAccount) => vec![hint("⏎", Msg::LegendAddIt, 0)],
        Some(Listed::Connection) => {
            let managed = match &panel.screen {
                Screen::Detail(account) => view.account(account).is_some_and(|row| row.managed),
                _ => true,
            };
            match managed {
                true => vec![],
                false => vec![hint("⏎", Msg::LegendEditIt, 0)],
            }
        }
        Some(Listed::Available(j)) => {
            let busy = match (&panel.screen, view.available(panel, j)) {
                (Screen::Detail(account), Some(item)) => {
                    panel.adding.contains(&(account.clone(), item.id.clone()))
                }
                _ => false,
            };
            match busy {
                true => vec![],
                false => vec![
                    hint("⏎", Msg::LegendAddIt, 0),
                    hint("Ctrl+E", Msg::LegendEditThenAdd, 1),
                ],
            }
        }
        Some(Listed::Failed) => vec![hint("⏎", Msg::LegendRetry, 0)],
        Some(Listed::Manual) => vec![hint("⏎", Msg::ProviderManualAdd, 0)],
        _ => vec![],
    };
    out.push(esc);
    out
}

/// The footer's hints that fit `w`, each with the key a click on it presses.
///
/// Drawn and hit-tested from this one list, so a click lands on the hint it
/// was drawn as. When they do not all fit, the most yielding goes first —
/// never the row's main action or Esc.
fn legend_buttons(
    view: &ProvidersView,
    panel: &Panel,
    caps: crate::caps::Caps,
    w: usize,
) -> Vec<(String, Option<crate::surface::KeyPress>)> {
    use crate::surface::{Key, KeyPress};
    let mut hints = legend(view, panel);
    let text_of = |hint: &Hint| match hint.key.is_empty() {
        true => hint.label.clone(),
        false => crate::widget::keys(&[(hint.key.as_str(), hint.label.as_str())], caps),
    };
    let gap = width::str_width(&format!(" {} ", caps.g(crate::caps::Glyph::Separator)));
    let room = w.saturating_sub(2);
    loop {
        let total: usize = hints
            .iter()
            .map(|hint| width::str_width(&text_of(hint)))
            .sum::<usize>()
            + gap * hints.len().saturating_sub(1);
        if total <= room {
            break;
        }
        // The most yielding hint, the later one on a tie.
        let Some(at) = hints
            .iter()
            .enumerate()
            .filter(|(_, hint)| hint.yield_rank > 0)
            .max_by_key(|(at, hint)| (hint.yield_rank, *at))
            .map(|(at, _)| at)
        else {
            break;
        };
        hints.remove(at);
    }
    hints
        .iter()
        .map(|hint| {
            let press = match hint.key.as_str() {
                "⏎" => Some(KeyPress::plain(Key::Enter)),
                "esc" => Some(KeyPress::plain(Key::Esc)),
                "space" => Some(KeyPress::plain(Key::Char(' '))),
                "F2" => Some(KeyPress::plain(Key::F(2))),
                "Ctrl+E" => Some(KeyPress::ctrl('e')),
                "Ctrl+D" => Some(KeyPress::ctrl('d')),
                "Ctrl+O" => Some(KeyPress::ctrl('o')),
                "Ctrl+F" => Some(KeyPress::ctrl('f')),
                _ => None,
            };
            (text_of(hint), press)
        })
        .collect()
}

/// Only visible cells of a drawn hint can activate its key.
pub fn action_at(
    moment: &Moment,
    vp: &Viewport<'_>,
    col: usize,
    row: usize,
) -> Option<crate::surface::KeyPress> {
    let panel = moment.providers_panel.as_ref()?;
    let w = vp.rect.w as usize;
    let rows = layout(&moment.providers, panel, vp.rect.h as usize, w);
    if !matches!(rows.get(row), Some(Row::Legend)) {
        return None;
    }
    let mut start = 2;
    let gap = width::str_width(&format!(
        " {} ",
        moment.caps.g(crate::caps::Glyph::Separator)
    ));
    for (text, key) in legend_buttons(&moment.providers, panel, moment.caps, w) {
        let end = start + width::str_width(&text);
        if col >= start && col < end && end <= w {
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
}

impl Geometry {
    /// Which listed row is on this screen row, if one is.
    pub fn listed_at(&self, row: usize) -> Option<usize> {
        self.rows.get(row).copied().flatten()
    }
}

pub fn geometry(moment: &Moment, vp: &Viewport<'_>) -> Geometry {
    let Some(panel) = moment.providers_panel.as_ref() else {
        return Geometry { rows: Vec::new() };
    };
    let rows = layout(
        &moment.providers,
        panel,
        vp.rect.h as usize,
        vp.rect.w as usize,
    );
    // 小标题不接鼠标:点它既不该选中它(停不住),也不该顺手把它底下那个模型
    // 换上——点一条分界线不是在挑东西。
    let listed = moment.providers.listed(panel);
    Geometry {
        rows: rows
            .into_iter()
            .map(|row| match row {
                Row::Listed(at) if listed.get(at).is_some_and(|row| row.selectable()) => Some(at),
                _ => None,
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn model_card_height_is_stable_and_has_no_model_switch_hit_rows() {
        let row = crate::providers::ModelRow {
            id: "card".into(),
            account: "account".into(),
            model: "glm-5.3-flash".into(),
            window: 12345,
            max_tokens: None,
            vision: Some(true),
            effort: None,
            levels: vec![],
            current: true,
            managed: false,
            alias: None,
            desc: None,
            alias_en: None,
            desc_en: None,
        };

        let view = crate::providers::ProvidersView::new(vec![], vec![row.clone()], vec![], vec![]);
        let mut panel = crate::providers::Panel {
            card: Some(crate::model_cards::CardPanel::new(&row)),
            ..Default::default()
        };
        for width in [24, 80, 140] {
            let height = super::layout(&view, &panel, usize::MAX, width).len();
            let rows = super::layout(&view, &panel, height, width);
            assert_eq!(rows.len(), height);
            assert!(!rows.iter().any(|row| matches!(row, super::Row::Listed(_))));
            for scroll in [1, 3, 16, usize::MAX] {
                panel.card.as_mut().unwrap().scroll = scroll;
                assert_eq!(
                    super::layout(&view, &panel, usize::MAX, width).len(),
                    height
                );
                assert_eq!(super::layout(&view, &panel, height, width).len(), height);
            }
        }
    }

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
            max_tokens: None,
            vision: None,
            effort: None,
            levels: Vec::new(),
            current,
            managed: false,
            alias: None,
            desc: None,
            alias_en: None,
            desc_en: None,
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
    fn ollama_startup_guidance_and_retry_are_visible_in_a_narrow_panel() {
        let view = ProvidersView::new(
            vec![],
            vec![],
            vec![Protocol {
                id: "ollama".into(),
                label: "Ollama".into(),
                endpoint: Some("http://localhost:11434".into()),
                needs_key: false,
            }],
            vec![],
        );
        let mut form = AccountForm::add(&view);
        form.discovery = Some(crate::providers::AccountDiscovery {
            id: 1,
            result: Some(Err("Ollama: run ollama serve on the machine running atomcode, then retry. Default port: 11434.".into())),
        });
        let mut m = moment(Some(Panel {
            form: Some(Form::Account(form)),
            ..Panel::new()
        }));
        m.providers = view;
        let rows = layout(&m.providers, m.providers_panel.as_ref().unwrap(), 40, 40);
        let text = rows
            .iter()
            .filter_map(|row| match row {
                Row::DiscoveryMessage(text) => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>();
        assert!(text.contains("ollama serve"));
        assert!(text.contains("11434"));
        assert!(drawn(&m, 40, 40).contains("Ctrl+F"));
        // Editing an address clears its diagnostic, but must keep retry discoverable.
        let mut edited = m.clone();
        let Some(Form::Account(form)) = edited.providers_panel.as_mut().unwrap().form.as_mut()
        else {
            panic!("account");
        };
        form.discovery = None;
        assert!(drawn(&edited, 40, 40).contains("Ctrl+F"));
        assert!(
            rows.iter()
                .filter(|row| matches!(row, Row::DiscoveryMessage(_)))
                .count()
                > 1
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
                    screen: crate::providers::Screen::Models,
                    query: "loc".into(),
                    ..Panel::new()
                },
                Panel {
                    form: Some(Form::Account(AccountForm::add(&view()))),
                    ..Panel::new()
                },
                Panel {
                    screen: crate::providers::Screen::Models,
                    form: ModelForm::add(&view(), "deepseek").map(Form::Model),
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
                screen: crate::providers::Screen::Models,
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
                let (before, _) = l.split_once("128K").expect("described");
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
            narrow.contains("128K"),
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
        assert!(out.contains("＋ 添加提供商"), "the add row is last: {out}");
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

    #[test]
    fn the_manual_model_caret_stays_near_its_label_on_wide_terminals() {
        let v = view();
        let mut form = ModelForm::add(&v, "deepseek").unwrap();
        form.advanced = false;
        form.focus = ModelField::Model;
        let at = form
            .fields(&v)
            .iter()
            .position(|f| *f == ModelField::Model)
            .unwrap();
        let panel = Panel {
            form: Some(Form::Model(form)),
            ..Panel::new()
        };
        for w in [40, 80, 280] {
            let line = field_line(&v, &panel, at, w, crate::caps::Caps::default());
            let caret_column: usize = line
                .spans
                .iter()
                .take_while(|span| !span.style.reverse)
                .map(|span| span.width())
                .sum();
            assert!(line.spans.iter().any(|span| span.style.reverse));
            assert!(
                caret_column <= 12,
                "caret too far from model label: {line:?}"
            );
        }
    }

    #[test]
    fn input_carets_are_visible_on_focused_account_and_model_keys() {
        let original = view();
        let mut accounts = original.accounts().to_vec();
        accounts[0].has_key = false;
        let v = ProvidersView::new(
            accounts,
            original.models().to_vec(),
            original.protocols().to_vec(),
            original.efforts().to_vec(),
        );
        for len in [0, 5, 128] {
            let mut account = AccountForm::connect(&v, v.account("deepseek").unwrap());
            account.focus = AccountField::Key;
            account.key_len = len;
            let mut model = ModelForm::add(&v, &v.open_accounts()[0]).unwrap();
            model.focus = ModelField::Key;
            model.key_len = len;
            for form in [Form::Account(account), Form::Model(model)] {
                let field = match &form {
                    Form::Account(f) => f
                        .fields(&v)
                        .iter()
                        .position(|field| *field == AccountField::Key),
                    Form::Model(f) => f
                        .fields(&v)
                        .iter()
                        .position(|field| *field == ModelField::Key),
                }
                .expect("key field is offered");
                let panel = Panel {
                    form: Some(form),
                    ..Panel::new()
                };
                for w in [8, 20, 40, 80] {
                    let line = field_line(&v, &panel, field, w, crate::caps::Caps::default());
                    assert!(line.width() <= w);
                    assert!(
                        line.spans.iter().any(|s| s.style.reverse && s.width() > 0),
                        "{line:?}"
                    );
                }
            }
        }
        let mut form = AccountForm::edit(&v, v.account("deepseek").unwrap());
        form.focus = AccountField::Key;
        let field = form
            .fields(&v)
            .iter()
            .position(|f| *f == AccountField::Key)
            .unwrap();
        let mut panel = Panel {
            form: Some(Form::Account(form)),
            ..Panel::new()
        };
        let focused = field_line(&v, &panel, field, 80, crate::caps::Caps::default());
        assert!(focused.plain().contains(&*t(Msg::KeyLeaveBlankToKeep)));
        assert!(focused.spans.iter().any(|s| s.style.reverse));
        if let Some(Form::Account(form)) = panel.form.as_mut() {
            form.focus = AccountField::Name;
        }
        let unfocused = field_line(&v, &panel, field, 80, crate::caps::Caps::default());
        assert!(!unfocused.spans.iter().any(|s| s.style.reverse));
    }

    /// On the levels row the legend says what Space does there — turn the
    /// bracketed level on or off — which the other fields' legend does not.
    /// Off that row it is the ordinary legend again.
    #[test]
    fn the_levels_row_says_space_turns_a_level_on_or_off() {
        let mut form = ModelForm::add(&view(), "deepseek").expect("an account to add to");
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
            out.contains(&format!("‹ {} ›", t(Msg::WindowAutomatic))),
            "the window is a choice, automatic to begin with: {out}"
        );
    }

    /// The window row says that typing a number makes a custom window, and
    /// the custom field says what it takes and how to leave it.
    #[test]
    fn the_output_row_shows_exact_budget_and_validation_hint() {
        let mut form = ModelForm::add(&view(), "deepseek").unwrap();
        form.advanced = true;
        form.focus = ModelField::Output;
        form.max_tokens = Some(4096);
        let render = |form: &ModelForm, width| {
            drawn(
                &moment(Some(Panel {
                    form: Some(Form::Model(form.clone())),
                    ..Panel::new()
                })),
                width,
                30,
            )
        };
        for width in [80, 120] {
            let out = render(&form, width);
            assert!(
                out.contains(&t(Msg::FieldOutputTokens).to_string()),
                "{out}"
            );
            assert!(out.contains("4096"), "{out}");
        }
        form.max_tokens_typed = Some("-4096".into());
        form.caret = 5;
        let out = render(&form, 160);
        assert!(out.contains("-4096"), "{out}");
        assert!(out.contains("1–4294967295"), "{out}");
    }

    #[test]
    fn the_window_row_says_how_to_type_a_custom_one() {
        let mut form = ModelForm::add(&view(), "deepseek").expect("an account to add to");
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
    fn a_connection_shows_protocol_and_address_and_offers_clickable_advanced_settings() {
        use crate::surface::KeyPress;
        let v = view();
        let form = AccountForm::connect(&v, v.account("deepseek").unwrap());
        let m = moment(Some(Panel {
            form: Some(Form::Account(form)),
            ..Panel::new()
        }));
        let text = drawn(&m, 120, 24);
        assert!(text.contains(&t(Msg::FieldKey).to_string()), "{text}");
        assert!(text.contains("https://"), "{text}");
        assert!(text.contains("OpenAI"), "{text}");
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
        assert_eq!(out.matches("再按一次 Ctrl+D 删除").count(), 1, "{out}");
    }

    /// A click is answered from the layout that was drawn, not from a formula
    /// beside it: move the panel's chrome and this moves with it.
    #[test]
    fn a_click_finds_the_row_that_was_drawn() {
        let m = moment(Some(Panel::new()));
        let vp = Viewport::new(Rect::sized(80, 24), &m);
        let geom = geometry(&m, &vp);
        let rows = layout(&m.providers, m.providers_panel.as_ref().unwrap(), 24, 80);
        let first = rows
            .iter()
            .position(|row| matches!(row, Row::Listed(0)))
            .expect("the first account is drawn");
        assert_eq!(geom.listed_at(first), Some(0));
        assert_eq!(geom.listed_at(first + 1), Some(1));
        assert_eq!(geom.listed_at(0), None, "the rule is not a row");
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
        let rows = layout(&m.providers, &panel, 14, 80);
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

    /// `deepseek`'s page with `n` server models listed and the cursor on `at`.
    fn page(n: usize, at: Option<usize>) -> Moment {
        let mut panel = Panel {
            screen: crate::providers::Screen::Detail("deepseek".into()),
            ..Panel::new()
        };
        panel.start_discovery("deepseek");
        let request = panel.discovery_request("deepseek").unwrap();
        let items = (0..n)
            .map(|i| crate::providers::Discovered {
                id: format!("server-model-{i}"),
                window: Some(128_000),
                max_tokens: Some(8192),
                vision: Some(true),
                levels: Some(vec!["low".into(), "high".into()]),
                ..Default::default()
            })
            .collect();
        panel.discovered(&view(), "deepseek", request, Ok(items));
        if let Some(at) = at {
            panel.cursor = at;
        }
        moment(Some(panel))
    }

    #[test]
    fn the_add_row_stays_in_sight_under_a_long_list() {
        let m = page(60, Some(2));
        let out = drawn(&m, 80, 24);
        assert!(
            out.contains("＋ 手动添加模型…"),
            "pinned under the scroll:\n{out}"
        );
        assert!(out.contains("↓"), "the rest is out of sight below:\n{out}");
        // On the pinned row the window rests at the end of the list.
        let last = m
            .providers
            .listed(m.providers_panel.as_ref().unwrap())
            .len()
            - 1;
        let end = page(60, Some(last));
        let out = drawn(&end, 80, 24);
        assert!(out.contains("server-model-59"), "{out}");
        let pinned = out
            .lines()
            .find(|l| l.contains("＋ 手动添加模型…"))
            .unwrap();
        assert!(
            pinned.trim_start().starts_with('▸'),
            "the cursor is on it: {out}"
        );
    }

    #[test]
    fn the_footer_says_what_enter_does_on_this_row() {
        // On an added model: Enter switches.
        let out = drawn(&page(2, Some(2)), 100, 24);
        assert!(
            out.contains("⏎ 切换") || out.contains("enter 切换"),
            "{out}"
        );
        assert!(
            out.contains("Ctrl+E 编辑") && out.contains("Ctrl+D 移除"),
            "{out}"
        );
        // On a server model: Enter adds, and Ctrl+E edits it first.
        let out = drawn(&page(2, Some(4)), 100, 24);
        assert!(
            out.contains("添加") && out.contains("Ctrl+E 编辑后添加"),
            "{out}"
        );
        assert!(!out.contains("切换"), "{out}");
        assert!(!out.contains("^"), "keys are spelled Ctrl+X: {out}");
        // No row button anywhere: each action is drawn once, in the footer.
        assert!(!out.contains("[ 编辑 ]") && !out.contains("[ "), "{out}");
    }

    #[test]
    fn a_narrow_footer_keeps_enter_and_esc_and_gives_up_f2_first() {
        let m = Moment {
            providers: view(),
            providers_panel: Some(Panel::models()),
            ..Moment::default()
        };
        let footer = |w: u16| {
            drawn(&m, w, 24)
                .lines()
                .last()
                .unwrap_or_default()
                .to_string()
        };
        let wide = footer(120);
        for word in ["切换", "Ctrl+E", "Ctrl+D", "F2", "esc"] {
            assert!(wide.contains(word), "{word}: {wide}");
        }
        let mid = footer(50);
        assert!(!mid.contains("F2"), "F2 goes first: {mid}");
        assert!(mid.contains("切换") && mid.contains("esc"), "{mid}");
        let narrow = footer(24);
        assert!(
            narrow.contains("切换") && narrow.contains("esc"),
            "{narrow}"
        );
        assert!(!narrow.contains("Ctrl+D"), "{narrow}");
    }

    #[test]
    fn a_server_row_says_what_it_can_do_and_drops_detail_when_narrow() {
        let wide = drawn(&page(1, Some(4)), 120, 24);
        let row = wide.lines().find(|l| l.contains("server-model-0")).unwrap();
        for fact in ["128K", "最大输出", "视觉", "low/high"] {
            assert!(row.contains(fact), "{fact}: {row}");
        }
        let narrow = drawn(&page(1, Some(4)), 48, 24);
        let row = narrow
            .lines()
            .find(|l| l.contains("server-model-0"))
            .unwrap();
        assert!(row.contains("128K"), "the window stays: {row}");
        assert!(!row.contains("low/high"), "reasoning goes first: {row}");
    }

    #[test]
    fn a_row_being_added_or_that_failed_says_so() {
        let mut m = page(2, Some(4));
        let panel = m.providers_panel.as_mut().unwrap();
        panel
            .adding
            .insert(("deepseek".into(), "server-model-0".into()));
        panel.add_errors.insert(
            ("deepseek".into(), "server-model-1".into()),
            "HTTP 500".into(),
        );
        let out = drawn(&m, 100, 24);
        let row = |id: &str| out.lines().find(|l| l.contains(id)).unwrap().to_string();
        assert!(row("server-model-0").contains("添加中…"), "{out}");
        assert!(row("server-model-1").contains("HTTP 500"), "{out}");
        assert!(
            !out.lines().last().unwrap().contains("添加"),
            "a row on its way offers no add: {out}"
        );
    }
}
