// crates/atomcode-tuix/src/event_loop/commands.rs
//
// Slash-command dispatcher. Everything the user can invoke by typing
// `/name` lives here — built-in info commands, modal openers, the cd
// helper, and the blocking OAuth flow that suspends the reader + renderer.
//
// New commands should be:
//   1. Registered in `CommandRegistry::builtin` (crates/.../commands.rs)
//   2. Added as an arm in `execute_slash_command` below
//   3. Any long handler factored to a private helper in this file
//
// Modals open by pushing `Some(Box::new(...))` into `active_modal` — the
// handler arms for `/model`, `/resume`, `/provider` show the pattern.

use std::path::PathBuf;

use super::{bg_runtime, save_and_reload, LoopCtx};
use crate::i18n::{t, Msg};
use crate::modals::{DirPicker, IssueWizard, LanguagePicker, Modal, ModelPicker, ProviderWizard, SessionPicker};
use crate::render::{Renderer, UiLine};
use crate::state::{AgentMode, UiState};
use anyhow::Result;
use atomcode_core::agent::AgentCommand;
use atomcode_core::config::Config;
use atomcode_core::conversation::Conversation;
use atomcode_core::session::{Session, SessionId, SessionManager};

/// Maximum recent project dirs we keep in memory + persist to disk.
const MAX_RECENT_DIRS: usize = 5;

fn foreground_state_from_ui(state: &UiState) -> bg_runtime::RuntimeState {
    if matches!(
        state.phase,
        crate::state::UiPhase::Streaming | crate::state::UiPhase::Approval
    ) {
        bg_runtime::RuntimeState::Running
    } else {
        bg_runtime::RuntimeState::Idle
    }
}

fn render_welcome(renderer: &mut dyn Renderer, ctx: &LoopCtx) {
    let dir_display = crate::platform::collapse_home(&ctx.working_dir.to_string_lossy());
    renderer.render(UiLine::Welcome {
        model: ctx.model_name.clone(),
        working_dir: dir_display,
    });
}

pub(crate) fn bind_telemetry_to_session(ctx: &LoopCtx, session: &Session) {
    if let Ok(uuid) = uuid::Uuid::parse_str(session.id.as_str()) {
        ctx.telemetry.set_session_id(uuid);
    }
    // Mirror the session's persistent id onto the agent so the
    // `x-atomcode-session-id` header tracks the conversation identity —
    // resuming a saved session reuses its original id for gateway prefix-
    // cache affinity, instead of minting a fresh per-process uuid.
    ctx.agent
        .cmd_tx
        .send(AgentCommand::SetSessionId(session.id.as_str().to_string()))
        .ok();
}

/// Scan session messages for a pending tool approval — an
/// `AssistantWithToolCalls` message whose tool calls lack corresponding
/// `ToolResult` entries.  Returns `(display_name, detail)` of the first
/// unpaired tool call, or `None` if all tool calls have results.
fn find_pending_approval(session: &Session) -> Option<(String, String)> {
    use atomcode_core::conversation::message::{MessageContent, Role};
    use crate::event_loop::format_tool_detail;

    // Collect all call_ids that already have a ToolResult.
    let mut answered_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    for m in &session.messages {
        if let (Role::Tool, MessageContent::ToolResult(r)) = (&m.role, &m.content) {
            answered_ids.insert(r.call_id.clone());
        }
    }

    // Walk messages in reverse to find the most recent unpaired tool call.
    for m in session.messages.iter().rev() {
        if let (
            Role::Assistant,
            MessageContent::AssistantWithToolCalls { tool_calls, .. },
        ) = (&m.role, &m.content)
        {
            for tc in tool_calls.iter().rev() {
                if !answered_ids.contains(&tc.id) {
                    let display = super::display_tool_name(&tc.name);
                    let detail = format_tool_detail(&tc.name, &tc.arguments);
                    return Some((display, detail));
                }
            }
        }
    }
    None
}

fn short_task_name(task: &str) -> String {
    let first_line = task.lines().next().unwrap_or(task).trim();
    let mut out: String = first_line.chars().take(80).collect();
    if out.is_empty() {
        out = "background task".to_string();
    }
    out
}

fn spawn_runtime(
    ctx: &mut LoopCtx,
    session: Session,
) -> (
    bg_runtime::RuntimeId,
    atomcode_core::agent::AgentClient,
    Session,
) {
    let runtime_id = ctx.bg_manager.allocate_runtime_id();
    let (client, event_rx) = ctx.runtime_factory.spawn_runtime(Conversation::new());
    bg_runtime::spawn_event_forwarder(runtime_id, event_rx, ctx.runtime_event_tx.clone());
    (runtime_id, client, session)
}

/// Synchronise the current foreground session into `BgRuntimeManager`.
///
/// Mid-turn session state (including conversations where the agent is
/// waiting for tool approval) is already persisted to
/// `ctx.current_session` by `handle_agent_event` when it processes
/// `AgentEvent::ApprovalNeeded` (which carries a snapshot of
/// `conversation.messages`).  So by the time `/bg` runs,
/// `ctx.current_session.messages` should be up-to-date.
fn sync_bg_foreground(ctx: &mut LoopCtx) {
    ctx.bg_manager.set_foreground_runtime(
        ctx.foreground_runtime_id,
        ctx.agent.clone(),
        ctx.current_session.clone(),
    );
}

// Historical note: there was a `const OAUTH_PROVIDER_NAME = "AtomGit"`
// and a `build_oauth_provider` helper here. Both are owned by
// `coding_plan::setup` now — `/login` runs the full CodingPlan
// orchestrator (claim + model list + provider registration), so there
// is no need for a separately maintained hardcoded fallback provider.

/// Maximum length for a session name.
pub const MAX_SESSION_NAME_LEN: usize = 100;

/// Validates a session name and returns an error message if invalid.
/// Returns None if the name is valid.
pub fn validate_session_name(name: &str) -> Option<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Some(t(Msg::SessionNameEmpty).into_owned());
    }
    if trimmed.chars().count() > MAX_SESSION_NAME_LEN {
        return Some(t(Msg::SessionNameTooLong { max: MAX_SESSION_NAME_LEN }).into_owned());
    }
    if trimmed.chars().any(char::is_control) {
        return Some(t(Msg::SessionNameControlChars).into_owned());
    }
    None
}

/// Rename a session after validation, persist it, and return old/new names.
pub fn perform_session_rename(
    session_manager: &SessionManager,
    session_id: &SessionId,
    new_name: &str,
) -> Result<(String, String), String> {
    if let Some(err) = validate_session_name(new_name) {
        return Err(err);
    }
    let new_name = new_name.trim().to_string();
    let session = session_manager
        .load(session_id)
        .map_err(|e| t(Msg::SessionLoadFailed { error: &e.to_string() }).into_owned())?;
    let old_name = session.name.clone();
    let renamed_session = atomcode_core::session::Session {
        name: new_name.clone(),
        updated_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(session.updated_at),
        user_renamed: true,
        ..session
    };
    session_manager
        .save(&renamed_session)
        .map_err(|e| t(Msg::SessionSaveFailed { error: &e.to_string() }).into_owned())?;
    Ok((old_name, new_name))
}

/// Render the "Instruction files:" status block — the same one shown
/// by `/status`, factored out so `/init` can also display it after
/// writing `.atomcode.md` (so users see the new file appear under
/// PROJECT immediately, rather than trusting the success message).
fn render_instruction_status_block(working_dir: &std::path::Path) -> String {
    use atomcode_core::config::instructions::LayeredInstructions;
    let instructions = LayeredInstructions::load(working_dir);
    let mut out = t(Msg::StatusInstructionFilesHeader).into_owned();
    for (level, path) in instructions.status_lines() {
        match path {
            Some(p) => out.push_str(&t(Msg::StatusInstructionPresent {
                path: &p.display().to_string(),
                label: level.label(),
            })),
            None => out.push_str(&t(Msg::StatusInstructionMissing { label: level.label() })),
        }
    }
    out
}

/// 把 TUI 附着到指定的 LiveSession（回放快照 + 启动转发器 + 渲染确认）。
/// 供 `/webui` 自动附着和 `/sync` 手动附着共用，不重复逻辑。
fn attach_live_session(
    ctx: &mut LoopCtx,
    renderer: &mut dyn Renderer,
    session: std::sync::Arc<atomcode_core::live::LiveSession>,
    render_snapshot: bool,
) {
    // 幂等附着：先终止已有的转发器，否则旧任务仍订阅同一广播、同样投进
    // runtime_event_tx，导致每个 LiveEvent 被渲染两次（输入回显、文本增量、
    // 工具调用全部重复）。tokio 里 drop JoinHandle 只会分离任务、不会取消，
    // 所以必须显式 abort 后再 spawn 新转发器。
    if let Some(h) = ctx.sync_forwarder.take() {
        h.abort();
    }
    // 回放快照：渲染既有消息。`/webui` 用 TUI 当前会话播种 LiveSession，画面里早已有这些
    // 消息（如 `atomcode -c` 续聊），此时 render_snapshot=false 跳过回放、避免重复刷一遍。
    if render_snapshot {
        let snapshot: Vec<atomcode_core::conversation::message::Message> =
            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(session.snapshot())
            });
        use atomcode_core::conversation::message::{MessageContent, Role};
        renderer.render(UiLine::TurnSeparator {
            label: "— 同步快照 —".to_string(),
        });
        for m in &snapshot {
            match (&m.role, &m.content) {
                (Role::User, MessageContent::Text(s)) => {
                    renderer.render(UiLine::User(s.clone()));
                }
                (Role::Assistant, MessageContent::Text(s)) => {
                    if !s.is_empty() {
                        renderer.render(UiLine::AssistantText(s.clone()));
                        renderer.render(UiLine::AssistantLineBreak);
                    }
                }
                (
                    Role::Assistant,
                    MessageContent::AssistantWithToolCalls { text, tool_calls, .. },
                ) => {
                    if let Some(t) = text {
                        if !t.is_empty() {
                            renderer.render(UiLine::AssistantText(t.clone()));
                            renderer.render(UiLine::AssistantLineBreak);
                        }
                    }
                    for tc in tool_calls {
                        renderer.render(UiLine::ToolCall {
                            name: tc.name.clone(),
                            detail: super::format_tool_detail(&tc.name, &tc.arguments),
                        });
                    }
                }
                (Role::Tool, MessageContent::ToolResult(r)) => {
                    renderer.render(UiLine::ToolResult {
                        success: r.success,
                        summary: super::summarise(&r.output, r.success),
                    });
                }
                _ => {}
            }
        }
        renderer.render(UiLine::TurnSeparator {
            label: "— 同步快照结束 —".to_string(),
        });
    }
    let handle = super::live_sync::spawn_live_forwarder(
        session.clone(),
        ctx.foreground_runtime_id,
        ctx.runtime_event_tx.clone(),
    );
    ctx.sync_forwarder = Some(handle);
    ctx.sync_session = Some(session);
    renderer.render(UiLine::CommandOutput(
        "已同步当前会话（与浏览器实时互通）".to_string(),
    ));
}

