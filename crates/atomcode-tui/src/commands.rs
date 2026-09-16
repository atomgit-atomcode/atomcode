//! The commands this build ships, grouped by what they are about.
//!
//! Split into sets on purpose: the screen's own verbs, the session's, and
//! `/help`. Removing a set removes its commands, and a capability that wants a
//! command of its own contributes a set rather than editing anything here.
//!
//! There are no commands that read or rewrite the agent's config tree: the
//! agent is in the host's App, and what a person may change about it is what
//! host control offers (`docs/adr/0022` §7).

use std::sync::Arc;

use async_trait::async_trait;
use atomcode_kernel::host::{HostCommand, HostError, HostReply};
use atomcode_kernel::provider::ReasoningEffort;
use atomcode_kernel::session::{derive_messages, SessionEvent};
use atomcode_plexus::Context;

use crate::command::{Command, CommandSet, Commands, Outcome};
use crate::keymap::Action;

/// Quit, clear, fold — things the screen itself owns.
pub struct ScreenCommands;

const SCREEN: &[Command] = &[
    Command::new("quit", "退出"),
    Command::new("exit", "退出"),
    Command::new("clear", "清空输入行"),
    Command::new("reasoning", "思考:一行、全文、收起,循环"),
    Command::new("tools", "展开或折叠工具调用的结果"),
    Command::new(
        "showinject",
        "环境注入:收起、只留标签、全文,循环;不带名字则全部",
    ),
    Command::new("mouse", "把鼠标交还终端,或收回来"),
    Command::new("keys", "列出快捷键"),
];

#[async_trait]
impl CommandSet for ScreenCommands {
    fn id(&self) -> &'static str {
        "cmd-screen"
    }
    fn commands(&self) -> Vec<Command> {
        SCREEN.to_vec()
    }
    async fn run(&self, name: &str, args: &str, _ctx: &Context) -> Outcome {
        match name {
            "quit" | "exit" => Outcome::Do(Action::Quit),
            "clear" => Outcome::Do(Action::Clear),
            "reasoning" => Outcome::Do(Action::ToggleFold("reasoning")),
            "tools" => Outcome::Do(Action::ToggleFold("tool_call")),
            "showinject" => match showinject(&args.to_ascii_lowercase()) {
                Ok(action) => Outcome::Do(action),
                Err(why) => Outcome::Refused(why),
            },
            "mouse" => Outcome::Do(Action::ToggleMouse),
            "keys" => Outcome::Said(
                "enter 发送 · shift+enter 换行(或 ctrl-j) · ctrl-d 退出 · ctrl-w 删词\n\
                 esc 依次:取消选中 -> 清空输入 -> 停止当轮 · ctrl-c 直接停止当轮\n\
                 上/下 在输入里移动游标,到头则翻历史 · 点击输入框定位游标\n\
                 pgup/pgdn 与滚轮滚动对话\n\
                 ctrl-r 思考(一行/全文/收起,循环) · ctrl-t 折叠工具 · ctrl-l 重画屏幕\n\
                 /showinject [名字] 环境注入(默认不显示;不带名字则全部,all 含同伴报告)\n\
                 拖动选中并复制 · esc 取消选中 · 点击思考或工具调用折叠展开那一个\n\
                 ctrl-o 把鼠标交还终端(改用终端自己的框选)"
                    .into(),
            ),
            _ => Outcome::Quiet,
        }
    }
}

/// Turn `/showinject <what>` into the one action it means.
///
/// Split out from the dispatch because the interesting part is the refusal, and
/// a refusal that has to be written to be tested is a refusal that says what the
/// alternatives were. `/showinject` with nothing after it is the group — every
/// environmental injection at once — since that is the thing a person forms an
/// opinion about, not any one of them.
///
/// A named one is the same `Hidden → Folded → Open → Hidden` cycle `/reasoning`
/// is, with `Folded` standing in the label `[reminder]` alone: the useful middle
/// state for something that is off the screen because it is noise but is not
/// hidden from anybody who goes looking.
fn showinject(what: &str) -> Result<Action, String> {
    if what.is_empty() {
        return Ok(Action::ToggleFolds(
            crate::content::ENVIRONMENTAL_INJECTIONS.to_vec(),
        ));
    }
    // `all` rather than a fourth name: every kind in the table, peers included,
    // because "show me the injections" is a question about the screen and a
    // teammate's report is an injection flatly.
    if what == "all" {
        return Ok(Action::ToggleFolds(
            crate::content::INJECTIONS
                .iter()
                .map(|(_, kind)| *kind)
                .collect(),
        ));
    }
    match crate::content::injected_kind(what) {
        Some(kind) => Ok(Action::ToggleFold(kind)),
        None => Err(format!(
            "没有 `{what}` 这种注入;可以写 {} 或 all",
            crate::content::INJECTIONS
                .iter()
                .map(|(name, _)| *name)
                .collect::<Vec<_>>()
                .join(" / ")
        )),
    }
}

