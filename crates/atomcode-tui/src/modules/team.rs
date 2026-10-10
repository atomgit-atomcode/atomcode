//! The team: who the lead delegated to, what they are doing, what they said.
//!
//! # Why this module has two sources
//!
//! Everything else on this screen is a fold over one log. A team cannot be:
//! half of what a person wants to know is in *this* conversation and half is
//! not.
//!
//! * **From the log** — who was delegated to, with which role, and every word
//!   a member has said *to the lead*. These are facts here: the `team` call,
//!   its result, and each report arriving as an [`InjectionOrigin::Peer`]
//!   message. They replay, so a resumed session draws the same panel.
//! * **From [`Moment`]** — whether a member is working right now and which
//!   turn it is on. No fact in this log is committed when a member starts a
//!   turn, and there must not be: a member's conversation is its own
//!   (`docs/adr/0016`). The host reads it from the agent registry each frame
//!   and hands it over the same way it hands over the clock.
//!
//! What this panel deliberately does **not** show is the member's own stream.
//! That was the bug this panel was built after: every member fact folded into
//! the lead's transcript, two conversations interleaved by turn coordinate.
//! The answer is not a filtered copy of someone else's screen — it is a
//! summary of what the lead knows, which is what a lead has.
//!
//! Nor does it ordinarily show a member that has stopped. A stopped member is
//! not on the team any more — it does not come back or take room in the next
//! delegation. The exception is one already on screen through `/agents`: its
//! row and the lead remain until the person returns, because removing the
//! navigation while it is being used would strand that screen. `/agents`
//! lists every member the registry has announced, stopped ones included
//! (`docs/adr/0023` §5).
//!
//! # A way in
//!
//! It is also where the person switches the screen to one of them and back
//! (`docs/adr/0023` §3): the lead is its first row, `主`, and each member still
//! running follows. Which rows can be switched to is [`targets`] — the lead and
//! the members that are not gone — and the panel lights the row
//! [`Moment::team_cursor`] points at and marks the one on screen. Both are the
//! moment's, so the row under the pointer and the row a press takes are one row.
//! [`targets`] and [`rows`] filter on the same predicate for that reason: the
//! *n*th drawn row and the *n*th target must be the same agent, or a click would
//! take the one below it.
//!
//! The **keyboard** is a separate question from where the pointer is
//! ([`Moment::team_keyboard`]), and the panel is deliberate about which of the
//! two each thing follows. The lit row follows the pointer, because pointing at
//! a row is what a pointer does by being there. The legend — the line naming
//! `↑↓` / `Enter` / `Esc` — follows the keyboard, because there are no keys to
//! name until `Tab` hands them over. A mouse crossing a panel that is always on
//! screen must not become a keyboard grab: the composer keeps what is being
//! typed, and a person who never presses `Tab` never loses a keystroke.

use crate::i18n::product::{t as pt, Msg as PMsg};
use crate::i18n::{t, Msg};
use std::collections::HashMap;

use atomcode_harness::session::{InjectionOrigin, SessionEvent};

use crate::frame::Line;
use crate::module::{Height, View};
use crate::moment::{Activity, Moment, Viewport};
use crate::theme::{self, Role};
use crate::width;

pub const ID: &str = "team";

/// One member, as the lead's own log describes it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Member {
    name: String,
    role: String,
    /// The last thing it told the lead, without the `[name]` the team prefixes.
    last: String,
    /// Stopped by the lead. Kept on screen rather than removed: a member that
    /// did work and was stopped is part of what happened this session.
    stopped: bool,
}

#[derive(Default)]
pub struct State {
    /// In delegation order, which is the order the person met them in.
    members: Vec<Member>,
    /// Delegations and stops whose tool result has not come back yet, by call
    /// id. A call that fails must not leave a member on the panel — the team
    /// refuses duplicate names and a full team, and both come back as errors.
    pending: HashMap<String, Pending>,
}

#[derive(Clone, Debug)]
enum Pending {
    Delegate { name: String, role: String },
    Stop { name: Option<String> },
}

/// How wide a name or a role column may get before it is cut.
const NAME_CAP: usize = 14;
/// A subagent is listed by what it was asked to do, which runs longer than a
/// name a lead picks.
const LABEL_CAP: usize = 40;
const ROLE_CAP: usize = 12;

pub struct Team;

/// Whether any member is still on the team — the panel's own condition for
/// being on screen at all.
///
/// A stopped member is not one: it left the panel ([`rows`]), so a team whose
/// every member has stopped is not a team to draw. The panel is up while there
/// is something running under this agent, and gone when there is not.
///
/// Here rather than folded into [`targets`] alone because the keyboard and the
/// pointer ask the same question through `targets`: an empty answer is what
/// keeps `Tab` from giving the keyboard to a panel nobody drew.
fn running(moment: &Moment) -> bool {
    moment.members.iter().any(|m| !m.gone)
        || !background(moment).is_empty()
        || moment.team_return.is_some()
        || (!moment.lead.is_empty() && !moment.viewing.is_empty() && moment.viewing != moment.lead)
}

/// The row called `main`. Ordinarily it is this runtime's lead; while looking
/// at a background session opened from the strip it is the conversation that
/// opened it, and choosing it performs another `/resume` rather than an
/// in-runtime member switch.
fn main_target(moment: &Moment) -> &str {
    moment
        .team_return
        .as_ref()
        .map(|back| back.session.as_str())
        .unwrap_or(&moment.lead)
}