pub(super) fn execute_slash_command(
    cmd: &str,
    arg: &str,
    state: &mut UiState,
    ctx: &mut LoopCtx,
    renderer: &mut dyn Renderer,
    active_modal: &mut Option<Box<dyn Modal>>,
    fixissue_pending: &mut Option<atomcode_core::atomgit::IssueRef>,
    fixissue_buffer: &mut String,
    setup_pending: &mut bool,
) -> Result<()> {
    // `fixissue_pending` / `fixissue_buffer` no longer have a slash-command
    // entry that consumes them (the `/fixissue` arm was removed; the
    // `atomcode fixissue` CLI subcommand seeds these via cli/main.rs and
    // event_loop/mod.rs's AgentEvent handler still drains them on
    // TurnComplete). They stay in the signature so callers don't have to
    // change, and so a future restoration of the slash command is a
    // one-arm-add rather than a refactor.
    let _ = (&fixissue_pending, &fixissue_buffer);

    // Built-in commands are all lowercase ASCII; normalise the user's
    // input so `/SESSION`, `/Session`, `/sEssIon` all hit the same arm
    // as `/session`. `arg` is left untouched — paths / URLs are
    // case-sensitive in general.
    let cmd_lower = cmd.to_ascii_lowercase();
    let cmd = cmd_lower.as_str();

    // Emit use_command telemetry before dispatch so the event fires
    // regardless of whether the command succeeds or errors out.
    {
        use atomcode_telemetry::Event;
        let cmd_name = cmd.trim_start_matches('/').to_string();
        ctx.telemetry.track(Event::UseCommand { type_: cmd_name, success: Some(true), error_kind: None, error_data: None });
    }

    match cmd {
        "quit" | "exit" => {
            ctx.agent.cmd_tx.send(AgentCommand::Shutdown).ok();
        }
        "help" => {
            if arg.trim() == "commands" {
                let config_dir = Config::config_dir();
                let cmds = ctx.custom_commands.list();
                let mut out = t(Msg::HelpCustomCommandsHeader).into_owned();
                for cmd in &cmds {
                    let source_label = if cmd.source.starts_with(&config_dir) {
                        t(Msg::HelpSourceGlobal)
                    } else {
                        t(Msg::HelpSourceProject)
                    };
                    out.push_str(&format!(
                        "    /{}  — {} ({})\n",
                        cmd.name, cmd.description, source_label
                    ));
                }
                if cmds.is_empty() {
                    out.push_str(&t(Msg::HelpCustomNone));
                    out.push_str(&t(Msg::HelpCustomCreateHint));
                }
                renderer.render(UiLine::CommandOutput(out));
            } else {
                renderer.render(UiLine::CommandOutput(ctx.commands.help_text()));
            }
            renderer.flush();
        }
        "guide" => {
            if arg.is_empty() {
                let mut menu = String::new();
                menu.push_str(&t(Msg::GuideMenuHeader));
                menu.push_str("\n\n  ");
                menu.push_str(&t(Msg::GuideMenuTopics));
                menu.push_str("\n    /guide ");
                menu.push_str(&t(Msg::GuideMenuGettingStarted));
                menu.push_str("\n    /guide ");
                menu.push_str(&t(Msg::GuideMenuSwitchModel));
                menu.push_str("\n    /guide ");
                menu.push_str(&t(Msg::GuideMenuMcp));
                menu.push_str("\n    /guide ");
                menu.push_str(&t(Msg::GuideMenuSkills));
                menu.push_str("\n    /guide ");
                menu.push_str(&t(Msg::GuideMenuMemory));
                menu.push_str("\n    /guide ");
                menu.push_str(&t(Msg::GuideMenuBackground));
                menu.push_str("\n    /guide ");
                menu.push_str(&t(Msg::GuideMenuContext));
                menu.push_str("\n    /guide ");
                menu.push_str(&t(Msg::GuideMenuKeybindings));
                menu.push_str("\n    /guide ");
                menu.push_str(&t(Msg::GuideMenuConfig));
                menu.push_str(&t(Msg::GuideMenuTip));
                menu.push('\n');
                menu.push_str(&t(Msg::GuideMenuDocUrl));
                renderer.render(UiLine::CommandOutput(menu));
                renderer.flush();
            } else {
                // Try expanding the "ask" skill inline first (fast path).
                if let Some(rendered) = expand_skill(ctx, "ask", arg) {
                    ctx.agent
                        .cmd_tx
                        .send(AgentCommand::SendMessage {
                            text: rendered,
                            images: vec![],
                            image_markers: vec![],
                        })
                        .ok();
                    state.on_submit();
                } else {
                    // "ask" skill is not installed — trigger async install
                    // and stash the topic so handle_plugin_job_event can
                    // auto-invoke once the install completes.
                    let topic = arg.to_string();

                    if ctx.pending_guide_topic.is_some() {
                        renderer.render(UiLine::CommandOutput(
                            t(Msg::CmdGuideInstalling).into_owned(),
                        ));
                        renderer.flush();
                        return Ok(());
                    }

                    ctx.pending_guide_topic = Some(topic);

                    let tx = ctx.plugin_job_tx.clone();
                    renderer.render(UiLine::CommandOutput(
                        t(Msg::CmdGuideAutoInstall).into_owned(),
                    ));
                    renderer.flush();

                    tokio::task::spawn_blocking(move || {
                        let ev = match atomcode_core::plugin::installer::ensure_plugin_installed(
                            "atomcode",
                            "atomcode-skills",
                            "https://atomgit.com/atomgit_atomcode/atomcode-skills.git",
                        ) {
                            Ok(info) => {
                                atomcode_core::plugin::PluginJobEvent::PluginInstalled(info)
                            }
                            Err(e) => {
                                if let Some(_aie) = e.downcast_ref::<
                                    atomcode_core::plugin::installer::AlreadyInstalledError,
                                >() {
                                    atomcode_core::plugin::PluginJobEvent::PluginAlreadyInstalled {
                                        id: _aie.id.clone(),
                                    }
                                } else {
                                    atomcode_core::plugin::PluginJobEvent::Failed {
                                        op: "install".into(),
                                        msg: format!("{:#}", e),
                                    }
                                }
                            }
                        };
                        let _ = tx.send(ev);
                    });
                }
            }
        }
        "keys" => {
            // Dump the full keyboard-shortcut reference into scrollback.
            // i18n string owns column alignment so translators can adjust
            // per locale without touching this arm. /help complements
            // this with the slash-command list.
            renderer.render(UiLine::CommandOutput(
                t(Msg::KeybindingsHelp).into_owned(),
            ));
            renderer.flush();
        }
        "plan" => {
            state.agent_mode = AgentMode::Plan;
            ctx.agent.cmd_tx.send(AgentCommand::SetPlanMode(true)).ok();
            renderer.render(UiLine::CommandOutput(
                t(Msg::CmdSwitchedPlanMode).into_owned(),
            ));
            renderer.flush();
        }
        "build" => {
            state.agent_mode = AgentMode::Build;
            ctx.agent.cmd_tx.send(AgentCommand::SetPlanMode(false)).ok();
            renderer.render(UiLine::CommandOutput(
                t(Msg::CmdSwitchedBuildMode).into_owned(),
            ));
            renderer.flush();
        }
        "config" => {
            // Head: current active provider + config path so users know
            // which provider is talking and where to edit.
            let config_path = Config::default_path().display().to_string();
            let mut txt = t(Msg::ConfigProviderLabel {
                provider: &ctx.config.default_provider,
                path: &config_path,
            }).into_owned();
            // Body: one minimal runnable example + pointer to the full
            // reference so users know where to get Claude / OpenAI /
            // Ollama variants without flooding the terminal here.
            txt.push_str(
                "  Example:\n\
                 \n\
                 ```toml\n\
                 default_provider = \"deepseek\"\n\
                 \n\
                 [providers.deepseek]\n\
                 type           = \"openai\"\n\
                 api_key        = \"sk-...\"\n\
                 model          = \"deepseek-chat\"\n\
                 base_url       = \"https://api.deepseek.com/v1\"\n\
                 context_window = 64000\n\
                 ```\n\
                 \n\
                 Full reference: docs/config.example.toml (every field, every provider flavour).\n\
                 Edit the file, then run /reload — no restart needed.\n",
            );
            renderer.render(UiLine::CommandOutput(txt));
            renderer.flush();
        }
        "reload" => {
            // Re-read ~/.atomcode/config.toml from disk and push it to the
            // running daemon. Streaming-safe: the agent picks the new config
            // up on the *next* turn; anything already in-flight finishes on
            // the old config (ReloadConfig is queued behind the current
            // AgentCommand stream, not a hot swap).
            let path = Config::default_path();
            match Config::load(&path) {
                Ok(new_cfg) => {
                    let new_default = new_cfg.default_provider.clone();
                    let new_model = new_cfg
                        .providers
                        .get(&new_default)
                        .map(|p| p.model.clone())
                        .unwrap_or_else(|| new_default.clone());
                    ctx.config = new_cfg.clone();
                    ctx.runtime_factory.set_config(new_cfg.clone());
                    ctx.model_name = new_model.clone();
                    ctx.agent
                        .cmd_tx
                        .send(AgentCommand::ReloadConfig(new_cfg))
                        .ok();
                    renderer.render(UiLine::CommandOutput(
                        t(Msg::CmdReloadDone {
                            provider: &new_default, model: &new_model,
                        }).into_owned(),
                    ));
                }
                Err(e) => {
                    let msg = format!("{}", e);
                    renderer.render(UiLine::Error(
                        t(Msg::CmdReloadFailed { error: &msg }).into_owned(),
                    ));
                }
            }
            renderer.flush();
        }
        "clear" => {
            // Physical clear via the renderer (keeps cached footer state
            // coherent with the terminal). Scrollback is preserved by
            // most terminals — \x1b[3J would nuke it, which we don't
            // want; `clear_screen` emits \x1b[2J\x1b[H.
            renderer.clear_screen();
            let dir_display = ctx.working_dir.to_string_lossy().to_string();
            renderer.render(UiLine::Welcome {
                model: ctx.model_name.clone(),
                working_dir: dir_display,
            });
            renderer.flush();
        }
        "session" => {
            // Start fresh: tell the agent to drop conversation history,
            // clear the scrollback + type-ahead queue + UI state, and
            // redraw the welcome screen so the user sees they're in a
            // brand-new session. Ports `/session` from the legacy TUI.
            ctx.agent.cmd_tx.send(AgentCommand::ClearConversation).ok();
            ctx.current_session_id = None;
            state.total_tokens = 0;
            state.prompt_tokens = 0;
            state.completion_tokens = 0;
            state.cached_tokens = 0;
            state.last_context = None;
            state.pending_context_render = None;
            state.thinking_idx = 0;
            state.on_turn_complete();
            // New session = new session file on disk. Old session
            // (already saved at its last TurnComplete) stays on disk so
            // it can still be `/resume`d; we just stop writing into it.
            ctx.current_session =
                atomcode_core::session::Session::default_session(ctx.working_dir.clone());
            ctx.bg_manager
                .set_foreground_session(ctx.current_session.clone());
            // Bind telemetry + agent session id to the new session's UUID
            // (the ClearConversation above intentionally leaves the id alone;
            // this is the single source of truth).
            bind_telemetry_to_session(ctx, &ctx.current_session);
            // `reset()` wipes the terminal AND the renderer's cached
            // footer/stream state, so the next Welcome renders against
            // a known (row 1, col 1) anchor. This is what makes
            // /session behave like a fresh launch.
            renderer.reset();
            let dir_display = crate::platform::collapse_home(&ctx.working_dir.to_string_lossy());
            renderer.render(UiLine::Welcome {
                model: ctx.model_name.clone(),
                working_dir: dir_display,
            });
            renderer.render(UiLine::CommandOutput(
                t(Msg::CmdNewSession).into_owned(),
            ));
            renderer.flush();
        }
        "model" => {
            if ctx.config.providers.is_empty() {
                renderer.render(UiLine::CommandOutput(
                    t(Msg::CmdNoProviders).into_owned(),
                ));
                renderer.flush();
            } else {
                *active_modal = Some(Box::new(ModelPicker::open(&ctx.config)));
            }
        }
        "language" => {
            if arg.is_empty() {
                *active_modal = Some(Box::new(LanguagePicker::open()));
            } else {
                match arg.parse::<atomcode_core::locale::Locale>() {
                    Ok(locale) => {
                        crate::i18n::set_locale(locale);
                        ctx.config.language = Some(locale);
                        let config_path = atomcode_core::config::Config::default_path();
                        if let Err(e) = ctx.config.save(&config_path) {
                            // TODO: surface via renderer once a non-modal error display is available
                            eprintln!("[language] failed to save config: {e}");
                        }
                        // Display label matches the picker's option list
                        // so /language en and /language zh both echo a
                        // human-readable name, not just the locale code.
                        let label = match locale {
                            atomcode_core::locale::Locale::En => "English",
                            atomcode_core::locale::Locale::ZhCn => "简体中文",
                        };
                        renderer.render(UiLine::CommandOutput(
                            t(Msg::LanguageSwitched {
                                label,
                                locale: &locale.to_string(),
                            })
                            .into_owned(),
                        ));
                        renderer.flush();
                    }
                    Err(_) => {
                        let msg = t(Msg::ErrUnsupportedLocale { input: arg });
                        renderer.render(UiLine::CommandOutput(format!("  {msg}\n")));
                        renderer.flush();
                    }
                }
            }
        }
        "resume" => match ctx.session_manager.list() {
            Ok(all) => {
                let sessions: Vec<_> = all.into_iter().filter(|s| s.message_count > 0).collect();
                if sessions.is_empty() {
                    renderer.render(UiLine::CommandOutput(
                        t(Msg::CmdNoSessions).into_owned(),
                    ));
                    renderer.flush();
                } else {
                    *active_modal = Some(Box::new(SessionPicker::open(sessions)));
                }
            }
            Err(e) => {
                renderer.render(UiLine::Error(
                    t(Msg::SessionListFailed { error: &e.to_string() }).into_owned(),
                ));
                renderer.flush();
            }
        },
        "rename" => {
            // Rename targets `ctx.current_session` (the in-flight conversation),
            // not whichever id `/resume` last loaded — the user expects /rename
            // to relabel the conversation they're currently typing into. The
            // session is always initialised at startup, so we never need a
            // "load a session first" fallback.
            if let Some(err) = validate_session_name(arg) {
                renderer.render(UiLine::Error(err));
                renderer.flush();
            } else {
                let old_name = ctx.current_session.name.clone();
                let new_name = arg.trim().to_string();
                ctx.current_session.rename(new_name.clone());
                match ctx.session_manager.save(&ctx.current_session) {
                    Ok(()) => {
                        renderer.render(UiLine::CommandOutput(
                            t(Msg::SessionRenamed { old: &old_name, new: &new_name })
                                .into_owned(),
                        ));
                        renderer.flush();
                    }
                    Err(e) => {
                        // Revert the in-memory rename so a follow-up retry
                        // still reports the original name.
                        ctx.current_session.name = old_name;
                        renderer.render(UiLine::Error(
                            t(Msg::SessionSaveFailed { error: &e.to_string() })
                                .into_owned(),
                        ));
                        renderer.flush();
                    }
                }
            }
        }
        "provider" => {
            *active_modal = Some(Box::new(ProviderWizard::MainMenu { selected: 0 }));
            renderer.render(UiLine::CommandOutput(
                t(Msg::ProviderWizardHeader).into_owned(),
            ));
            renderer.flush();
        }
        "status" => {
            let mut txt = t(Msg::StatusBody {
                model: &ctx.model_name,
                dir: &ctx.working_dir.display().to_string(),
                config: &Config::default_path().display().to_string(),
                tokens: state.total_tokens,
            }).into_owned();
            txt.push_str(&render_codingplan_status_for_status_cmd());

            txt.push('\n');
            txt.push_str(&render_instruction_status_block(&ctx.working_dir));

            renderer.render(UiLine::CommandOutput(txt));
            renderer.flush();
        }
        "diff" => {
            let out = std::process::Command::new("git")
                .args(["diff", "--stat"])
                .current_dir(&ctx.working_dir)
                .output();
            match out {
                Ok(o) => {
                    let s = String::from_utf8_lossy(&o.stdout).to_string();
                    renderer.render(UiLine::CommandOutput(if s.is_empty() {
                        t(Msg::CmdNoChanges).into_owned()
                    } else {
                        s
                    }));
                }
                Err(e) => {
                    renderer.render(UiLine::Error(t(Msg::DiffFailed { error: &format!("{}", e) }).into_owned()));
                }
            }
            renderer.flush();
        }
        "undo" => {
            if state.phase != crate::state::UiPhase::Idle {
                renderer.render(UiLine::CommandOutput(
                    t(Msg::CmdUndoBusy).into_owned(),
                ));
                renderer.flush();
            } else {
                let a = arg.trim();
                // None = bare /undo (last turn); Some(n) = /undo n; Err = bad arg.
                let parsed: Result<Option<usize>, ()> = if a.is_empty() {
                    Ok(None)
                } else {
                    match a.parse::<usize>() {
                        Ok(n) if n >= 1 => Ok(Some(n)),
                        _ => Err(()),
                    }
                };
                match parsed {
                    Ok(nth) => {
                        ctx.agent
                            .cmd_tx
                            .send(AgentCommand::UndoToPrompt { nth })
                            .ok();
                    }
                    Err(()) => {
                        renderer.render(UiLine::CommandOutput(
                            t(Msg::CmdUndoBadArg).into_owned(),
                        ));
                        renderer.flush();
                    }
                }
            }
        }
        "cost" => {
            let total = state.prompt_tokens + state.completion_tokens;
            let cache_rate = if state.prompt_tokens > 0 {
                ((state.cached_tokens as f64 / state.prompt_tokens as f64 * 100.0) + 0.5) as usize
            } else {
                0
            };
            let cost = atomcode_core::pricing::calculate_cost(
                &ctx.model_name,
                state.prompt_tokens,
                state.completion_tokens,
                state.cached_tokens,
            );
            let cost_str = atomcode_core::pricing::format_cost(cost);
            renderer.render(UiLine::CommandOutput(
                t(Msg::CostReport {
                    prompt: state.prompt_tokens,
                    completion: state.completion_tokens,
                    cached: state.cached_tokens,
                    cache_rate,
                    total,
                    cost: &cost_str,
                }).into_owned(),
            ));
            renderer.flush();
        }
        "context" => {
            // `/context` = breakdown only.
            // `/context prompt` = breakdown + full assembled system prompt
            // (the exact bytes the most recent turn sent). Useful when
            // the model is misbehaving and you want to verify what's
            // actually in the prompt.
            //
            // The cached ContextSnapshot only refreshes on LLM round-trips.
            // Between turns — or after out-of-turn mutations like
            // `inject_post_compress_state` — the cache lags the actual
            // conversation. Dispatch a refresh and render when the
            // resulting rich stats event lands (see `handle_agent_event`
            // → `AgentEvent::ContextStats`). `pending_context_render =
            // Some(show_prompt)` marks the pending request; cleared after
            // the event handler fires the report. If the agent is busy
            // in a turn, the next rich emission (at the next LLM call)
            // serves the render — still fresh, just a tick later.
            let show_prompt = arg.trim().eq_ignore_ascii_case("prompt");
            state.pending_context_render = Some(show_prompt);
            ctx.agent
                .cmd_tx
                .send(AgentCommand::RefreshContextStats)
                .ok();
        }
        "compact" => {
            let prompt = (!arg.trim().is_empty()).then(|| arg.trim().to_string());
            // Agent streams the authoritative result back as TextDelta
            // ("nothing to compact" / "compacted — dropped N messages").
            // Don't pre-render a placeholder — the agent's reply could
            // contradict it when the conversation is too short.
            ctx.agent.cmd_tx.send(AgentCommand::Compact { prompt }).ok();
        }
        "remember" => {
            let text = arg.trim();
            if text.is_empty() {
                renderer.render(UiLine::Error(t(Msg::RememberUsage).into_owned()));

                renderer.flush();
            } else {
                let (content, global) = if text.starts_with("--global ") {
                    (text[9..].trim().to_string(), true)
                } else {
                    (text.to_string(), false)
                };
                if content.is_empty() {
                    renderer.render(UiLine::Error(t(Msg::RememberUsage).into_owned()));

                    renderer.flush();
                } else {
                    ctx.agent
                        .cmd_tx
                        .send(AgentCommand::Remember { content, global })
                        .ok();
                }
            }
        }
        "forget" => {
            let keyword = arg.trim();
            if keyword.is_empty() {
                renderer.render(UiLine::Error(t(Msg::ForgetUsage).into_owned()));
                renderer.flush();
            } else {
                ctx.agent
                    .cmd_tx
                    .send(AgentCommand::Forget {
                        keyword: keyword.to_string(),
                    })
                    .ok();
            }
        }
        "memory" => {
            ctx.agent.cmd_tx.send(AgentCommand::ShowMemory).ok();
        }
        "webui" => {
            let a = arg.trim();
            let msg = if a == "stop" {
                // 同步停止，无需 block_on。
                atomcode_daemon::stop_server()
            } else {
                // 解析绑定地址：默认 127.0.0.1；支持 `--host <addr>` / `--host=<addr>`，
                // 以及快捷词 `lan`（= 0.0.0.0，暴露到局域网/外网）。
                fn parse_host(a: &str) -> String {
                    if a == "lan" || a == "0.0.0.0" {
                        return "0.0.0.0".to_string();
                    }
                    let toks: Vec<&str> = a.split_whitespace().collect();
                    for (i, tok) in toks.iter().enumerate() {
                        if let Some(v) = tok.strip_prefix("--host=") {
                            if !v.is_empty() {
                                return v.to_string();
                            }
                        }
                        if *tok == "--host" {
                            if let Some(v) = toks.get(i + 1) {
                                return v.to_string();
                            }
                        }
                    }
                    "127.0.0.1".to_string()
                }
                let host = parse_host(a);
                // #561 修复：先用 TUI 当前会话播种 LiveSession（session_id + 历史），
                // 再开浏览器——否则浏览器抢先连 /live 会建出空白 LiveSession。
                let session = atomcode_daemon::ensure_live_session(
                    ctx.working_dir.clone(),
                    ctx.telemetry.clone(),
                    Some(ctx.current_session.id.clone()),
                    ctx.current_session.messages.clone(),
                );
                let open_msg = tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current()
                        .block_on(atomcode_daemon::ensure_server_and_open(
                            &host,
                            atomcode_daemon::WEBUI_DEFAULT_PORT,
                            true,
                        ))
                });
                // 附着把 TUI 接入同步。画面里已有当前会话（播种来源），故 render_snapshot=false
                // 跳过快照回放，避免把同一段对话重复刷一遍。
                attach_live_session(ctx, renderer, session, false);
                open_msg
            };
            renderer.render(UiLine::CommandOutput(msg));
            renderer.flush();
        }
        "sync" => {
            if arg.trim() == "off" {
                if let Some(h) = ctx.sync_forwarder.take() {
                    h.abort();
                }
                ctx.sync_session = None;
                renderer.render(UiLine::CommandOutput(
                    "已退出同步，回到独立会话".to_string(),
                ));
            } else {
                // #561 修复：始终用 ensure_live_session 把当前会话上下文传给 LiveSession，
                // 这样即使 WebUI 先启动了 LiveSession（用不同 session_id），/sync 也能
                // 把它替换为 TUI 的会话。
                let session = atomcode_daemon::ensure_live_session(
                    ctx.working_dir.clone(),
                    ctx.telemetry.clone(),
                    Some(ctx.current_session.id.clone()),
                    ctx.current_session.messages.clone(),
                );
                attach_live_session(ctx, renderer, session, true);
            }
            renderer.flush();
        }
        "login" => {
            run_login_flow(renderer, ctx)?;
        }
        "logout" => {
            // /logout only invalidates the OAuth token on disk.
            // Provider config is a user asset and stays in config.toml
            // untouched — if the user's default is an AtomGit* provider,
            // the next LLM request fails with a "re-run /codingplan"
            // hint instead of the TUI crashing on next startup because
            // `default_provider` got cleared.
            match atomcode_core::auth::logout() {
                Ok(()) => {
                    ctx.telemetry.set_account_id(None);
                    let _ = ctx
                        .agent
                        .cmd_tx
                        .send(AgentCommand::ReloadConfig(ctx.config.clone()));
                    renderer.render(UiLine::CommandOutput(
                        t(Msg::CmdLogoutDone).into_owned(),
                    ));
                }
                Err(e) => {
                    let msg = format!("{}", e);
                    renderer.render(UiLine::Error(
                        t(Msg::CmdLogoutFailed { error: &msg }).into_owned(),
                    ));
                }
            }
            renderer.flush();
        }
        "whoami" => {
            let txt = if let Some(auth) = atomcode_core::auth::get_stored_auth() {
                let email = auth.user.email.as_deref().unwrap_or("—");
                let name = auth.user.name.as_deref().unwrap_or(&auth.user.username);
                format!(
                    "  {} ({})\n  {}\n  auth: {}\n",
                    name,
                    auth.user.username,
                    email,
                    atomcode_core::auth::auth_file_path().display(),
                )
            } else {
                t(Msg::CmdWhoamiNotSignedIn).into_owned()
            };
            renderer.render(UiLine::CommandOutput(txt));
            renderer.flush();
        }
        "upgrade" => {
            // Sub-dispatch: `/upgrade`, `/upgrade rollback`, `/upgrade --force`.
            // Keep parsing deliberately tolerant — users type these things
            // with assorted capitalization and whitespace; a command that
            // refuses `/upgrade Rollback` is user-hostile.
            let arg_norm = arg.trim().to_ascii_lowercase();
            if arg_norm == "rollback" {
                // Rollback is sync and fast (three renames). Run inline
                // so the user sees the result immediately without waiting
                // for an async task to schedule.
                match atomcode_core::self_update::run_rollback() {
                    Ok(sum) => {
                        // Route through the event channel so rendering
                        // and "set done → exit" logic stays in one place.
                        let _ = ctx.upgrade_tx.send(
                            atomcode_core::self_update::UpgradeEvent::RolledBack {
                                exe: sum.exe,
                                backup: sum.backup,
                            },
                        );
                    }
                    Err(e) => {
                        let _ =
                            ctx.upgrade_tx
                                .send(atomcode_core::self_update::UpgradeEvent::Failed(format!(
                                    "{:#}",
                                    e
                                )));
                    }
                }
            } else {
                let force = arg_norm == "--force" || arg_norm == "-f";
                if !force && !arg_norm.is_empty() {
                    renderer.render(UiLine::Error(
                        t(Msg::UpgradeUnknownArg { arg }).into_owned(),
                    ));
                    renderer.flush();
                    return Ok(());
                }
                renderer.render(UiLine::CommandOutput(
                    t(Msg::CmdCheckingUpdate).into_owned(),
                ));
                renderer.flush();
                let current = format!("v{}", env!("CARGO_PKG_VERSION"));
                let tx = ctx.upgrade_tx.clone();
                tokio::spawn(async move {
                    // The driver emits Done via `tx` on success; on error
                    // we translate to a Failed event so the TUI layer
                    // only has to handle one event stream.
                    if let Err(e) =
                        atomcode_core::self_update::run_upgrade(current, force, tx.clone()).await
                    {
                        let _ = tx.send(atomcode_core::self_update::UpgradeEvent::Failed(format!(
                            "{:#}",
                            e
                        )));
                    }
                });
            }
        }
        "issue" => {
            // Two-step wizard to file a NEW issue against the **atomcode
            // upstream repo** (atomgit_atomcode/atomcode), NOT against
            // the user's current working project. Use case is in-tool
            // bug reports / feature requests for atomcode itself; using
            // cwd would be confusing (a user reporting an atomcode bug
            // while in some unrelated repo would land their issue in
            // the wrong place, or get blocked by cwd validation).
            //
            // Step 1 collects a title (required), step 2 collects a
            // description (required, Shift+Enter for newlines). On
            // submit the event loop's post-close branch POSTs
            // `/repos/atomgit_atomcode/atomcode/issues` and echoes the
            // new issue URL into scrollback.
            let _ = arg; // reserved for future options (e.g. --template)
            let mut wiz = IssueWizard::open(
                atomcode_core::atomgit::UPSTREAM_OWNER.to_string(),
                atomcode_core::atomgit::UPSTREAM_REPO.to_string(),
            );
            wiz.emit_prompt(renderer);
            *active_modal = Some(Box::new(wiz));
        }
        "cd" => {
            // Bare `/cd` — open the interactive history picker (matches legacy
            // TUI behaviour). The picker's Enter-handler invokes `apply_cd`
            // itself, so there's nothing else to do here.
            if arg.is_empty() {
                if ctx.recent_dirs.is_empty() {
                    let cwd = ctx.working_dir.display().to_string();
                    renderer.render(UiLine::CommandOutput(
                        t(Msg::CdWorkingDir { cwd: &cwd }).into_owned(),
                    ));
                    renderer.flush();
                } else {
                    *active_modal = Some(Box::new(DirPicker::open(
                        ctx.recent_dirs.clone(),
                        ctx.working_dir.clone(),
                    )));
                }
                return Ok(());
            }
            let new_dir = resolve_cd(arg, &ctx.working_dir, ctx.previous_dir.as_deref());
            match new_dir {
                Ok(path) => {
                    apply_cd(ctx, path.clone());
                    let p = path.display().to_string();
                    renderer.render(UiLine::CommandOutput(
                        t(Msg::DirChanged { path: &p }).into_owned(),
                    ));
                }
                Err(e) => {
                    renderer.render(UiLine::Error(e));
                }
            }
            renderer.flush();
        }
        "bg" => {
            match bg_runtime::parse_bg_command(arg) {
                bg_runtime::BgCommand::Help => {
                    renderer.render(UiLine::CommandOutput(bg_runtime::render_bg_help()));
                }
                bg_runtime::BgCommand::List => {
                    renderer.render(UiLine::CommandOutput(bg_runtime::render_bg_list(
                        ctx.bg_manager.backgrounds(),
                    )));
                }
                bg_runtime::BgCommand::BackgroundCurrent => {
                    sync_bg_foreground(ctx);
                    if !ctx.bg_manager.has_capacity() {
                        renderer.render(UiLine::Error(
                            t(Msg::BgSlotLimitReached { max: bg_runtime::MAX_BACKGROUND_SLOTS }).into_owned(),
                        ));
                        renderer.flush();
                        return Ok(());
                    }
                    let old_short_id = ctx.current_session.short_id().to_string();
                    let new_session = Session::default_session(ctx.working_dir.clone());
                    let new_short_id = new_session.short_id().to_string();
                    let (runtime_id, client, new_session) = spawn_runtime(ctx, new_session);
                    let old_state = foreground_state_from_ui(state);
                    let slot = match ctx.bg_manager.background_current(
                        client.clone(),
                        new_session.clone(),
                        runtime_id,
                        old_state,
                    ) {
                        Ok(slot) => slot,
                        Err(bg_runtime::BgError::SlotLimit { max }) => {
                            renderer.render(UiLine::Error(
                                t(Msg::BgSlotLimitReached { max }).into_owned(),
                            ));
                            renderer.flush();
                            return Ok(());
                        }
                        Err(bg_runtime::BgError::InvalidSlot { .. }) => unreachable!(),
                    };

                    ctx.agent = client;
                    ctx.foreground_runtime_id = runtime_id;
                    ctx.current_session = new_session;
                    bind_telemetry_to_session(ctx, &ctx.current_session);
                    state.on_turn_complete();
                    renderer.reset();
                    render_welcome(renderer, ctx);
                    renderer.render(UiLine::CommandOutput(
                        t(Msg::BgBackgroundCurrent {
                            new_id: &new_short_id,
                            slot,
                            old_id: &old_short_id,
                            state: &old_state.localised(),
                        }).into_owned(),
                    ));
                }
                bg_runtime::BgCommand::Resume(slot) => {
                    sync_bg_foreground(ctx);
                    let outcome = match ctx
                        .bg_manager
                        .resume_slot(slot, foreground_state_from_ui(state))
                    {
                        Ok(outcome) => outcome,
                        Err(bg_runtime::BgError::InvalidSlot { slot, len }) => {
                            renderer.render(UiLine::Error(
                                t(Msg::BgInvalidSlot { slot, available: len }).into_owned(),
                            ));
                            renderer.flush();
                            return Ok(());
                        }
                        Err(bg_runtime::BgError::SlotLimit { max }) => {
                            renderer.render(UiLine::Error(
                                t(Msg::BgSlotLimitReached { max }).into_owned(),
                            ));
                            renderer.flush();
                            return Ok(());
                        }
                    };
                    let Some(client) = outcome.resumed_client else {
                        renderer.render(UiLine::Error(
                            t(Msg::BgNoRuntimeClient).into_owned(),
                        ));
                        renderer.flush();
                        return Ok(());
                    };

                    ctx.agent = client;
                    ctx.foreground_runtime_id = outcome.resumed_runtime_id;
                    ctx.current_session = outcome.resumed_session;
                    bind_telemetry_to_session(ctx, &ctx.current_session);
                    state.on_turn_complete();
                    crate::modals::session_picker::replay_session(
                        renderer,
                        &ctx.current_session,
                        true,
                    );

                    // If the resumed session was waiting for tool approval,
                    // re-render the approval prompt so the user can
                    // continue interacting.  Detect this by looking for
                    // an AssistantWithToolCalls message whose tool_calls
                    // lack corresponding ToolResult entries.
                    let pending_approval = find_pending_approval(&ctx.current_session);
                    if let Some((tool_name, detail)) = pending_approval {
                        renderer.render(UiLine::ApprovalPrompt { tool: tool_name, detail });
                        state.on_approval_needed("");
                    }

                    let short_id = ctx.current_session.short_id().to_string();
                    let mut msg = t(Msg::BgResumed { slot, short_id: &short_id }).into_owned();
                    if let Some(previous_slot) = outcome.previous_foreground_slot {
                        msg.push_str(&t(Msg::BgPreviousForegroundMoved { slot: previous_slot }).into_owned());
                    }
                    renderer.render(UiLine::CommandOutput(msg));
                }
                bg_runtime::BgCommand::Drop(slot) => {
                    let dropped = match ctx.bg_manager.drop_slot(slot) {
                        Ok(dropped) => dropped,
                        Err(bg_runtime::BgError::InvalidSlot { slot, len }) => {
                            renderer.render(UiLine::Error(
                                t(Msg::BgInvalidSlot { slot, available: len }).into_owned(),
                            ));
                            renderer.flush();
                            return Ok(());
                        }
                        Err(bg_runtime::BgError::SlotLimit { .. }) => unreachable!(),
                    };
                    if matches!(dropped.state, bg_runtime::RuntimeState::Running) {
                        if let Some(client) = dropped.client.as_ref() {
                            client.cmd_tx.send(AgentCommand::Cancel).ok();
                        }
                    }
                    if !dropped.session.messages.is_empty() {
                        let _ = ctx.session_manager.save(&dropped.session);
                    }
                    let short_id = dropped.session.short_id().to_string();
                    renderer.render(UiLine::CommandOutput(
                        t(Msg::BgDropped { slot, short_id: &short_id }).into_owned(),
                    ));
                }
            }
            renderer.flush();
        }
        "background" => {
            // Compatibility wrapper around `/bg`: start a one-shot task in a
            // real background runtime, keep the current foreground active.
            let task = arg.trim();
            if task.is_empty() {
                renderer.render(UiLine::CommandOutput(
                    t(Msg::BackgroundUsage).into_owned(),
                ));
                renderer.flush();
                return Ok(());
            }
            if !ctx.bg_manager.has_capacity() {
                renderer.render(UiLine::Error(
                    t(Msg::BgSlotLimitReached { max: bg_runtime::MAX_BACKGROUND_SLOTS }).into_owned(),
                ));
                renderer.flush();
                return Ok(());
            }
            let mut session = Session::default_session(ctx.working_dir.clone());
            session.name = short_task_name(task);
            let short_id = session.short_id().to_string();
            let (runtime_id, client, session) = spawn_runtime(ctx, session);
            let slot = match ctx.bg_manager.push_background_runtime(
                runtime_id,
                client.clone(),
                session,
                bg_runtime::RuntimeState::Running,
            ) {
                Ok(slot) => slot,
                Err(bg_runtime::BgError::SlotLimit { max }) => {
                    renderer.render(UiLine::Error(
                        t(Msg::BgSlotLimitReached { max }).into_owned(),
                    ));
                    renderer.flush();
                    return Ok(());
                }
                Err(bg_runtime::BgError::InvalidSlot { .. }) => unreachable!(),
            };
            client
                .cmd_tx
                .send(AgentCommand::SendMessage { text: task.to_string(), images: Vec::new(), image_markers: Vec::new() })
                .ok();
            renderer.render(UiLine::CommandOutput(
                t(Msg::BgTaskStarted { slot, short_id: &short_id }).into_owned(),
            ));
            renderer.flush();
        }
        "init" => {
            // Generate .atomcode.md from project structure. Refuses to
            // overwrite by default — `/init --force` opts in. The file is
            // picked up by agent::prompt next time the system prompt is
            // built; in-flight turns finish on the old prompt.
            let target = ctx.working_dir.join(".atomcode.md");
            let force = matches!(arg.trim(), "--force" | "force");
            if target.exists() && !force {
                let path_str = target.display().to_string();
                renderer.render(UiLine::CommandOutput(
                    t(Msg::InitAlreadyExists { path: &path_str }).into_owned(),
                ));
                renderer.flush();
                return Ok(());
            }
            let content = atomcode_core::init::generate_project_instructions(&ctx.working_dir);
            match std::fs::write(&target, &content) {
                Ok(()) => {
                    let path_str = target.display().to_string();
                    renderer.render(UiLine::CommandOutput(
                        t(Msg::InitWrote { path: &path_str, bytes: content.len() }).into_owned(),
                    ));
                    // Confirm the file is reachable for the prompt-builder by
                    // re-running the same load that `/status` uses. If the
                    // freshly written file does NOT appear under PROJECT here,
                    // the user knows immediately — instead of asking the AI
                    // a question and trying to infer load state from its
                    // answer.
                    renderer.render(UiLine::CommandOutput(
                        render_instruction_status_block(&ctx.working_dir),
                    ));
                }
                Err(e) => {
                    renderer.render(UiLine::Error(
                        t(Msg::InitFailed { error: &format!("{}", e) }).into_owned(),
                    ));
                }
            }
            renderer.flush();
        }
        "mcp" => {
            let sub = arg.trim();
            if let Some(rest) = sub.strip_prefix("login") {
                let server = rest.trim();
                if server.is_empty() {
                    renderer.render(UiLine::CommandOutput(
                        t(Msg::McpOAuthLoginUsage).into_owned(),
                    ));
                    renderer.flush();
                    return Ok(());
                }
                let configs = match atomcode_core::mcp::load_mcp_config(&ctx.working_dir) {
                    Ok(configs) => configs,
                    Err(e) => {
                        renderer.render(UiLine::Error(
                            t(Msg::McpOAuthLoadConfigFailed { error: &format!("{:#}", e) }).into_owned(),
                        ));
                        renderer.flush();
                        return Ok(());
                    }
                };
                let Some(config) = configs.into_iter().find(|config| config.name == server) else {
                    renderer.render(UiLine::Error(
                        t(Msg::McpOAuthServerNotFound { server }).into_owned(),
                    ));
                    renderer.flush();
                    return Ok(());
                };
                renderer.render(UiLine::CommandOutput(
                    t(Msg::McpOAuthStarting { server }).into_owned(),
                ));
                renderer.flush();
                let is_github_server = matches!(
                    &config.config,
                    atomcode_core::mcp::McpTransportConfig::Http {
                        auth: Some(atomcode_core::mcp::McpHttpAuthConfig::OAuth(auth)),
                        ..
                    } if auth.provider.as_deref() == Some("github")
                );
                let result = tokio::task::block_in_place(|| {
                    atomcode_core::mcp::login_mcp_oauth(
                        &config,
                        atomcode_core::mcp::McpOAuthLoginOptions {
                            client_id: if is_github_server {
                                std::env::var("ATOMCODE_GITHUB_MCP_CLIENT_ID").ok()
                            } else {
                                None
                            },
                            client_secret_env: None,
                            scopes: Vec::new(),
                        },
                    )
                });
                match result {
                    Ok(token) => renderer.render(UiLine::CommandOutput(
                        t(Msg::McpOAuthSaved { provider: &token.provider, server }).into_owned(),
                    )),
                    Err(e) => renderer.render(UiLine::Error(
                        t(Msg::McpOAuthFailed { error: &format!("{:#}", e) }).into_owned(),
                    )),
                }
                renderer.flush();
                return Ok(());
            }

            if let Some(rest) = sub.strip_prefix("logout") {
                let server = rest.trim();
                if server.is_empty() {
                    renderer.render(UiLine::CommandOutput(
                        t(Msg::McpOAuthLogoutUsage).into_owned(),
                    ));
                    renderer.flush();
                    return Ok(());
                }
                match atomcode_core::mcp::McpTokenStore::default().delete_token(server) {
                    Ok(true) => renderer.render(UiLine::CommandOutput(
                        t(Msg::McpOAuthTokenRemoved { server }).into_owned(),
                    )),
                    Ok(false) => renderer.render(UiLine::CommandOutput(
                        t(Msg::McpOAuthNoToken { server }).into_owned(),
                    )),
                    Err(e) => renderer.render(UiLine::Error(
                        t(Msg::McpOAuthLogoutFailed { error: &format!("{:#}", e) }).into_owned(),
                    )),
                }
                renderer.flush();
                return Ok(());
            }

            if sub.eq_ignore_ascii_case("reload") {
                // Preflight: parse merged MCP config so we can show progress immediately.
                // (Connection attempts happen in background and may take up to timeout_ms.)
                let configs = match atomcode_core::mcp::load_mcp_config(&ctx.working_dir) {
                    Ok(c) => c,
                    Err(e) => {
                        renderer.render(UiLine::Error(
                            t(Msg::McpReloadFailed { error: &format!("{:#}", e) }).into_owned(),
                        ));
                        renderer.flush();
                        return Ok(());
                    }
                };

                let mut header = t(Msg::McpReloading { count: configs.len() }).into_owned();

                if !configs.is_empty() {
                    header.push_str(&t(Msg::McpConnecting));
                    for c in &configs {
                        header.push_str(&t(Msg::McpConnectingServer { name: &c.name }));
                    }
                } else {
                    header.push_str(&t(Msg::McpNoServersConfigured));
                }
                renderer.render(UiLine::CommandOutput(header));
                renderer.flush();

                // 1) Drop all previously-registered MCP tools so any adapters holding the
                // old registry Arc are released and stdio child processes can be killed.
                let removed = tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current().block_on(async {
                        ctx.agent.tool_registry.unregister_prefix("mcp__").await
                    })
                });

                // 2) Drop old registry + event receiver (stop consuming old events).
                ctx.mcp_connect_rx = None;
                ctx.mcp_registry = None;
                ctx.mcp_reload = None;

                // If no servers are configured, we're done after cleanup.
                if configs.is_empty() {
                    renderer.render(UiLine::CommandOutput(
                        t(Msg::McpClearedNoServers { removed }).into_owned(),
                    ));
                    renderer.flush();
                    return Ok(());
                }

                // 2.5) Arm progress tracker (event loop prints a summary once all results land).
                ctx.mcp_reload = Some(super::McpReloadProgress {
                    total: configs.len(),
                    done: 0,
                    connected: 0,
                    failed: 0,
                    started_at: std::time::Instant::now(),
                });

                // 3) Recreate registry and event channel. Connections happen in background
                // and will stream Connected/Failed events into scrollback (event loop select!).
                use atomcode_core::mcp::McpConnectEvent;
                let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<McpConnectEvent>();
                let registry = atomcode_core::mcp::McpRegistry::from_config_background_with_events(
                    &ctx.working_dir,
                    Some(tx),
                );
                ctx.mcp_registry = Some(std::sync::Arc::new(registry));
                ctx.mcp_connect_rx = Some(rx);

                renderer.render(UiLine::CommandOutput(
                    t(Msg::McpClearedReconnecting { removed }).into_owned(),
                ));
                renderer.flush();
                return Ok(());
            }

            // `/mcp tools <server>`: list remote tool names for a connected server.
            // This is intentionally separate from a global `/tools` so we keep the surface minimal.
            if let Some(rest) = sub.strip_prefix("tools") {
                let server = rest.trim();
                if server.is_empty() {
                    renderer.render(UiLine::CommandOutput(
                        t(Msg::McpToolsUsage).into_owned(),
                    ));
                    renderer.flush();
                    return Ok(());
                }
                if let Some(registry) = &ctx.mcp_registry {
                    let server = server.to_string();
                    let server_for_msg = server.clone();
                    let registry = registry.clone();
                    let tx = registry.event_sender();
                    tokio::spawn(async move {
                        let list_timeout = registry.list_tools_timeout(&server).await;
                        let tools = match tokio::time::timeout(
                            list_timeout,
                            registry.list_tools_for_server(&server),
                        )
                        .await
                        {
                            Ok(v) => v,
                            Err(_) => {
                                if let Some(tx) = &tx {
                                    let _ = tx.send(atomcode_core::mcp::McpConnectEvent::Warning {
                                        name: server.clone(),
                                        message: format!(
                                            "tools/list timed out after {}s (server connected but tools not listed yet)",
                                            list_timeout.as_secs()
                                        ),
                                    });
                                }
                                return;
                            }
                        };
                        let mut msg = format!("tools:\n");
                        if tools.is_empty() {
                            msg.push_str("  (none — tools/list may have failed, timed out, or returned empty)\n");
                        } else {
                            for t in tools {
                                msg.push_str(&format!("  - mcp__{}__{}\n", server, t.tool_name));
                            }
                        }
                        if let Some(tx) = tx {
                            let _ = tx.send(atomcode_core::mcp::McpConnectEvent::Warning {
                                name: server,
                                message: msg.trim_end().to_string(),
                            });
                        }
                    });
                    renderer.render(UiLine::CommandOutput(
                        t(Msg::McpToolsListing { server: &server_for_msg }).into_owned(),
                    ));
                } else {
                    renderer.render(UiLine::CommandOutput(
                        t(Msg::McpNoRegistry).into_owned(),
                    ));
                }
                renderer.flush();
                return Ok(());
            }

            // Default: show status.
            if let Some(registry) = &ctx.mcp_registry {
                let statuses = tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current().block_on(registry.server_statuses())
                });
                if statuses.is_empty() {
                    renderer.render(UiLine::CommandOutput(
                        t(Msg::McpNoServersConfigured).into_owned(),
                    ));
                } else {
                    let mut txt = t(Msg::McpServersHeader).into_owned();
                    for (name, status) in statuses {
                        txt.push_str(&format!("    {}  {}\n", name, status));
                    }
                    renderer.render(UiLine::CommandOutput(txt));
                }
            } else {
                renderer.render(UiLine::CommandOutput(
                    t(Msg::McpNoServersConfigured).into_owned(),
                ));
            }
            renderer.flush();
        }
        "welcome" => {
            // /welcome always opens the OnboardingWizard at the Confirm
            // step. The spec differentiates "empty body" (no confirm)
            // from "non-empty body" (confirm), but Renderer doesn't
            // expose body-emptiness, so we simplify: always show the
            // y/N gate. A user who explicitly typed /welcome by
            // definition wants the wizard, so a single keystroke is
            // acceptable friction; the upside is we never silently
            // clobber prior conversation.
            let _ = arg;
            *active_modal = Some(Box::new(
                crate::modals::OnboardingWizard::new_with_confirm()
                    .with_initial_language(ctx.config.language),
            ));
        }
        "worktree" => {
            handle_worktree(arg, ctx, renderer)?;
        }
        "think" => {
            let sub = arg.trim().to_ascii_lowercase();
            let provider_name = ctx.config.default_provider.clone();
            let provider = ctx.config.providers.get_mut(&provider_name);
            match provider {
                None => {
                    renderer.render(UiLine::Error(
                        t(Msg::CmdNoActiveProvider).into_owned(),
                    ));
                    renderer.flush();
                }
                Some(p) => {
                    if sub.is_empty() {
                        // Show current status
                        let enabled = p.thinking_enabled.unwrap_or(false);
                        let budget = p.thinking_budget.unwrap_or(10_000);
                        let status = if enabled { "enabled" } else { "disabled" };
                        renderer.render(UiLine::CommandOutput(
                            t(Msg::ThinkStatus { status, budget, provider: &provider_name }).into_owned(),
                        ));
                        renderer.flush();
                    } else if sub == "on" {
                        p.thinking_enabled = Some(true);
                        let budget = p.thinking_budget.unwrap_or(10_000);
                        save_and_reload(ctx, renderer);
                        renderer.render(UiLine::CommandOutput(
                            t(Msg::ThinkEnabled { budget }).into_owned(),
                        ));
                        renderer.flush();
                    } else if sub == "off" {
                        p.thinking_enabled = Some(false);
                        save_and_reload(ctx, renderer);
                        renderer.render(UiLine::CommandOutput(
                            t(Msg::ThinkDisabled).into_owned(),
                        ));
                        renderer.flush();
                    } else if let Some(rest) = sub.strip_prefix("budget") {
                        let num_str = rest.trim();
                        match num_str.parse::<u32>() {
                            Ok(n) if n >= 1024 => {
                                p.thinking_budget = Some(n);
                                save_and_reload(ctx, renderer);
                                renderer.render(UiLine::CommandOutput(
                                    t(Msg::ThinkBudgetSet { n }).into_owned(),
                                ));
                                renderer.flush();
                            }
                            Ok(n) => {
                                renderer.render(UiLine::Error(
                                    t(Msg::ThinkBudgetTooSmall { n }).into_owned(),
                                ));
                                renderer.flush();
                            }
                            Err(_) => {
                                renderer.render(UiLine::Error(
                                    t(Msg::ThinkBudgetUsage).into_owned(),
                                ));

                                renderer.flush();
                            }
                        }
                    } else {
                        renderer.render(UiLine::CommandOutput(
                            t(Msg::ThinkUsage).into_owned(),
                        ));
                        renderer.flush();
                    }
                }
            }
        }
        "plugin" => {
            // Bare `/plugin` opens the interactive manager; subcommands
            // (`marketplace …`, `install x@mp`, …) keep their old behavior.
            if arg.trim().is_empty() {
                *active_modal = Some(Box::new(crate::modals::PluginManager::open()));
            } else {
                handle_plugin(arg, ctx, renderer);
            }
        }
        "skills" => {
            // Gateway command. With no arg, list user-invocable skills
            // so the user knows what's available without opening the
            // menu (useful in non-TTY transcripts and copy/paste).
            // With an arg, treat the first word as a skill name and
            // dispatch its expanded template as a user message — same
            // path the menu's sub-mode submission lands on.
            let arg_trim = arg.trim();
            if arg_trim.is_empty() {
                // Show fully qualified names (`<plugin>:<skill>`) so users
                // can see which plugin owns each skill — bare-name listing
                // becomes ambiguous quickly once two plugins coexist.
                // `SkillRegistry::get`'s suffix-fallback still resolves
                // `/skills <bare>` for unambiguous bare names, so users
                // don't have to type the full prefix unless there's a
                // collision.
                let lines: Vec<String> = ctx
                    .skill_registry
                    .read()
                    .ok()
                    .map(|r| {
                        let mut v: Vec<String> = r
                            .user_invocable()
                            .map(|s| format!("  /skills {:<48}  {}", s.name, s.description))
                            .collect();
                        v.sort();
                        v
                    })
                    .unwrap_or_default();
                if lines.is_empty() {
                    renderer.render(UiLine::CommandOutput(
                        t(Msg::SkillsNone).into_owned(),
                    ));
                } else {
                    renderer.render(UiLine::CommandOutput(format!(
                        "{}{}\n",
                        t(Msg::SkillsAvailable),
                        lines.join("\n")
                    )));
                }
                renderer.flush();
            } else {
                let mut parts = arg_trim.splitn(2, char::is_whitespace);
                let skill_name = parts.next().unwrap_or("");
                let skill_args = parts.next().unwrap_or("").trim_start();
                // Pass the bare name straight through — `SkillRegistry::get`
                // falls back to a unique `:name` suffix match, which resolves
                // both loose skills (`skills:foo`) and plugin-contributed
                // skills (`<plugin>:foo`) without us needing to guess the
                // prefix here. A user-typed qualified name (`foo:bar`) still
                // works because exact match runs first.
                if let Some(rendered) = expand_skill(ctx, skill_name, skill_args) {
                    ctx.agent
                        .cmd_tx
                        .send(AgentCommand::SendMessage { text: rendered, images: vec![], image_markers: vec![] })
                        .ok();
                    state.on_submit();
                } else {
                    renderer.render(UiLine::Error(
                        t(Msg::SkillUnknown { name: skill_name }).into_owned(),
                    ));
                    renderer.flush();
                }
            }
        }
        "setup" => {
            // Check if the setup skill is already installed. If so, skip
            // the seed-install step and directly invoke the skill — this
            // avoids unnecessary file I/O, locking, and reloading every
            // time the user runs /setup on a project that's already set up.
            let skill_already_installed = {
                let reg = ctx.skill_registry.read().ok();
                reg.as_ref().map_or(false, |r| r.get("setup").is_some())
            };

            if skill_already_installed {
                // Fast path: skill already present — just invoke it.
                if let Some(rendered) = expand_skill(ctx, "setup", arg) {
                    renderer.render(UiLine::CommandOutput(
                        t(Msg::CmdSetupRunningSkill).into_owned(),
                    ));
                    renderer.flush();
                    ctx.agent
                        .cmd_tx
                        .send(AgentCommand::SendMessage {
                            text: rendered,
                            images: vec![],
                            image_markers: vec![],
                        })
                        .ok();
                    *setup_pending = true;
                    state.on_submit();
                } else {
                    renderer.render(UiLine::Error(
                        t(Msg::CmdSetupSkillMissing).into_owned(),
                    ));
                    renderer.flush();
                }
            } else {
                // First run: install seeds, reload, then invoke.
                renderer.render(UiLine::CommandOutput(
                    t(Msg::CmdSetupRunning).into_owned(),
                ));
                renderer.flush();

                let project_root = ctx.working_dir.clone();
                let opts = atomcode_core::setup::RunOptions::new(project_root);

                // `setup::run` is synchronous (file I/O only). Run it on the
                // current thread via `block_in_place` to avoid blocking the
                // tokio runtime — no `block_on` needed since it's not async.
                let result = tokio::task::block_in_place(|| {
                    atomcode_core::setup::run(opts)
                });

                match result {
                    Ok(report) => {
                        for line in report.render_cli().lines() {
                            renderer.render(UiLine::CommandOutput(line.to_string()));
                        }

                        // Reload skills/commands so newly-installed seeds are
                        // visible immediately — without this the user would need
                        // to restart AtomCode to see them in /skills.
                        let (skills_loaded, _) = super::reload_plugins(ctx);
                        renderer.render(UiLine::CommandOutput(
                            t(Msg::CmdSetupSkillsReloaded { count: skills_loaded }).into_owned(),
                        ));
                        renderer.flush();

                        // After installing seeds and reloading, automatically
                        // invoke the "setup" skill (atomcode-automation-recommender)
                        // so the user gets a full project analysis + recommendations
                        // in one step instead of having to run /skills setup manually.
                        if let Some(rendered) = expand_skill(ctx, "setup", arg) {
                            renderer.render(UiLine::CommandOutput(
                                t(Msg::CmdSetupRunningSkill).into_owned(),
                            ));
                            renderer.flush();
                            ctx.agent
                                .cmd_tx
                                .send(AgentCommand::SendMessage {
                                    text: rendered,
                                    images: vec![],
                                    image_markers: vec![],
                                })
                                .ok();
                            *setup_pending = true;
                            state.on_submit();
                        } else {
                            renderer.render(UiLine::Error(
                                t(Msg::CmdSetupSkillMissing).into_owned(),
                            ));
                            renderer.flush();
                        }
                    }
                    Err(e) => {
                        renderer.render(UiLine::Error(
                            t(Msg::CmdSetupError { error: &e.to_string() }).into_owned(),
                        ));
                    }
                }
                renderer.flush();
            }
        }
        other => {
            // Before reporting "unknown", check user-defined custom commands,
            // then user-invocable skills (loaded from .claude/skills,
            // .atomcode/skills, etc.). Both expand to a prompt and dispatch
            // as a regular user message.
            if let Some(rendered) = ctx.custom_commands.render(other, arg) {
                ctx.agent
                    .cmd_tx
                    .send(AgentCommand::SendMessage { text: rendered, images: vec![], image_markers: vec![] })
                    .ok();
                state.on_submit();
            } else if let Some(rendered) = expand_skill(ctx, other, arg) {
                ctx.agent
                    .cmd_tx
                    .send(AgentCommand::SendMessage { text: rendered, images: vec![], image_markers: vec![] })
                    .ok();
                state.on_submit();
            } else {
                // Unknown command — emit failure telemetry
                let available_commands: Vec<&str> = vec![
                    "help", "quit", "exit", "clear", "compact", "reload", "config",
                    "plan", "build", "session", "model", "language", "resume",
                    "rename", "provider", "status", "diff", "undo", "cost",
                    "context", "remember", "forget", "memory", "login", "logout",
                    "whoami", "upgrade", "issue", "cd", "bg", "codingplan",
                ];
                ctx.telemetry.track(atomcode_telemetry::Event::UseCommand {
                    type_: other.to_string(),
                    success: Some(false),
                    error_kind: Some(atomcode_telemetry::UseCommandErrorKind::NotFound),
                    error_data: Some(serde_json::json!({
                        "command": other,
                        "duration_ms": 0,
                        "message": format!("Unknown command: {}", other),
                        "reason": "用户输入了不存在的斜杠命令",
                        "resolution": "使用 /help 查看所有可用命令",
                        "available_commands": available_commands,
                    }).to_string()),
                });
                renderer.render(UiLine::Error(
                    t(Msg::CmdUnknownCommand { name: other }).into_owned(),
                ));
                renderer.flush();
            }
        }
    }
    Ok(())
}

