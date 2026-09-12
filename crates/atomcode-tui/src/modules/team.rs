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

/// One frame per tick, as in the status line.
const SPINNER: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];

/// How wide a name or a role column may get before it is cut.
const NAME_CAP: usize = 14;
const ROLE_CAP: usize = 12;

pub struct Team;

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
        let caps = vp.moment.caps;
        let muted = theme::fg(Role::Muted);

        let mut out = vec![Line::styled(
            width::take_width(
                &if rows.is_empty() {
                    "团队 · 还没有成员".to_string()
                } else {
                    format!("团队 · {} 名成员", rows.len())
                },
                w as usize,
            ),
            muted,
        )];

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
            let (mark, mark_style) = match row.state {
                Shown::Working => (
                    SPINNER[(vp.moment.tick as usize) % SPINNER.len()].to_string(),
                    theme::fg(Role::Warning),
                ),
                Shown::Idle => (caps.g(Glyph::Ok).to_string(), theme::fg(Role::Success)),
                Shown::Stopped => (
                    caps.g(Glyph::Interrupted).to_string(),
                    theme::fg(Role::Muted),
                ),
            };
            let said = match row.state {
                Shown::Working => format!("第 {} 轮", row.turn.max(1)),
                Shown::Idle => "空闲".to_string(),
                Shown::Stopped => "已结束".to_string(),
            };
            let mut line: Vec<El> = vec![
                El::styled(format!("{mark} "), mark_style),
                El::styled(pad(&row.member.name, name_w), theme::fg(Role::Secondary)),
            ];
            if role_w > 0 {
                line.push(El::styled(
                    format!(" {}", pad(&row.member.role, role_w)),
                    muted,
                ));
            }
            line.push(El::styled(format!(" {said}"), muted));
            if !row.member.last.is_empty() {
                line.push(El::styled(
                    format!(" {} {}", caps.g(Glyph::Separator), row.member.last),
                    muted,
                ));
            }
            out.extend(El::row(line).lay(w));
        }
        out
    }

    /// A header plus a line each. `Hug`, so a team of one does not reserve
    /// room for six — and the host still caps it, because a module requests
    /// and never seizes.
    fn height(state: &State, moment: &Moment, _: u16) -> Height {
        Height::Hug(1 + rows(state, moment).len().min(u16::MAX as usize) as u16)
    }

    /// The spinner needs frames. The same cadence as the status line, for the
    /// same reason: an idle screen renders identically, so nothing repaints.
    fn tick() -> Option<std::time::Duration> {
        Some(std::time::Duration::from_millis(110))
    }
}

/// What a member's mark says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shown {
    Working,
    Idle,
    Stopped,
}

struct Row {
    member: Member,
    state: Shown,
    turn: u64,
}

/// The two sources, joined by name.
///
/// The log's order first — that is the order the person delegated in — then
/// anything the registry knows about that this log never mentioned: a
/// subagent the `task` tool created, or a member delegated before this panel
/// was mounted. Live state wins over folded state, because it is now.
fn rows(state: &State, moment: &Moment) -> Vec<Row> {
    let mut out: Vec<Row> = Vec::new();
    for member in &state.members {
        let live = moment.members.iter().find(|m| m.name == member.name);
        let shown = match (live, member.stopped) {
            (None, _) => Shown::Stopped,
            (Some(_), true) => Shown::Stopped,
            (Some(l), false) if l.activity == Activity::Idle => Shown::Idle,
            (Some(_), false) => Shown::Working,
        };
        out.push(Row {
            member: member.clone(),
            state: shown,
            turn: live.map(|l| l.turn).unwrap_or_default(),
        });
    }
    for live in &moment.members {
        if state.members.iter().any(|m| m.name == live.name) {
            continue;
        }
        out.push(Row {
            member: Member {
                name: live.name.clone(),
                ..Member::default()
            },
            state: if live.activity == Activity::Idle {
                Shown::Idle
            } else {
                Shown::Working
            },
            turn: live.turn,
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
            drew(&state, &Moment::default()).contains("还没有成员"),
            "a call in flight is not a member yet"
        );
        Team::absorb(&mut state, &result("c1", false));
        assert!(drew(&state, &Moment::default()).contains("scout"));
    }

    #[test]
    fn a_refused_delegation_leaves_no_member() {
        let state = fold(&[delegate("c1", "scout", "explorer"), result("c1", true)]);
        let screen = drew(&state, &Moment::default());
        assert!(
            !screen.contains("scout") && screen.contains("还没有成员"),
            "{screen}"
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

    #[test]
    fn a_member_the_registry_still_has_is_working_a_member_it_lost_is_done() {
        let state = fold(&[delegate("c1", "scout", "explorer"), result("c1", false)]);
        let live = Moment::default().with_members(vec![MemberNow {
            name: "scout".into(),
            activity: Activity::Working,
            turn: 2,
        }]);
        assert!(
            drew(&state, &live).contains("第 2 轮"),
            "{}",
            drew(&state, &live)
        );
        assert!(
            drew(&state, &Moment::default()).contains("已结束"),
            "gone from the registry is gone"
        );
    }

    #[test]
    fn an_agent_this_log_never_mentioned_is_still_on_the_panel() {
        let live = Moment::default().with_members(vec![MemberNow {
            name: "task-1".into(),
            activity: Activity::Working,
            turn: 1,
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
                origin: InjectionOrigin::Peer { from: "l/lib".into() },
            },
        ]);
        let moment = Moment::default().with_members(vec![
            MemberNow { name: "巡查员-with-a-very-long-name".into(), activity: Activity::Working, turn: 3 },
            MemberNow { name: "lib".into(), activity: Activity::Idle, turn: 1 },
        ]);
        for w in 1u16..100 {
            let vp = Viewport::new(Rect::sized(w, 10), &moment);
            for (i, line) in Team::render(&state, &vp).iter().enumerate() {
                assert!(line.width() <= w as usize, "team line {i} is {} cells at width {w}: {:?}", line.width(), line.plain());
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
        assert_eq!(Team::height(&state, &Moment::default(), 60), Height::Hug(3));
        assert_eq!(
            Team::height(&State::default(), &Moment::default(), 60),
            Height::Hug(1)
        );
    }
}