/// Background sessions this conversation started and is still waiting on — a
/// `code-review` run in the background, say: working, or stopped on a question.
///
/// They are listed here, beside the members, because to the person they are
/// the same thing — work this agent handed off that is still out — and a line
/// that only said `等待 1 个后台任务完成` gave no way to see which, or how far
/// along. Claude Code lists its background agents in the same strip.
///
/// Done ones are not: their result has come home into this conversation, which
/// is where it is read. The `/bg` panel keeps the full list.
pub fn background(moment: &Moment) -> Vec<&crate::bg::Session> {
    if moment.lead.is_empty() {
        return Vec::new();
    }
    moment
        .bg
        .sessions()
        .iter()
        .filter(|s| {
            s.group != crate::bg::Group::Completed && s.origin.as_deref() == Some(&*moment.lead)
        })
        .collect()
}

/// Whether a target is a background session rather than an agent the screen
/// can switch to: a press on it opens the `/bg` panel at that session.
pub fn is_background(moment: &Moment, session: &str) -> bool {
    background(moment).iter().any(|s| s.id == session)
}

/// The sessions the panel's selectable rows switch to, in the order they are
/// drawn: the lead, then each member still running. Empty when the panel is not
/// on screen — there is nothing to switch between, and nothing for the arrows
/// to point at.
///
/// A stopped member is not one of them unless it is the member currently being
/// read through `/agents`; that one stays beside the lead until the person
/// returns. [`rows`] makes exactly the same exception, and the two must agree
/// or a press would land on the agent below the one it hit.
pub fn targets(moment: &Moment) -> Vec<String> {
    let main = main_target(moment);
    if main.is_empty() || !running(moment) {
        return Vec::new();
    }
    let mut targets: Vec<String> = std::iter::once(main.to_string())
        .chain(
            moment
                .members
                .iter()
                .filter(|m| !m.gone || m.session == moment.viewing)
                .map(|m| m.session.clone()),
        )
        // After the members, in the order [`rows`] draws them.
        .chain(background(moment).into_iter().map(|s| s.id.clone()))
        .collect();
    if !moment.viewing.is_empty()
        && moment.viewing != main
        && !targets.iter().any(|s| s == &moment.viewing)
    {
        targets.push(moment.viewing.clone());
    }
    targets
}

/// Which selectable row a line of the drawn panel is, when it is one: the
/// header is line 0, the lead line 1.
pub fn target_at_line(moment: &Moment, line: usize) -> Option<usize> {
    let index = line.checked_sub(1)?;
    (index < targets(moment).len()).then_some(index)
}

impl Team {
    fn find<'a>(state: &'a mut State, name: &str) -> Option<&'a mut Member> {
        state.members.iter_mut().find(|m| m.name == name)
    }
}

impl View for Team {
    type State = State;