/// Look up a user-invocable skill by name and expand it with the current
/// session id. Returns the rendered prompt to send as a user message, or
/// `None` if no matching skill exists.
pub(super) fn expand_skill(ctx: &LoopCtx, name: &str, arg: &str) -> Option<String> {
    let reg = ctx.skill_registry.read().ok()?;
    let skill = reg.get(name)?;
    if !skill.user_invocable {
        return None;
    }
    Some(skill.expand(arg, ctx.current_session.id.as_str()))
}

/// Handle `/plugin` subcommands: marketplace add/remove/update/list,
/// install <plugin>@<marketplace>, uninstall <plugin>@<marketplace>, list.
/// On success each mutating subcommand calls `super::reload_plugins(ctx)`
/// so newly-installed skill/command assets are visible immediately.
fn handle_plugin(arg: &str, ctx: &mut super::LoopCtx, renderer: &mut dyn Renderer) {
    let rest = arg.trim();
    let mut parts = rest.splitn(3, char::is_whitespace);
    let sub = parts.next().unwrap_or("");

    let ok = |renderer: &mut dyn Renderer, msg: String| {
        renderer.render(UiLine::CommandOutput(format!("  {}\n", msg)));
        renderer.flush();
    };
    let err = |renderer: &mut dyn Renderer, msg: String| {
        renderer.render(UiLine::Error(msg));
        renderer.flush();
    };

    match sub {
        "marketplace" => {
            let action = parts.next().unwrap_or("");
            let arg = parts.next().unwrap_or("").trim();
            match action {
                "add" => {
                    // Network-bound: git clone happens off the event loop so
                    // the input thread keeps drawing. Result event is
                    // consumed by handle_plugin_job_event and rendered there.
                    let url = arg.to_string();
                    let tx = ctx.plugin_job_tx.clone();
                    ok(renderer, t(Msg::PluginMarketplaceCloning { url: &url }).into_owned());
                    tokio::task::spawn_blocking(move || {
                        let ev = match atomcode_core::plugin::marketplace::add_marketplace(&url) {
                            Ok(info) => atomcode_core::plugin::PluginJobEvent::MarketplaceAdded(info),
                            Err(e) => atomcode_core::plugin::PluginJobEvent::Failed {
                                op: "add marketplace".into(),
                                msg: format!("{:#}", e),
                            },
                        };
                        let _ = tx.send(ev);
                    });
                }
                "remove" => match atomcode_core::plugin::marketplace::remove_marketplace(arg) {
                    Ok(()) => {
                        super::reload_plugins(ctx);
                        ok(renderer, t(Msg::PluginMarketplaceRemoved { name: arg }).into_owned());
                    }
                    Err(e) => err(renderer, t(Msg::PluginMarketplaceRemoveFailed { error: &e.to_string() }).into_owned()),
                },
                "update" => {
                    let name = arg.to_string();
                    let tx = ctx.plugin_job_tx.clone();
                    ok(renderer, t(Msg::PluginMarketplaceUpdating { name: &name }).into_owned());
                    tokio::task::spawn_blocking(move || {
                        let ev = match atomcode_core::plugin::marketplace::update_marketplace(&name) {
                            Ok(info) => atomcode_core::plugin::PluginJobEvent::MarketplaceUpdated(info),
                            Err(e) => atomcode_core::plugin::PluginJobEvent::Failed {
                                op: "update marketplace".into(),
                                msg: format!("{:#}", e),
                            },
                        };
                        let _ = tx.send(ev);
                    });
                }
                "list" => match atomcode_core::plugin::marketplace::list_marketplaces() {
                    Ok(items) if items.is_empty() => {
                        ok(renderer, t(Msg::PluginNoMarketplaces).into_owned());
                    }
                    Ok(items) => {
                        let mut lines = vec![t(Msg::PluginMarketplacesHeader).into_owned()];
                        for m in items {
                            lines.push(format!(
                                "  {}  {}  {}  ({} plugins)",
                                m.name,
                                m.source,
                                &m.git_commit[..7.min(m.git_commit.len())],
                                m.plugins.len()
                            ));
                        }
                        renderer.render(UiLine::CommandOutput(format!(
                            "  {}\n",
                            lines.join("\n  ")
                        )));
                        renderer.flush();
                    }
                    Err(e) => err(renderer, t(Msg::PluginMarketplaceListFailed { error: &e.to_string() }).into_owned()),
                },
                _ => err(
                    renderer,
                    t(Msg::PluginMarketplaceUsage).into_owned(),
                ),
            }
        }
        "install" => {
            // Parse: /plugin install <plugin>@<marketplace> [--scope user|project|local]
            let rest = parts.next().unwrap_or("").trim();
            let scope_arg = parts.next().unwrap_or("").trim();
            let scope = parse_scope_arg(scope_arg);
            match parse_plugin_arg(rest) {
                Some(PluginArg::Qualified { plugin, marketplace: mp }) => {
                    // Explicit plugin@marketplace — install directly.
                    let tx = ctx.plugin_job_tx.clone();
                    ok(renderer, t(Msg::PluginInstalling { plugin: &plugin, marketplace: &mp }).into_owned());
                    tokio::task::spawn_blocking(move || {
                        let ev = match atomcode_core::plugin::installer::install(&plugin, &mp, scope) {
                            Ok(info) => atomcode_core::plugin::PluginJobEvent::PluginInstalled(info),
                            Err(e) => {
                                if let Some(_aie) = e.downcast_ref::<atomcode_core::plugin::installer::AlreadyInstalledError>() {
                                    atomcode_core::plugin::PluginJobEvent::PluginAlreadyInstalled {
                                        id: _aie.id.clone(),
                                    }
                                } else {
                                    atomcode_core::plugin::PluginJobEvent::Failed {
                                        op: "install".into(),
                                        msg: format!("{:#}", e),
                                    }
                                }
                            }
                        };
                        let _ = tx.send(ev);
                    });
                }
                Some(PluginArg::Bare { plugin }) => {
                    // Bare plugin name — resolve across all marketplaces.
                    match atomcode_core::plugin::installer::resolve_plugin_marketplace(&plugin) {
                        Ok(matches) if matches.len() == 1 => {
                            let m = &matches[0];
                            let mp = m.marketplace.clone();
                            let resolved_plugin = m.plugin.clone();
                            let tx = ctx.plugin_job_tx.clone();
                            ok(renderer, t(Msg::PluginInstallingByName { plugin: &plugin }).into_owned());
                            tokio::task::spawn_blocking(move || {
                                let ev = match atomcode_core::plugin::installer::install(&resolved_plugin, &mp, scope) {
                                    Ok(info) => atomcode_core::plugin::PluginJobEvent::PluginInstalled(info),
                                    Err(e) => {
                                        if let Some(_aie) = e.downcast_ref::<atomcode_core::plugin::installer::AlreadyInstalledError>() {
                                            atomcode_core::plugin::PluginJobEvent::PluginAlreadyInstalled {
                                                id: _aie.id.clone(),
                                            }
                                        } else {
                                            atomcode_core::plugin::PluginJobEvent::Failed {
                                                op: "install".into(),
                                                msg: format!("{:#}", e),
                                            }
                                        }
                                    }
                                };
                                let _ = tx.send(ev);
                            });
                        }
                        Ok(matches) if matches.len() > 1 => {
                            // Multiple marketplaces contain this plugin — show a
                            // disambiguation list with the install command to use.
                            let mut msg = t(Msg::PluginInstallAmbiguous { plugin: &plugin }).into_owned();
                            for m in &matches {
                                msg.push_str(&format!("  /plugin install {}@{}\n", m.plugin, m.marketplace));
                            }
                            err(renderer, msg);
                        }
                        _ => {
                            ok(renderer, t(Msg::PluginInstallNotFound { plugin: &plugin }).into_owned());
                        }
                    }
                }
                None => err(renderer, t(Msg::PluginInstallUsage).into_owned()),
            }
        },
        "uninstall" => match parse_plugin_arg(parts.next().unwrap_or("").trim()) {
            Some(PluginArg::Qualified { plugin, marketplace: mp }) => {
                match atomcode_core::plugin::installer::uninstall(&plugin, &mp, atomcode_core::plugin::InstallScope::User) {
                    Ok(()) => {
                        super::reload_plugins(ctx);
                        ok(renderer, t(Msg::PluginUninstalled { plugin: &plugin, marketplace: &mp }).into_owned());
                    }
                    Err(e) => err(renderer, t(Msg::PluginUninstallFailed { error: &e.to_string() }).into_owned()),
                }
            }
            Some(PluginArg::Bare { plugin }) => {
                // Look up which installed plugins match this name.
                let installed = atomcode_core::plugin::installer::list_installed().unwrap_or_default();
                let matches: Vec<_> = installed
                    .into_iter()
                    .filter(|p| p.plugin == plugin || p.plugin == atomcode_core::plugin::marketplace::sanitize_name(&plugin))
                    .collect();
                match matches.len() {
                    0 => ok(renderer, t(Msg::PluginUninstallNotFound { plugin: &plugin }).into_owned()),
                    1 => {
                        let p = &matches[0];
                        let (plug, mp, scope) = (p.plugin.clone(), p.marketplace.clone(), p.scope.clone());
                        match atomcode_core::plugin::installer::uninstall(&plug, &mp, scope) {
                            Ok(()) => {
                                super::reload_plugins(ctx);
                                ok(renderer, t(Msg::PluginUninstalled { plugin: &plug, marketplace: &mp }).into_owned());
                            }
                            Err(e) => err(renderer, t(Msg::PluginUninstallFailed { error: &e.to_string() }).into_owned()),
                        }
                    }
                    _ => {
                        let mut msg = t(Msg::PluginUninstallAmbiguous { plugin: &plugin }).into_owned();
                        for p in &matches {
                            msg.push_str(&format!("  /plugin uninstall {}@{}\n", p.plugin, p.marketplace));
                        }
                        err(renderer, msg);
                    }
                }
            }
            None => err(
                renderer,
                t(Msg::PluginUninstallUsage).into_owned(),
            ),
        },
        "list" => match atomcode_core::plugin::installer::list_installed() {
            Ok(items) if items.is_empty() => {
                ok(renderer, t(Msg::PluginNoInstalled).into_owned());
            }
            Ok(items) => {
                let mut lines = vec![t(Msg::PluginInstalledHeader).into_owned()];
                for p in items {
                    lines.push(format!("  {}@{}  {}", p.plugin, p.marketplace, p.plugin_dir));
                }
                renderer.render(UiLine::CommandOutput(format!(
                    "  {}\n",
                    lines.join("\n  ")
                )));
                renderer.flush();
            }
            Err(e) => err(renderer, t(Msg::PluginListFailed { error: &e.to_string() }).into_owned()),
        },
        "reload" => {
            let (skills_loaded, warnings) = super::reload_plugins(ctx);
            let warn_count = warnings.len();
            ok(renderer, t(Msg::PluginReloadDone {
                skills: skills_loaded,
                warnings: warn_count,
            }).into_owned());
            if !warnings.is_empty() {
                for w in &warnings {
                    err(renderer, w.clone());
                }
            }
        }
        _ => err(
            renderer,
            t(Msg::PluginUsage).into_owned(),
        ),
    }
}

