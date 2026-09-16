//! The commands this build ships, grouped by what they are about.
//!
//! Split into sets on purpose: the session commands belong with the session,
//! the tree commands with the tree. Removing a set removes its commands, and a
//! capability that wants a command of its own contributes a set rather than
//! editing anything here.

use std::sync::Arc;

use async_trait::async_trait;
use atomcode_harness::seams::{CompactionSvc, ControlSvc, ToolsSvc};
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
    async fn run(&self, name: &str, args: &str, ctx: &Context) -> Outcome {
        // The screen's agent owns the conversation; the tree has no log of
        // its own any more.
        let Some(log) = ctx
            .service::<crate::plugin::AgentClientSvc>()
            .map(|c| c.session())
        else {
            return Outcome::Refused("这块屏幕没接上 agent".into());
        };
        match name {
            "compact" => {
                if ctx.service::<CompactionSvc>().is_none() {
                    return Outcome::Refused(
                        "这棵树没挂压缩策略;`compaction-tail` 那一行是关的".into(),
                    );
                }
                let Some(client) = ctx.service::<crate::plugin::AgentClientSvc>() else {
                    return Outcome::Refused("这块屏幕没接上 agent".into());
                };
                // Over the handle, so it waits behind a running turn like every
                // other driver's `/compact`. The outcome comes back as an event
                // and is said then; saying "done" here would be saying it
                // before it is true.
                let focus = args.trim();
                client.compact((!focus.is_empty()).then(|| focus.to_string()));
                Outcome::Quiet
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
    Command::taking(
        "effort",
        "<low|medium|high|xhigh|max>",
        "运行时改思考强度(与模型无关;开关在 llm 行的 thinking_type)",
    ),
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
            // A level is the session's, not the model route's, so this survives
            // a model switch. The switch itself is NOT here on purpose: whether
            // a route can reason at all is `thinking_type` on the `llm` row.
            "effort" => {
                let wanted = args.trim();
                // One vocabulary, taken from the place that defines it, so this
                // command cannot offer a level nothing parses.
                let levels = atomcode_harness::REASONING_EFFORT_LEVELS;
                let current = control
                    .row_config(atomcode_harness::REASONING_EFFORT_ROW)
                    .await
                    .and_then(|c| c.get("level").and_then(|v| v.as_str()).map(str::to_string));
                if wanted.is_empty() {
                    return Outcome::Said(format!(
                        "当前思考强度:{}\n可选:{}",
                        current.unwrap_or_else(|| "端点默认".into()),
                        levels.join(", ")
                    ));
                }
                if !levels.contains(&wanted) {
                    return Outcome::Refused(format!(
                        "未知强度 `{wanted}`;可选:{}",
                        levels.join(", ")
                    ));
                }
                let toml = format!(
                    "[[patch]]\nid = \"reasoning-effort\"\nconfig = {{ level = {wanted:?} }}\n"
                );
                match control.patch(&toml).await {
                    Ok(what) => Outcome::Said(format!("思考强度 → {wanted}\n{what}")),
                    Err(e) => Outcome::Refused(e),
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
        let _ = c.add(Arc::new(TreeCommands));
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