    fn id() -> &'static str {
        ID
    }

    fn absorb(state: &mut State, fact: &SessionEvent) {
        match fact {
            // The call is a promise, not a fact about the team yet.
            SessionEvent::AssistantMessage { tool_calls, .. } => {
                for call in tool_calls.iter().filter(|c| c.name == "team") {
                    let Ok(args) = serde_json::from_str::<serde_json::Value>(&call.arguments)
                    else {
                        continue;
                    };
                    let str_at = |key: &str| {
                        args.get(key)
                            .and_then(|v| v.as_str())
                            .map(str::to_string)
                            .filter(|s| !s.trim().is_empty())
                    };
                    match args.get("action").and_then(|a| a.as_str()).unwrap_or("") {
                        "delegate" => {
                            if let (Some(name), Some(role)) = (str_at("name"), str_at("role")) {
                                state
                                    .pending
                                    .insert(call.id.clone(), Pending::Delegate { name, role });
                            }
                        }
                        "stop" => {
                            state.pending.insert(
                                call.id.clone(),
                                Pending::Stop {
                                    name: str_at("name"),
                                },
                            );
                        }
                        _ => {}
                    }
                }
            }

            // The result is the fact. An error means it did not happen.
            SessionEvent::ToolResultLogged {
                call_id, is_error, ..
            } => {
                let Some(pending) = state.pending.remove(call_id) else {
                    return;
                };
                if *is_error {
                    return;
                }
                match pending {
                    Pending::Delegate { name, role } => {
                        // A name is unique per team, so a second delegation
                        // under a name that was stopped is the same row again.
                        match Self::find(state, &name) {
                            Some(existing) => {
                                existing.role = role;
                                existing.stopped = false;
                                existing.last.clear();
                            }
                            None => state.members.push(Member {
                                name,
                                role,
                                ..Member::default()
                            }),
                        }
                    }
                    Pending::Stop { name: Some(name) } => {
                        if let Some(m) = Self::find(state, &name) {
                            m.stopped = true;
                        }
                    }
                    Pending::Stop { name: None } => {
                        state.members.iter_mut().for_each(|m| m.stopped = true)
                    }
                }
            }

            // A report. The sender is named by session id — `<lead>/<name>` —
            // and the team prefixes the text with the same name, so the panel
            // takes the prefix off rather than saying it twice.
            SessionEvent::Injected {
                text,
                origin: InjectionOrigin::Peer { from, .. },
                ..
            } => {
                let name = from.rsplit('/').next().unwrap_or(from).to_string();
                // Its first sentence, in plain words: the whole report flattened
                // onto one row read `· 结论: **这套…** ## 1. importer…`, markdown
                // and all, and the `[… finished turn 1: Stopped]` the team
                // wraps a turn-end report in.
                let said =
                    crate::content::report_gist(crate::content::member_report_body(&name, text));
                if let Some(m) = Self::find(state, &name) {
                    m.last = said;
                }
            }
            _ => {}
        }
    }

    fn render(state: &State, vp: &Viewport<'_>) -> Vec<Line> {
        use crate::caps::Glyph;
        use crate::el::El;

        let w = vp.rect.w;
        if w == 0 || vp.rect.h == 0 {
            return Vec::new();
        }
        let rows = rows(state, vp.moment);
        // Nobody on the team is no panel: not a header, not a blank row. The
        // predicate is `height`'s, so the row this asks for is the row it draws.
        if rows.is_empty() {
            return Vec::new();
        }
        let caps = vp.moment.caps;
        let muted = theme::fg(Role::Muted);
        if all_resting(&rows, vp.moment) {
            return vec![Line::styled(
                width::take_width(&t(Msg::TeamHeaderAllIdle { count: rows.len() }), w as usize),
                muted,
            )];
        }

        let switchable = targets(vp.moment);
        // Both of these are true while the panel has the keyboard, and one of
        // them can hold without it: a pointer lights a row merely by being over
        // it. The legend follows the keyboard — it is a list of keys, and there
        // are no keys to list until `Tab` hands them over.
        let focused = vp.moment.team_keyboard && !switchable.is_empty();
        let pointing = vp.moment.team_cursor.is_some() && !switchable.is_empty();
        let background = rows.iter().filter(|r| r.background).count();
        let members = rows.len() - background;
        let mut out = vec![Line::styled(
            width::take_width(
                &if focused {
                    t(Msg::TeamHeaderFocused {
                        count: members,
                        background,
                    })
                } else {
                    t(Msg::TeamHeader {
                        count: members,
                        background,
                    })
                },
                w as usize,
            ),
            muted,
        )];
        // Where the person is, and where the keyboard or the pointer is. The lit
        // row is the pointed-at one: with the keyboard it is where the arrows
        // are, without it it is where the pointer is, and either way it is the
        // row a press or `Enter` would take.
        let lit = |i: usize| pointing && vp.moment.team_cursor == Some(i);
        let here = |session: &str| !vp.moment.viewing.is_empty() && vp.moment.viewing == session;
        // A row's own words on the left, and how long it has run and how big
        // its context is at the right edge — when the row is wide enough for
        // both, which is what keeps the figures from pushing the name off.
        let lay = |line: Vec<El>, right: Option<String>| -> Vec<Line> {
            let Some(right) = right else {
                return El::row(line).lay(w);
            };
            let right_w = width::str_width(&right);
            if (w as usize) < right_w + 24 {
                return El::row(line).lay(w);
            }
            let room = w as usize - right_w - 2;
            let mut lines = El::row(line).lay(room as u16);
            if let Some(first) = lines.first_mut() {
                let used = first.width();
                let mut spans = std::mem::take(&mut first.spans);
                spans.push(crate::frame::Span::raw(
                    " ".repeat(w as usize - used - right_w),
                ));
                spans.push(crate::frame::Span::styled(right, theme::fg(Role::Muted)));
                *first = Line::from_spans(spans);
            }
            lines
        };
        let band = |lines: Vec<Line>, lit: bool| -> Vec<Line> {
            if !lit {
                return lines;
            }
            // A band across the row, the panel one step brighter — the same
            // mark a question's pointed-at answer gets.
            let band = theme::bg(Role::PanelSelBg);
            lines
                .into_iter()
                .map(|line| {
                    let used = line.width();
                    let mut spans: Vec<crate::frame::Span> = line
                        .spans
                        .into_iter()
                        .map(|span| crate::frame::Span::styled(span.text, span.style.under(band)))
                        .collect();
                    if used < w as usize {
                        spans.push(crate::frame::Span::styled(
                            " ".repeat(w as usize - used),
                            band,
                        ));
                    }
                    Line::from_spans(spans).truncate(w as usize)
                })
                .collect()
        };
        // The agent on screen is the one filled in and in full ink; every
        // other is a hollow ring, muted — Claude Code's agent list, where the
        // mark says which one you are looking at and the words say the rest.
        let mark_of = |session: &str| -> (String, crate::frame::Style) {
            // `●`, the conversation's own dot — `Bullet` is the smaller `•`.
            if here(session) {
                (
                    caps.g(Glyph::ToolMark).to_string(),
                    theme::fg(Role::Secondary),
                )
            } else {
                (caps.g(Glyph::Hollow).to_string(), muted)
            }
        };
        if !switchable.is_empty() {
            let (mark, ink) = mark_of(main_target(vp.moment));
            out.extend(band(
                lay(
                    vec![
                        El::styled(format!("{mark} "), ink),
                        El::styled(t(Msg::TeamLead).into_owned(), ink),
                    ],
                    None,
                ),
                lit(0),
            ));
        }

        // One column width for everyone, so the eye reads down rather than
        // hunting: the longest name, capped, and the same for roles.
        let name_w = rows
            .iter()
            .map(|r| {
                width::str_width(&r.member.name).min(if r.labelled { LABEL_CAP } else { NAME_CAP })
            })
            .max()
            .unwrap_or(0);
        let role_w = rows
            .iter()
            .map(|r| width::str_width(&r.member.role).min(ROLE_CAP))
            .max()
            .unwrap_or(0);

        for row in &rows {
            let selectable = switchable.iter().position(|s| *s == row.session);
            let (mark, ink) = mark_of(&row.session);
            // An idle member's row recedes — it is waiting, not working — unless
            // it is the one on screen, which keeps the mark of where you are.
            let ink = match row.state {
                Shown::Idle if !here(&row.session) => muted,
                _ => ink,
            };
            // Only idle says so. Working is what the running time at the edge
            // already shows; a turn count said `第 1 轮` for every subagent,
            // which only ever runs one.
            let said = match row.state {
                Shown::Working => String::new(),
                Shown::Idle => format!(" {}", pt(PMsg::BgStateIdle)),
                Shown::Waiting => format!(" {}", t(Msg::TeamBackgroundWaiting)),
            };
            let said_ink = match row.state {
                Shown::Waiting => theme::fg(Role::Warning),
                _ => muted,
            };
            let mut line: Vec<El> = Vec::new();
            line.push(El::styled(format!("{mark} "), ink));
            line.push(El::styled(pad(&row.member.name, name_w), ink));
            if role_w > 0 {
                line.push(El::styled(
                    format!(" {}", pad(&row.member.role, role_w)),
                    muted,
                ));
            }
            line.push(El::styled(said, said_ink));
            if !row.member.last.is_empty() {
                line.push(El::styled(
                    format!(" {} {}", caps.g(Glyph::Separator), row.member.last),
                    muted,
                ));
            }
            // `1 分 52 秒 · ↓ 61.6k tok`: the figures a person scans the list
            // for — which one is still going, and how much it has read.
            let figures = row.elapsed.map(|took| {
                let took = crate::text::spoken_duration(took.as_secs());
                match row.tokens {
                    0 => took,
                    n => format!(
                        "{took} {} ↓ {} tok",
                        caps.g(Glyph::Separator),
                        crate::content::token_count(n)
                    ),
                }
            });
            out.extend(band(lay(line, figures), selectable.is_some_and(lit)));
        }
        out
    }

    /// A header plus a line each — and nothing at all for a team with no one on
    /// it. `Hug`, so a team of one does not reserve room for six; `Hug(0)`, so a
    /// screen that has not delegated, or whose every member has stopped, keeps
    /// the row instead of showing a strip of chrome. The host still caps it,
    /// because a module requests and never seizes.
    ///
    /// `render` draws from the same predicate, which is what makes the two one
    /// decision rather than two that agree by luck: a header with no rows under
    /// it would be a line saying nothing, and it would take the conversation's
    /// row to say it.
    fn height(state: &State, moment: &Moment, _: u16) -> Height {
        if !running(moment) {
            return Height::Hug(0);
        }
        let rows = rows(state, moment);
        if all_resting(&rows, moment) {
            return Height::Hug(1);
        }
        Height::Hug(
            1 + u16::from(!targets(moment).is_empty()) + rows.len().min(u16::MAX as usize) as u16,
        )
    }

    /// The spinner needs frames. The same cadence as the status line, for the
    /// same reason: an idle screen renders identically, so nothing repaints.
    fn tick() -> Option<std::time::Duration> {
        Some(std::time::Duration::from_millis(110))
    }
}