/// Parsed argument for `/plugin install` / `/plugin uninstall`.
/// Supports both `plugin@marketplace` (fully qualified) and bare
/// `plugin` (resolved across all marketplaces).
enum PluginArg {
    /// Explicit `plugin@marketplace` — use as-is.
    Qualified { plugin: String, marketplace: String },
    /// Bare plugin name — needs marketplace resolution.
    Bare { plugin: String },
}

fn parse_plugin_arg(s: &str) -> Option<PluginArg> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if let Some((plugin, mp)) = s.split_once('@') {
        if !plugin.is_empty() && !mp.is_empty() {
            return Some(PluginArg::Qualified {
                plugin: plugin.to_string(),
                marketplace: mp.to_string(),
            });
        }
    }
    Some(PluginArg::Bare {
        plugin: s.to_string(),
    })
}

/// Parse a `--scope user|project|local` argument.
/// Defaults to `User` if missing or unrecognized.
fn parse_scope_arg(s: &str) -> atomcode_core::plugin::InstallScope {
    // Accept both `--scope user` and bare `user`.
    let val = s.strip_prefix("--scope=").unwrap_or(s).trim();
    match val.to_lowercase().as_str() {
        "project" => atomcode_core::plugin::InstallScope::Project,
        "local" => atomcode_core::plugin::InstallScope::Local,
        _ => atomcode_core::plugin::InstallScope::User,
    }
}

