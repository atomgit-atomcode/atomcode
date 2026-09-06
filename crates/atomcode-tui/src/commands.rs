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
    async fn run(&self, name: &str, _args: &str, _ctx: &Context) -> Outcome {
        match name {
            "quit" | "exit" => Outcome::Do(Action::Quit),
            "clear" => Outcome::Do(Action::Clear),
            "reasoning" => Outcome::Do(Action::ToggleFold("reasoning")),
            "tools" => Outcome::Do(Action::ToggleFold("tool_call")),
            "mascot" => Outcome::Do(Action::ToggleModule("mascot")),
            "mouse" => Outcome::Do(Action::ToggleMouse),
            "keys" => Outcome::Said(
                "enter 发送 · esc/ctrl-c 停止 · ctrl-d 退出 · ctrl-u 清空 · ctrl-w 删词\n\
                 上/下 在输入里移动游标,到头则翻历史 · pgup/pgdn 与滚轮滚动对话\n\
                 ctrl-r 折叠思考 · ctrl-t 折叠工具 · ctrl-n 吉祥物 · ctrl-l 重画屏幕\n\
                 拖动选中并复制 · esc 取消选中 · 点击工具调用折叠展开\n\
                 ctrl-o 把鼠标交还终端(改用终端自己的框选)"
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

/// The screen's shape, changed while it runs.
///
/// One of the three ways in — the others are a key and the model's
/// `adjust_layout` tool — and all three land on `Layout::apply`, so there is one
/// implementation of each op rather than three.
pub struct LayoutCommands {
    pub layout: Arc<crate::layout::Layout>,
    pub modules: Arc<crate::module::Modules>,
}

const LAYOUT: &[Command] = &[
    Command::new("layout", "挑一个命名布局,或显示/隐藏一个面板"),
    Command::taking(
        "show",
        "<模块> [top|bottom|left|right]",
        "把一个面板放上屏幕",
    ),
    Command::taking("hide", "<模块>", "把一个面板收起来"),
    Command::new("undo-layout", "撤销上一次布局改动"),
];

const LAYOUT_HIDDEN: &[Command] = &[Command::taking("layout-set", "<名字>", "")];

impl LayoutCommands {
    fn known(&self) -> Vec<String> {
        self.modules
            .view_ids()
            .into_iter()
            .map(str::to_string)
            .chain(["mascot".to_string(), "findings".to_string()])
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect()
    }
    fn run_op(&self, op: crate::layout::LayoutOp) -> Outcome {
        match self.layout.apply(&op, &self.known()) {
            Ok(what) => Outcome::Said(what),
            Err(e) => Outcome::Refused(e.to_string()),
        }
    }
}

#[async_trait]
impl CommandSet for LayoutCommands {
    fn id(&self) -> &'static str {
        "cmd-layout"
    }
    fn commands(&self) -> Vec<Command> {
        LAYOUT.to_vec()
    }
    fn hidden(&self) -> Vec<Command> {
        LAYOUT_HIDDEN.to_vec()
    }
    async fn run(&self, name: &str, args: &str, _ctx: &Context) -> Outcome {
        use crate::layout::{LayoutOp, Side};
        match name {
            "layout" => {
                let on = self.layout.tree().modules();
                let mut choices: Vec<crate::overlay::Choice> = crate::layout::presets()
                    .iter()
                    .map(|(n, d)| {
                        crate::overlay::Choice::new(format!("/layout-set {n}"), format!("布局 {n}"))
                            .about(*d)
                    })
                    .collect();
                for m in self.known() {
                    let shown = on.contains(&m);
                    choices.push(
                        crate::overlay::Choice::new(
                            format!("/{} {m}", if shown { "hide" } else { "show" }),
                            m.clone(),
                        )
                        .about(if shown { "在屏幕上" } else { "未显示" })
                        .marked(shown),
                    );
                }
                Outcome::Open(crate::overlay::Picker::new(
                    "layout",
                    "布局 · enter 应用",
                    choices,
                ))
            }
            "layout-set" => self.run_op(LayoutOp::Preset {
                name: args.trim().to_string(),
            }),
            "show" => {
                let mut parts = args.split_whitespace();
                let Some(module) = parts.next() else {
                    return Outcome::Refused("用法:/show <模块> [top|bottom|left|right]".into());
                };
                let side = match parts.next() {
                    Some("top") => Side::Top,
                    Some("left") => Side::Left,
                    Some("right") => Side::Right,
                    _ => Side::Bottom,
                };
                self.run_op(LayoutOp::Show {
                    module: module.to_string(),
                    side,
                    size: parts.next().and_then(|s| s.parse().ok()),
                })
            }
            "hide" => {
                let module = args.trim();
                if module.is_empty() {
                    return Outcome::Refused("用法:/hide <模块>".into());
                }
                self.run_op(LayoutOp::Hide {
                    module: module.to_string(),
                })
            }
            "undo-layout" => self.run_op(LayoutOp::Undo),
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
        let _ = c.add(Arc::new(TreeCommands));
        let _ = c.add(Arc::new(LayoutCommands {
            layout: Arc::new(crate::layout::Layout::new(crate::host::default_layout())),
            modules: Arc::new(crate::module::Modules::new()),
        }));
        let _ = c.add(Arc::new(HelpCommands { all: c.clone() }));
        c
    }

    #[test]
    fn the_shipped_set_mounts_without_conflicting_with_itself() {
        let c = builtin_for_test();
        let names: Vec<_> = c.all().iter().map(|x| x.name).collect();
        assert!(names.contains(&"help") && names.contains(&"compact") && names.contains(&"rows"));
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
        let c = builtin_for_test();
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
        let c = builtin_for_test();
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
}
