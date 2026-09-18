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
    Command::new("config", "拉出设置面板:搜索、改值;esc 关"),
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
            "config" => Outcome::Do(Action::ToggleSettings),
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
    Command::taking("model", "<模型 id>", "这个会话从现在起用哪个模型"),
    Command::taking(
        "mcp",
        "[withdraw]",
        "MCP 服务器的状态;withdraw 立刻撤下全部 MCP 工具",
    ),
    Command::new("reload", "重新读取 skills、MCP 与配置,会话不变"),
    Command::new("logout", "把凭据拿出进程;会话留着"),
    Command::new("login", "用现在配置的凭据重新登录"),
];

/// A host's refusal, in words a person can act on.
///
/// `pub(crate)` because a host command is not only a command's business: the
/// settings seam hands one to the runtime after writing a file, and its failure
/// has to read the same as every other host failure. One renderer, so one error
/// does not get two wordings depending on which path it came back along.
pub(crate) fn refusal(error: HostError) -> String {
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
                // With nothing after it, the command asks rather than reports:
                // the levels are a closed set this command already knows, so the
                // answer is a list to pick from, and picking one dispatches the
                // command it stands for. A pick is expressed as a command, so
                // this and a typed `/effort high` reach one implementation.
                if wanted.is_empty() {
                    let current = client
                        .described()
                        .and_then(|d| d.reasoning_effort)
                        .map(|level| level.as_str().to_string());
                    let mut choices: Vec<crate::overlay::Choice> = levels
                        .iter()
                        .map(|level| {
                            crate::overlay::Choice::new(
                                format!("/effort {level}"),
                                (*level).to_string(),
                            )
                            .about("这个会话的思考强度")
                            .marked(current.as_deref() == Some(*level))
                        })
                        .collect();
                    choices.push(
                        crate::overlay::Choice::new("/effort default", "default")
                            .about("交给端点决定")
                            .marked(current.is_none()),
                    );
                    let title = match &current {
                        Some(level) => format!("思考强度 · 现在 {level} · enter 改"),
                        None => "思考强度 · 现在交给端点 · enter 改".to_string(),
                    };
                    return Outcome::Open(crate::overlay::Picker::new("effort", title, choices));
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
                if wanted.is_empty() {
                    let current = client
                        .described()
                        .and_then(|d| d.model)
                        .unwrap_or_else(|| "(未知)".into());
                    return Outcome::Said(format!("当前模型:{current}"));
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
                    other => {
                        Outcome::Refused(format!("`/mcp {other}` 不认识;可用:/mcp、/mcp withdraw"))
                    }
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
        // `/config` is the settings panel, and it reaches the screen the same
        // way every other screen command does: as an action, so the command and
        // any key bound to it later are one implementation.
        assert_eq!(
            c.dispatch("/config", &app.context()).await,
            Outcome::Do(Action::ToggleSettings)
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
