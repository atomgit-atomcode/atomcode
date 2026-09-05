//! The commands this build ships, grouped by what they are about.
//!
//! Split into sets on purpose: the session commands belong with the session,
//! the tree commands with the tree. Removing a set removes its commands, and a
//! capability that wants a command of its own contributes a set rather than
//! editing anything here.

use std::sync::Arc;

use async_trait::async_trait;
use atomcode_harness::seams::{CompactionSvc, ControlSvc, SessionSvc, ToolsSvc};
use atomcode_plexus::Context;

use crate::command::{Command, CommandSet, Commands, Outcome};
use crate::keymap::Action;

/// Quit, clear, fold — things the screen itself owns.
pub struct ScreenCommands;

const SCREEN: &[Command] = &[
    Command::new("quit", "退出"),
    Command::new("exit", "退出"),
    Command::new("clear", "清空输入行"),
    Command::new("reasoning", "展开或折叠模型的思考"),
    Command::new("tools", "展开或折叠工具调用的结果"),
    Command::new("mascot", "显示或隐藏吉祥物"),
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
    async fn run(&self, name: &str, _args: &str, _ctx: &Context) -> Outcome {
        match name {
            "quit" | "exit" => Outcome::Do(Action::Quit),
            "clear" => Outcome::Do(Action::Clear),
            "reasoning" => Outcome::Do(Action::ToggleFold("reasoning")),
            "tools" => Outcome::Do(Action::ToggleFold("tool_call")),
            "mascot" => Outcome::Do(Action::ToggleModule("mascot")),
            "keys" => Outcome::Said(
                "enter 发送 · esc/ctrl-c 停止 · ctrl-d 退出 · ctrl-u 清空 · ctrl-w 删词\n\
                 ctrl-r 折叠思考 · ctrl-t 折叠工具 · ctrl-n 吉祥物 · pgup/pgdn 滚动"
                    .into(),
            ),
            _ => Outcome::Quiet,
        }
    }
}

/// The conversation: what is in it, and what to do with it.
pub struct SessionCommands;

const SESSION: &[Command] = &[
    Command::new("compact", "压缩历史,给上下文腾地方"),
    Command::new("context", "这次会话用掉了多少"),
    Command::new("transcript", "把对话按模型看到的样子列出来"),
];

#[async_trait]
impl CommandSet for SessionCommands {
    fn id(&self) -> &'static str {
        "cmd-session"
    }
    fn commands(&self) -> Vec<Command> {
        SESSION.to_vec()
    }
    async fn run(&self, name: &str, _args: &str, ctx: &Context) -> Outcome {
        let Some(log) = ctx.service::<SessionSvc>() else {
            return Outcome::Refused("这棵树没挂会话日志".into());
        };
        match name {
            "compact" => {
                let Some(c) = ctx.service::<CompactionSvc>() else {
                    return Outcome::Refused(
                        "这棵树没挂压缩策略;`compaction-tail` 那一行是关的".into(),
                    );
                };
                match c.compact(&log).await {
                    Some(d) => {
                        let turn = log.current_turn();
                        atomcode_harness::session::commit(
                            ctx,
                            &log,
                            atomcode_harness::session::SessionEvent::Compacted {
                                turn,
                                through: d.through,
                                summary: d.summary,
                            },
                        );
                        Outcome::Said("已压缩".into())
                    }
                    // Refused, not failed: nothing was worth compacting and the
                    // history is byte-identical.
                    None => Outcome::Said("暂时没有值得压缩的".into()),
                }
            }
            "context" => {
                let messages = log.derive_messages().len();
                let events = log.len();
                Outcome::Said(format!(
                    "{} 轮 · {messages} 条模型可见消息 · {events} 条事实",
                    log.current_turn()
                ))
            }
            "transcript" => {
                let text = log
                    .derive_messages()
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
            _ => Outcome::Quiet,
        }
    }
}

/// The running tree itself. These exist because the tree is data — so a person
/// can look at it and change it without restarting.
pub struct TreeCommands;

const TREE: &[Command] = &[
    Command::new("rows", "挑一行开关它——运行时换插件"),
    Command::new("rows-list", "把行列出来,不开模态"),
    Command::new("tools-list", "列出模型能用的工具"),
    Command::new("audit", "检查这棵树的组合是否自洽"),
    Command::taking("patch", "<TOML>", "在运行中改一行配置"),
];

/// Hidden: only a pick dispatches to it.
const TREE_HIDDEN: &[Command] = &[Command::taking("row-toggle", "<行> on|off", "")];

