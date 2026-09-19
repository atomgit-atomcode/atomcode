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
//! Nor does it show a member that has stopped. A stopped member is not on the
//! team any more — it does not come back, it takes no room in the next
//! delegation, and a row for it would be a line that says nothing and does
//! nothing. The panel is what this agent is *running*; the way to read what a
//! stopped member said is `/agents`, which lists every member the registry has
//! announced, stopped ones included, and switches to any of them
//! (`docs/adr/0023` §5).
//!
//! # A way in
//!
//! It is also where the person switches the screen to one of them and back
//! (`docs/adr/0023` §3): the lead is its first row, `主`, and each member still
//! running follows. Which rows can be switched to is [`targets`] — the lead and
//! the members that are not gone — and the panel lights the row
//! [`Moment::team_cursor`] points at and marks the one on screen. Both are the
//! moment's, so the row the arrows are on, the row under the pointer and the row
//! a press takes are one row. [`targets`] and [`rows`] filter on the same
//! predicate for that reason: the *n*th drawn row and the *n*th target must be
//! the same agent, or a click would take the one below it.

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
}

/// The sessions the panel's selectable rows switch to, in the order they are
/// drawn: the lead, then each member still running. Empty when the panel is not
/// on screen — there is nothing to switch between, and nothing for the arrows
/// to point at.
///
/// A stopped member is not one of them: it is not on the panel, so a target for
/// it would be a row nothing drew. [`rows`] drops exactly the same members, and
/// the two must agree or a press would land on the agent below the one it hit.
/// Reading a stopped member is `/agents`.
pub fn targets(moment: &Moment) -> Vec<String> {
    if moment.lead.is_empty() || !running(moment) {
        return Vec::new();
    }
    std::iter::once(moment.lead.clone())
        .chain(
            moment
                .members
                .iter()
                .filter(|m| !m.gone)
                .map(|m| m.session.clone()),
        )
        .collect()
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
                origin: InjectionOrigin::Peer { from },
                ..
            } => {
                let name = from.rsplit('/').next().unwrap_or(from).to_string();
                let said = text
                    .strip_prefix(&format!("[{name}] "))
                    .unwrap_or(text)
                    .replace('\n', " ");
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

        let switchable = targets(vp.moment);
        let focused = vp.moment.team_cursor.is_some() && !switchable.is_empty();
        let mut out = vec![Line::styled(
            width::take_width(
                &if focused {
                    format!(
                        "团队 · {} 名成员 · ↑↓ 选 · Enter 切换 · Esc 返回",
                        rows.len()
                    )
                } else {
                    format!("团队 · {} 名成员 · Tab 切换查看", rows.len())
                },
                w as usize,
            ),
            muted,
        )];
        // Where the person is, and where the keyboard or the pointer is.
        let lit = |i: usize| focused && vp.moment.team_cursor == Some(i);
        let here = |session: &str| !vp.moment.viewing.is_empty() && vp.moment.viewing == session;
        let band = |line: Vec<El>, lit: bool| -> Vec<Line> {
            let lines = El::row(line).lay(w);
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
        if !switchable.is_empty() {
            let mark = if here(&vp.moment.lead) { "› " } else { "  " };
            out.extend(band(
                vec![
                    El::styled(mark.to_string(), theme::fg(Role::Accent)),
                    El::styled("主".to_string(), theme::fg(Role::Secondary)),
                    El::styled(
                        if here(&vp.moment.lead) {
                            " 正在看".to_string()
                        } else {
                            String::new()
                        },
                        muted,
                    ),
                ],
                lit(0),
            ));
        }

        // One column width for everyone, so the eye reads down rather than
        // hunting: the longest name, capped, and the same for roles.
        let name_w = rows
            .iter()
            .map(|r| width::str_width(&r.member.name).min(NAME_CAP))
            .max()
            .unwrap_or(0);
        let role_w = rows
            .iter()
            .map(|r| width::str_width(&r.member.role).min(ROLE_CAP))
            .max()
            .unwrap_or(0);

        for row in &rows {
            let selectable = switchable.iter().position(|s| *s == row.session);
            let (mark, mark_style) = match row.state {
                Shown::Working => (
                    caps.spinner(vp.moment.tick).to_string(),
                    theme::fg(Role::Warning),
                ),
                Shown::Idle => (caps.g(Glyph::Ok).to_string(), theme::fg(Role::Success)),
            };
            let said = match row.state {
                Shown::Working => format!("第 {} 轮", row.turn.max(1)),
                Shown::Idle => "空闲".to_string(),
            };
            let mut line: Vec<El> = Vec::new();
            if !switchable.is_empty() {
                line.push(El::styled(
                    if here(&row.session) { "› " } else { "  " }.to_string(),
                    theme::fg(Role::Accent),
                ));
            }
            line.push(El::styled(format!("{mark} "), mark_style));
            line.push(El::styled(
                pad(&row.member.name, name_w),
                theme::fg(Role::Secondary),
            ));
            if role_w > 0 {
                line.push(El::styled(
                    format!(" {}", pad(&row.member.role, role_w)),
                    muted,
                ));
            }
            line.push(El::styled(format!(" {said}"), muted));
            if here(&row.session) {
                line.push(El::styled(" 正在看".to_string(), muted));
            }
            if !row.member.last.is_empty() {
                line.push(El::styled(
                    format!(" {} {}", caps.g(Glyph::Separator), row.member.last),
                    muted,
                ));
            }
            out.extend(band(line, selectable.is_some_and(lit)));
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
        let rows = rows(state, moment).len();
        Height::Hug(1 + u16::from(!targets(moment).is_empty()) + rows.min(u16::MAX as usize) as u16)
    }

    /// The spinner needs frames. The same cadence as the status line, for the
    /// same reason: an idle screen renders identically, so nothing repaints.
    fn tick() -> Option<std::time::Duration> {
        Some(std::time::Duration::from_millis(110))
    }
}

/// What a member's mark says: whether it is running a turn right now.
///
/// There is no third state. A stopped member is not drawn at all (`rows`), so
/// "stopped" is the absence of a row rather than a kind of one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shown {
    Working,
    Idle,
}

struct Row {
    member: Member,
    state: Shown,
    turn: u64,
    /// Empty for a member only this log knows of, which cannot be switched to.
    session: String,
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
        if live.gone || logged.is_some_and(|m| m.stopped) {
            continue;
        }
        out.push(Row {
            member: logged.cloned().unwrap_or_else(|| Member {
                name: live.name.clone(),
                ..Member::default()
            }),
            state: match live.activity {
                Activity::Idle => Shown::Idle,
                _ => Shown::Working,
            },
            turn: live.turn,
            session: live.session.clone(),
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
            state: Shown::Working,
            turn: 0,
            session: String::new(),
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
            drew(&state, &live).contains("第 2 轮"),
            "{}",
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
        assert_eq!(Team::height(&state, &live, 60), Height::Hug(4));
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