/// Handle `/worktree` subcommands: create, list, done, cleanup.
fn handle_worktree(arg: &str, ctx: &mut LoopCtx, renderer: &mut dyn Renderer) -> Result<()> {
    use atomcode_core::git::worktree::WorktreeManager;

    let parts: Vec<&str> = arg.split_whitespace().collect();
    let sub = parts.first().map(|s| s.to_ascii_lowercase());

    match sub.as_deref() {
        Some("create") => {
            let branch = match parts.get(1) {
                Some(b) => *b,
                None => {
                    renderer.render(UiLine::CommandOutput(
                        t(Msg::WorktreeCreateUsage).into_owned(),
                    ));
                    renderer.flush();
                    return Ok(());
                }
            };
            let base = parts
                .get(2)
                .map(|s| (*s).to_string())
                .or_else(|| detect_current_branch(&ctx.working_dir))
                .unwrap_or_else(|| "HEAD".to_string());
            let mgr = match WorktreeManager::from_dir(ctx.working_dir.clone()) {
                Ok(mgr) => mgr,
                Err(e) => {
                    renderer.render(UiLine::Error(
                        t(Msg::WorktreeCreateFailed { error: &format!("{:#}", e) }).into_owned(),
                    ));
                    renderer.flush();
                    return Ok(());
                }
            };
            match mgr.create(branch, &base) {
                Ok(wt) => {
                    // Save original dir before switching
                    ctx.worktree_original_dir = Some(ctx.working_dir.clone());
                    apply_cd(ctx, wt.path.clone());
                    let path_str = wt.path.display().to_string();
                    renderer.render(UiLine::CommandOutput(
                        t(Msg::WorktreeCreated { branch: &wt.branch, base: &wt.base_branch, path: &path_str }).into_owned(),
                    ));
                }
                Err(e) => {
                    renderer.render(UiLine::Error(
                        t(Msg::WorktreeCreateFailed { error: &format!("{:#}", e) }).into_owned(),
                    ));
                }
            }
            renderer.flush();
        }
        Some("list") => {
            let mgr = match WorktreeManager::from_dir(ctx.working_dir.clone()) {
                Ok(mgr) => mgr,
                Err(e) => {
                    renderer.render(UiLine::Error(
                        t(Msg::WorktreeListFailed { error: &format!("{:#}", e) }).into_owned(),
                    ));
                    renderer.flush();
                    return Ok(());
                }
            };
            match mgr.list() {
                Ok(worktrees) => {
                    if worktrees.is_empty() {
                        renderer.render(UiLine::CommandOutput(
                            t(Msg::WorktreeNoActive).into_owned(),
                        ));

                    } else {
                        let mut txt = t(Msg::WorktreeActiveHeader).into_owned();
                        for (branch, path, has_changes) in &worktrees {
                            let is_current = path == &ctx.working_dir;
                            let marker = if is_current { "\u{25cf}" } else { "\u{25cb}" };
                            let change_label = if *has_changes {
                                t(Msg::WorktreeHasChanges)
                            } else {
                                t(Msg::WorktreeClean)
                            };
                            let current_hint = if is_current {
                                t(Msg::WorktreeCurrent)
                            } else {
                                "".into()
                            };

                            txt.push_str(&format!(
                                "    {} {:<16} {}  {}{}\n",
                                marker,
                                branch,
                                path.display(),
                                change_label,
                                current_hint,
                            ));
                        }
                        renderer.render(UiLine::CommandOutput(txt));
                    }
                }
                Err(e) => {
                    renderer.render(UiLine::Error(
                        t(Msg::WorktreeListFailed { error: &format!("{:#}", e) }).into_owned(),
                    ));
                }
            }
            renderer.flush();
        }
        Some("done") => {
            if let Some(original) = ctx.worktree_original_dir.take() {
                let current_branch = detect_current_branch(&ctx.working_dir);
                apply_cd(ctx, original.clone());
                let path_str = original.display().to_string();
                renderer.render(UiLine::CommandOutput(
                    t(Msg::WorktreeDoneBack { path: &path_str }).into_owned(),
                ));
                if let Some(branch) = current_branch {
                    renderer.render(UiLine::CommandOutput(
                        t(Msg::WorktreeDoneMergeHint { branch: &branch }).into_owned(),
                    ));
                }
            } else {
                renderer.render(UiLine::CommandOutput(
                    t(Msg::WorktreeNoSession).into_owned(),
                ));
            }
            renderer.flush();
        }
        Some("cleanup") => {
            let branch = match parts.get(1) {
                Some(b) => *b,
                None => {
                    renderer.render(UiLine::CommandOutput(
                        t(Msg::WorktreeCleanupUsage).into_owned(),
                    ));
                    renderer.flush();
                    return Ok(());
                }
            };
            let force = parts
                .get(2)
                .map(|s| *s == "--force" || *s == "-f")
                .unwrap_or(false);
            let manager_dir = ctx
                .worktree_original_dir
                .as_ref()
                .cloned()
                .unwrap_or_else(|| ctx.working_dir.clone());
            let mgr = match WorktreeManager::from_dir(manager_dir) {
                Ok(mgr) => mgr,
                Err(e) => {
                    renderer.render(UiLine::Error(
                        t(Msg::WorktreeCleanupFailed { error: &format!("{:#}", e) }).into_owned(),
                    ));
                    renderer.flush();
                    return Ok(());
                }
            };
            let cleanup_path = mgr
                .find_worktree_path(branch)
                .unwrap_or_else(|_| None)
                .unwrap_or_else(|| mgr.worktree_path(branch));
            let removing_current = paths_same(&cleanup_path, &ctx.working_dir);
            match mgr.remove(branch, force) {
                Ok(()) => {
                    let switched_to = if removing_current {
                        let target = ctx
                            .worktree_original_dir
                            .take()
                            .unwrap_or_else(|| mgr.repo_root().to_path_buf());
                        apply_cd(ctx, target.clone());
                        Some(target)
                    } else {
                        None
                    };
                    renderer.render(UiLine::CommandOutput(
                        t(Msg::WorktreeCleaned { branch }).into_owned(),
                    ));
                    if let Some(target) = switched_to {
                        let path_str = target.display().to_string();
                        renderer.render(UiLine::CommandOutput(
                            t(Msg::WorktreeCleanedSwitched { path: &path_str }).into_owned(),
                        ));
                    }
                }
                Err(e) => {
                    let err_msg = format!("{:#}", e);
                    if !force
                        && (err_msg.contains("untracked")
                            || err_msg.contains("modified")
                            || err_msg.contains("changes"))
                    {
                        renderer.render(UiLine::CommandOutput(
                            t(Msg::WorktreeCleanupUncommitted { branch }).into_owned(),
                        ));
                    } else {
                        renderer.render(UiLine::Error(
                            t(Msg::WorktreeCleanupFailed { error: &err_msg }).into_owned(),
                        ));
                    }
                }
            }
            renderer.flush();
        }
        _ => {
            renderer.render(UiLine::CommandOutput(
                t(Msg::WorktreeUsage).into_owned(),
            ));
            renderer.flush();
        }
    }
    Ok(())
}