/// The panel as one line: every member idle, the lead's turn over, and nobody
/// pointing at the panel. Nothing is moving then, and three rows of `空闲` held
/// the bottom of the screen after the work was done.
///
/// It opens again when a member starts work or the lead starts a turn, and when
/// a person points at it — `↓` as the line says, or a press on the line: either
/// sets `team_cursor`. Pointing, not the keyboard: a panel a person opened stays
/// open while the pointer is on its rows without the keys having been handed
/// over. The next turn the lead starts lets go of a pointing that was not the
/// keyboard's, so a team idle again after it folds again.
fn all_resting(rows: &[Row], moment: &Moment) -> bool {
    !rows.is_empty()
        && (moment.viewing.is_empty() || moment.viewing == main_target(moment))
        && !moment.turn_open
        && moment.team_cursor.is_none()
        && rows.iter().all(|r| r.state == Shown::Idle)
}

/// What a member's mark says: whether it is running a turn right now.
///
/// There is no third state. A stopped member is not drawn at all (`rows`), so
/// "stopped" is the absence of a row rather than a kind of one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shown {
    Working,
    Idle,
    /// A background session stopped on a question for the person.
    Waiting,
}

struct Row {
    member: Member,
    /// Its name is the label it was delegated with, not one a lead gave it.
    labelled: bool,
    elapsed: Option<std::time::Duration>,
    tokens: u32,
    state: Shown,
    /// Empty for a member only this log knows of, which cannot be switched to.
    session: String,
    /// A background session (see [`background`]), not a member.
    background: bool,
}