/// The conversation: what is in it, what to do with it, and which one it is.
pub struct SessionCommands;

const SESSION: &[Command] = &[
    Command::new("compact", "压缩历史,给上下文腾地方"),
    Command::new("context", "这次会话用掉了多少"),
    Command::new("transcript", "把对话按模型看到的样子列出来"),
    Command::new("new", "开一个新会话"),
    Command::taking("resume", "[会话 id]", "回到一个存下的会话;不带 id 则挑一个"),
    Command::taking(
        "effort",
        "<low|medium|high|xhigh|max|default>",
        "改这个会话的思考强度(与模型无关)",
    ),
];

/// A host's refusal, in words a person can act on.
fn refusal(error: HostError) -> String {
    match error {
        HostError::Busy { reason } => format!("现在不行:{reason}"),
        HostError::NotFound => "找不到:会话已经换过,或者没有这个会话".into(),
        HostError::SessionInUse { id } => format!("会话 {id} 正在别处用着"),
        HostError::Unavailable => "宿主现在不可用".into(),
        HostError::ProviderUnavailable { reason } => format!("没有可用的模型:{reason:?}"),
        HostError::Failed { message } => message,
        other => format!("{other:?}"),
    }
}

#[async_trait]
impl CommandSet for SessionCommands {
    fn id(&self) -> &'static str {
        "cmd-session"
    }
    fn commands(&self) -> Vec<Command> {
        SESSION.to_vec()
    }
    async fn run(&self, name: &str, args: &str, ctx: &Context) -> Outcome {
        let Some(client) = ctx.service::<crate::plugin::AgentClientSvc>() else {
            return Outcome::Refused("这块屏幕没接上 agent".into());
        };
        let control = client.control();
        match name {
            "compact" => {
                if !client.described().is_some_and(|d| d.compaction) {
                    return Outcome::Refused("这个 agent 没有压缩策略".into());
                }
                // Over the handle, so it waits behind a running turn like every
                // other driver's `/compact`. The outcome comes back as an event
                // and is said then; saying "done" here would be saying it
                // before it is true.
                let focus = args.trim();
                client.compact((!focus.is_empty()).then(|| focus.to_string()));
                Outcome::Quiet
            }
            "context" => {
                let events = client.events();
                let turn = events
                    .iter()
                    .filter_map(|logged| match logged.event {
                        SessionEvent::TurnStart { turn } => Some(turn),
                        _ => None,
                    })
                    .max()
                    .unwrap_or(0);
                Outcome::Said(format!(
                    "{turn} 轮 · {} 条模型可见消息 · {} 条事实",
                    derive_messages(&events).len(),
                    events.len()
                ))
            }
            "transcript" => {
                let text = derive_messages(&client.events())
                    .iter()
                    .map(|m| format!("{:?}: {}", m.role, first_line(&m.text)))
                    .collect::<Vec<_>>()
                    .join("\n");
                Outcome::Said(if text.is_empty() {
                    "还没有对话".into()
                } else {
                    text
                })
            }
            // The switch itself is not done here: the host announces the new
            // session, and the screen moves to it on that — the same way it
            // moves when something else replaced the session.
            "new" => {
                let Some(control) = control else {
                    return Outcome::Refused("这块屏幕没接上宿主".into());
                };
                match control
                    .call(HostCommand::NewSession {
                        session: client.session(),
                    })
                    .await
                {
                    Ok(_) => Outcome::Quiet,
                    Err(error) => Outcome::Refused(refusal(error)),
                }
            }
            "resume" => {
                let Some(control) = control else {
                    return Outcome::Refused("这块屏幕没接上宿主".into());
                };
                let target = args.trim();
                if target.is_empty() {
                    let working_dir = std::env::current_dir()
                        .ok()
                        .map(|dir| dir.display().to_string());
                    return match control
                        .call(HostCommand::ListSessions { working_dir })
                        .await
                    {
                        Ok(HostReply::Sessions { sessions }) => {
                            let live = client.session();
                            let choices: Vec<crate::overlay::Choice> = sessions
                                .into_iter()
                                .filter(|stored| stored.id != live)
                                .map(|stored| {
                                    crate::overlay::Choice::new(
                                        format!("/resume {}", stored.id),
                                        stored.title.clone().unwrap_or_else(|| stored.id.clone()),
                                    )
                                    .about(format!("{} 轮 · {}", stored.turns, stored.id))
                                })
                                .collect();
                            if choices.is_empty() {
                                Outcome::Said("没有别的存下的会话".into())
                            } else {
                                Outcome::Open(crate::overlay::Picker::new(
                                    "resume",
                                    "回到哪个会话 · enter 打开",
                                    choices,
                                ))
                            }
                        }
                        Ok(other) => Outcome::Refused(format!("{other:?}")),
                        Err(error) => Outcome::Refused(refusal(error)),
                    };
                }
                match control
                    .call(HostCommand::Resume {
                        session: client.session(),
                        target: target.to_string(),
                    })
                    .await
                {
                    Ok(_) => Outcome::Quiet,
                    Err(error) => Outcome::Refused(refusal(error)),
                }
            }
            // A level is the session's, not the model route's, so this survives
            // a model switch. Whether a route can reason at all is the model's.
            "effort" => {
                let wanted = args.trim();
                // One vocabulary, taken from the place that defines it, so this
                // command cannot offer a level nothing parses.
                let levels = atomcode_harness::REASONING_EFFORT_LEVELS;
                if wanted.is_empty() {
                    let current = client
                        .described()
                        .and_then(|d| d.reasoning_effort)
                        .map(|level| level.as_str().to_string())
                        .unwrap_or_else(|| "端点默认".into());
                    return Outcome::Said(format!(
                        "当前思考强度:{current}\n可选:{}, default",
                        levels.join(", ")
                    ));
                }
                let level = if wanted == "default" {
                    None
                } else if levels.contains(&wanted) {
                    ReasoningEffort::from_config(Some(wanted))
                } else {
                    return Outcome::Refused(format!(
                        "未知强度 `{wanted}`;可选:{}, default",
                        levels.join(", ")
                    ));
                };
                let Some(control) = control else {
                    return Outcome::Refused("这块屏幕没接上宿主".into());
                };
                match control
                    .call(HostCommand::SetReasoningEffort {
                        session: client.session(),
                        level,
                    })
                    .await
                {
                    Ok(_) => {
                        client.chose_effort(level);
                        Outcome::Said(format!("思考强度 → {wanted}"))
                    }
                    Err(error) => Outcome::Refused(refusal(error)),
                }
            }
            _ => Outcome::Quiet,
        }
    }
}