/// Detect the current branch name in a directory.
fn detect_current_branch(dir: &std::path::Path) -> Option<String> {
    std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(dir)
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
            } else {
                None
            }
        })
}

fn paths_same(a: &std::path::Path, b: &std::path::Path) -> bool {
    if a == b {
        return true;
    }
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Build the `/context` report — horizontal bar + category breakdown,
/// optionally followed by the full system prompt when `show_prompt`.
///
/// Thin wrapper around `format_context_report` that pulls the inputs
/// (snapshot + model name + flag) out of state/ctx. Split for
/// unit-testability: the inner function takes plain values and can be
/// asserted on directly.
pub(super) fn render_context_report(state: &UiState, ctx: &LoopCtx, show_prompt: bool) -> String {
    format_context_report(state.last_context.as_ref(), &ctx.model_name, show_prompt)
}

/// Fetch + format the CodingPlan section appended to `/status`. Runs a
/// blocking HTTP call (~100–500ms) against `/coding-plan/status` — same
/// endpoint as the `/codingplan` flow's step 4. Falls back to a one-line
/// hint when the user isn't signed in, has no active plan, or the API
/// call fails. Never panics and never returns an error: `/status` is a
/// quick-glance command, so any fetch problem degrades into a visible
/// note instead of aborting the whole command.
fn render_codingplan_status_for_status_cmd() -> String {
    use atomcode_core::coding_plan::client::Client;

    let client = match Client::from_stored_auth() {
        Ok(c) => c,
        Err(_) => {
            return t(Msg::StatusCpNotSignedIn).into_owned();
        }
    };
    let status = match client.status_v2() {
        Ok(s) => s,
        Err(e) => {
            return t(Msg::StatusCpFetchFailed { error: &format!("{:#}", e) }).into_owned();
        }
    };
    let plan = match &status.codingplan_free {
        Some(p) => p,
        None => {
            return t(Msg::StatusCpNoActive).into_owned();
        }
    };

    let mut out = t(Msg::StatusCpLine {
        plan: &plan.plan_name,
        expires_at: &plan.expires_at,
        remaining_days: plan.remaining_days,
        total_days: plan.total_days,
    }).into_owned();
    // Prefer the per-window `rate_limit_windows` schema when present, mirroring
    // `/login` (setup.rs). When the monthly cap is exhausted the server flags it
    // via `quota_exhausted` while hiding the window (`show_enable=0`) and leaving
    // the 5h rolling window visible at a misleading 0% — so we detect exhaustion
    // via `blocking_exhausted_window` and suppress the rolling-window usage line.
    if !status.rate_limit_windows.is_empty() {
        use atomcode_core::coding_plan::setup::{blocking_exhausted_window, format_duration_secs};
        if let Some(w) = blocking_exhausted_window(&status.rate_limit_windows) {
            out.push_str(&t(Msg::StatusCpMonthlyExhausted {
                duration: &format_duration_secs(w.seconds_until_reset),
            }));
        } else {
            for w in status.rate_limit_windows.iter().filter(|w| w.show_enable == 1) {
                out.push_str(&t(Msg::StatusCpUsage {
                    usage: &w.usage_status_desc,
                    reset_at: &w.reset_at_display,
                    duration: &format_duration_secs(w.seconds_until_reset),
                }));
            }
        }
    } else if status.window_quota_exhausted {
        // Legacy backward-compat path (old server, no `rate_limit_windows`):
        // when `window_quota_exhausted` is set we suppress the usage line
        // (which the server often reports as 0% for a freshly-reset short
        // window even while the longer quota is exhausted). Showing both
        // produced the visibly contradictory `用量 0% / ⚠额度已满` pair the
        // user surfaced as the "v4.23.2 still displays it this way" report.
        if let Some(hint) = &status.window_quota_hint {
            out.push_str(&t(Msg::StatusCpWindowHint { hint }));
        } else {
            out.push_str(&t(Msg::StatusCpWindowExhausted));
        }
    } else if let Some(u) = &status.current_usage {
        out.push_str(&t(Msg::StatusCpUsage {
            usage: &u.display_desc(),
            reset_at: &u.reset_at_display,
            duration: &atomcode_core::coding_plan::setup::format_duration_secs(
                u.seconds_until_reset,
            ),
        }));
    }
    out
}

/// Pure-function core of `/context` — testable without constructing
/// `LoopCtx`. Returns the rendered CommandOutput body.
fn format_context_report(
    snapshot: Option<&crate::state::ContextSnapshot>,
    model_name: &str,
    show_prompt: bool,
) -> String {
    let header = t(Msg::CtxUsageHeader);
    let Some(snap) = snapshot else {
        return format!("  {}\n  \n  {}\n", header, t(Msg::CtxUsageNoTurns));
    };
    if snap.ctx_window == 0 {
        return format!("  {}\n  \n  {}\n", header, t(Msg::CtxUsageWaiting));
    }

    let window = snap.ctx_window;
    // Sum components excluding tool_defs (which in most providers counts
    // against input tokens but atomcode tracks separately). Clamp used to
    // window so a single oversized tool_defs doesn't drive "free" negative.
    let sys = snap.system_tokens;
    let tools = snap.tool_defs_tokens;
    let cold = snap.cold_zone_tokens;
    // Sent = everything sent minus the system message (ctx's own accounting).
    // Cold zone is injected as a System message inside `sent`, so we avoid
    // double-counting: subtract cold from sent for the "messages" bucket.
    let messages = snap.sent_tokens.saturating_sub(cold);
    let total_used = sys
        .saturating_add(tools)
        .saturating_add(cold)
        .saturating_add(messages);
    let free = window.saturating_sub(total_used);

    // Horizontal bar: 40 cells, one segment per category with a distinct glyph.
    // Terminals universally render these blocks, no ANSI color required.
    const BAR_WIDTH: usize = 40;
    let cells = |tokens: usize| -> usize {
        if window == 0 {
            return 0;
        }
        (tokens as u128 * BAR_WIDTH as u128 / window as u128) as usize
    };
    let sys_cells = cells(sys);
    let tools_cells = cells(tools);
    let cold_cells = cells(cold);
    let msg_cells = cells(messages);
    // Guard: cell sum shouldn't exceed BAR_WIDTH (rounding can give +1).
    let used_cells = sys_cells + tools_cells + cold_cells + msg_cells;
    let free_cells = BAR_WIDTH.saturating_sub(used_cells.min(BAR_WIDTH));

    let mut bar = String::with_capacity(BAR_WIDTH * 3);
    bar.push_str(&"▒".repeat(sys_cells)); // system prompt
    bar.push_str(&"▓".repeat(tools_cells)); // tool defs
    bar.push_str(&"░".repeat(cold_cells)); // cold zone
    bar.push_str(&"█".repeat(msg_cells)); // messages
    bar.push_str(&"·".repeat(free_cells)); // free

    let pct = |t: usize| -> String {
        if window == 0 {
            return "  —".to_string();
        }
        format!("{:>4.1}%", (t as f64 * 100.0) / window as f64)
    };
    let k = |t: usize| -> String {
        if t >= 1000 {
            format!("{:.1}K", t as f64 / 1000.0)
        } else {
            format!("{}", t)
        }
    };

    let used_pct = pct(total_used);

    // Localised legend labels. Pad each to the widest display-width
    // in the current locale so the `:` column aligns regardless of
    // whether the active translation uses ASCII or CJK glyphs (CJK
    // chars are 2 cells; char-count padding would mis-align).
    let l_sys = t(Msg::CtxLabelSystemPrompt).into_owned();
    let l_tools = t(Msg::CtxLabelToolDefs).into_owned();
    let l_cold = t(Msg::CtxLabelColdZone).into_owned();
    let l_msgs = t(Msg::CtxLabelMessages).into_owned();
    let l_free = t(Msg::CtxLabelFree).into_owned();
    let max_label = [&l_sys, &l_tools, &l_cold, &l_msgs, &l_free]
        .iter()
        .map(|s| unicode_width::UnicodeWidthStr::width(s.as_str()))
        .max()
        .unwrap_or(0);
    let pad_label = |label: &str| -> String {
        let w = unicode_width::UnicodeWidthStr::width(label);
        format!("{}{}", label, " ".repeat(max_label.saturating_sub(w)))
    };

    let ctx_name = if snap.ctx_name.is_empty() {
        "default"
    } else {
        snap.ctx_name.as_str()
    };

    let mut out = format!(
        "  {header}\n  \
         \n  \
         {bar}\n  \
         {used}/{window} {tokens} ({used_pct})\n  \
         \n  \
         {provider}: {model}  ·  {ctx_label}: {ctx_name}\n  \
         \n  \
         ▒ {l_sys} : {sys_s:>7}  ({sys_p})\n  \
         ▓ {l_tools} : {tools_s:>7}  ({tools_p})\n  \
         ░ {l_cold} : {cold_s:>7}  ({cold_p})\n  \
         █ {l_msgs} : {msgs_s:>7}  ({msgs_p})\n  \
         · {l_free} : {free_s:>7}  ({free_p})\n  \
         \n  \
         {msg_count}\n",
        header = t(Msg::CtxUsageHeader),
        bar = bar,
        used = k(total_used),
        window = k(window),
        tokens = t(Msg::CtxTokensSuffix),
        used_pct = used_pct,
        provider = t(Msg::CtxProvider),
        ctx_label = t(Msg::CtxCtxName),
        model = model_name,
        ctx_name = ctx_name,
        l_sys = pad_label(&l_sys),
        l_tools = pad_label(&l_tools),
        l_cold = pad_label(&l_cold),
        l_msgs = pad_label(&l_msgs),
        l_free = pad_label(&l_free),
        sys_s = k(sys),
        sys_p = pct(sys),
        tools_s = k(tools),
        tools_p = pct(tools),
        cold_s = k(cold),
        cold_p = pct(cold),
        msgs_s = k(messages),
        msgs_p = pct(messages),
        free_s = k(free),
        free_p = pct(free),
        msg_count = t(Msg::CtxMessagesInWindow { n: snap.total_messages }),
    );

    // `/context prompt` — append the full system-prompt bytes the last
    // turn sent. Kept out of the default output because the prompt is
    // 5–15 KB and would swamp the breakdown dashboard every invocation.
    // Hint line added when empty so the user knows WHY nothing showed
    // (snapshot is populated only by the rich emission path, which
    // fires once the first complete turn lands).
    if show_prompt {
        out.push('\n');
        out.push_str(&format!("  {}\n", t(Msg::CtxSystemPromptHeader)));
        if snap.system_prompt.is_empty() {
            out.push_str(&format!("  {}\n", t(Msg::CtxSystemPromptEmpty)));
        } else {
            // Indent each line with two spaces to match the surrounding
            // CommandOutput formatting (every other block uses a 2-space
            // left gutter). Avoids the model-prompt bytes looking like
            // they're escaping the command-output indentation.
            for line in snap.system_prompt.lines() {
                out.push_str("  ");
                out.push_str(line);
                out.push('\n');
            }
        }
    }

    out
}

/// Prepare + dispatch the fixissue pipeline for a given URL. Shared by:
/// (a) the `/fixissue <url>` arm, (b) the `/issue <url>` arm, and (c)
/// the event loop's post-close hook when `IssueWizard` has stashed a
/// URL in `ctx.pending_issue_url`. Handles all three `Prepared` cases
/// (Run / Skip / Err) and prints appropriate scrollback feedback. On
/// Run it arms the post-completion hook (`fixissue_pending` +
/// `fixissue_buffer`), sends `AgentCommand::SendMessage`, and flips
/// UiState to Streaming via `state.on_submit()`.
/// Currently unused — the `/fixissue` slash command was removed from
/// the menu and dispatcher. Kept (with `#[allow(dead_code)]`) so that
/// a future restoration of the slash command can re-add a one-line
/// dispatcher arm without re-implementing this whole flow. The
/// `atomcode fixissue` CLI subcommand uses `atomcode_core::atomgit::fixissue`
/// directly and does not depend on this function.
#[allow(dead_code)]
pub(crate) fn launch_fixissue(
    url: &str,
    state: &mut UiState,
    ctx: &mut LoopCtx,
    renderer: &mut dyn Renderer,
    fixissue_pending: &mut Option<atomcode_core::atomgit::IssueRef>,
    fixissue_buffer: &mut String,
) {
    match atomcode_core::atomgit::fixissue::prepare(url, &ctx.working_dir) {
        Ok(atomcode_core::atomgit::fixissue::Prepared::Run {
            prompt,
            issue_title,
            issue_number,
            issue_ref,
        }) => {
            renderer.render(UiLine::CommandOutput(format!(
                "  [fixissue] issue #{}: {}\n  Handing off to agent... (will post summary + 'fixed' label on completion)\n",
                issue_number, issue_title,
            )));
            renderer.flush();
            *fixissue_pending = Some(issue_ref);
            fixissue_buffer.clear();
            ctx.agent
                .cmd_tx
                .send(AgentCommand::SendMessage { text: prompt, images: vec![], image_markers: vec![] })
                .ok();
            state.on_submit();
        }
        Ok(atomcode_core::atomgit::fixissue::Prepared::Skip { reason }) => {
            renderer.render(UiLine::CommandOutput(format!("  {}\n", reason)));
            renderer.flush();
        }
        Err(e) => {
            renderer.render(UiLine::CommandOutput(format!(
                "  fixissue failed: {:#}\n",
                e
            )));
            renderer.flush();
        }
    }
}

/// Commit a new working-directory choice: notify the agent, update cwd +
/// previous_dir on the shared context, push the new entry into the
/// recent-dirs ring, and persist. Shared by the `/cd <path>` arm and the
/// DirPicker modal's Enter handler so both paths keep state coherent.
pub(crate) fn apply_cd(ctx: &mut LoopCtx, path: PathBuf) {
    ctx.agent
        .cmd_tx
        .send(AgentCommand::ChangeDir(path.to_string_lossy().to_string()))
        .ok();
    ctx.previous_dir = Some(std::mem::replace(&mut ctx.working_dir, path.clone()));
    ctx.runtime_factory.set_working_dir(path.clone());
    push_recent_dir(&mut ctx.recent_dirs, path);
    save_recent_dirs(&ctx.recent_dirs);
}

/// Move `new` to the front of `dirs`, dedup, and cap at `MAX_RECENT_DIRS`.
/// Does NOT persist — call `save_recent_dirs` after, or use `apply_cd`
/// which does both.
pub(crate) fn push_recent_dir(dirs: &mut Vec<PathBuf>, new: PathBuf) {
    dirs.retain(|d| d != &new);
    dirs.insert(0, new);
    dirs.truncate(MAX_RECENT_DIRS);
}

/// Read `~/.atomcode/recent_dirs.txt`. Silently drops missing directories
/// so stale entries from a deleted project don't linger in the picker.
pub(crate) fn load_recent_dirs() -> Vec<PathBuf> {
    let path = atomcode_core::config::Config::config_dir().join("recent_dirs.txt");
    std::fs::read_to_string(&path)
        .ok()
        .map(|s| {
            s.lines()
                .filter(|l| !l.trim().is_empty())
                .map(PathBuf::from)
                .filter(|p| p.is_dir())
                .take(MAX_RECENT_DIRS)
                .collect()
        })
        .unwrap_or_default()
}

/// Persist `dirs` to `~/.atomcode/recent_dirs.txt`. Best-effort — a write
/// failure (read-only HOME, permission denied) is swallowed so it can
/// never break an interactive `/cd`.
pub(crate) fn save_recent_dirs(dirs: &[PathBuf]) {
    let path = atomcode_core::config::Config::config_dir().join("recent_dirs.txt");
    let content = dirs
        .iter()
        .map(|d| d.to_string_lossy().to_string())
        .collect::<Vec<_>>()
        .join("\n");
    let _ = std::fs::write(&path, content);
}

fn resolve_cd(
    arg: &str,
    cwd: &std::path::Path,
    prev: Option<&std::path::Path>,
) -> std::result::Result<PathBuf, String> {
    let home = crate::platform::home_dir();
    let target = if arg.is_empty() {
        home.ok_or_else(|| "home directory not known".to_string())?
    } else if arg == "-" {
        prev.map(|p| p.to_path_buf())
            .ok_or_else(|| "No previous directory".to_string())?
    } else if let Some(rest) = arg.strip_prefix('~') {
        let home = home.ok_or_else(|| "home directory not known".to_string())?;
        let rest = rest.strip_prefix('/').unwrap_or(rest);
        if rest.is_empty() {
            home
        } else {
            home.join(rest)
        }
    } else {
        let p = PathBuf::from(arg);
        if p.is_absolute() {
            p
        } else {
            cwd.join(p)
        }
    };
    let canon = target
        .canonicalize()
        .map_err(|e| format!("{}: {}", target.display(), e))?;
    if !canon.is_dir() {
        return Err(t(Msg::DirNotADirectory { path: &canon.display().to_string() }).into_owned());
    }
    Ok(canon)
}

/// Build the OAuth-prompt body shown in scrollback while waiting for
/// the user to complete sign-in. Always includes the URL and ESC
/// affordance; renders a QR code above the URL when the terminal can
/// display it and the rendered block fits the current width.
///
/// Style selection (Unicode-capable terminals):
/// * `ATOMCODE_QR_DENSE=1` → force `Dense1x2` half-block (≈ 45 cols).
///   Override for users on terminals where braille mis-renders.
/// * `ATOMCODE_QR_BRAILLE=1` → force braille (≈ 23 cols). Opt-in for
///   users who know their terminal renders braille at single cell
///   width and don't add line spacing.
/// * JediTerm (Android Studio / IntelliJ / GoLand / any JetBrains IDE
///   embedded terminal) → no QR. JediTerm renders rows with extra
///   line spacing, vertically stretching every text-based QR beyond
///   scanner aspect tolerance. URLs are clickable in JediTerm
///   anyway, so URL-only is actually a better UX.
/// * Otherwise → `Dense1x2`. Block elements (U+2580–U+259F) are
///   Unicode-Neutral width and render at single cell on every
///   terminal — universally scannable.
///
/// On terminals without Unicode block-glyph support
/// (`TerminalCaps::unicode_symbols == false` — POSIX locale, dumb
/// TERM, legacy Windows conhost) we likewise skip the QR: the only
/// scannable ASCII form is ≈ 90 columns wide, which doesn't fit any
/// realistic terminal window, and those environments are typically
/// keyboard-driven anyway.
fn compose_login_chrome(url: &str, unicode: bool) -> String {
    compose_login_chrome_inner(url, unicode, cfg!(target_env = "ohos"))
}

/// Testable core of `compose_login_chrome`. `omit_url=true` drops the
/// clickable URL block — wired to `cfg!(target_env = "ohos")` by the
/// outer fn because the AtomGit OAuth callback's redirect-based flow
/// breaks on OpenHarmony PC (system browser hands control back with
/// "Invalid state" before the callback can complete; WeChat QR scan
/// works because it's a phone-side approval that posts directly to the
/// gateway). Surfacing the URL there would just lead users into the
/// dead path; QR-only is the better UX. Parameterised so the QR-present
/// vs URL-fallback shapes can be unit-tested on every platform.
fn compose_login_chrome_inner(url: &str, unicode: bool, omit_url: bool) -> String {
    let qr_block = pick_qr_style(unicode).and_then(|style| {
        let s = crate::render::qr::render_login_qr(url, style)?;
        let cols = crate::render::qr::block_cols(&s);
        let term_cols = crossterm::terminal::size().map(|(c, _)| c).unwrap_or(80);
        // Reserve 2 cols for the leading indent + 2 cols breathing room.
        if (cols as u16).saturating_add(4) <= term_cols {
            Some(
                s.lines()
                    .map(|l| format!("  {}", l))
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
        } else {
            None
        }
    });

    let mut out = String::new();
    if let Some(block) = qr_block {
        out.push_str(&t(Msg::LoginQrHeader));
        out.push_str(&block);
        if !omit_url {
            out.push_str(&t(Msg::LoginUrlAfterQr));
            out.push_str(url);
        }
    } else if omit_url {
        // No QR + URL doesn't work on this platform → there's nothing
        // actionable to offer. Tell the user explicitly rather than
        // dropping them into a screen with just "Press ESC to cancel".
        out.push_str(&t(Msg::LoginNoQrNoUrl));
    } else {
        out.push_str(&t(Msg::LoginUrlOnly));
        out.push_str(url);
    }
    out.push_str(&t(Msg::LoginCancelHint));
    out
}

/// Choose a QR rendering style for the current environment, or return
/// `None` to skip the QR entirely (URL-only output).
///
/// Pure function — env vars / TERMINAL_EMULATOR are read once and
/// passed through `decide_qr_style` so the decision logic stays unit
/// testable.
fn pick_qr_style(unicode: bool) -> Option<crate::render::qr::QrStyle> {
    let env_flag = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty()).is_some();
    let is_jediterm = std::env::var("TERMINAL_EMULATOR")
        .map(|v| v == "JetBrains-JediTerm")
        .unwrap_or(false);
    decide_qr_style(
        unicode,
        env_flag("ATOMCODE_QR_DENSE"),
        env_flag("ATOMCODE_QR_BRAILLE"),
        is_jediterm,
    )
}

/// Pure decision table for `pick_qr_style`. Explicit overrides win
/// over auto-detection; auto-detection only suppresses the QR when
/// no override is set.
fn decide_qr_style(
    unicode: bool,
    force_dense: bool,
    force_braille: bool,
    is_jediterm: bool,
) -> Option<crate::render::qr::QrStyle> {
    use crate::render::qr::QrStyle;
    if !unicode {
        return None;
    }
    if force_dense {
        return Some(QrStyle::Dense1x2);
    }
    if force_braille {
        return Some(QrStyle::Braille);
    }
    if is_jediterm {
        // JediTerm adds line spacing — every text-based QR vertically
        // stretches past scanner tolerance. URL-only is the better UX.
        return None;
    }
    Some(QrStyle::Dense1x2)
}

#[cfg(test)]
mod qr_style_tests {
    use super::*;
    use crate::render::qr::QrStyle;

    #[test]
    fn no_unicode_means_no_qr() {
        assert_eq!(decide_qr_style(false, false, false, false), None);
        // overrides do not bring back QR when terminal can't render unicode
        assert_eq!(decide_qr_style(false, true, false, false), None);
        assert_eq!(decide_qr_style(false, false, true, false), None);
    }

    #[test]
    fn jediterm_default_skips_qr() {
        assert_eq!(decide_qr_style(true, false, false, true), None);
    }

    #[test]
    fn jediterm_with_braille_override_renders_braille() {
        assert_eq!(
            decide_qr_style(true, false, true, true),
            Some(QrStyle::Braille)
        );
    }

    #[test]
    fn jediterm_with_dense_override_renders_dense() {
        assert_eq!(
            decide_qr_style(true, true, false, true),
            Some(QrStyle::Dense1x2)
        );
    }

    #[test]
    fn dense_override_wins_over_braille_override() {
        assert_eq!(
            decide_qr_style(true, true, true, false),
            Some(QrStyle::Dense1x2)
        );
    }

    #[test]
    fn braille_override_picks_braille_outside_jediterm() {
        assert_eq!(
            decide_qr_style(true, false, true, false),
            Some(QrStyle::Braille)
        );
    }

    #[test]
    fn default_is_dense1x2() {
        assert_eq!(
            decide_qr_style(true, false, false, false),
            Some(QrStyle::Dense1x2)
        );
    }
}

#[cfg(test)]
mod compose_login_chrome_tests {
    use super::*;

    const URL: &str = "https://acs.atomgit.com/login?client_id=test";

    /// Non-OH default: QR + URL fallback line both present.
    #[test]
    fn omit_url_false_keeps_url_block_alongside_qr() {
        let _g = crate::i18n::test_lock();
        crate::i18n::set_locale(crate::i18n::Locale::En);
        let s = compose_login_chrome_inner(URL, true, false);
        assert!(s.contains("scan the QR code"), "QR header missing:\n{s}");
        assert!(
            s.contains("OR open the URL below"),
            "URL fallback header missing on non-OH build:\n{s}"
        );
        assert!(s.contains(URL), "URL itself missing on non-OH build:\n{s}");
    }

    /// OH: QR present, URL line dropped entirely. The clickable AtomGit
    /// callback fails on OpenHarmony PC, so surfacing the URL would just
    /// lead the user into a dead path.
    #[test]
    fn omit_url_true_drops_url_block_when_qr_present() {
        let _g = crate::i18n::test_lock();
        crate::i18n::set_locale(crate::i18n::Locale::En);
        let s = compose_login_chrome_inner(URL, true, true);
        assert!(s.contains("scan the QR code"), "QR header missing:\n{s}");
        assert!(
            !s.contains("OR open the URL below"),
            "URL fallback header must NOT appear when omit_url:\n{s}"
        );
        assert!(
            !s.contains(URL),
            "URL itself must NOT appear when omit_url:\n{s}"
        );
    }

    /// OH + terminal too narrow / non-unicode: no QR available, URL
    /// path disabled. Must tell the user explicitly that switching to a
    /// Unicode-capable terminal is the way out, otherwise they'd see
    /// only "Press ESC to cancel" with no actionable hint.
    #[test]
    fn omit_url_true_without_qr_explains_dead_end() {
        let _g = crate::i18n::test_lock();
        crate::i18n::set_locale(crate::i18n::Locale::En);
        let s = compose_login_chrome_inner(URL, false, true);
        assert!(
            !s.contains(URL),
            "URL must not appear when omit_url:\n{s}"
        );
        assert!(
            s.contains("Unicode-capable terminal"),
            "must guide the user to a unicode terminal:\n{s}"
        );
    }

    /// Non-OH terminal too narrow / non-unicode: URL fallback header
    /// present. Regression guard for the existing pre-OH behaviour.
    #[test]
    fn omit_url_false_without_qr_shows_url_fallback() {
        let _g = crate::i18n::test_lock();
        crate::i18n::set_locale(crate::i18n::Locale::En);
        let s = compose_login_chrome_inner(URL, false, false);
        assert!(
            s.contains("Open this URL in any browser"),
            "URL fallback header missing on non-OH terminal-without-unicode:\n{s}"
        );
        assert!(s.contains(URL));
    }
}

/// Render the OAuth URL block + ESC affordance into scrollback, then
/// drive the auth/check poll loop without leaving raw mode. ESC is read
/// from `ctx.input_rx` (the same channel the main event loop uses) so
/// no termios manipulation is needed and the input box stays visible
/// alongside the URL — same UX as any other slash command.
///
/// Earlier revisions suspended `renderer` for the OAuth window and let
/// `auth::login()` println straight to stdout. That collapsed the input
/// box and (worse) wrote URL bytes on top of existing scrollback because
/// the cursor was wherever the last paint left it. The renderer-driven
/// path here avoids both problems.
fn run_oauth_with_renderer(
    renderer: &mut dyn Renderer,
    ctx: &mut LoopCtx,
) -> Result<atomcode_core::auth::AuthInfo> {
    use crossterm::event::KeyCode;
    use std::time::{Duration, Instant};
    use tokio::sync::mpsc::error::TryRecvError;

    let session = atomcode_core::auth::start_login()?;

    // QR + URL + ESC affordance go through the body via UiLine::CommandOutput
    // so they sit in scrollback above the input box exactly like any other
    // slash-command output. The QR is the primary CTA (scan with phone); the
    // URL is the fallback for users who'd rather click into a desktop browser.
    // Both render before the best-effort browser launch so the QR is on
    // screen even when the browser opens instantly.
    renderer.render(UiLine::CommandOutput(compose_login_chrome(
        session.url(),
        ctx.caps.unicode_symbols,
    )));
    renderer.flush();

    session.open_browser_best_effort();

    // Poll loop. We stay in raw mode and consume keyboard events from
    // the existing reader thread via `input_rx`. The main event loop is
    // blocked while we run, so non-ESC events queue harmlessly — we
    // drain them here so they don't fire as stale input the moment
    // we return.
    loop {
        match session.poll_once()? {
            atomcode_core::auth::PollOutcome::Authorized => break,
            atomcode_core::auth::PollOutcome::Pending => {}
        }

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if Instant::now() >= deadline {
                break;
            }
            match ctx.input_rx.try_recv() {
                Ok(crate::input::InputEvent::Key(k)) if k.code == KeyCode::Esc => {
                    anyhow::bail!("login cancelled by user");
                }
                Ok(_) => {
                    // Non-ESC events during OAuth are silently dropped:
                    // typing in the input box wouldn't render anyway
                    // (main thread blocked) and processing them after
                    // the loop would replay stale state.
                    continue;
                }
                Err(TryRecvError::Empty) => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(TryRecvError::Disconnected) => {
                    anyhow::bail!("input channel closed");
                }
            }
        }
    }

    session.finish(Some(&ctx.telemetry))
}