/// The two sources, joined by name.
///
/// The log's order first — that is the order the person delegated in — then
/// anything the registry knows about that this log never mentioned: a
/// subagent the `task` tool created, or a member delegated before this panel
/// was mounted. Live state wins over folded state, because it is now.
///
/// What is stopped is not here at all, on either source: a member the registry
/// still has but has marked gone, and a member only this log remembers because
/// it was stopped before this screen opened. That is the same decision
/// [`targets`] makes, and the two have to match — a drawn row with no target,
/// or a target with no row, would put the pointer on the wrong agent.
fn rows(state: &State, moment: &Moment) -> Vec<Row> {
    // The registry's first, in the order `targets` switches between them, so
    // the drawn rows and the selectable ones agree; what the log adds — a role,
    // the last thing said — is joined on by name.
    let mut out: Vec<Row> = Vec::new();
    for live in &moment.members {
        let logged = state.members.iter().find(|m| m.name == live.name);
        if (live.gone || logged.is_some_and(|m| m.stopped)) && live.session != moment.viewing {
            continue;
        }
        // One this log never delegated — a subagent the `task` tool made — is
        // listed by what it was asked to do: its name is a minted id.
        let labelled = logged.is_none() && live.label.is_some();
        out.push(Row {
            member: logged.cloned().unwrap_or_else(|| Member {
                name: live.label.clone().unwrap_or_else(|| live.name.clone()),
                ..Member::default()
            }),
            labelled,
            elapsed: live.elapsed,
            tokens: live.tokens,
            state: match live.activity {
                Activity::Idle => Shown::Idle,
                _ => Shown::Working,
            },
            session: live.session.clone(),
            background: false,
        });
    }
    // Then the background sessions, still among the switchable rows: [`targets`]
    // lists them in this place, and a drawn row must be the target it says.
    for session in background(moment) {
        let stats = session.stats.as_ref();
        out.push(Row {
            member: Member {
                name: session.title.clone(),
                role: t(Msg::TeamBackgroundRole).into_owned(),
                last: session.last.clone().unwrap_or_default().replace('\n', " "),
                stopped: false,
            },
            labelled: true,
            // What the host folded from its log; nothing to show before it ran.
            elapsed: stats
                .filter(|s| s.elapsed_ms > 0)
                .map(|s| std::time::Duration::from_millis(s.elapsed_ms)),
            tokens: stats.map_or(0, |s| s.prompt),
            state: if session.waiting {
                Shown::Waiting
            } else {
                Shown::Working
            },
            session: session.id.clone(),
            background: true,
        });
    }
    if !moment.viewing.is_empty()
        && moment.viewing != main_target(moment)
        && !out.iter().any(|row| row.session == moment.viewing)
    {
        out.push(Row {
            member: Member {
                name: moment
                    .team_return
                    .as_ref()
                    .map(|back| back.current_label.clone())
                    .unwrap_or_else(|| {
                        moment
                            .viewing
                            .rsplit('/')
                            .next()
                            .unwrap_or(&moment.viewing)
                            .to_string()
                    }),
                ..Member::default()
            },
            labelled: false,
            elapsed: None,
            tokens: 0,
            state: Shown::Idle,
            session: moment.viewing.clone(),
            background: false,
        });
    }
    // Then what only this log remembers — a member stopped before this screen
    // was opened: stopped, so not drawn, and not switchable either.
    for member in &state.members {
        if member.stopped || moment.members.iter().any(|m| m.name == member.name) {
            continue;
        }
        out.push(Row {
            member: member.clone(),
            labelled: false,
            elapsed: None,
            tokens: 0,
            state: Shown::Working,
            session: String::new(),
            background: false,
        });
    }
    out
}

