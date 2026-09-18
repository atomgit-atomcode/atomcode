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
use atomcode_kernel::message::Role;
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
    Command::taking(
        "paste",
        "[路径]",
        "把剪贴板(或一个文件)的内容放进输入框;Ctrl+V 被终端或系统拦下时用它",
    ),
];

#[async_trait]
impl CommandSet for ScreenCommands {
    fn id(&self) -> &'static str {
        "cmd-screen"
    }
    fn commands(&self) -> Vec<Command> {
        SCREEN.to_vec()
    }
    async fn run(&self, name: &str, args: &str, ctx: &Context) -> Outcome {
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
            // A typed way in to the thing ctrl-v does, because ctrl-v does not
            // always arrive: Windows terminals hand the paste to the key layer
            // as a keystroke, and some platforms have no clipboard this process
            // can read at all. With a path it does not need one.
            "paste" => match args.trim() {
                "" => {
                    let Some(surface) = ctx.service::<crate::plugin::SurfaceSvc>() else {
                        return Outcome::Refused("这块屏幕没有剪贴板".into());
                    };
                    match surface.clipboard_text() {
                        Some(text) if !text.is_empty() => Outcome::Do(Action::Paste(text)),
                        // Not an error. "There is nothing in it" is how a person
                        // finds out there is nothing in it.
                        _ => {
                            Outcome::Refused("剪贴板里没有文字;`/paste 路径` 可以贴一个文件".into())
                        }
                    }
                }
                path => match std::fs::read_to_string(path) {
                    Ok(text) if text.is_empty() => Outcome::Refused(format!("{path} 是空的")),
                    Ok(text) => Outcome::Do(Action::Paste(text)),
                    Err(error) => Outcome::Refused(format!("读不了 {path}:{error}")),
                },
            },
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

/// Taking the conversation out of the terminal: onto the clipboard, onto disk.
///
/// Its own set rather than two more arms in [`SessionCommands`], because it is
/// the one part of the command surface a downstream build is most likely to
/// have an opinion about — a house that saves to its own wiki drops this row
/// and mounts its own, or keeps it and overrides `save` alone
/// ([`CommandSet::overrides`]).
pub struct TakeAwayCommands;

const TAKE_AWAY: &[Command] = &[
    Command::taking(
        "copy",
        "[N|all]",
        "复制模型最后一条回复里的代码块;N 指定第几块,all 全要",
    ),
    Command::taking("save", "[文件名]", "把这段对话存成 markdown"),
];

#[async_trait]
impl CommandSet for TakeAwayCommands {
    fn id(&self) -> &'static str {
        "cmd-take-away"
    }
    fn commands(&self) -> Vec<Command> {
        TAKE_AWAY.to_vec()
    }
    async fn run(&self, name: &str, args: &str, ctx: &Context) -> Outcome {
        let Some(client) = ctx.service::<crate::plugin::AgentClientSvc>() else {
            return Outcome::Refused("这块屏幕没接上 agent".into());
        };
        match name {
            // Copying a code block is the one thing people do with an answer
            // that the answer itself cannot do: the model wrote it to be run,
            // and dragging across a wrapped terminal is how it ends up with
            // line numbers and gutters in it.
            "copy" => {
                let blocks = code_blocks(&last_answer(&client.events()));
                if blocks.is_empty() {
                    return Outcome::Refused("最后一条回复里没有代码块".into());
                }
                let text = match args.trim() {
                    "" if blocks.len() == 1 => blocks[0].clone(),
                    "" => {
                        return Outcome::Refused(format!(
                            "有 {} 块;`/copy N` 指定哪一块,`/copy all` 全要",
                            blocks.len()
                        ))
                    }
                    "all" => blocks.join("\n\n"),
                    n => match n.parse::<usize>().ok().filter(|n| *n >= 1) {
                        Some(n) if n <= blocks.len() => blocks[n - 1].clone(),
                        _ => {
                            return Outcome::Refused(format!(
                                "只有 {} 块,没有第 {n} 块",
                                blocks.len()
                            ))
                        }
                    },
                };
                let Some(surface) = ctx.service::<crate::plugin::SurfaceSvc>() else {
                    return Outcome::Refused("这块屏幕没有剪贴板".into());
                };
                let lines = text.lines().count();
                surface.copy(&text);
                Outcome::Said(format!("复制了 {lines} 行"))
            }
            // Markdown rather than the screen's own rendering: what is saved is
            // read elsewhere — in an editor, in a review, in an issue — and the
            // gutters and the fold marks belong to this screen.
            "save" => {
                let text = as_markdown(&client.events());
                if text.trim().is_empty() {
                    return Outcome::Refused("这段对话还没有内容可存".into());
                }
                let name = match args.trim() {
                    "" => format!("atomcode-{}.md", client.session().replace('/', "-")),
                    given => given.to_string(),
                };
                // Relative to where the session is working, not to wherever the
                // process happened to be started: a person saying `/save` means
                // "beside the code I am looking at".
                let path = std::path::Path::new(&name);
                let path = if path.is_absolute() {
                    path.to_path_buf()
                } else {
                    std::path::Path::new(&client.root()).join(path)
                };
                match std::fs::write(&path, text) {
                    Ok(()) => Outcome::Said(format!("存到 {}", path.display())),
                    Err(error) => Outcome::Refused(format!("存不下:{error}")),
                }
            }
            _ => Outcome::Quiet,
        }
    }
}