/// Run the full login + CodingPlan setup flow: OAuth (if needed) →
/// claim → fetch models + register providers → fetch status. Shares
/// the orchestrator with `atomcode login` / `atomcode codingplan` (CLI).
///
/// `/codingplan` used to be a separate slash command; it has been
/// folded into `/login` so users have one canonical entry point.
/// The CLI keeps `atomcode codingplan` as a hidden alias for
/// `atomcode login` to avoid breaking scripts / muscle memory.
///
/// When the user isn't already logged in we pre-flight the OAuth via
/// `run_oauth_with_renderer` so the URL/ESC UI integrates with the TUI
/// (input box stays visible). The subsequent `coding_plan::run` call
/// then sees `is_logged_in() == true` and skips its own `auth::login`
/// path — that path prints to stdout and is reserved for CLI callers.
pub(crate) fn run_login_flow(renderer: &mut dyn Renderer, ctx: &mut LoopCtx) -> Result<()> {
    // Phase 1: pre-flight login if needed.
    if !atomcode_core::auth::is_logged_in() {
        if let Err(e) = run_oauth_with_renderer(renderer, ctx)
            .and_then(|auth| atomcode_core::auth::save_auth(&auth).map(|_| auth))
        {
            // Login failed/cancelled. Surface as a top-level error;
            // skip the rest of setup since claim/models/status all
            // need a token.
            renderer.render(UiLine::Error(
                t(Msg::CodingPlanSetupFailed { error: &e.to_string() }).into_owned(),
            ));
            renderer.flush();
            return Ok(());
        }
    }

    // Phase 2: claim/models/status. Pure HTTP + config mutation — no
    // stdin / stdout interaction, so we don't need to suspend the
    // renderer. `step_login` short-circuits via `is_logged_in()`.
    //
    // If the stored token is locally valid (file present, expires_in
    // not yet past) but the server rejects it (revoked, refresh-token
    // dead, etc.), the orchestrator surfaces `report.auth_expired =
    // true`. Run OAuth *once* on that path — same flow `/login` would
    // have used — then re-run setup against the fresh token. Without
    // this the user sees "✓ already logged in as X" followed by
    // "✗ claim failed — run `atomcode login` again" and has to do
    // manually what `/codingplan` could do itself.
    let mut report = atomcode_core::coding_plan::run(&mut ctx.config, Some(&ctx.telemetry));
    if matches!(&report, Ok(r) if r.auth_expired) {
        renderer.render(UiLine::CommandOutput(
            t(Msg::CpReauthAfter401).into_owned(),
        ));
        renderer.flush();
        match run_oauth_with_renderer(renderer, ctx)
            .and_then(|auth| atomcode_core::auth::save_auth(&auth).map(|_| auth))
        {
            Ok(_) => {
                report = atomcode_core::coding_plan::run(&mut ctx.config, Some(&ctx.telemetry));
            }
            Err(e) => {
                // Re-OAuth itself failed (user pressed ESC, network
                // dead, etc.). Render the *original* report so they
                // still see what triggered the retry, then surface the
                // OAuth error.
                if let Ok(r) = &report {
                    renderer.render(UiLine::CommandOutput(r.render()));
                }
                renderer.render(UiLine::Error(
                    t(Msg::CodingPlanSetupFailed { error: &e.to_string() }).into_owned(),
                ));
                renderer.flush();
                return Ok(());
            }
        }
    }

    match report {
        Ok(report) => {
            if report.should_persist_config() {
                // Config mutation only persists when critical steps passed —
                // don't write a half-set-up config if login or models failed.
                save_and_reload(ctx, renderer);
                // Stamp the drift-monitor sync marker alongside the config
                // write. Failures are non-fatal: at worst the 24h staleness
                // hint mis-fires once.
                let _ = atomcode_core::coding_plan::write_last_sync_now();
                // Also bump our own last-seen timestamp so the cross-process
                // sync-check on the next keystroke doesn't redundantly
                // reload the config we just saved ourselves.
                ctx.monitor_last_sync_seen = atomcode_core::coding_plan::read_last_sync();
                // Sync ctx.model_name with the freshly-picked default so the
                // status line and the next turn use the right model without
                // requiring a /reload.
                if let Some(p) = ctx.config.providers.get(&ctx.config.default_provider) {
                    ctx.model_name = p.model.clone();
                }
                // Clear any stale drift warning now that we've just
                // re-synced. Also reset the cooldown so the next
                // pre-turn trigger (if conditions change) can fire
                // immediately — no need to wait 15 min after a manual
                // refresh.
                if let Ok(mut g) = ctx.monitor_warning.lock() {
                    *g = None;
                }
                ctx.monitor_last_check_at = None;
                // Same for usage slot — a fresh /login run may have
                // rotated the quota window or switched plan tiers.
                if let Ok(mut g) = ctx.usage_slot.lock() {
                    *g = None;
                }
                ctx.usage_last_check_at = None;
            }
            renderer.render(UiLine::CommandOutput(report.render()));
            renderer.flush();
        }
        Err(e) => {
            renderer.render(UiLine::Error(
                t(Msg::CodingPlanSetupFailed { error: &format!("{:#}", e) }).into_owned(),
            ));
            renderer.flush();
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Create a subdir inside a tempdir and return both. Paths are
    /// canonicalized because `resolve_cd` canonicalizes its output, and
    /// on macOS `/var/folders/...` → `/private/var/folders/...`.
    fn make_dirs() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cwd = tmp.path().canonicalize().expect("canon cwd");
        let sub = cwd.join("sub");
        std::fs::create_dir(&sub).expect("mkdir sub");
        let sub = sub.canonicalize().expect("canon sub");
        (tmp, cwd, sub)
    }

    #[test]
    fn relative_path_resolves_against_cwd() {
        let (_tmp, cwd, sub) = make_dirs();
        let got = resolve_cd("sub", &cwd, None).expect("relative resolves");
        assert_eq!(got, sub);
    }

    #[test]
    fn absolute_path_ignores_cwd() {
        let (_tmp, _cwd, sub) = make_dirs();
        let alt_cwd = PathBuf::from("/"); // unrelated cwd
        let got = resolve_cd(sub.to_str().unwrap(), &alt_cwd, None).expect("absolute resolves");
        assert_eq!(got, sub);
    }

    #[test]
    fn dash_uses_previous_dir() {
        let (_tmp, cwd, sub) = make_dirs();
        let got = resolve_cd("-", &sub, Some(&cwd)).expect("dash uses prev");
        assert_eq!(got, cwd);
    }

    #[test]
    fn dash_without_previous_errors() {
        let (_tmp, cwd, _sub) = make_dirs();
        let err = resolve_cd("-", &cwd, None).expect_err("dash w/o prev");
        assert!(err.contains("No previous directory"), "got: {}", err);
    }

    #[test]
    fn nonexistent_path_errors() {
        let (_tmp, cwd, _sub) = make_dirs();
        let err = resolve_cd("nope-does-not-exist", &cwd, None).expect_err("nonexistent errors");
        assert!(err.contains("nope-does-not-exist"), "got: {}", err);
    }

    #[test]
    fn file_path_rejected_with_not_a_directory() {
        let (_tmp, cwd, _sub) = make_dirs();
        let file = cwd.join("a.txt");
        std::fs::write(&file, "hi").expect("write");
        let err = resolve_cd(file.to_str().unwrap(), &cwd, None).expect_err("file is not a dir");
        assert!(err.contains("Not a directory"), "got: {}", err);
    }

    #[test]
    fn tilde_expands_to_home() {
        // Only run when HOME is actually resolvable; skip quietly on
        // hosts where it isn't (some CI sandboxes).
        let Some(home) = crate::platform::home_dir() else {
            return;
        };
        let Ok(canon_home) = home.canonicalize() else {
            return;
        };
        let (_tmp, cwd, _sub) = make_dirs();
        let got = resolve_cd("~", &cwd, None).expect("~ resolves");
        assert_eq!(got, canon_home);
    }

    #[test]
    fn paths_same_accepts_canonical_equivalents() {
        let (_tmp, cwd, sub) = make_dirs();
        let via_parent = sub.join("..").join("sub");
        assert!(paths_same(&sub, &via_parent));
        assert!(!paths_same(&cwd, &sub));
    }

    #[test]
    fn context_report_without_snapshot_prompts_to_run_turn() {
        let _g = crate::i18n::test_lock();
        crate::i18n::set_locale(crate::i18n::Locale::En);
        let out = format_context_report(None, "claude-opus-4-7", false);
        assert!(out.contains("run at least one turn"));
        // Never leak a window/totals when there's nothing to show
        assert!(!out.contains("tokens ("));
    }

    #[test]
    fn context_report_with_zero_window_flags_partial_stats() {
        let _g = crate::i18n::test_lock();
        crate::i18n::set_locale(crate::i18n::Locale::En);
        let snap = crate::state::ContextSnapshot {
            system_tokens: 100,
            sent_tokens: 200,
            tool_defs_tokens: 0,
            cold_zone_tokens: 0,
            total_messages: 5,
            ctx_window: 0,
            ctx_name: String::new(),
            system_prompt: String::new(),
        };
        let out = format_context_report(Some(&snap), "test-model", false);
        assert!(out.contains("waiting for first complete turn"));
    }

    #[test]
    fn context_report_renders_full_breakdown() {
        let _g = crate::i18n::test_lock();
        crate::i18n::set_locale(crate::i18n::Locale::En);
        let snap = crate::state::ContextSnapshot {
            system_tokens: 8_000,
            sent_tokens: 30_000, // includes cold
            tool_defs_tokens: 14_500,
            cold_zone_tokens: 2_000,
            total_messages: 42,
            ctx_window: 128_000,
            ctx_name: "default".into(),
            system_prompt: String::new(),
        };
        let out = format_context_report(Some(&snap), "claude-opus-4-7", false);

        // Header
        assert!(out.contains("Context Usage"));
        // Bar renders (unicode blocks present)
        assert!(out.contains("▒") || out.contains("█"));
        // Category labels
        assert!(out.contains("System prompt"));
        assert!(out.contains("Tool defs"));
        assert!(out.contains("Cold zone"));
        assert!(out.contains("Messages"));
        assert!(out.contains("Free"));
        // Token values (K formatting)
        assert!(out.contains("8.0K")); // system
        assert!(out.contains("14.5K")); // tool defs
        assert!(out.contains("2.0K")); // cold zone
        assert!(out.contains("128.0K")); // window
                                         // Messages count
        assert!(out.contains("42"));
        // ctx name + model
        assert!(out.contains("default"));
        assert!(out.contains("claude-opus-4-7"));
    }

    #[test]
    fn context_report_messages_excludes_cold_zone() {
        let _g = crate::i18n::test_lock();
        crate::i18n::set_locale(crate::i18n::Locale::En);
        // sent_tokens = messages + cold_zone (cold is injected as a
        // System message inside `sent`). Renderer must subtract so
        // "Messages" doesn't double-count.
        let snap = crate::state::ContextSnapshot {
            system_tokens: 1_000,
            sent_tokens: 10_000,
            tool_defs_tokens: 0,
            cold_zone_tokens: 3_000,
            total_messages: 10,
            ctx_window: 100_000,
            ctx_name: "default".into(),
            system_prompt: String::new(),
        };
        let out = format_context_report(Some(&snap), "m", false);
        // Messages bucket should be 10K - 3K = 7K, not 10K.
        let messages_line = out
            .lines()
            .find(|l| l.contains("Messages"))
            .expect("messages line must exist");
        assert!(
            messages_line.contains("7.0K"),
            "expected Messages=7.0K (sent-cold), got line: {}",
            messages_line
        );
    }

    #[test]
    fn context_report_free_is_nonneg_under_rounding() {
        let _g = crate::i18n::test_lock();
        crate::i18n::set_locale(crate::i18n::Locale::En);
        // Pathological: sum of components exactly = window. Free must
        // render as 0, never blow up the subtraction.
        let snap = crate::state::ContextSnapshot {
            system_tokens: 20_000,
            sent_tokens: 80_000,
            tool_defs_tokens: 20_000,
            cold_zone_tokens: 0,
            total_messages: 50,
            ctx_window: 120_000,
            ctx_name: "default".into(),
            system_prompt: String::new(),
        };
        let out = format_context_report(Some(&snap), "m", false);
        // Free = window - (sys + tools + cold + messages)
        //      = 120_000 - (20_000 + 20_000 + 0 + 80_000) = 0
        assert!(out.contains("Free"));
        // Should not panic and should render — look for "0" tokens on the Free line
        let free_line = out
            .lines()
            .find(|l| l.contains("Free"))
            .expect("free line must exist");
        assert!(free_line.contains("0"), "free line: {}", free_line);
    }

    #[test]
    fn context_report_without_show_prompt_omits_system_prompt_section() {
        let _g = crate::i18n::test_lock();
        crate::i18n::set_locale(crate::i18n::Locale::En);
        // Default `/context` output must not include the prompt dump
        // even when the snapshot HAS a cached prompt. Otherwise the
        // breakdown dashboard gets buried under 5-15K chars every call.
        let snap = crate::state::ContextSnapshot {
            system_tokens: 1_000,
            sent_tokens: 5_000,
            tool_defs_tokens: 500,
            cold_zone_tokens: 0,
            total_messages: 8,
            ctx_window: 100_000,
            ctx_name: "default".into(),
            system_prompt: "You are AtomCode.\nSOME SENTINEL BYTES".into(),
        };
        let out = format_context_report(Some(&snap), "m", false);
        assert!(
            !out.contains("SYSTEM PROMPT"),
            "SYSTEM PROMPT header must not appear in default /context output"
        );
        assert!(
            !out.contains("SOME SENTINEL BYTES"),
            "raw prompt body must not leak into default /context output"
        );
    }

    #[test]
    fn context_report_with_show_prompt_appends_cached_prompt() {
        let _g = crate::i18n::test_lock();
        crate::i18n::set_locale(crate::i18n::Locale::En);
        let snap = crate::state::ContextSnapshot {
            system_tokens: 1_000,
            sent_tokens: 5_000,
            tool_defs_tokens: 500,
            cold_zone_tokens: 0,
            total_messages: 8,
            ctx_window: 100_000,
            ctx_name: "default".into(),
            system_prompt: "You are AtomCode.\nRULE_LINE_ABC\nEND".into(),
        };
        let out = format_context_report(Some(&snap), "m", true);
        assert!(out.contains("=== SYSTEM PROMPT ==="));
        // Each line indented with leading 2 spaces — verify one line
        // survives through the gutter indentation.
        assert!(
            out.contains("  RULE_LINE_ABC"),
            "prompt lines should keep content after 2-space indent"
        );
        // Breakdown still present (append, not replace)
        assert!(out.contains("Context Usage"));
        assert!(out.contains("System prompt"));
    }

    #[test]
    fn context_report_show_prompt_with_empty_cached_prompt_shows_hint() {
        // Partial snapshot: no turn has landed rich stats yet, so
        // system_prompt is "". `/context prompt` should tell the user
        // that — not just silently show an empty section.
        let snap = crate::state::ContextSnapshot {
            system_tokens: 100,
            sent_tokens: 200,
            tool_defs_tokens: 0,
            cold_zone_tokens: 0,
            total_messages: 3,
            ctx_window: 100_000,
            ctx_name: "default".into(),
            system_prompt: String::new(),
        };
        let out = format_context_report(Some(&snap), "m", true);
        assert!(out.contains("=== SYSTEM PROMPT ==="));
        assert!(
            out.contains("(empty"),
            "empty cached prompt must show an explanation, got: {}",
            out
        );
    }
}