/// `/help`, which has to know about everything, so it holds the registry.
pub struct HelpCommands {
    pub all: Arc<Commands>,
}

const HELP: &[Command] = &[Command::new("help", "列出所有命令")];

#[async_trait]
impl CommandSet for HelpCommands {
    fn id(&self) -> &'static str {
        "cmd-help"
    }
    fn commands(&self) -> Vec<Command> {
        HELP.to_vec()
    }
    async fn run(&self, _name: &str, _args: &str, _ctx: &Context) -> Outcome {
        let width = self
            .all
            .all()
            .iter()
            .map(|c| c.name.len() + c.takes.map(|t| t.len() + 1).unwrap_or(0))
            .max()
            .unwrap_or(8);
        Outcome::Said(
            self.all
                .all()
                .iter()
                .map(|c| {
                    let head = match c.takes {
                        Some(t) => format!("/{} {t}", c.name),
                        None => format!("/{}", c.name),
                    };
                    format!("{head:<w$}  {}", c.about, w = width + 2)
                })
                .collect::<Vec<_>>()
                .join("\n"),
        )
    }
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or("")
}

// `builtin()` used to live here and mount all five sets at once. It is gone on
// purpose: with each set a row, a function that mounted "the usual five" would
// be a second answer to "what commands does a screen have", and the second
// answer is the one that goes stale. See `crate::rows::SCREEN`.

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_plexus::{App, ConfigTree, PluginRegistry};

    fn bare() -> App {
        App::new(PluginRegistry::new(), ConfigTree::default())
    }

    /// The five shipped sets, assembled directly.
    ///
    /// Whether these are the sets a real screen gets is not this test's job any
    /// more — `crate::rows::SCREEN` decides that, and `rows`' own tests check
    /// that every row it names exists. What is tested here is the property that
    /// survives either way: the shipped sets do not collide, and `/help`
    /// renders them.
    fn builtin_for_test() -> Arc<Commands> {
        let c = Arc::new(Commands::new());
        let _ = c.add(Arc::new(ScreenCommands));
        let _ = c.add(Arc::new(SessionCommands));
        let _ = c.add(Arc::new(HelpCommands { all: c.clone() }));
        c
    }

    #[test]
    fn the_shipped_set_mounts_without_conflicting_with_itself() {
        let c = builtin_for_test();
        let names: Vec<_> = c.all().iter().map(|x| x.name).collect();
        assert!(names.contains(&"help") && names.contains(&"compact") && names.contains(&"effort"));
        let mut sorted = names.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len(), "no duplicates: {names:?}");
    }

    #[tokio::test]
    async fn help_lists_everything_including_itself() {
        let c = builtin_for_test();
        let app = bare();
        match c.dispatch("/help", &app.context()).await {
            Outcome::Said(text) => {
                assert!(text.contains("/help"));
                assert!(
                    text.contains("/resume [会话 id]"),
                    "argument hints show:\n{text}"
                );
                assert_eq!(text.lines().count(), c.all().len());
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn a_command_whose_seam_is_missing_says_so_instead_of_panicking() {
        let c = builtin_for_test();
        let app = bare(); // no session, no control, no tools
        for line in ["/compact", "/context", "/new", "/resume", "/effort high"] {
            match c.dispatch(line, &app.context()).await {
                Outcome::Refused(m) => assert!(!m.is_empty(), "{line} refused with nothing"),
                other => panic!("{line} should refuse, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn screen_commands_become_actions_so_a_key_and_a_command_share_one_path() {
        let c = builtin_for_test();
        let app = bare();
        assert_eq!(
            c.dispatch("/quit", &app.context()).await,
            Outcome::Do(Action::Quit)
        );
        assert_eq!(
            c.dispatch("/reasoning", &app.context()).await,
            Outcome::Do(Action::ToggleFold("reasoning"))
        );
    }

    #[tokio::test]
    async fn showinject_names_one_injection_the_group_or_all_of_them() {
        let c = builtin_for_test();
        let app = bare();

        // Bare: the group, which is the gesture a person forms an opinion about.
        assert_eq!(
            c.dispatch("/showinject", &app.context()).await,
            Outcome::Do(Action::ToggleFolds(
                crate::content::ENVIRONMENTAL_INJECTIONS.to_vec()
            ))
        );
        // One, by the name that appears in the menu and by the kind that appears
        // in a fold state. Both spellings, because people name what they see.
        assert_eq!(
            c.dispatch("/showinject reminder", &app.context()).await,
            Outcome::Do(Action::ToggleFold("injected:reminder"))
        );
        assert_eq!(
            c.dispatch("/showinject injected:reminder", &app.context())
                .await,
            Outcome::Do(Action::ToggleFold("injected:reminder"))
        );
        // The injection a person may want most, since it is the one that is off
        // the screen and also the one carrying someone else's words.
        assert_eq!(
            c.dispatch("/showinject peer", &app.context()).await,
            Outcome::Do(Action::ToggleFold("injected:peer"))
        );
        assert_eq!(
            c.dispatch("/showinject all", &app.context()).await,
            Outcome::Do(Action::ToggleFolds(
                crate::content::INJECTIONS.iter().map(|(_, k)| *k).collect()
            ))
        );

        // Case is not the person's problem: the word is lower-cased before it is
        // looked up, so what arrives from a menu or a paste lands the same way.
        assert_eq!(
            c.dispatch("/showinject REMINDER", &app.context()).await,
            Outcome::Do(Action::ToggleFold("injected:reminder"))
        );

        // And a refusal names what would have worked. A silent no-op here looks
        // exactly like the injection not being there.
        match c.dispatch("/showinject nonsense", &app.context()).await {
            Outcome::Refused(why) => {
                assert!(why.contains("reminder"), "{why}");
                assert!(why.contains("all"), "{why}");
            }
            other => panic!("a name nobody has should be refused, not {other:?}"),
        }
    }
}