/// Cut or pad to exactly `cells`, counting what the terminal draws rather than
/// bytes — a member called `巡查` is two characters and four cells.
fn pad(text: &str, cells: usize) -> String {
    let cut = width::take_width(text, cells);
    let short = cells.saturating_sub(width::str_width(&cut));
    format!("{cut}{}", " ".repeat(short))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::Rect;
    use crate::moment::MemberNow;
    use atomcode_kernel::tool::ToolCall;

    crate::tui_conformance!(view Team as team_conformance);

    fn delegate(id: &str, name: &str, role: &str) -> SessionEvent {
        SessionEvent::AssistantMessage {
            turn: 1,
            round: 1,
            text: String::new(),
            reasoning: String::new(),
            tool_calls: vec![ToolCall {
                id: id.into(),
                name: "team".into(),
                arguments: format!(
                    r#"{{"action":"delegate","name":"{name}","role":"{role}","task":"go"}}"#
                ),
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
            content: "ok".into(),
            is_error,
            images: Vec::new(),
        }
    }

    fn drew(state: &State, moment: &Moment) -> String {
        let vp = Viewport::new(Rect::sized(60, 10), moment);
        Team::render(state, &vp)
            .iter()
            .map(|l| l.plain())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// A subagent the `task` tool made is listed by what it was asked to do —
    /// its session id is minted and says nothing — with how long it has run
    /// and how big its context is at the right edge; the lead is `main`.
    #[test]
    fn a_subagent_is_listed_by_its_label_with_its_time_and_context() {
        let live = Moment::default()
            .with_lead("lead-1")
            .with_members(vec![MemberNow {
                name: "sub-1791210735".into(),
                activity: Activity::Working,
                turn: 1,
                session: "sub-1791210735".into(),
                label: Some("审查 Chat.tsx 缺陷".into()),
                elapsed: Some(std::time::Duration::from_secs(112)),
                tokens: 61_600,
                ..MemberNow::default()
            }]);
        let live = Moment {
            viewing: "lead-1".into(),
            ..live
        };
        let screen = drew(&State::default(), &live);
        // Claude Code's marks: the one on screen filled, every other hollow.
        assert!(
            screen.contains("● main"),
            "the lead is on screen:\n{screen}"
        );
        assert!(screen.contains("○ 审查"), "the subagent is not:\n{screen}");
        let row = screen
            .lines()
            .find(|l| l.contains("审查 Chat.tsx 缺陷"))
            .unwrap_or_else(|| panic!("listed by its label:\n{screen}"));
        assert!(
            !screen.contains("sub-1791210735"),
            "not by its id:\n{screen}"
        );
        assert!(
            row.trim_end().ends_with("↓ 61.6K tok") && row.contains("1 分 52 秒"),
            "time and context at the right edge: {row:?}"
        );
    }

    /// A member log outlives its registry row. While that log is on screen the
    /// team strip is navigation, not merely live-status chrome: it must retain
    /// both the current member and the way back to `main`.
    #[test]
    fn a_member_view_keeps_the_road_back_after_its_live_row_is_gone() {
        let moment = Moment {
            lead: "lead-1".into(),
            viewing: "lead-1/code-review".into(),
            ..Moment::default()
        };
        let state = State::default();
        let screen = drew(&state, &moment);
        assert!(
            screen.contains("○ main"),
            "the lead stays reachable:\n{screen}"
        );
        assert!(
            screen.contains("● code-review"),
            "the historical member is still the current row:\n{screen}"
        );
        assert_eq!(
            targets(&moment),
            vec!["lead-1".to_string(), "lead-1/code-review".to_string()]
        );
        assert_eq!(
            Team::height(&state, &moment, 60),
            Height::Hug(3),
            "member navigation never folds away while the member is viewed"
        );
    }

    /// A background session is a separate runtime, so it cannot use the
    /// member switch path. The breadcrumb still draws the same two-row road
    /// back; the TUI turns its `main` target into `/resume <origin>`.
    #[test]
    fn a_background_session_opened_from_team_keeps_main_on_the_strip() {
        let moment = Moment {
            lead: "review-7".into(),
            viewing: "review-7".into(),
            team_return: Some(crate::moment::TeamReturn {
                session: "lead-1".into(),
                current_label: "code-review".into(),
            }),
            ..Moment::default()
        };
        let state = State::default();
        let screen = drew(&state, &moment);
        assert!(
            screen.contains("○ main"),
            "the way back is drawn:\n{screen}"
        );
        assert!(
            screen.contains("● code-review"),
            "the background session keeps its human title:\n{screen}"
        );
        assert_eq!(
            targets(&moment),
            vec!["lead-1".to_string(), "review-7".to_string()]
        );
        assert_eq!(Team::height(&state, &moment, 60), Height::Hug(3));
    }

    fn bg_session(
        id: &str,
        title: &str,
        group: crate::bg::Group,
        origin: &str,
    ) -> crate::bg::Session {
        crate::bg::Session {
            id: id.into(),
            title: title.into(),
            group,
            last: Some("Tracing ChangeDirectory\nand current_dir usage".into()),
            waiting: group == crate::bg::Group::NeedsInput,
            failed: false,
            origin: Some(origin.into()),
            stats: Some(atomcode_host_api::BackgroundStats {
                steps: 3,
                tools: 5,
                prompt: 94_400,
                cached: 0,
                completion: 800,
                elapsed_ms: 86_000,
                sent: 0,
                sent_cached: 0,
            }),
        }
    }

    /// A background session this conversation started — `code-review` run in
    /// the background — is a row under `main`, Claude Code's way: its name,
    /// that it is background work, what it last said, and its time and
    /// context at the edge. Someone else's, and finished ones, are not.
    #[test]
    fn our_background_work_is_listed_under_the_lead() {
        use crate::bg::{BgView, Group};
        let mut moment = Moment::default().with_lead("lead-1");
        moment.viewing = "lead-1".into();
        moment.bg = BgView::new(vec![
            bg_session("bg-1", "code-review", Group::Working, "lead-1"),
            bg_session("bg-2", "theirs", Group::Working, "other"),
            bg_session("bg-3", "finished", Group::Completed, "lead-1"),
        ]);
        let state = State::default();
        let screen = drew(&state, &moment);
        assert!(screen.contains("● main"), "{screen}");
        let row = screen
            .lines()
            .find(|l| l.contains("code-review"))
            .unwrap_or_else(|| panic!("listed:\n{screen}"));
        assert!(row.contains("○ code-review"), "a hollow ring: {row:?}");
        assert!(row.contains(&*t(Msg::TeamBackgroundRole)), "{row:?}");
        assert!(
            row.contains("· Tracing") && !screen.contains("\nand current_dir"),
            "its last words, on one line, cut to fit: {row:?}"
        );
        assert!(
            row.trim_end().ends_with("↓ 94.4K tok") && row.contains("1 分 26 秒"),
            "time and context at the edge: {row:?}"
        );
        assert!(
            !screen.contains("theirs") && !screen.contains("finished"),
            "{screen}"
        );
        assert!(
            screen.lines().next().is_some_and(|h| h.contains("1")
                && h.contains(&*t(Msg::TeamHeader {
                    count: 0,
                    background: 1
                }))),
            "the header counts it as background work: {screen}"
        );
        // The panel is up for it alone, and its row is a target in drawn order.
        assert!(Team::height(&state, &moment, 60) != Height::Hug(0));
        assert_eq!(
            targets(&moment),
            vec!["lead-1".to_string(), "bg-1".to_string()]
        );
        assert!(is_background(&moment, "bg-1") && !is_background(&moment, "lead-1"));
        assert_eq!(target_at_line(&moment, 2), Some(1), "line 2 is the bg row");
    }

    /// One waiting on a question says so, in the warning colour's words.
    #[test]
    fn a_background_session_waiting_on_the_person_says_so() {
        use crate::bg::{BgView, Group};
        let mut moment = Moment::default().with_lead("lead-1");
        moment.bg = BgView::new(vec![bg_session(
            "bg-1",
            "code-review",
            Group::NeedsInput,
            "lead-1",
        )]);
        let screen = drew(&State::default(), &moment);
        assert!(screen.contains(&*t(Msg::TeamBackgroundWaiting)), "{screen}");
    }

    /// Members first, then background work: the order `targets` switches in.
    #[test]
    fn members_come_before_background_work() {
        use crate::bg::{BgView, Group};
        let mut moment = Moment::default()
            .with_lead("lead-1")
            .with_members(vec![MemberNow {
                name: "scout".into(),
                activity: Activity::Working,
                turn: 1,
                session: "lead-1/scout".into(),
                ..MemberNow::default()
            }]);
        moment.bg = BgView::new(vec![bg_session(
            "bg-1",
            "code-review",
            Group::Working,
            "lead-1",
        )]);
        let screen = drew(&State::default(), &moment);
        let at = |needle: &str| {
            screen
                .find(needle)
                .unwrap_or_else(|| panic!("{needle}:\n{screen}"))
        };
        assert!(at("scout") < at("code-review"), "{screen}");
        assert_eq!(
            targets(&moment),
            vec!["lead-1".to_string(), "lead-1/scout".into(), "bg-1".into()]
        );
    }

    fn fold(facts: &[SessionEvent]) -> State {
        let mut state = State::default();
        for fact in facts {
            Team::absorb(&mut state, fact);
        }
        state
    }

    #[test]
    fn a_delegation_appears_only_once_its_call_came_back() {
        let mut state = fold(&[delegate("c1", "scout", "explorer")]);
        assert!(
            drew(&state, &Moment::default()).is_empty(),
            "a call in flight is not a member yet, and no member is no panel"
        );
        Team::absorb(&mut state, &result("c1", false));
        assert!(drew(&state, &Moment::default()).contains("scout"));
    }

    #[test]
    fn a_refused_delegation_leaves_no_member() {
        let state = fold(&[delegate("c1", "scout", "explorer"), result("c1", true)]);
        let screen = drew(&state, &Moment::default());
        assert!(
            !screen.contains("scout") && screen.is_empty(),
            "a refused delegation leaves nobody, so there is nothing to draw: {screen}"
        );
    }

    #[test]
    fn a_report_is_shown_once_without_the_name_twice() {
        let state = fold(&[
            delegate("c1", "scout", "explorer"),
            result("c1", false),
            SessionEvent::Injected {
                turn: 1,
                text: "[scout] sessions are made in agent.rs".into(),
                origin: InjectionOrigin::Peer {
                    from: "lead-1/scout".into(),
                    outside: false,
                },
            },
        ]);
        let screen = drew(&state, &Moment::default());
        assert!(screen.contains("sessions are made in agent.rs"), "{screen}");
        assert_eq!(screen.matches("scout").count(), 1, "{screen}");
    }

    /// A member that stopped is not a kind of row — it is not a row. It cannot
    /// come back, it takes no room in the next delegation, and the panel is
    /// what this agent is running. Reading what it said is `/agents`.
    #[test]
    fn a_member_the_registry_lost_leaves_the_panel_with_the_row() {
        let state = fold(&[delegate("c1", "scout", "explorer"), result("c1", false)]);
        let live = Moment::default()
            .with_lead("lead-1")
            .with_members(vec![MemberNow {
                name: "scout".into(),
                activity: Activity::Working,
                turn: 2,
                session: "lead-1/scout".into(),
                ..MemberNow::default()
            }]);
        assert!(
            drew(&state, &live).contains("scout") && !drew(&state, &live).contains("轮"),
            "a working member is drawn, without a turn count:\n{}",
            drew(&state, &live)
        );
        assert_eq!(targets(&live), vec!["lead-1", "lead-1/scout"]);

        // The registry still has it and says it is gone: the row goes, and so
        // does the target — the panel must not draw one of the two.
        let stopped = Moment::default()
            .with_lead("lead-1")
            .with_members(vec![MemberNow {
                name: "scout".into(),
                activity: Activity::Idle,
                turn: 2,
                session: "lead-1/scout".into(),
                gone: true,
                ..MemberNow::default()
            }]);
        let screen = drew(&state, &stopped);
        assert!(
            !screen.contains("scout"),
            "a stopped member is not drawn:\n{screen}"
        );
        assert!(
            !screen.contains("已结束"),
            "there is no such state on a row:\n{screen}"
        );
        // Nobody is running, so there is no panel and nothing to point at.
        // What keeps this from stranding a person who was reading the member is
        // not a spare row here — it is the lead coming back on screen, which the
        // host does when the agent on screen goes (`host::switch_view` callers).
        assert_eq!(targets(&stopped), Vec::<String>::new());

        // And the log alone — no registry — is the same answer, which is the
        // half a resumed screen sees: the stop is a fact in this log.
        let stopped_folded = fold(&[
            delegate("c1", "scout", "explorer"),
            result("c1", false),
            SessionEvent::AssistantMessage {
                turn: 2,
                round: 1,
                text: String::new(),
                reasoning: String::new(),
                tool_calls: vec![ToolCall {
                    id: "c2".into(),
                    name: "team".into(),
                    arguments: r#"{"action":"stop","name":"scout"}"#.into(),
                }],
                reasoning_blocks: Vec::new(),
                meta: None,
            },
            result("c2", false),
        ]);
        let screen = drew(&stopped_folded, &Moment::default().with_lead("lead-1"));
        assert!(
            !screen.contains("scout"),
            "a member this log stopped is not drawn either:\n{screen}"
        );
    }

    /// A team whose every member has stopped is no team on screen: the panel
    /// goes, and with it the rows the arrows and the pointer work on.
    ///
    /// This is the half that is easy to get wrong. Leaving the lead as a lone
    /// row would draw a panel with one line in it whose only content is "back to
    /// the lead" — a strip of chrome for a session that has nobody left to look
    /// at — and it would hand `Tab` a row to focus on a screen with no panel.
    #[test]
    fn a_team_of_nothing_but_stopped_members_is_no_panel_at_all() {
        let state = fold(&[
            delegate("c1", "scout", "explorer"),
            result("c1", false),
            SessionEvent::AssistantMessage {
                turn: 2,
                round: 1,
                text: String::new(),
                reasoning: String::new(),
                tool_calls: vec![ToolCall {
                    id: "c2".into(),
                    name: "team".into(),
                    arguments: r#"{"action":"stop"}"#.into(),
                }],
                reasoning_blocks: Vec::new(),
                meta: None,
            },
            result("c2", false),
        ]);
        let live = Moment::default()
            .with_lead("lead-1")
            .with_members(vec![MemberNow {
                name: "scout".into(),
                session: "lead-1/scout".into(),
                gone: true,
                ..MemberNow::default()
            }]);
        assert!(drew(&state, &live).is_empty(), "no member, no panel");
        assert_eq!(Team::height(&state, &live, 60), Height::Hug(0));
        assert!(
            targets(&live).is_empty(),
            "and nothing for Tab or a press to land on"
        );
    }

    #[test]
    fn an_agent_this_log_never_mentioned_is_still_on_the_panel() {
        let live = Moment::default().with_members(vec![MemberNow {
            name: "task-1".into(),
            activity: Activity::Working,
            turn: 1,
            ..MemberNow::default()
        }]);
        let screen = drew(&State::default(), &live);
        assert!(screen.contains("task-1"), "{screen}");
        assert!(screen.contains("1 名成员"), "{screen}");
    }

    /// Nothing the panel draws is wider than the strip it was given — with
    /// members on it, which is the only interesting case and the one the
    /// shared property suite cannot reach: conformance folds a corpus with no
    /// team in it, so an empty panel is all it ever checks.
    #[test]
    fn nothing_it_draws_is_wider_than_its_rect_at_any_width() {
        let state = fold(&[
            delegate("c1", "巡查员-with-a-very-long-name", "explorer"),
            result("c1", false),
            delegate("c2", "lib", "docs_writer"),
            result("c2", false),
            SessionEvent::Injected {
                turn: 1,
                text: "[lib] 列了 12 个文档,还有一些很长很长很长的中文说明文字".into(),
                origin: InjectionOrigin::Peer {
                    from: "l/lib".into(),
                    outside: false,
                },
            },
        ]);
        let moment = Moment::default().with_members(vec![
            MemberNow {
                name: "巡查员-with-a-very-long-name".into(),
                activity: Activity::Working,
                turn: 3,
                ..MemberNow::default()
            },
            MemberNow {
                name: "lib".into(),
                activity: Activity::Idle,
                turn: 1,
                ..MemberNow::default()
            },
        ]);
        for w in 1u16..100 {
            let vp = Viewport::new(Rect::sized(w, 10), &moment);
            for (i, line) in Team::render(&state, &vp).iter().enumerate() {
                assert!(
                    line.width() <= w as usize,
                    "team line {i} is {} cells at width {w}: {:?}",
                    line.width(),
                    line.plain()
                );
            }
        }
    }

    #[test]
    fn the_panel_asks_for_a_line_each_plus_its_header() {
        let state = fold(&[
            delegate("c1", "scout", "explorer"),
            result("c1", false),
            delegate("c2", "lib", "docs_writer"),
            result("c2", false),
        ]);
        // Two members down there, and the lead is a row too, so two header-row
        // plus two. Asked for with no team at all — the row is mounted and the
        // panel still takes nothing.
        let live = Moment::default().with_lead("lead-1").with_members(vec![
            MemberNow {
                name: "scout".into(),
                session: "lead-1/scout".into(),
                ..MemberNow::default()
            },
            MemberNow {
                name: "lib".into(),
                session: "lead-1/lib".into(),
                ..MemberNow::default()
            },
        ]);
        // While the lead's turn runs the panel is open: a line each.
        let mut working = live.clone();
        working.turn_open = true;
        assert_eq!(Team::height(&state, &working, 60), Height::Hug(4));
        // Every member idle and the turn over: one line, until the keyboard
        // takes the panel.
        assert_eq!(Team::height(&state, &live, 60), Height::Hug(1));
        let rows: Vec<String> = Team::render(&state, &Viewport::new(Rect::sized(60, 1), &live))
            .iter()
            .map(|l| l.plain())
            .collect();
        assert_eq!(rows, vec!["团队 · 2 名成员空闲 · ↓ 选择查看".to_string()]);
        let mut focused = live.clone();
        focused.team_cursor = Some(0);
        assert_eq!(Team::height(&state, &focused, 60), Height::Hug(4));
        assert_eq!(
            Team::height(&State::default(), &Moment::default(), 60),
            Height::Hug(0),
            "no members is no panel, not a header saying so"
        );
    }

    /// A member's own row is only ever asked for when there is one to draw, at
    /// every height the panel is handed.
    #[test]
    fn an_empty_team_is_drawn_as_nothing_rather_than_a_header() {
        let state = State::default();
        let moment = Moment::default();
        assert!(Team::render(&state, &Viewport::new(Rect::sized(60, 10), &moment)).is_empty());
        assert_eq!(Team::height(&state, &moment, 60), Height::Hug(0));
    }
}