#[async_trait]
impl CommandSet for TreeCommands {
    fn id(&self) -> &'static str {
        "cmd-tree"
    }
    fn commands(&self) -> Vec<Command> {
        TREE.to_vec()
    }
    fn hidden(&self) -> Vec<Command> {
        TREE_HIDDEN.to_vec()
    }
    async fn run(&self, name: &str, args: &str, ctx: &Context) -> Outcome {
        if name == "tools-list" {
            let Some(tools) = ctx.service::<ToolsSvc>() else {
                return Outcome::Refused("这棵树没挂工具目录".into());
            };
            let mut names = tools.names();
            names.sort();
            return Outcome::Said(if names.is_empty() {
                "一个工具都没挂".into()
            } else {
                names.join(" ")
            });
        }
        // Check the argument before the seam: "you left out the TOML" is true
        // in every tree and teaches the syntax, while "no control seam here" is
        // about this tree and teaches nothing.
        if name == "patch" && args.is_empty() {
            return Outcome::Refused(
                "/patch 要一段 TOML,例如:/patch [[patch]]\\nid = \"llm\"\\nconfig = { model = \"…\" }"
                    .into(),
            );
        }
        let Some(control) = ctx.service::<ControlSvc>() else {
            return Outcome::Refused("只有启动器能给出这个能力,这棵树里没有".into());
        };
        match name {
            "rows-list" => {
                let rows = control.rows().await;
                Outcome::Said(
                    rows.iter()
                        .map(|(id, plugin, on)| {
                            format!("{} {id:20} {plugin}", if *on { "●" } else { "○" })
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                )
            }
            // The modal that makes runtime reconfiguration a thing a person can
            // do, not just a thing the architecture claims: pick a row, press
            // enter, the tree changes under a running session.
            "rows" => {
                let rows = control.rows().await;
                Outcome::Open(crate::overlay::Picker::new(
                    "rows",
                    "行 · enter 开关",
                    rows.into_iter()
                        .map(|(id, plugin, on)| {
                            crate::overlay::Choice::new(
                                format!("/row-toggle {id} {}", if on { "off" } else { "on" }),
                                id,
                            )
                            .about(plugin)
                            .marked(on)
                        })
                        .collect(),
                ))
            }
            // Not in the menu: it exists so a pick has something to dispatch to,
            // which is how a modal and a command share one implementation.
            "row-toggle" => {
                let mut parts = args.split_whitespace();
                let (Some(id), Some(state)) = (parts.next(), parts.next()) else {
                    return Outcome::Refused("用法:/row-toggle <行> on|off".into());
                };
                let disabled = state == "off";
                let toml = format!("[[patch]]\nid = \"{id}\"\ndisabled = {disabled}\n");
                match control.patch(&toml).await {
                    Ok(what) => Outcome::Said(format!("{id} → {state}\n{what}")),
                    Err(e) => Outcome::Refused(e),
                }
            }
            "audit" => {
                let findings = control.audit().await;
                Outcome::Said(if findings.is_empty() {
                    "组合自洽".into()
                } else {
                    findings.join("\n")
                })
            }
            "patch" => match control.patch(&args.replace("\\n", "\n")).await {
                Ok(what) => Outcome::Said(what),
                Err(e) => Outcome::Refused(e),
            },
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

/// The registry a shipped tree starts with.
pub fn builtin() -> Arc<Commands> {
    let c = Arc::new(Commands::new());
    let _ = c.add(Arc::new(ScreenCommands));
    let _ = c.add(Arc::new(SessionCommands));
    let _ = c.add(Arc::new(TreeCommands));
    let _ = c.add(Arc::new(HelpCommands { all: c.clone() }));
    c
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_plexus::{App, ConfigTree, PluginRegistry};

    fn bare() -> App {
        App::new(PluginRegistry::new(), ConfigTree::default())
    }

    #[test]
    fn the_shipped_set_mounts_without_conflicting_with_itself() {
        let c = builtin();
        let names: Vec<_> = c.all().iter().map(|x| x.name).collect();
        assert!(names.contains(&"help") && names.contains(&"compact") && names.contains(&"rows"));
        let mut sorted = names.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len(), "no duplicates: {names:?}");
    }

    #[tokio::test]
    async fn help_lists_everything_including_itself() {
        let c = builtin();
        let app = bare();
        match c.dispatch("/help", &app.context()).await {
            Outcome::Said(text) => {
                assert!(text.contains("/help"));
                assert!(
                    text.contains("/patch <TOML>"),
                    "argument hints show:\n{text}"
                );
                assert_eq!(text.lines().count(), c.all().len());
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn a_command_whose_seam_is_missing_says_so_instead_of_panicking() {
        let c = builtin();
        let app = bare(); // no session, no control, no tools
        for line in ["/compact", "/context", "/rows", "/audit", "/tools-list"] {
            match c.dispatch(line, &app.context()).await {
                Outcome::Refused(m) => assert!(!m.is_empty(), "{line} refused with nothing"),
                other => panic!("{line} should refuse, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn patch_without_an_argument_shows_what_it_wants() {
        let c = builtin();
        let app = bare();
        match c.dispatch("/patch", &app.context()).await {
            Outcome::Refused(m) => assert!(m.contains("TOML"), "{m}"),
            // With no `control` seam it refuses for that reason first, which is
            // also correct — either refusal is a refusal, never a silent no-op.
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn screen_commands_become_actions_so_a_key_and_a_command_share_one_path() {
        let c = builtin();
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
}