/// The conversation: what is in it, what to do with it, and which one it is.
pub struct SessionCommands;

const SESSION: &[Command] = &[
    Command::new("compact", "压缩历史,给上下文腾地方"),
    Command::new(
        "cancel-all",
        "停下这个会话与每个团队成员正在跑的回合;成员留在团队里",
    ),
    Command::new("context", "这次会话用掉了多少"),
    Command::new("transcript", "把对话按模型看到的样子列出来"),
    Command::new("new", "开一个新会话"),
    Command::taking("resume", "[会话 id]", "回到一个存下的会话;不带 id 则挑一个"),
    Command::taking(
        "effort",
        "<low|medium|high|xhigh|max|default>",
        "改这个会话的思考强度(与模型无关)",
    ),
    Command::taking(
        "undo",
        "[回合]",
        "撤回最后一句话(或某一回合)及其后的一切,那句话放回输入框",
    ),
    Command::taking(
        "rewind",
        "[回合 [对话|代码|全部]]",
        "回到某一回合之前:对话、工作区或两者;不带参数则挑一个",
    ),
    Command::taking(
        "model",
        "[模型 id]",
        "这个会话从现在起用哪个模型;不带 id 则挑一个",
    ),
    Command::taking("rename", "<名字>", "给这个会话改个名字"),
    Command::taking(
        "mode",
        "[plan|ask|edits|auto]",
        "改要不要问:plan 只看不动、ask 动手前问、edits 改文件不问、auto 全不问;不带参数则说现在是哪个",
    ),
    Command::taking("cd", "<目录>", "换到另一个目录干活;会开一条新会话"),
    Command::taking(
        "config",
        "[项 值]",
        "看设置;带上项和值就改它。改的是配置文件,不是运行中的行",
    ),
    Command::taking(
        "mcp",
        "[tools <服务器>|withdraw]",
        "MCP 服务器的状态;tools 列某个服务器挂上来的工具;withdraw 立刻撤下全部 MCP 工具",
    ),
    Command::new("reload", "重新读取 skills、MCP 与配置,会话不变"),
    Command::new("logout", "把凭据拿出进程;会话留着"),
    Command::new("login", "用现在配置的凭据重新登录"),
    Command::new("whoami", "现在是谁登录着"),
    Command::taking(
        "think",
        "[on|off]",
        "要不要思考(与 /effort「思考多狠」是两个旋钮);不带参数则说现在是哪个",
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
        // What host control acts on is the session this screen follows, whoever
        // is on screen.
        let root = client.root();
        let host = |control: Option<std::sync::Arc<dyn atomcode_kernel::host::HostControl>>| {
            control.ok_or_else(|| Outcome::Refused("这块屏幕没接上宿主".into()))
        };
        match name {
            "cancel-all" => {
                let members = client.cancel_all();
                Outcome::Said(if members == 0 {
                    "已停下当前回合".into()
                } else {
                    format!("已停下当前回合,以及 {members} 个成员的")
                })
            }
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
                        session: root.clone(),
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
                                    .about(
                                        if stored.needs_newer_version {
                                            format!("需要更新版本才能打开 · {}", stored.id)
                                        } else {
                                            format!("{} 轮 · {}", stored.turns, stored.id)
                                        },
                                    )
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
                        session: root.clone(),
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
                        session: root.clone(),
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
            // The conversation goes back; the words the person said go back to
            // where they type, to change and send again (`docs/adr/0024` §17).
            "undo" | "rewind" => {
                let control = match host(control) {
                    Ok(control) => control,
                    Err(refused) => return refused,
                };
                if client.session() != root {
                    return Outcome::Refused("撤销只对主会话:先切回「主」".into());
                }
                let mut words = args.split_whitespace();
                let turn = match words.next().map(str::parse::<u64>) {
                    None => None,
                    Some(Ok(turn)) => Some(turn),
                    Some(Err(_)) => {
                        return Outcome::Refused(format!("`{}` 不是回合号", args.trim()))
                    }
                };
                let based_on = client.root_high();
                let reply = if name == "undo" {
                    control
                        .call(HostCommand::Undo {
                            session: root,
                            turn,
                            based_on,
                        })
                        .await
                } else {
                    let Some(turn) = turn else {
                        return match control
                            .call(HostCommand::RewindPoints { session: root })
                            .await
                        {
                            Ok(HostReply::RewindPoints {
                                points,
                                code_unavailable,
                            }) if !points.is_empty() => {
                                let choices = points
                                    .into_iter()
                                    .map(|point| {
                                        let scope = if point.code && code_unavailable.is_none() {
                                            " 全部"
                                        } else {
                                            ""
                                        };
                                        crate::overlay::Choice::new(
                                            format!("/rewind {}{scope}", point.turn),
                                            point.prompt.clone(),
                                        )
                                        .about(format!(
                                            "回合 {} · {} 个文件改动",
                                            point.turn, point.files
                                        ))
                                    })
                                    .collect();
                                Outcome::Open(crate::overlay::Picker::new(
                                    "rewind",
                                    "回到哪一回合之前 · enter 回去",
                                    choices,
                                ))
                            }
                            Ok(HostReply::RewindPoints { .. }) => {
                                Outcome::Said("还没有可以回去的回合".into())
                            }
                            Ok(other) => Outcome::Refused(format!("{other:?}")),
                            Err(error) => Outcome::Refused(refusal(error)),
                        };
                    };
                    let scope = match words.next() {
                        None | Some("对话") | Some("conversation") => {
                            atomcode_kernel::session::RewindScope::Conversation
                        }
                        Some("代码") | Some("code") => {
                            atomcode_kernel::session::RewindScope::Code
                        }
                        Some("全部") | Some("both") => {
                            atomcode_kernel::session::RewindScope::Both
                        }
                        Some(other) => {
                            return Outcome::Refused(format!(
                                "`{other}` 不是范围;可选:对话、代码、全部"
                            ))
                        }
                    };
                    control
                        .call(HostCommand::Rewind {
                            session: root,
                            turn,
                            scope,
                            based_on,
                        })
                        .await
                };
                match reply {
                    Ok(HostReply::Undone {
                        prompt: Some(prompt),
                        ..
                    }) => Outcome::Do(Action::Paste(prompt)),
                    Ok(HostReply::Undone { restored_files, .. }) => {
                        Outcome::Said(format!("已还原 {} 个文件", restored_files.len()))
                    }
                    Ok(other) => Outcome::Refused(format!("{other:?}")),
                    Err(error) => Outcome::Refused(refusal(error)),
                }
            }
            "model" => {
                let wanted = args.trim();
                // With no argument: the catalog, to pick from. It used to print
                // the current model and stop, which left the id itself as
                // something a person had to know by heart — the host has the
                // catalog and now says so (`HostCommand::Models`).
                if wanted.is_empty() {
                    let control = match host(control) {
                        Ok(control) => control,
                        Err(refused) => return refused,
                    };
                    return match control.call(HostCommand::Models { session: root }).await {
                        Ok(HostReply::Models { models, current }) if models.is_empty() => {
                            Outcome::Said(match current {
                                Some(current) => format!("当前模型:{current};没有别的可选"),
                                None => "没有配置可选的模型".into(),
                            })
                        }
                        Ok(HostReply::Models { models, current }) => {
                            let choices: Vec<crate::overlay::Choice> = models
                                .into_iter()
                                .map(|model| {
                                    let here = current.as_deref() == Some(model.id.as_str());
                                    crate::overlay::Choice::new(
                                        format!("/model {}", model.id),
                                        model.id.clone(),
                                    )
                                    .about(model.about)
                                    .marked(here)
                                })
                                .collect();
                            Outcome::Open(crate::overlay::Picker::new(
                                "model",
                                "换成哪个模型 · enter 换过去",
                                choices,
                            ))
                        }
                        Ok(other) => Outcome::Refused(format!("{other:?}")),
                        Err(error) => Outcome::Refused(refusal(error)),
                    };
                }
                let control = match host(control) {
                    Ok(control) => control,
                    Err(refused) => return refused,
                };
                match control
                    .call(HostCommand::SwitchModel {
                        session: root,
                        model: wanted.to_string(),
                    })
                    .await
                {
                    Ok(_) => Outcome::Said(format!("模型 → {wanted}")),
                    Err(error) => Outcome::Refused(refusal(error)),
                }
            }
            "mode" => {
                use atomcode_kernel::host::Mode;
                let wanted = match args.trim() {
                    "" => {
                        return Outcome::Said(
                            "plan 只看不动 · ask 动手前问 · edits 改文件不问 · auto 全不问".into(),
                        )
                    }
                    "plan" => Mode::Plan,
                    "ask" => Mode::Ask,
                    "edits" | "accept-edits" => Mode::AcceptEdits,
                    "auto" => Mode::Auto,
                    other => {
                        return Outcome::Refused(format!(
                            "`{other}` 不是一档;可选:plan、ask、edits、auto"
                        ))
                    }
                };
                let control = match host(control) {
                    Ok(control) => control,
                    Err(refused) => return refused,
                };
                match control
                    .call(HostCommand::SetMode {
                        session: root,
                        mode: wanted,
                    })
                    .await
                {
                    Ok(_) => Outcome::Said(format!("现在是 {}", args.trim())),
                    Err(error) => Outcome::Refused(refusal(error)),
                }
            }
            "cd" => {
                let directory = args.trim();
                if directory.is_empty() {
                    return Outcome::Refused("要一个目录:/cd <目录>".into());
                }
                let control = match host(control) {
                    Ok(control) => control,
                    Err(refused) => return refused,
                };
                match control
                    .call(HostCommand::ChangeDirectory {
                        session: root,
                        directory: directory.to_string(),
                    })
                    .await
                {
                    // A new session: what was read and written belongs to where
                    // it ran, so the screen follows the new stream.
                    Ok(HostReply::SessionChanged { session }) => {
                        Outcome::Said(format!("现在在 {directory} 里干活 · 新会话 {session}"))
                    }
                    Ok(_) => Outcome::Said(format!("现在在 {directory} 里干活")),
                    Err(error) => Outcome::Refused(refusal(error)),
                }
            }
            "config" => {
                let control = match host(control) {
                    Ok(control) => control,
                    Err(refused) => return refused,
                };
                let (id, value) = match args.trim().split_once(char::is_whitespace) {
                    Some((id, value)) => (id.trim(), value.trim()),
                    None => (args.trim(), ""),
                };
                // Nothing typed: what there is, and what each is set to.
                if id.is_empty() {
                    return match control.call(HostCommand::Settings { session: root }).await {
                        Ok(HostReply::Settings { settings }) if settings.is_empty() => {
                            Outcome::Said("这个宿主没有可改的设置".into())
                        }
                        Ok(HostReply::Settings { settings }) => Outcome::Said(
                            settings
                                .into_iter()
                                .map(|s| {
                                    format!(
                                        "{} = {}  · {} · {} · {}",
                                        s.id, s.value, s.label, s.accepts, s.applies
                                    )
                                })
                                .collect::<Vec<_>>()
                                .join("\n"),
                        ),
                        Ok(other) => Outcome::Refused(format!("{other:?}")),
                        Err(error) => Outcome::Refused(refusal(error)),
                    };
                }
                if value.is_empty() {
                    return Outcome::Refused(format!("要一个值:/config {id} <值>"));
                }
                match control
                    .call(HostCommand::SetSetting {
                        session: root,
                        id: id.to_string(),
                        value: value.to_string(),
                    })
                    .await
                {
                    Ok(_) => Outcome::Said(format!("{id} = {value}")),
                    Err(error) => Outcome::Refused(refusal(error)),
                }
            }
            "whoami" => {
                let control = match host(control) {
                    Ok(control) => control,
                    Err(refusal) => return refusal,
                };
                match control
                    .call(HostCommand::WhoAmI {
                        session: root.clone(),
                    })
                    .await
                {
                    Ok(HostReply::Identity {
                        signed_in: true,
                        who,
                        detail,
                    }) => {
                        let who = who.unwrap_or_else(|| "登录着,但宿主没说是谁".into());
                        Outcome::Said(match detail {
                            Some(detail) => format!("{who} · {detail}"),
                            None => who,
                        })
                    }
                    Ok(HostReply::Identity { .. }) => {
                        Outcome::Said("没有人登录;这份配置用的是自带的凭据".into())
                    }
                    Ok(other) => Outcome::Refused(format!("宿主答了别的:{other:?}")),
                    Err(error) => Outcome::Refused(refusal(error)),
                }
            }
            // Two knobs, not one: `/effort` is how hard, this is whether at all.
            "think" => {
                let control = match host(control) {
                    Ok(control) => control,
                    Err(refusal) => return refusal,
                };
                let wanted = args.trim().to_ascii_lowercase();
                let on = match wanted.as_str() {
                    "" => {
                        return match control
                            .call(HostCommand::Thinking {
                                session: root.clone(),
                            })
                            .await
                        {
                            Ok(HostReply::Settings { settings }) => match settings.first() {
                                Some(setting) => Outcome::Said(format!(
                                    "思考:{};改用 /think on 或 /think off",
                                    setting.value
                                )),
                                None => Outcome::Refused("这个宿主没有思考开关".into()),
                            },
                            Ok(other) => Outcome::Refused(format!("宿主答了别的:{other:?}")),
                            Err(error) => Outcome::Refused(refusal(error)),
                        }
                    }
                    "on" | "true" => true,
                    "off" | "false" => false,
                    other => {
                        return Outcome::Refused(format!("`{other}` 不是 on 或 off"));
                    }
                };
                match control
                    .call(HostCommand::SetThinking {
                        session: root.clone(),
                        on,
                    })
                    .await
                {
                    Ok(_) => Outcome::Said(format!("思考:{}", if on { "on" } else { "off" })),
                    Err(error) => Outcome::Refused(refusal(error)),
                }
            }
            "rename" => {
                let title = args.trim();
                if title.is_empty() {
                    return Outcome::Refused("要一个名字:/rename <名字>".into());
                }
                let control = match host(control) {
                    Ok(control) => control,
                    Err(refused) => return refused,
                };
                match control
                    .call(HostCommand::Rename {
                        session: root,
                        title: title.to_string(),
                    })
                    .await
                {
                    Ok(_) => Outcome::Said(format!("这个会话现在叫「{title}」")),
                    Err(error) => Outcome::Refused(refusal(error)),
                }
            }
            "mcp" => {
                let control = match host(control) {
                    Ok(control) => control,
                    Err(refused) => return refused,
                };
                match args.trim() {
                    "" => match control.call(HostCommand::McpStatus { session: root }).await {
                        Ok(HostReply::McpServers { servers }) if servers.is_empty() => {
                            Outcome::Said("没有配置 MCP 服务器".into())
                        }
                        Ok(HostReply::McpServers { servers }) => Outcome::Said(
                            servers
                                .into_iter()
                                .map(|server| {
                                    use atomcode_kernel::host::McpServerState as S;
                                    let state = match server.state {
                                        S::Connecting => "连接中".to_string(),
                                        S::Connected => "已连接".to_string(),
                                        S::Untrusted => "未信任项目,未启动".to_string(),
                                        S::Failed { message } => format!("失败:{message}"),
                                        S::Disconnected => "已断开".to_string(),
                                        _ => "未知".to_string(),
                                    };
                                    format!("{} · {state}", server.name)
                                })
                                .collect::<Vec<_>>()
                                .join("\n"),
                        ),
                        Ok(other) => Outcome::Refused(format!("{other:?}")),
                        Err(error) => Outcome::Refused(refusal(error)),
                    },
                    "withdraw" => match control
                        .call(HostCommand::WithdrawMcpTools { session: root })
                        .await
                    {
                        Ok(_) => Outcome::Said("已撤下全部 MCP 工具".into()),
                        Err(error) => Outcome::Refused(refusal(error)),
                    },
                    // `tools <server>`: which tools that server actually put on
                    // the model. The status line says a server is connected;
                    // this says what came of it.
                    rest if rest.starts_with("tools") => {
                        let server = rest.trim_start_matches("tools").trim();
                        if server.is_empty() {
                            return Outcome::Refused("要一个服务器名:/mcp tools <服务器>".into());
                        }
                        match control
                            .call(HostCommand::McpTools {
                                session: root,
                                server: server.to_string(),
                            })
                            .await
                        {
                            Ok(HostReply::McpTools { tools }) if tools.is_empty() => {
                                Outcome::Said(format!("{server} 没有挂上任何工具"))
                            }
                            Ok(HostReply::McpTools { tools }) => Outcome::Said(tools.join("\n")),
                            Ok(other) => Outcome::Refused(format!("{other:?}")),
                            Err(error) => Outcome::Refused(refusal(error)),
                        }
                    }
                    other => Outcome::Refused(format!(
                        "`/mcp {other}` 不认识;可用:/mcp、/mcp tools <服务器>、/mcp withdraw"
                    )),
                }
            }
            "reload" | "logout" | "login" => {
                let control = match host(control) {
                    Ok(control) => control,
                    Err(refused) => return refused,
                };
                let (command, done) = match name {
                    "reload" => (
                        HostCommand::Reload { session: root },
                        "已重新读取 skills、MCP 与配置",
                    ),
                    "logout" => (
                        HostCommand::SignOut { session: root },
                        "已登出;/login 重新登录",
                    ),
                    _ => (HostCommand::SignIn { session: root }, "已登录"),
                };
                match control.call(command).await {
                    Ok(_) => Outcome::Said(done.into()),
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
            .map(|c| c.name.len() + c.takes.as_ref().map(|t| t.len() + 1).unwrap_or(0))
            .max()
            .unwrap_or(8);
        Outcome::Said(
            self.all
                .all()
                .iter()
                .map(|c| {
                    let head = match &c.takes {
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

/// The agent's own commands, as its description lists them (`docs/adr/0021`
/// §10): whatever the rows in its tree registered — stopping a team member, say.
/// Run by name through the connection; what one produced comes back on screen.
///
/// Read when asked rather than copied at mount, so what is listed is what the
/// agent on screen was last described as offering.
pub struct AgentCatalogCommands {
    pub client: Arc<crate::plugin::AgentClient>,
}

#[async_trait]
impl CommandSet for AgentCatalogCommands {
    fn id(&self) -> &'static str {
        "cmd-agent-catalog"
    }
    fn commands(&self) -> Vec<Command> {
        self.client
            .described()
            .map(|d| d.commands)
            .unwrap_or_default()
            .into_iter()
            .map(|c| Command {
                name: c.name.into(),
                about: c.summary.into(),
                takes: c.usage.map(Into::into),
            })
            .collect()
    }
    async fn run(&self, name: &str, args: &str, _ctx: &Context) -> Outcome {
        self.client.invoke(name, args);
        Outcome::Quiet
    }
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or("")
}

/// The last thing the model said, as text. Empty when it has not said anything
/// yet — a session that has only been typed into.
fn last_answer(events: &[atomcode_kernel::session::LoggedEvent]) -> String {
    derive_messages(events)
        .into_iter()
        .rfind(|m| m.role == Role::Assistant)
        .map(|m| m.text)
        .unwrap_or_default()
}

/// The fenced code blocks in `text`, in the order they appear, without their
/// fences. An unclosed fence still counts: a model that stopped mid-block wrote
/// the part a person wants to run, and refusing to copy it because the closing
/// line never arrived is the wrong answer.
fn code_blocks(text: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut current: Option<Vec<&str>> = None;
    for line in text.lines() {
        let fence = line.trim_start().starts_with("```");
        match (&mut current, fence) {
            (None, true) => current = Some(Vec::new()),
            (Some(_), true) => {
                let lines = current.take().unwrap_or_default();
                blocks.push(lines.join("\n"));
            }
            (Some(lines), false) => lines.push(line),
            (None, false) => {}
        }
    }
    if let Some(lines) = current {
        blocks.push(lines.join("\n"));
    }
    blocks.retain(|b| !b.trim().is_empty());
    blocks
}

/// The conversation as markdown: who said what, in order, with tool traffic
/// left out. What is saved is read somewhere else — an editor, a review, an
/// issue — so it is the conversation, not this screen's rendering of it.
fn as_markdown(events: &[atomcode_kernel::session::LoggedEvent]) -> String {
    let mut out = String::new();
    for message in derive_messages(events) {
        let who = match message.role {
            Role::User => "## 我",
            Role::Assistant => "## 模型",
            Role::System | Role::Tool => continue,
        };
        if message.text.trim().is_empty() {
            continue;
        }
        out.push_str(who);
        out.push_str("\n\n");
        out.push_str(message.text.trim_end());
        out.push_str("\n\n");
    }
    out
}

// `builtin()` used to live here and mount all five sets at once. It is gone on
// purpose: with each set a row, a function that mounted "the usual five" would
// be a second answer to "what commands does a screen have", and the second
// answer is the one that goes stale. See `crate::rows::SCREEN`.

#[cfg(test)]
mod tests {
    use super::*;

    /// A host that answers from a script and keeps what it was asked.
    #[derive(Default)]
    struct Recording {
        asked: std::sync::Mutex<Vec<HostCommand>>,
        replies: std::sync::Mutex<std::collections::VecDeque<Result<HostReply, HostError>>>,
    }

    #[async_trait]
    impl atomcode_kernel::host::HostControl for Recording {
        async fn call(&self, command: HostCommand) -> Result<HostReply, HostError> {
            self.asked.lock().unwrap().push(command);
            self.replies
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Ok(HostReply::Done))
        }
        fn subscribe(
            &self,
        ) -> tokio::sync::mpsc::UnboundedReceiver<atomcode_kernel::host::HostEvent> {
            tokio::sync::mpsc::unbounded_channel().1
        }
    }

    /// A screen following session `lead`, whose last fact it saw is number 7.
    fn following(host: &Arc<Recording>) -> (App, Arc<crate::plugin::AgentClient>, Arc<Commands>) {
        let app = bare();
        let client = Arc::new(crate::plugin::AgentClient::default());
        let (commands, _agent) = tokio::sync::mpsc::unbounded_channel();
        client.connect(commands, host.clone());
        client.follow("lead");
        client.keep(&atomcode_kernel::session::Committed {
            session: "lead".into(),
            seq: 7,
            at: 0,
            event: SessionEvent::TurnStart { turn: 1 },
        });
        let _ = app
            .context()
            .provide::<crate::plugin::AgentClientSvc>(client.clone());
        let all = Arc::new(Commands::new());
        let _ = all.add(Arc::new(SessionCommands));
        (app, client, all)
    }

    /// `/undo` asks the host about the session this screen follows, based on the
    /// last fact it saw, and puts the words it hands back where the person
    /// types (`docs/adr/0024` §17).
    #[tokio::test]
    async fn undo_is_asked_of_the_host_and_the_words_come_back_to_the_composer() {
        let host = Arc::new(Recording::default());
        host.replies
            .lock()
            .unwrap()
            .push_back(Ok(HostReply::Undone {
                prompt: Some("fix the parser".into()),
                restored_files: Vec::new(),
            }));
        let (app, client, all) = following(&host);
        assert_eq!(
            all.dispatch("/undo", &app.context()).await,
            Outcome::Do(Action::Paste("fix the parser".into()))
        );
        let _ = all.dispatch("/undo 3", &app.context()).await;
        assert_eq!(
            *host.asked.lock().unwrap(),
            vec![
                HostCommand::Undo {
                    session: "lead".into(),
                    turn: None,
                    based_on: 7,
                },
                HostCommand::Undo {
                    session: "lead".into(),
                    turn: Some(3),
                    based_on: 7,
                },
            ]
        );

        // With a member on screen, the lead's conversation is not what is shown.
        let _ = client.look_at("lead/scout");
        assert!(matches!(
            all.dispatch("/undo", &app.context()).await,
            Outcome::Refused(_)
        ));
        assert_eq!(
            host.asked.lock().unwrap().len(),
            2,
            "nothing more was asked"
        );
    }

    /// `/rewind` with nothing after it offers the points to pick from; with a
    /// turn and a scope it goes back.
    #[tokio::test]
    async fn rewind_offers_the_points_and_goes_back_with_a_scope() {
        let host = Arc::new(Recording::default());
        host.replies
            .lock()
            .unwrap()
            .push_back(Ok(HostReply::RewindPoints {
                points: vec![atomcode_kernel::host::RewindPoint {
                    turn: 2,
                    prompt: "two".into(),
                    files: 1,
                    code: true,
                }],
                code_unavailable: None,
            }));
        host.replies
            .lock()
            .unwrap()
            .push_back(Ok(HostReply::Undone {
                prompt: None,
                restored_files: vec!["src/a.rs".into()],
            }));
        let (app, _client, all) = following(&host);
        match all.dispatch("/rewind", &app.context()).await {
            Outcome::Open(picker) => assert_eq!(picker.id(), "rewind"),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            all.dispatch("/rewind 2 代码", &app.context()).await,
            Outcome::Said("已还原 1 个文件".into())
        );
        assert_eq!(
            host.asked.lock().unwrap().last(),
            Some(&HostCommand::Rewind {
                session: "lead".into(),
                turn: 2,
                scope: atomcode_kernel::session::RewindScope::Code,
                based_on: 7,
            })
        );
    }

    /// The rest of host control over a session is a command each, for the
    /// session this screen follows even while a member is on screen.
    #[tokio::test]
    async fn model_mcp_reload_and_signing_in_and_out_are_asked_of_the_host() {
        let host = Arc::new(Recording::default());
        let (app, client, all) = following(&host);
        host.replies.lock().unwrap().extend([
            Ok(HostReply::Done),
            Ok(HostReply::McpServers {
                servers: Vec::new(),
            }),
        ]);
        let _ = client.look_at("lead/scout");
        for line in [
            "/model glm-5",
            "/mcp",
            "/mcp withdraw",
            "/reload",
            "/logout",
            "/login",
        ] {
            assert!(
                !matches!(
                    all.dispatch(line, &app.context()).await,
                    Outcome::Refused(_)
                ),
                "{line}"
            );
        }
        let lead = || "lead".to_string();
        assert_eq!(
            *host.asked.lock().unwrap(),
            vec![
                HostCommand::SwitchModel {
                    session: lead(),
                    model: "glm-5".into(),
                },
                HostCommand::McpStatus { session: lead() },
                HostCommand::WithdrawMcpTools { session: lead() },
                HostCommand::Reload { session: lead() },
                HostCommand::SignOut { session: lead() },
                HostCommand::SignIn { session: lead() },
            ]
        );
    }
    /// Who is signed in, and the other thinking knob.
    ///
    /// `/think` is not `/effort`: one says whether the model thinks at all, the
    /// other how hard. Both are asked of the host against the session this
    /// screen follows.
    #[tokio::test]
    async fn who_is_signed_in_and_whether_the_model_thinks_at_all() {
        let host = Arc::new(Recording::default());
        host.replies.lock().unwrap().extend([
            Ok(HostReply::Identity {
                signed_in: true,
                who: Some("lichao".into()),
                detail: Some("li@example.com".into()),
            }),
            Ok(HostReply::Identity {
                signed_in: false,
                who: None,
                detail: None,
            }),
            Ok(HostReply::Settings {
                settings: vec![atomcode_kernel::host::Setting {
                    id: "thinking".into(),
                    label: "思考".into(),
                    value: "off".into(),
                    accepts: "on | off".into(),
                    applies: "下一回合".into(),
                }],
            }),
        ]);
        let (app, _client, all) = following(&host);
        assert_eq!(
            all.dispatch("/whoami", &app.context()).await,
            Outcome::Said("lichao · li@example.com".into())
        );
        // Nobody signed in is an answer, not a refusal.
        match all.dispatch("/whoami", &app.context()).await {
            Outcome::Said(text) => assert!(text.contains("没有人登录"), "{text}"),
            other => panic!("{other:?}"),
        }
        match all.dispatch("/think", &app.context()).await {
            Outcome::Said(text) => assert!(text.contains("off"), "{text}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            all.dispatch("/think on", &app.context()).await,
            Outcome::Said("思考:on".into())
        );
        assert!(matches!(
            all.dispatch("/think 一点点", &app.context()).await,
            Outcome::Refused(_)
        ));
        let lead = || "lead".to_string();
        assert_eq!(
            *host.asked.lock().unwrap(),
            vec![
                HostCommand::WhoAmI { session: lead() },
                HostCommand::WhoAmI { session: lead() },
                HostCommand::Thinking { session: lead() },
                HostCommand::SetThinking {
                    session: lead(),
                    on: true,
                },
            ],
            "the refused one asked nothing"
        );
    }

    /// A screen following a session the model has answered in, with `answer` as
    /// its last reply.
    fn answered(answer: &str) -> (App, Arc<Commands>, Arc<crate::surface::Headless>) {
        let app = bare();
        let client = Arc::new(crate::plugin::AgentClient::default());
        let (commands, _agent) = tokio::sync::mpsc::unbounded_channel();
        client.connect(commands, Arc::new(Recording::default()));
        client.follow("lead");
        for (seq, event) in [
            SessionEvent::UserMessage {
                turn: 1,
                text: "写个 hello".into(),
                images: Vec::new(),
            },
            SessionEvent::AssistantMessage {
                turn: 1,
                round: 1,
                text: answer.into(),
                reasoning: String::new(),
                tool_calls: Vec::new(),
                reasoning_blocks: Vec::new(),
                meta: None,
            },
        ]
        .into_iter()
        .enumerate()
        {
            client.keep(&atomcode_kernel::session::Committed {
                session: "lead".into(),
                seq: seq as u64 + 1,
                at: 0,
                event,
            });
        }
        let surface = crate::surface::Headless::new(80, 24);
        let ctx = app.context();
        let _ = ctx.provide::<crate::plugin::AgentClientSvc>(client);
        let _ = ctx.provide::<crate::plugin::SurfaceSvc>(surface.clone());
        let all = Arc::new(Commands::new());
        let _ = all.add(Arc::new(TakeAwayCommands));
        (app, all, surface)
    }

    /// `/copy` takes the code out of the last answer and nothing else — not the
    /// prose around it, not the fences. With more than one block it asks which,
    /// rather than guessing.
    #[tokio::test]
    async fn copy_takes_the_code_out_of_the_last_answer() {
        let (app, all, surface) =
            answered("这样写:\n\n```rust\nfn main() {}\n```\n\n或者:\n\n```sh\necho hi\n```\n");
        match all.dispatch("/copy", &app.context()).await {
            Outcome::Refused(why) => assert!(why.contains("2"), "{why}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(surface.clipboard_text(), None, "nothing was copied yet");
        let _ = all.dispatch("/copy 2", &app.context()).await;
        assert_eq!(surface.clipboard_text(), Some("echo hi".into()));
        let _ = all.dispatch("/copy all", &app.context()).await;
        assert_eq!(
            surface.clipboard_text(),
            Some("fn main() {}\n\necho hi".into())
        );
        assert!(matches!(
            all.dispatch("/copy 9", &app.context()).await,
            Outcome::Refused(_)
        ));

        // An answer with no code in it says so rather than copying the prose.
        let (app, all, surface) = answered("没有代码,就这么说说");
        assert!(matches!(
            all.dispatch("/copy", &app.context()).await,
            Outcome::Refused(_)
        ));
        assert_eq!(surface.clipboard_text(), None);
    }

    /// `/save` writes the conversation as markdown, beside the code the session
    /// is working on.
    #[tokio::test]
    async fn save_writes_the_conversation_as_markdown() {
        let (app, all, _surface) = answered("写好了");
        let dir = tempfile::tempdir().expect("tempdir");
        let into = dir.path().join("聊天.md");
        match all
            .dispatch(&format!("/save {}", into.display()), &app.context())
            .await
        {
            Outcome::Said(text) => assert!(text.contains("聊天.md"), "{text}"),
            other => panic!("{other:?}"),
        }
        let written = std::fs::read_to_string(&into).expect("written");
        assert!(
            written.contains("## 我") && written.contains("写个 hello"),
            "{written}"
        );
        assert!(
            written.contains("## 模型") && written.contains("写好了"),
            "{written}"
        );
    }

    /// `/paste` is the typed way in to what ctrl-v does, for the terminals and
    /// the platforms where ctrl-v never arrives. With a path it does not need a
    /// clipboard at all.
    #[tokio::test]
    async fn paste_reaches_the_composer_from_the_clipboard_or_from_a_file() {
        let app = bare();
        let surface = crate::surface::Headless::new(80, 24);
        let _ = app
            .context()
            .provide::<crate::plugin::SurfaceSvc>(surface.clone());
        let all = Arc::new(Commands::new());
        let _ = all.add(Arc::new(ScreenCommands));

        // Nothing in it is an answer, not a failure — and it says what else to
        // try.
        match all.dispatch("/paste", &app.context()).await {
            Outcome::Refused(why) => assert!(why.contains("路径"), "{why}"),
            other => panic!("{other:?}"),
        }

        {
            use crate::surface::Surface as _;
            surface.copy("从剪贴板来的");
        }
        assert_eq!(
            all.dispatch("/paste", &app.context()).await,
            Outcome::Do(Action::Paste("从剪贴板来的".into()))
        );

        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("note.txt");
        std::fs::write(&file, "从文件来的").expect("write");
        assert_eq!(
            all.dispatch(&format!("/paste {}", file.display()), &app.context())
                .await,
            Outcome::Do(Action::Paste("从文件来的".into()))
        );
        assert!(matches!(
            all.dispatch("/paste /nowhere/at/all", &app.context()).await,
            Outcome::Refused(_)
        ));
    }

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
        let _ = c.add(Arc::new(TakeAwayCommands));
        let _ = c.add(Arc::new(HelpCommands { all: c.clone() }));
        c
    }

    #[test]
    fn the_shipped_set_mounts_without_conflicting_with_itself() {
        let c = builtin_for_test();
        let names: Vec<_> = c.all().iter().map(|x| x.name.to_string()).collect();
        assert!(["help", "compact", "effort"]
            .iter()
            .all(|n| names.contains(&n.to_string())));
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
